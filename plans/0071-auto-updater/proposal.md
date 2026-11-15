# 0071 — In-app auto updater

## Goal

Keitora tells the user when a newer release is published on github.com/rudoi/keitora and can
install it from inside the app. One click downloads it, verifies it, installs it and relaunches
Keitora, and running herdr sessions keep going.

Today a user has to notice a new tag, download `Keitora_<v>_x64-setup.exe` or the dmg by hand, and
reinstall. The release flow is already automated (tag → Actions → published release). What's
missing is getting those releases onto machines.

## Decisions taken in planning

| Question | Decision |
|---|---|
| Transport | Plain HTTPS. The repo is public, so release discovery is an unauthenticated `GET https://api.github.com/repos/rudoi/keitora/releases/latest`, and downloads use each asset's `browser_download_url`. No `gh` CLI and no token. |
| Integrity | **Minisign signatures verified by `tauri-plugin-updater`.** CI signs the updater artifacts with a Tauri signing key. The public key ships in `tauri.conf.json`, and the plugin refuses bytes that don't verify. |
| Platforms | **Windows (NSIS x64) and macOS (Apple Silicon).** Linux reports "unsupported" and never checks. |
| Sessions on update | **Keep them running.** Applying an update leaves herdr up, the same as *quit, keep sessions*, and the new Keitora reattaches on launch. |
| Check cadence | A **background check** 30 s after launch, then every 6 h, gated by an *Automatically check for updates* setting (on by default). Settings has a **Check now** button, and the macOS app menu has a **Check for Updates…** item. Both work whatever the toggle says. |
| NSIS relaunch | Automatic. With `installMode: passive` the plugin passes `/P /UPDATE /R`, and the installer's `.onInstSuccess` relaunches `keitora-app.exe` as the user. Keitora adds nothing. |
| CI overlay | One inline `--config '<json>'` overlay, computed by a pure, tested `bundleOverlay()` in `scripts/release.lib.ts`. A tag build without the key fails. |
| Key custody | The updater key lives in the **`release` environment** (tag-only), like the macOS certificate. Windows gets a tag-only twin job, mirroring the existing macOS split. |

## Scope

**In**
- `src-tauri/src/updater/`, a new module covering:
  - release discovery over HTTPS;
  - version and asset assessment;
  - a status and phase machine;
  - the background schedule;
  - download, verify and install through `tauri-plugin-updater`;
  - cleanup of leftover temp folders.
- Commands and events for the frontend: status, check, download (with progress), apply, and the
  auto-check toggle.
- A persisted `[updates] auto_check` user setting (`keitora-tasks` `UserConfig`).
- A macOS app-menu item, *Check for Updates…*. Keitora has no custom menu today, so this means
  creating one from Tauri's default.
- Frontend: an update banner strip, an *Updates* section in the settings drawer (current version,
  toggle, *Check now*, status), toasts for manual results, and harness mocks.
- Dependency bump: tauri 2.11 → ≥ 2.12 (and `@tauri-apps/cli` to match), which the plugin requires.
- Release CI:
  - signed updater artifacts on tag runs;
  - a tag-only Windows job;
  - the macOS `app` bundle target for the updater tarball;
  - a key-id consistency check;
  - publishing the extra assets.
- Docs: CONTRIBUTING "Releases" (key generation and custody, secrets), and the `docs/build-plan.md` status.

**Out**
- Prerelease or beta channels. The release script only cuts `X.Y.Z`, and `/releases/latest` skips
  prereleases anyway.
- "Skip this version", or dismissals that survive a restart. Dismissing hides the banner for the
  session only.
- Cancelling an in-flight download.
- Linux, Intel macOS and Windows arm64. No such release artifacts exist.
- Authenticode for Windows and notarization for macOS. See Risks for why the in-app path doesn't
  need them.
- Rendering release notes as HTML or Markdown. They show as plain text (truncated), and a link opens
  the release page.
- Delta updates, rollback, and moving the app into /Applications for the user.

## Architecture

```
             ┌──────────────────────── src-tauri/src/updater/ ────────────────────────┐
 schedule ─► │ Updater (managed state: Mutex<Inner>, check lock)                      │
 (30s, 6h)   │                                                                         │
 commands ─► │ check(manual) ─► ReleaseSource::latest() ── HTTPS ──► api.github.com    │
 mac menu ─► │                   (trait: real = reqwest, tests = fake)                 │
             │        ▼                                                                │
             │ release::assess(json, current, platform)   (pure)                       │
             │        ▼                                                                │
             │ UpdateStatus ── emit (on change, or always if manual) ─► "updater://status"
             │                                                                         │
             │ download() ─► fetch .sig ─► manifest ─► plugin restore_update()         │
             │            ─► Update::download(on_chunk) ─► "updater://progress"        │
             │            (verifies minisign + signed version; bytes held in memory)   │
             │ apply()    ─► latch Quitting ─► Update::install(bytes)                  │
             │                Windows: NSIS /P /UPDATE /R, process exits, relaunch     │
             │                macOS:   bundle swapped in place ─► app.restart()        │
             └─────────────────────────────────────────────────────────────────────────┘
```

### Why the plugin, and why not its endpoint mode

`tauri-plugin-updater` already handles the parts that are easy to get wrong:
- minisign verification against the embedded public key, and the signed-version check;
- launching the NSIS installer with the right flags, then exiting;
- swapping the `.app` bundle atomically on macOS, with an admin-prompt fallback.

We use it only for **download + verify + install**.

The plugin can also discover releases itself, from an endpoint serving a static `latest.json`. We
don't use that, because CI would have to generate and upload `latest.json`, and a half-published
release could leave it stale. Instead Keitora reads the GitHub release JSON and builds the plugin's
*dynamic* manifest in memory, `{version, notes, pub_date, url, signature}` (it deserializes as
`tauri_plugin_updater::RemoteRelease`). It passes that to `Updater::restore_update` (plugin ≥ 2.14.0).

The plugin's builder rejects an empty endpoint list (`EmptyEndpoints`), and endpoints must be
`https`. So the builder is
`app.updater_builder().endpoints(vec![Url::parse("https://updates.invalid/keitora")?])?.timeout(Duration::from_secs(600)).build()?`.
- `endpoints` takes `Vec<Url>` and returns `Result`, so `url = "2"` becomes a direct dependency
  (it is already in the lockfile).
- The timeout matters because the plugin applies it to the download request; without it, a stalled
  connection hangs forever.
- `restore_update` never fetches the endpoint URL, and the `.invalid` TLD guarantees it can't resolve. Always
go through `UpdaterExt::updater_builder()`, which fills in the Windows `current_exe_args` and the
`on_before_exit` cleanup.

## Technical design

### 1. Release discovery (`updater/source.rs`)

- **The `ReleaseSource` trait** (`Send + Sync`) has two methods:
  - `latest(&self) -> Result<Option<String>, SourceError>` returns the raw JSON of
    `/repos/{owner}/{repo}/releases/latest`. A **404 → `Ok(None)`** means no stable release yet.
  - `fetch_text(&self, url: &str) -> Result<String, SourceError>` fetches the small `.sig` asset.
- **The real implementation, `GithubSource`, uses `reqwest` 0.13** (already in the lockfile through
  tauri and the plugin), as a direct dependency with `default-features = false, features = ["json", "rustls-no-provider"]`:
  - It doesn't use the `rustls` feature, which pulls in `aws-lc-rs` and needs cmake and NASM on Windows.
  - Install the ring provider (`rustls::crypto::ring::default_provider().install_default()`, ignoring
    `Err` because it may already be installed) at the top of `updater::startup`, before any reqwest
    client is built.
    - With `rustls-no-provider`, `ClientBuilder::build()` panics when no provider is installed.
    - The plugin installs ring only inside its own `check()`, which we never call, and its
      `Update::download` builds its own client.
- **Request details:**
  - `User-Agent: Keitora/<version>` (GitHub rejects requests without one);
  - `Accept: application/vnd.github+json` and `X-GitHub-Api-Version: 2022-11-28`;
  - a 30 s timeout, and redirects followed (the default; asset URLs redirect to
    `objects.githubusercontent.com`);
  - one client, built once and reused.
- **`SourceError` kinds:**
  - `Offline`, covering connect, DNS and timeout errors;
  - `RateLimited`, for 403/429 with `x-ratelimit-remaining: 0`;
  - `Http(u16)`;
  - `Body`.
  
  Every kind becomes a soft `failed` status: never a panic, never a modal. Unauthenticated requests
  are limited to 60 per hour per IP; the schedule makes about one every 6 h, plus manual checks.
- **Which repo:** owner and repo default to `rudoi` / `keitora`. The `KEITORA_UPDATE_REPO=owner/repo`
  environment variable overrides them **in every build**, so a fork or a test repo can be pointed
  at. This is safe because every download must still verify against the embedded public key.

### 2. Assessment (`updater/release.rs`, pure)

- **Parsing:** read only `tag_name`, `body`, `published_at`, `html_url` and
  `assets[].{name, browser_download_url}`.
- **`tag_version(tag)`:** strip an optional leading `v` and parse with `semver` 1 (a new direct
  dependency, already in the lockfile).
- **`assess(json, current: &semver::Version, platform: Platform) -> Assessment`.** `Platform` is
  `WindowsX64 | MacArm64`; unsupported platforms never reach `assess` (§3). Checks run in this order:
  1. Malformed JSON → `Failed("malformed release")`.
  2. Bad tag → `Failed("bad tag …")`.
  3. Tag version ≤ current → `UpToDate`. This is checked before any asset checks.
  4. The installer asset is missing → `Failed("release has no <platform> installer")`.
  5. Its `<asset>.sig` is missing → `Failed("release is not signed")`.
  6. Otherwise → `Available(Candidate { version, notes, release_url, published_at, asset_url, sig_url })`.
- **Installer asset names:**
  - `WindowsX64`: exactly `Keitora_<tagver>_x64-setup.exe`. A setup.exe for any other version is
    "missing".
  - `MacArm64`: exactly `Keitora.app.tar.gz`. Tauri names the updater tarball after the `.app`,
    with no version or arch in the name.
- **`notes`:** the release `body` truncated to 4 000 chars on a char boundary, with `…` appended
  when cut. `--generate-notes` bodies can be long.
- **`manifest(candidate, sig_text) -> serde_json::Value`** returns
  `{version, notes, pub_date: published_at, url: asset_url, signature: sig_text.trim()}`.
- **The running version:** `app.package_info().version`, threaded in as a `semver::Version`.

### 3. Status, phases and schedule (`updater/mod.rs`)

**`UpdateStatus`** goes to the frontend, serde-tagged by `type`, in snake_case:
- `up_to_date`
- `available { version, notes, release_url, published_at }`
- `failed { message }`
- `unsupported`, produced by `Updater` itself on any platform that isn't Windows-x64 or macOS-arm64
  (decided with `cfg!(target_os)` and `cfg!(target_arch)`). It is never produced by `assess`.

**`Phase`** is internal: `Idle → Downloading → Ready → Applying`.
- `Ready` holds `{ version, update: tauri_plugin_updater::Update, bytes: Arc<Vec<u8>> }`, roughly
  10–20 MB in memory.
- `Update` is `Clone + Send + Sync`.

**`PhaseKind`** is what the frontend sees:
- it serializes as `"idle" | "downloading" | "ready" | "applying"`;
- the `Ready` payload never leaves Rust.

**`Inner`** is `{ last: UpdateStatus, candidate: Option<Candidate>, phase: Phase }`, behind a
`Mutex`, plus a separate `check_lock: Mutex<()>`. Overlapping checks serialize: the second check
waits, then returns the fresh `last`.

**Rules:**
- **Sticky candidate:** a failed check after a successful `available` keeps reporting `available`.
- **Frozen candidate:** while the phase is `Downloading`, `Ready` or `Applying`, a check updates
  `last` but **does not replace `candidate`**. A newer release found mid-download is offered after a
  restart.
- **Emitting `updater://status`:**
  - The payload is `{ ...status, manual: bool }`.
  - Background checks emit only when the status changes. Manual checks (the command or the menu)
    always emit.
- **Startup:** `host.rs` `.setup` always calls `updater::startup(app.handle().clone())`, which:
  1. installs the rustls ring provider;
  2. manages the `Updater` state;
  3. runs the temp-folder cleanup;
  4. spawns the schedule thread only when `!cfg!(debug_assertions)` and the platform is supported.
- **The schedule:**
  - A `std::thread`, spawned by `startup`.
  - It sleeps 30 s, then checks every 6 h. Before each check it re-reads
    `UserConfig::load()…updates.auto_check()`, so the toggle takes effect without a restart.
  - It isn't spawned at all in debug builds (`cfg!(debug_assertions)`, a new pattern in this crate)
    or on unsupported platforms.
  - It lives until the process exits. `AppHandle` has no weak form, so there's no shutdown handshake.
- **Debug builds** still register the commands and the menu item, so manual **check and download**
  work in `yarn tauri dev`. Negative tests (a) and (b) rely on this.
  - Under `cfg!(debug_assertions)`, `update_apply` returns the error *Updates can't be installed from a
    development build.* before touching the phase. In dev the plugin's extract path is
    `target/debug`.
  - The downgrade test uses a bundled build.
- **Startup cleanup** (run by `startup`, in every build): remove the temp-dir folders whose
  names start with `"{package_info().name}-"` and contain `"-updater-"`. That is the plugin's
  `tempfile` prefix `"{app_name}-{version}-updater-"`, left behind on Windows because the process
  exits mid-install.
  - The matcher is pure and tested against `Keitora-0.4.0-updater-AbC123` (match) and
    `Keitora-foo`, `Other-0.4.0-updater-x` (no match).
  - Deletion is best-effort.
- **Logging:** log only outcome kinds (`up_to_date`, `available 0.4.1`, `failed: offline`), never
  response bodies.

### 4. Download and apply

**`download()`** is async; the network work runs on the async runtime.
1. Refuse unless there is a `candidate`.
   - If the phase is `Ready`, return `Ok` immediately. The frozen-candidate rule means it is always
     for the current candidate.
   - If it is `Downloading` or `Applying`, return the error "already downloading" or
     "already applying".
   - Only `Idle` proceeds.
2. Set the phase to `Downloading`.
3. Fetch the `.sig` **now**, from `sig_url`, so a re-uploaded asset gets a fresh signature, then
   build the manifest.
4. Call `restore_update(manifest)`.
   - `Ok(None)` means the release isn't newer, which is defensive → `failed`.
   - `restore_update` forces `require_signed_version`, so the `.sig` must embed the version, which
     the current Tauri CLI does.
5. Call `Update::download(on_chunk, on_finish)`.
   - `on_chunk(len, total)` receives each chunk's length, so a running sum is kept.
   - Emit `updater://progress { phase, downloaded, total }`, throttled to at most every 250 ms, plus a
     final emit. The same event is also emitted on **every phase change**.
   - **The plugin verifies the signature and the signed version at the end of `download`, and
     again in `install`.**
6. On success the phase becomes `Ready { version, update, bytes }`. On any failure it resets to
   `Idle` and the error comes back as a string.

**`apply()`**
1. In a debug build, return the development-build error.
   Otherwise, refuse unless the phase is `Ready`, then move it to `Applying`.
2. **macOS location guard:** if `current_exe()` is under `/Volumes/` or contains `/AppTranslocation/`,
   reset to `Ready` and return the error "Move Keitora to your Applications folder, then try again."
   The pure path check is unit-tested.
3. Set the existing `Quitting` latch (`host.rs`), and keep the prior value so it can be restored.
   - `Quitting` becomes `pub(crate)`, with a `pub(crate) fn set(&self, v: bool) -> bool` that returns
     the previous value.
   - `apply` clones the `Update` and the `Arc` bytes into the blocking closure, so the phase can return
     to `Ready` on error.
   - On Windows this doesn't strictly matter, because the plugin `process::exit`s.
   - On macOS `app.restart()` sends `ExitRequested`, not `CloseRequested`.
   - Setting it anyway keeps every exit path consistent with the quit commands.
4. Call `Update::install(bytes)` inside `tauri::async_runtime::spawn_blocking`.
   - **herdr is not stopped** ("keep sessions").
   - **Windows:** the plugin `ShellExecute`s the setup.exe with `/P /UPDATE /R /ARGS …`, runs
     `cleanup_before_exit`, and `process::exit(0)`s. The installer waits for no process, because the
     app is gone. It copies the files and relaunches `keitora-app.exe` as the user. The per-user
     install (`bundle.windows.nsis.installMode: currentUser`) means no UAC.
   - **macOS:** the plugin swaps the bundle in place, prompting for admin only if the swap fails
     with `PermissionDenied`, then returns. Keitora calls `app.restart()`.
5. On error, restore the latch, set the phase back to `Ready` so the user can retry or keep the
   bytes, and return the message.

### 5. Commands, events, menu

Everything below lives in `updater/commands.rs`. It's registered in `host.rs` `generate_handler!`,
and the module is declared in `src-tauri/src/lib.rs`.

| Command | Returns | Notes |
|---|---|---|
| `update_status` | `{ status: UpdateStatus, phase: PhaseKind, progress: { downloaded, total } \| null, current_version: string }` | Called when the UI mounts; seeds the phase and progress after a webview reload. |
| `update_check` | `UpdateStatus` | A manual check (always emits). The frontend uses the return value only to clear `checking`; toasts and state come from the event. |
| `update_download` | `()` | Progress arrives over `updater://progress`. |
| `update_apply` | `()` | Only returns on error. |
| `set_update_auto_check(enabled: bool)` | `()` | Persist-only, modelled on `set_skip_permissions` (`host.rs`). |

**Events:**
- `updater://status`, carrying `{ ...UpdateStatus, manual }`.
- `updater://progress`, carrying `{ phase: PhaseKind, downloaded: u64, total: Option<u64> }`. It is
  emitted on chunks (throttled) and on every phase change.
- A menu-triggered check emits `manual: true`.

**The webview gets no `updater:*` capability.** The flow runs entirely from Rust, so the frontend can
never point the plugin at another URL. `capabilities/default.json` is unchanged.

**The macOS menu** (`#[cfg(target_os = "macos")]`, in `.setup`):
1. Build `Menu::default(app.handle())?`.
2. Take `menu.items()?[0]`, which is `MenuItemKind::Submenu(s)`, the app submenu.
3. Call `s.insert(&MenuItem::with_id(app, "check_for_updates", "Check for Updates…", true, None::<&str>)?, 1)?`.
   Position 1 is after *About* and before the separator.
4. Call `app.set_menu(menu)?`.
5. Handle the event in `Builder::on_menu_event(|app, ev| …)`, which runs on the main thread:
   - match `ev.id() == "check_for_updates"`;
   - `std::thread::spawn` a manual check through the same `Updater::check(manual = true)` and
     `check_lock` path.

### 6. Settings

- **`crates/keitora-tasks/src/config.rs`:** a new `UpdatesConfig { auto_check: Option<bool> }`.
  - Its `auto_check()` returns `unwrap_or(true)`, and `is_empty()` is true when `auto_check` is `None`.
  - It sits on `UserConfig` as
    `#[serde(default, skip_serializing_if = "UpdatesConfig::is_empty")] pub updates: UpdatesConfig`.
  - Tests:
    - the default is on;
    - TOML round-trip;
    - `[updates]` is omitted when unset;
    - `auto_check = false` survives a save and reload;
    - an existing config without `[updates]` still loads.
- **`host.rs`:** `SettingsUser` gains `auto_check_updates: bool`, filled in `get_settings`. The new
  `set_update_auto_check` command loads, sets, and saves under `UserConfig::write_lock()`, following
  `set_skip_permissions`.
- **Frontend:** `src/lib/settings.ts` gains the field and a `setUpdateAutoCheck` wrapper, and
  `.harness/app.tsx` `SETTINGS.user` gains `auto_check_updates: true`.

### 7. Frontend

**`src/lib/updates.ts`**, pure logic tested with vitest (node environment):
- `UpdatesState { status, phase, progress, checking, error, dismissedVersion, currentVersion }`.
- `updatesReducer` handles status events, progress events and the check/download/apply lifecycle.
  - A manual `available` event clears the dismissal.
- `bannerOf(state)` returns the banner model, or `null`. It is `null` when the status isn't
  `available`, or when the version was dismissed.
- Toasts and reducer updates come **only** from `updater://status` and `updater://progress` events.
  `UpdatesProvider` seeds the phase and progress from `update_status` on mount.
- `toastFor(event)` toasts only manual results: *Keitora is up to date.* or *Update check failed: …*.
  Background failures appear only in Settings.
- `formatProgress(downloaded, total)` returns `Downloading 42%…`, or `Downloading 3.1 MB…` when the
  total is unknown.
- `installUpdate(deps)` downloads, then applies. It is testable with injected deps; a download
  failure leaves no apply call, and an apply failure surfaces the error.

**Typed wrappers** for the commands and events go in `src/lib/ipc.ts`, through `getTransport()`.

**`UpdatesProvider` / `useUpdates`:**
- Mounted once in `App.tsx`, inside the toast provider.
- It subscribes to both events, calls `update_status` on mount, and owns the reducer.
- It holds a re-entrancy guard (one install at a time).

**`UpdateBanner.tsx`**, a strip modelled on `RecoveryBanner` and mounted next to it, under `TopBar`:
- Text: *Keitora 0.4.1 is available. Your sessions keep running.*
- Buttons:
  - *Install & restart*, whose label becomes *Downloading 42%…* and then *Restarting…*;
  - *Release notes*, which uses `openUrl` from `@tauri-apps/plugin-opener` (already allowed by
    `opener:default`);
  - *Dismiss*.
- An install error switches it to the danger tone and shows the message; the button becomes *Retry*.

**The *Updates* section in `SettingsDrawer.tsx`**, placed after *Daemon*:
- *Keitora 0.4.0* (from `update_status.current_version`; there's no version display anywhere today);
- an *Automatically check for updates* toggle;
- a *Check now* button with a spinner;
- the last status line;
- when an update is available, *Install & restart* and the notes shown as plain pre-wrapped text;
- on `unsupported`, a *Updates aren't available on this platform.* message, with the controls disabled.

**Copy is sentence case**, following the existing banners and toasts (`recovery.ts`, `App.tsx` toasts).

**`.harness/app.tsx`** gets mock cases for every new command. `update_status` returns `available 0.4.1`
so the banner and the section can be screenshotted.

### 8. Tauri config and dependencies

- `src-tauri/tauri.conf.json` gains:
  ```json
  "plugins": { "updater": { "pubkey": "<base64 minisign public key>", "windows": { "installMode": "passive" } } }
  ```
  It deliberately has **no `endpoints`** (they're set in code, §Why the plugin) and **no
  `bundle.createUpdaterArtifacts`** (CI-only overlay, §9). With the flag committed, every local and
  PR build would fail for lack of the private key.
- **Verified in tauri-cli 2.12 `sign_updaters`:** with `createUpdaterArtifacts` off, a configured
  pubkey without a private key is a no-op, and `tauri dev` never signs.
  - Local `yarn tauri dev` and `yarn tauri:bundle` need no key and produce no updater artifacts.
  - The committed pubkey only verifies downloads.
- **Generating the key:** the implementer runs `yarn tauri signer generate -w <scratch path>` and commits
  only the public key.
  - The private key and password are handed to the maintainer to store as `release`-environment
    secrets, plus an offline copy.
  - They are **never committed or echoed into logs**.
  - If key generation can't happen during implementation, the pubkey is the exact constant
    `REPLACE_WITH_UPDATER_PUBKEY`. The plugin requires the field to exist.
    - The config test accepts either a valid minisign pubkey or that constant.
    - `bundleOverlay` on a tag throws `UPDATER_PUBKEY_PLACEHOLDER` when the configured pubkey is the
      constant (it takes the pubkey as an input), and so does `check-updater-sig`.
    - So a tag build can't ship it, but PR builds stay green.
- `host.rs` registers `tauri_plugin_updater::Builder::new().build()` next to opener, dialog and
  notification.
- **The config test** (Rust, reading `tauri.conf.json`) checks that:
  - the pubkey is present and either decodes as a minisign public key or is exactly the placeholder;
  - the updater `installMode` is `passive`;
  - there are no endpoints;
  - there's no `createUpdaterArtifacts`;
  - `bundle.targets` still lacks `app`, because the overlay adds it.
- **Version bump:**
  - `cargo update -p tauri` to the newest 2.x (≥ 2.12), plus the matching `tauri-build` and the
    related crates the resolver moves;
  - `@tauri-apps/cli` and `@tauri-apps/api` in `package.json` raised to the same minor;
  - `tauri-plugin-updater = "2.14"`.
  - Verify the whole workspace builds and tests stay green after the bump, before any updater code
    lands. This is its own commit.
- **New direct crates** in `src-tauri/Cargo.toml`:
  - `tauri-plugin-updater = "2.14"`;
  - `reqwest = { version = "0.13", default-features = false, features = ["json", "blocking", "rustls-no-provider"] }` (the `ReleaseSource` trait is synchronous: `GithubSource` uses `reqwest::blocking`, and callers run it on the schedule thread or in `spawn_blocking`);
  - `rustls = { version = "0.23", default-features = false, features = ["ring"] }`;
  - `semver = "1"`;
  - `url = "2"`.
  
  `Cargo.lock` has no rustls today. The plugin and this crate both resolve to a single 0.23.x.

### 9. Release CI (`.github/workflows/release.yml`, `scripts/`)

**`bundleOverlay({ target, updaterKey, ref, pubkey })`** in `scripts/release.lib.ts` is pure:
- `target: "windows" | "macos"`, `updaterKey: "true" | "false" | undefined`, `ref`: `GITHUB_REF`.
- On a tag ref (`refs/tags/v*`) without the key it **throws** `UPDATER_KEY_MISSING`.
- With the key, it returns `{"bundle":{"createUpdaterArtifacts":true}}`. For macOS it also sets
  `"targets":["app","dmg"]`.
  - Without `app`, the bundler builds the `.app` only as an intermediate for the dmg, deletes it,
    and makes **no** tarball. It only warns.
  - Arrays are replaced on merge, so this overrides `["dmg","nsis"]` on the macOS runner only.
- Without the key, it returns `{}`.
- **Tests** in `scripts/release.lib.test.ts`: key → flag (+ targets on macOS); no key → `{}`; tag
  without key → throws; non-tag without key → `{}`; tag with the placeholder pubkey → throws; the
  output parses as JSON.

**`scripts/bundle-overlay.ts`** is a thin CLI that writes `bundleOverlay(...)` to the file named by
`--out`, taking its inputs from `--target`, `UPDATER_KEY`, `GITHUB_REF` and the pubkey in
`tauri.conf.json`.
- Writing a file avoids yarn-1 and Windows shell quoting of inline JSON.
- Build steps pass `--config "$RUNNER_TEMP/bundle-overlay.json"`. Tauri accepts a JSON file path, and
  the `tauri:bundle` script's own `--config` comes first, so the overlay merges after it.

**`updaterKeyIdMatches(pubkeyB64, sigB64)`** (pure, tested) compares the key ids. Tauri stores both
values as base64 of the whole minisign *file text*, so each is decoded twice:
1. Base64 → the text.
2. Take the first non-comment line.
3. Base64 → bytes.

The key id is bytes `[2..10]` in both: after the 2-byte algorithm tag, before the key or the
signature.
- A thin CLI, `scripts/check-updater-sig.ts <sig path>`, reads the pubkey from `tauri.conf.json`.
- Tag jobs run it after the build. **A key/pubkey mismatch only warns at build time** and would ship
  updates that no installed copy can verify, so this check fails the job.

**Jobs:**
- **`build-windows`** becomes non-tag-only (`if: !startsWith(github.ref, 'refs/tags/v')`). It has no
  secrets and builds unsigned, as today.
- **`build-windows-release`** is new: tag-only, `environment: release`.
  - It runs the tag==version check.
  - *Detect the updater key*: `has_key=${{ secrets.TAURI_SIGNING_PRIVATE_KEY != '' && 'true' || 'false' }}`.
  - *Compute the overlay*: `yarn --silent tsx scripts/bundle-overlay.ts --target windows --out "$RUNNER_TEMP/bundle-overlay.json"`.
  - The build step is the **only** one with `TAURI_SIGNING_PRIVATE_KEY*` in its env:
    `yarn tauri:bundle --config "$RUNNER_TEMP/bundle-overlay.json"`.
  - It then runs `check-updater-sig` on the `.exe.sig` and uploads the setup.exe plus its `.sig`.
  - The artifact name is `keitora-windows`, as before.
- **`build-macos-release`** gets the same detect and overlay steps.
  - `scripts/build-dmg.sh` reads an optional `KEITORA_BUNDLE_OVERLAY`, a file path, and appends
    `${KEITORA_BUNDLE_OVERLAY:+--config "$KEITORA_BUNDLE_OVERLAY"}` to its `tauri build` call.
  - The script's "Tauri deletes Keitora.app" comment and logic are updated, because with `app` in
    targets the `.app` now stays.
  - The signing key env is added to the *Build and verify the signed dmg* step only.
  - Afterwards the job collects `target/release/bundle/macos/Keitora.app.tar.gz` and its `.sig`
    alongside the dmg (one flat artifact folder), and runs `check-updater-sig` on the `.sig`.
  - The tarball is made by the bundler from the `.app` it has just codesigned with the pinned
    identity. That identity is the stable designated requirement, so Screen Recording grants
    survive an in-app update.
- **`build-macos`** (non-tag) is unchanged and unsigned.
- **`publish`:**
  - `needs` gains `build-windows-release`, and its `if` requires that job instead of `build-windows`.
  - It uploads five files:
    - `Keitora_<v>_x64-setup.exe` and `Keitora_<v>_x64-setup.exe.sig`;
    - `Keitora_<v>_aarch64.dmg`;
    - `Keitora.app.tar.gz` and `Keitora.app.tar.gz.sig`.
  - It checks that each exists, as it already does.
- **`pull_request.paths`** gains `scripts/release.lib.ts`, `scripts/bundle-overlay.ts`,
  `scripts/check-updater-sig.ts` and `scripts/build-dmg.sh`.
- PR, schedule and dispatch runs never see the key and never sign. A same-repo PR can't read
  `release`-environment secrets, because that environment is restricted to `v*` tags
  (CONTRIBUTING "Releases").

### 10. Docs

- **CONTRIBUTING "Releases"** gets a new *Updater signing* subsection covering:
  - generating the key;
  - where the public key lives;
  - the two `release`-environment secrets;
  - the offline backup;
  - **"losing the key strands every installed copy: users must reinstall by hand"**;
  - "rotating the key requires shipping the new pubkey in a release signed by the old key";
  - the `KEITORA_UPDATE_REPO` override for testing.
- **`docs/build-plan.md`:** an updater status line.

## Bootstrapping and first-update testing

- **Bootstrapping:** installs at v0.3.0 and earlier have no updater. The first release with this
  feature has to be installed by hand; in-app updates work from that release onwards.
- **Downgrade test,** once a signed release exists: build locally with the version temporarily set
  lower (e.g. `0.0.9`) and the real pubkey. Run it, *Check now*, then install. Do this on Windows and
  on macOS (from /Applications).
- **Negative tests:**
  - (a) `KEITORA_UPDATE_REPO` set to a test repo whose release is signed by a different key → the
    download fails verification and nothing is installed;
  - (b) a release missing its `.sig` → *release is not signed*;
  - (c) running from the mounted dmg on macOS → the location error.
- **Live-session smoke:** apply an update with live herdr sessions. They survive, the new Keitora
  reattaches, and `%LOCALAPPDATA%\keitora\bin\keitora.exe` refreshes on the next launch, or is kept
  if it's locked (`refresh_alias`).
- **The first real update** (→ the next patch) confirms the macOS bundle swap and the persistence of
  the Screen Recording grant.

## Testing

- **Rust (`#[cfg(test)]`):**
  - **`release.rs`:**
    - newer, equal and older versions;
    - `v`/no-`v` tags;
    - numeric ordering (0.10 > 0.9);
    - bad tag;
    - missing installer;
    - an installer for the wrong version;
    - missing `.sig`;
    - each platform's asset name;
    - malformed JSON;
    - notes truncation on a multibyte boundary;
    - manifest shape (trimmed signature, `pub_date`).
  - **A contract test:** the manifest deserializes into `tauri_plugin_updater::RemoteRelease`.
  - **`mod.rs` with a fake `ReleaseSource` and a fake event sink.** Emission is abstracted behind a
    small `StatusSink` trait so that tests need no `AppHandle`. Cases:
    - emit on change vs manual;
    - the sticky candidate;
    - the frozen candidate during `Downloading`/`Ready`;
    - overlapping checks serialized;
    - download/apply refused in the wrong phase;
    - the `unsupported` platform;
    - a 404 → `up_to_date`;
    - rate limited → `failed`;
    - the schedule delays (pure `delay_before(n)`);
    - the auto-check gate;
    - the leftover-folder matcher;
    - the macOS location guard (a pure path fn).
  - **The config test** from §8.
  - **`UpdatesConfig` serde tests** in `keitora-tasks`.
  - **No test touches the network, the plugin's install, or herdr.**
- **TypeScript (vitest):**
  - `updates.test.ts`: the reducer, `bannerOf`, `toastFor`, `formatProgress`, and `installUpdate`
    ordering and failures.
  - `release.lib.test.ts`: `bundleOverlay` and `updaterKeyIdMatches`, using a test keypair
    generated for the fixture. **The fixture is a throwaway key, never the real one.**
- **Harness:** screenshots of the banner (available, downloading, error) and the *Updates* section.

## Risks

- **Signing key loss** strands installed copies, so the custody rules are documented. **Key leak:**
  someone who also controls a GitHub release could ship a malicious update. The key lives in the
  tag-restricted `release` environment and reaches only the build steps.
- **The tauri bump** (2.11 → ≥ 2.12) could change behaviour elsewhere. It is isolated as its own
  first commit and validated with the full test and build run before updater work starts.
- **Unsigned Windows installer and SmartScreen:** files that reqwest downloads carry no
  Mark-of-the-Web, and the plugin launches the installer directly, so no SmartScreen prompt is
  expected. To be confirmed in the downgrade test.
- **macOS quarantine:** in-app downloads get no quarantine xattr, and the bundle keeps its
  self-signed identity. Running from a dmg or under App Translocation is refused with guidance.
- **herdr and the install directory:**
  - herdr outlives the app and may hold its working directory. An NSIS upgrade overwrites only its
    own files.
  - The in-use CLI copy lives in `%LOCALAPPDATA%\keitora\bin`, outside the installer's file list, and
    is refreshed later.
  - Covered by the live-session smoke.
- **The GitHub API rate limit** behind a shared NAT is a soft failure; the next check is 6 h later.
- **Version and tag drift** is already blocked by the tag == `package.json` check. The plugin also
  rejects a mismatched signed version.
