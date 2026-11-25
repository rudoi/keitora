# 0076 — Dependency setup: install herdr on first run, doctor for the rest

## Goal

A fresh Keitora install should get to a working terminal without the user hunting
down herdr. Keitora detects its external dependencies, offers to install or upgrade
herdr (the only one it manages) from inside the app, and tells the user plainly
what's missing for everything else (git, claude, codex).

Today Keitora finds an herdr you already installed (`discover_exe`,
`src-tauri/src/backend/herdr/env.rs:434`, implemented by `discover_exe_with` at
`:494-547`) and, when none is found, the backend becomes `ExeMissing`
(`exe_missing.rs:102` `ExeMissing`, `NOT_FOUND_CODE = "herdr_not_found"` at `:17`) and the rail's
`OfflineCard` (`src/components/Rail.tsx:166`) shows "herdr was not found … press
Restart". Nothing installs herdr, and nothing compares its version or protocol.
The status loop reads `version` into `DaemonStats` for display only
(`status_loop.rs:515`).

## Decisions

| Question | Decision |
|---|---|
| What gets installed | **herdr only.** git / claude / codex are detected and reported ("doctor"); claude/codex updates stay with the existing `agent_updates` flow. |
| When setup runs | **In-app**: first-run onboarding step, Install/Upgrade on the rail offline card, a Settings section. No NSIS/dmg installer hooks. Never installs silently. |
| How herdr is obtained | **Tiered**: (1) herdr's official install script, run hidden; (2) direct download from the release manifest, sha256-verified; (3) manual instructions + "Browse for herdr" picker. |
| Versions | **Floor 0.9.5, protocol 22.** Install the newest release that is **compatible** (protocol 22, ≥ 0.9.5). An older herdr triggers an **upgrade prompt** (dialog, user confirms). |
| Which herdr Keitora uses after an install | **Pinned explicitly**: every successful install/upgrade/browse writes the binary path to `[pty] herdr_bin`. No discovery reordering. |
| Onboarding | Skippable; the rail card and Settings remain the fallback surfaces. |
| Platforms | windows-x86_64, macos-aarch64, macos-x86_64 (Intel costs nothing). |

## Scope

**In**
- New Rust module `src-tauri/src/deps/`: manifest client + release selection,
  tier choice, tier 1/2/manual installers, version floor, upgrade orchestration,
  doctor. Registered with `mod deps;` in `src-tauri/src/lib.rs` (alphabetical, after
  `db_watch`).
- Writing `[pty] herdr_bin` after install: a new associated fn
  `UserConfig::set_herdr_bin(path: Option<PathBuf>) -> Result<(), ResolveError>` in
  `crates/keitora-tasks/src/config.rs`, modeled on `UserConfig::set_skip_permissions`
  (`:1142`: `write_lock()`, `load()`, mutate, `save()`, which is `save_to` + `write_atomic`).
  It writes the global user file `user_config_path()` (`:845`:
  `<KEITORA_CONFIG_HOME or OS config dir>/keitora/keitora.toml`). The field and its
  validation already exist (`PtyConfig::herdr_bin`, `:453-530`).
- A server-version floor check on the status path, a `too_old` /
  `protocol_mismatch` / `restart_to_finish` state exposed to the frontend, and an
  upgrade prompt.
- Tauri commands + `deps://status` / `deps://progress` events mirroring the updater
  (exact names and payloads under Implementation notes).
- Frontend: onboarding step, rail card actions, Settings "Dependencies" section,
  upgrade dialog.
- Rust unit tests for all new logic; pure-logic vitest tests; new `.harness/`
  scenarios.
- Docs: CONTRIBUTING "How Keitora finds herdr" section (`CONTRIBUTING.md:61-83`), and
  README step "2. Install herdr" (`README.md:43-61`, which today only lists the vendor
  one-liners): add that Keitora can install/upgrade herdr itself from the first-run step,
  the rail card, and Settings.

**Out**
- Installing git, claude, codex, gh, shells.
- Installer-time hooks.
- A daily "newer herdr available" check (v1 only acts on the floor; herdr's own
  update check stays off: `herdr_config_toml` in `env.rs` writes
  `[update] version_check = false`).
- Cancelling an in-flight tier-1 script; byte-accurate progress (spinner +
  stage is enough for v1).
- Linux packaging (code shouldn't preclude it).

## Architecture

```
       ┌───────────────── frontend ─────────────────┐
       │ Onboarding deps step · Rail OfflineCard     │
       │ Settings ▸ Dependencies · UpgradeDialog     │
       └──────────────┬──────────────────────────────┘
          commands +  │ deps://status, deps://progress
       ┌──────────────▼──────────────────────────────┐
       │ src-tauri/src/deps/                          │
       │  mod.rs       DepsService: single install    │
       │               mutex, status snapshot, sink   │
       │  commands.rs  Tauri commands                 │
       │  manifest.rs  fetch + select release         │
       │  floor.rs     parse/compare, FloorState      │
       │  preflight.rs tier choice (pure)             │
       │  tier1.rs     vendor script (env-scrubbed)   │
       │  tier2.rs     download, verify, unpack       │
       │  manual.rs    tier-3 routes + browse         │
       │  upgrade.rs   install → swap → restart       │
       │  doctor.rs    herdr/git/claude/codex rows    │
       └───────┬─────────────────────────┬────────────┘
               │ write [pty] herdr_bin   │ backend restart()
               ▼                         ▼
       keitora-tasks config       backend/herdr (existing boot,
                                  recovery, watchdog)
```

## Technical design

### Release selection (`manifest.rs`)
`https://herdr.dev/latest.json` has a top-level `version`/`protocol`/`assets`/
`sha256` and a `releases` map: `{ "<ver>": { protocol, endpoint_generation,
assets{<platform>: url}, sha256{<platform>: hex} } }` (61 entries today).

`select_release(manifest, platform, installed: Option<Version>)`:
newest entry with `protocol == 22`, version ≥ 0.9.5, an asset + sha256 for
`platform`, and version ≥ the installed herdr (never downgrade). Returns
`None` → Manual with reason `NoCompatibleRelease`. This keeps first-run working
when herdr ships protocol 23 before Keitora supports it.

Validation: asset URLs must be `https://`; sha256 must be 64 hex (normalize to
lowercase); unknown fields ignored; a parse failure → Manual with reason, never a panic.

Platform keys: `windows-x86_64`, `macos-aarch64`, `macos-x86_64` (from
`std::env::consts`).

HTTP: reuse the updater's shared rustls `reqwest` blocking client
(`crate::updater::source::http_client()`, `updater/source.rs:80`, `pub(crate)`; it also
installs the crypto provider). Call it only from a blocking thread, never from async code
or the setup thread (the blocking client owns a runtime). A per-request `.timeout()` may
override its default; tier 2 downloads use 120 s. After redirects, require `https` and a
host in {`herdr.dev`, `github.com`, `objects.githubusercontent.com`,
`release-assets.githubusercontent.com`}. No HTTP fallback.

### Version floor (`floor.rs`)
- `HERDR_FLOOR = 0.9.5`, `HERDR_PROTOCOL = 22`. Use `semver` (already a direct
  dependency); parse defensively: accept a bare `x.y.z` (what
  `status server --json` returns, `cli.rs:84-89` `ServerStatus { running, version:
  Option<String>, protocol: Option<u64>, .. }`, parsed in `real.rs:442-453`; the protocol
  is already parsed, so no backend parsing change is needed) and
  a prefixed form (`herdr 0.9.5 …`), since no herdr `--version` output is recorded in
  the repo. Implementation records a real `herdr --version` sample as a fixture.
- `FloorState`: `Ok`, `TooOld { found }`, `ProtocolMismatch { found, want }`,
  `RestartToFinish { binary, server }` (binary on disk is new, but the running server is old),
  `Unknown` (warn, don't block).
- **The running server is authoritative**: the floor is judged on
  `status server --json` from the status loop (`StatusLoop::probe`,
  `backend/herdr/status_loop.rs`; wiring under Implementation notes). The binary's `--version` is used
  only before a server exists (preflight) and to detect `RestartToFinish`.
- Severity: `TooOld` is soft (Keitora keeps working; prompt to upgrade).
  `ProtocolMismatch` is hard: the herdr backend refuses to bind panes (`spawn`,
  `spawn_agent`, `attach` return `PtyBackendError::Other`, see Implementation notes);
  the dialog has no "Later". Judgement order: protocol first (`protocol` present and
  `!= 22` → `ProtocolMismatch`; a missing protocol field is not a mismatch), then
  version (`< 0.9.5` → `TooOld`), else `Ok`; a missing or unparseable version with no
  mismatch → `Unknown`.

### Upgrade prompt
- Raised by `DepsService` (Rust), once per **(found version, app session)**. It doesn't re-fire
  on every reconnect or crash recovery. "Later" snoozes it in Rust for the
  session; every window reads the same snapshot (`deps_status` command +
  `deps://status` event), so multiple windows can't disagree.
- Copy: "herdr X is older than Keitora needs (0.9.5+). Upgrade now?" →
  Upgrade / Later. The rail card and Settings keep a persistent badge + Upgrade.

### Tier choice (`preflight.rs`, pure)
Inputs: platform, manifest result, selected release, whether the selected release
equals the manifest's top-level latest, release vs dev build, `KEITORA_HOME` /
`KEITORA_HERDR_BIN` set, script-host probe result.

- Tier 1 only if: release build, `KEITORA_HOME` **and** `KEITORA_HERDR_BIN` unset
  (dev homes set both via `scripts/dev-herdr.mjs:324-350`; tier 1 writes to the
  shared `~/.herdr` / `~/.local/bin`), the vendor script would install the
  selected release (selected == top-level latest; the script can't pick a version),
  and the script-host probe passes.
- Else tier 2 if the selected release has an asset for the platform.
- Else Manual(reason). Offline → Manual(Offline).

Script-host probe (Windows; always passes on unix): hidden `powershell -NoProfile -NonInteractive -Command
"$ExecutionContext.SessionState.LanguageMode"` must print `FullLanguage`; a
non-zero exit with no output (e.g. AppLocker) → not usable.

### Tier 1 — vendor script (`tier1.rs`)
- Windows: hidden `powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass
  -Command "irm https://herdr.dev/install.ps1 | iex"` with `CREATE_NO_WINDOW`.
  Installs under `~/.herdr/packages/standalone/`. The binary path is **not assumed**:
  after the script exits 0, re-list `herdr_release_paths()` (`config.rs:2398`, newest
  version first, layout `releases/<ver>-<triple>/herdr.exe`; home injected for tests)
  and take the first entry that exists as the installed path.
- macOS: `sh -c "curl -fsSL https://herdr.dev/install.sh | sh"`. Installs to
  `~/.local/bin/herdr`, overwriting in place. If a herdr already exists there that
  Keitora didn't install, ask before running.
- Spawn with `crate::backend::herdr::configure_command(&mut cmd)` (`backend/herdr/mod.rs:197`,
  `pub(crate)`; sets `CREATE_NO_WINDOW` on Windows, a no-op elsewhere). Do not write a
  new helper. `keitora-tasks`' `CommandNoWindow` is `pub(crate)` to that crate and not
  usable from `src-tauri`.
- Env: remove `HERDR_HOME`, `HERDR_INSTALL_DIR`, `HERDR_MANIFEST_URL`,
  `HERDR_EXPECTED_BUILD_ID`, any `XDG_*` pointing inside `KEITORA_HOME`; set
  `HERDR_CHANNEL=stable`; pass proxy vars through. (The pane-env restoration
  in `env.rs` is the model for computing the user's original XDG values.)
- Output streamed (ANSI-stripped) to the log; hard timeout 300 s.
- Success = exit 0 **and** the installed path (see above; macOS `~/.local/bin/herdr`)
  exists **and** its `--version` is ≥ floor. Otherwise `Incomplete` / `TooOldAfterInstall`.
- Write the resulting path to `[pty] herdr_bin`.

### Tier 2 — direct download (`tier2.rs`)
- Download to `<KEITORA_HOME>/bin/herdr/.staging-<rand>/`; verify sha256 **before**
  unpacking.
- Windows asset is a zip **with the ConPTY bundle files**, not a lone exe: unpack every
  entry, each checked with `enclosed_name()` (no `..`, no absolute paths, no
  symlinks) under a total-size cap. macOS asset is a bare binary → chmod 755.
- Run `--version` from the staging dir (via `configure_command`); atomic rename to
  `<KEITORA_HOME>/bin/herdr/<version>/`. An existing version dir is reused.
- `<KEITORA_HOME>` is `crate::paths::keitora_home()` (`paths.rs:15`).
- Write `<KEITORA_HOME>/bin/herdr/<version>/herdr[.exe]` to `[pty] herdr_bin`.
- Prune: keep the in-use version plus one previous. Startup deletes stray
  `.staging-*`.
- No quarantine xattr / Mark-of-the-Web is added (reqwest doesn't add them), so
  Gatekeeper and SmartScreen won't prompt. A Defender quarantine shows up as a
  failed `--version` run → `BlockedByAV`.

### Manual (`manual.rs`)
Copyable install one-liners, a link to herdr.dev, the reason, a "Browse for herdr"
picker (validated by `--version` ≥ floor, then written to `[pty] herdr_bin`), and
"Copy details" (plain-text report: platform, tier tried, failure, log tail).

### Discovery
Unchanged order. Because every install pins `[pty] herdr_bin` (step 1 of
`discover_exe_with`), the new binary wins over a stale PATH herdr without
reordering, and the user can see and change the choice in Settings. CONTRIBUTING
gains a note that installs set `herdr_bin`.

### Upgrade sequence (`upgrade.rs`)
**Which route an upgrade takes depends on where the current herdr lives**
(`InstallLayout`, from the path in use):
- `Managed` (`<KEITORA_HOME>/bin/herdr/…`) → tier 2 again.
- `Standalone` (inside `~/.herdr/packages/standalone/releases/`, or `~/.local/bin` on
  macOS when Keitora installed it) → tier 1 again (falls back to tier 2).
- `External` (Homebrew prefix `/opt/homebrew` / `/usr/local/Cellar`, a package
  manager, or any other user path) → **never replaced automatically**. The dialog shows
  the right instruction (e.g. `brew upgrade herdr`) plus a secondary "Install a
  Keitora-managed copy instead" (tier 2 + pin, which leaves the user's copy untouched)
  and Re-check.

With live panes, minimize downtime and **keep every live session owed a resume**:
1. Install the new binary **with the server still running**, always side by side:
   tier 2 unpacks into a new `<KEITORA_HOME>/bin/herdr/<ver>/`; tier 1 on Windows
   writes a new `~/.herdr/packages/standalone/releases/<ver>-<triple>/` (the running
   binary is never overwritten); tier 1 on macOS replaces `~/.local/bin/herdr` by
   rename, which a running process survives. Verify the new binary's `--version`.
2. Confirm via `RestartDaemonDialog` (`src/components/RestartDaemonDialog.tsx`; add
   optional `title` / `description` / `confirmLabel` props whose defaults are today's
   hardcoded copy) showing the pane count (`SharedBackend::live_set_snapshot().len()`).
3. Switch `herdr_bin` to the new binary. If the restart below fails to bring the new
   binary up, restore the previous `herdr_bin` and restart onto it.
4. **Never call `PtyBackend::shutdown()` in an upgrade.** `shutdown()` is the
   user-requested stop: it tells recovery nothing is owed a resume and clears the live
   set and key map (`backend/herdr/mod.rs:~3065-3081`), so every live pane is orphaned
   (observed in a real upgrade: "0 session(s) to resume", panes stuck until the app
   was relaunched). The swap needs no stop because step 1 never touches the running
   binary.
5. **One** `PtyBackend::restart()` (the same path as the "Restart daemon" button, which
   marks live sessions owed a resume) and check the outcome. Exactly one restart per
   upgrade: the deferred-restart retry must not fire when the direct restart took effect. `restart()` re-reads
   `UserConfig::load()` and re-runs `discover_exe` (`HerdrOps::rediscover`,
   `real.rs:252`), so the freshly written `herdr_bin` is what it launches. It is a silent
   `Ok(())` no-op while recovery isn't idle (`backend/herdr/mod.rs:3033-3050`; idle =
   `Healthy | Down | AwaitHeadroom | Paused`). `DepsService` reads
   `backend.recovery_snapshot().phase` **before** calling it and reports "restart
   pending" when the phase is not idle. A returned `Err` (herdr still not found) is a
   `Failed` run with kind `Incomplete`.
6. Re-run `herdr integration install` unconditionally: add
   `Supervisor::refresh_integration(&self, ops: &dyn HerdrOps)` in `supervisor.rs`
   (resets the `ready_done` latch, then calls `ensure_ready`, `supervisor.rs:223-258`,
   which today runs once per process and skips when already installed; 0.9.4 → 0.9.5
   changed the Codex hooks) and expose it as a defaulted
   `PtyBackend::refresh_integration(&self) {}` implemented by `HerdrBackend`. Like
   `ensure_ready` it is skipped under `cfg(test)` and by `KEITORA_HERDR_NO_INTEGRATION`.
   Verify whether the hooks store an absolute herdr path; if so, pruning must never
   remove the version they point at (record the finding in a code comment next to the
   prune function).

Fresh install (backend is `ExeMissing`, no server): steps 1, 3, 5, 6 with no
confirmation needed beyond the Install click.

### Concurrency
One install mutex in `DepsService`. A second Install/Upgrade click (onboarding,
rail, Settings, another window) attaches to the in-flight run's progress instead
of starting another.

### Failures + progress
- Kinds: `Offline`, `Network` (incl. proxy/TLS), `PolicyBlocked`, `Checksum`,
  `BlockedByAV`, `Timeout`, `Incomplete`, `NoCompatibleRelease`, `Other`.
- `after_failure(kind, tier)`: tier 1 failed and tier 2 viable → offer "Try direct
  download"; else Manual(reason).
- Events mirror the updater (`updater/mod.rs:27-28` `updater://status` /
  `updater://progress`, `StatusSink` trait `:118`, snapshot command for webview
  reload; wire shapes under Implementation notes): stages `Checking → Downloading → Installing → Verifying → Restarting →
  Done | Failed`, forward-only, exactly one terminal event per run.

### Frontend
- **State plumbing**: `DaemonInfo` (`src/lib/ipc.ts:52`) has no reason code and the rail card
  keys off `reason` text (`src/lib/recovery.ts:61` `HERDR_NOT_FOUND`). The new
  `deps_status` snapshot (floor state, install run, prompt) is consumed through a
  `DepsProvider` context + `useDeps()` hook (the `UpdatesProvider` / `useUpdates()`
  pattern) instead of overloading `DaemonInfo`.
- **Onboarding**: `Onboarding.tsx` is a 15-line wrapper around `AddProjectFlow`
  gated by `project === null` (`App.tsx:1164-1168`). Add a dependencies step
  before Add Project, shown when herdr is missing/too old. Users who already have a
  project never see onboarding, so for them the rail card and the upgrade dialog carry it.
- **Rail `OfflineCard`** (`Rail.tsx:166`): Install (missing) / Upgrade (too old / mismatch) /
  "Restart to finish" next to the existing Restart.
- **Settings**: a "Dependencies" section (Title case like `Daemon`, `Updates`;
  pattern: `DaemonSection` `SettingsDrawer.tsx:74`, `UpdatesSection` `:260`):
  doctor rows + Install/Upgrade/Browse + Re-check + the pinned path with "Clear (use
  discovery)" (`set_herdr_bin(None)`). Mounted in the section list at
  `SettingsDrawer.tsx:1218-1224`, after `AgentUpdatesSection`.
- **UpgradeDialog**: driven by the Rust prompt state; Upgrade / Later (no Later
  for a protocol mismatch).

### Doctor (`doctor.rs`)
`deps_doctor` → rows `{ tool, path, version, status: Ok|Missing|TooOld|Unknown,
action }`:
- herdr: discovery + `FloorState`.
- git: presence + version, informational (no floor in v1): `git --version`, 5 s bound,
  spawned via `configure_command`, behind the `Proc` fake in tests.
- claude / codex: presence + version via the existing probes
  (`agent_updates/probe.rs`, `codex_env.rs:255-290`, which run a 5 s-bounded `--version` with no
  console window, cached); discovery via `discover_cli` (`config.rs:2506/2591`). Reuse
`agent_updates::probe::MachineProber` (`probe.rs:59`; `Prober::exe(cli, &cfg)` and
`Prober::installed(cli, exe)`) for claude/codex instead of re-implementing the probes.
Installs nothing.

## Implementation notes

Resolved specifics, checked against the code. Where this section and an earlier section
seem to differ, this section wins. Signatures and shapes only; the implementer writes the
bodies.

### Wire conventions
Match the updater (`updater/mod.rs:33-110`): `#[derive(Serialize)]`, **snake_case** field
names and values, internally tagged enums `#[serde(tag = "type", rename_all =
"snake_case")]`, payload structs without `rename_all` on fields. Events and commands use
the `deps://…` / `deps_…` names below. TypeScript mirrors live in `src/lib/ipc.ts` in a new
`// ── Dependencies ──` block after the Updater block (`:1218-1290`), same style
(`export function depsStatus(): Promise<DepsSnapshot> { return invoke("deps_status"); }`,
`onDepsStatus` / `onDepsProgress` via `listen`).

### Rust public API (`src-tauri/src/deps/`)
All types `#[derive(Debug, Clone, PartialEq, Eq, Serialize)]` unless noted.

```rust
// floor.rs
pub const HERDR_FLOOR: semver::Version;          // 0.9.5
pub const HERDR_PROTOCOL: u64 = 22;
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FloorState {
    Ok,
    TooOld { found: String },
    ProtocolMismatch { found: u64, want: u64 },
    RestartToFinish { binary: String, server: String },   // built by DepsService only
    Unknown,
}
pub fn parse_version(s: &str) -> Option<semver::Version>;              // bare or prefixed
pub fn judge(status: &ServerStatus) -> FloorState;                      // running server
pub fn judge_version(version: Option<&str>, protocol: Option<u64>) -> FloorState;
pub struct FloorSlot { .. }                                             // not Serialize
impl FloorSlot {
    pub fn new() -> Self;                       // starts at Unknown; also impl Default
    pub fn get(&self) -> FloorState;
    pub fn set(&self, next: FloorState);        // calls the observer only when it changed
    pub fn on_change(&self, f: Box<dyn Fn(&FloorState) + Send + Sync>); // one observer
    /// `Some(user-facing message)` iff the state is `ProtocolMismatch`.
    pub fn blocks_panes(&self) -> Option<String>;
}

// manifest.rs
pub struct Release { pub version: semver::Version, pub protocol: u64,
                     pub url: String, pub sha256: String }              // for one platform
pub enum Platform { WindowsX86_64, MacosAarch64, MacosX86_64 }
impl Platform { pub fn current() -> Option<Platform>; pub fn key(self) -> &'static str; }
pub trait Net: Send + Sync {                    // fake in tests
    fn get_text(&self, url: &str) -> Result<String, NetError>;
    fn download(&self, url: &str, dest: &Path, on_bytes: &mut dyn FnMut(u64, Option<u64>))
        -> Result<(), NetError>;
}
pub fn parse_manifest(json: &str, platform: Platform) -> Result<Manifest, ManifestError>;
pub fn select_release(m: &Manifest, installed: Option<&semver::Version>) -> Option<Release>;
pub fn host_allowed(url: &str) -> bool;         // https + allowlist, applied after redirects

// preflight.rs (pure)
#[serde(rename_all = "snake_case")] pub enum Tier { Script, Direct, Manual }
pub struct PreflightInput { .. }                // the inputs listed under Tier choice
pub enum TierChoice { Use(Tier), Manual(ManualReason) }
pub fn choose_tier(i: &PreflightInput) -> TierChoice;

// failure kinds (mod.rs)
#[serde(rename_all = "snake_case")]
pub enum FailureKind { Offline, Network, PolicyBlocked, Checksum, BlockedByAV, Timeout,
                       Incomplete, TooOldAfterInstall, NoCompatibleRelease, Other }
pub enum NextStep { TryDirect, Manual }          // serialized snake_case
pub fn after_failure(kind: FailureKind, tier: Tier, direct_viable: bool) -> NextStep;

// mod.rs
#[serde(rename_all = "snake_case")]
pub enum Stage { Checking, Downloading, Installing, Verifying, Restarting, Done, Failed }

#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunState {
    Idle,
    Running { run_id: u64, tier: Tier, stage: Stage, upgrade: bool },
    Done { run_id: u64, path: String, restart_pending: bool },
    Failed { run_id: u64, kind: FailureKind, message: String, next: NextStep },
}
#[serde(rename_all = "snake_case")] pub enum PromptKind { TooOld, ProtocolMismatch }
pub struct UpgradePrompt { pub kind: PromptKind, pub found: String, pub hard: bool }

/// `deps_status` reply and the whole `deps://status` payload (replaces, never merges).
pub struct DepsSnapshot {
    pub herdr_installed: bool,         // false iff recovery reason_code == "herdr_not_found"
    pub floor: FloorState,
    pub pinned_bin: Option<String>,    // `[pty] herdr_bin`
    pub platform: Option<String>,      // `Platform::key()`, None = unsupported OS
    pub run: RunState,
    pub prompt: Option<UpgradePrompt>,
    pub live_sessions: usize,          // for the confirm dialog
}
/// `deps://progress` payload: sent on every stage change and on (250 ms-throttled)
/// download chunks; no per-byte-accurate promise.
pub struct ProgressEvent { pub run_id: u64, pub stage: Stage, pub downloaded: u64,
                           pub total: Option<u64>, pub line: Option<String> }  // last log line

pub trait DepsSink: Send + Sync {               // updater's `StatusSink`, `updater/mod.rs:118`
    fn emit_status(&self, snapshot: &DepsSnapshot);
    fn emit_progress(&self, event: &ProgressEvent);
}
pub const STATUS_EVENT: &str = "deps://status";
pub const PROGRESS_EVENT: &str = "deps://progress";

pub struct DepsService { .. }                   // managed state
impl DepsService {
    pub fn new(backend: SharedBackend, floor: Arc<FloorSlot>, net: Box<dyn Net>,
               proc_: Box<dyn Proc>, fs: Box<dyn Fs>, sink: Box<dyn DepsSink>,
               platform: Option<Platform>) -> Self;
    pub fn snapshot(&self) -> DepsSnapshot;
    pub fn recheck(&self) -> DepsSnapshot;      // re-probes binary `--version`; emits status
    pub fn start_install(&self, tier: Option<Tier>) -> Result<(), String>; // attaches if running
    pub fn snooze_prompt(&self);
    pub fn set_herdr_bin(&self, path: Option<PathBuf>) -> Result<DepsSnapshot, String>;
    pub fn details_report(&self) -> String;     // "Copy details" text
    pub fn doctor(&self) -> Vec<DoctorRow>;
}
pub fn startup(app: tauri::AppHandle);          // like `updater::startup`

// doctor.rs
#[serde(rename_all = "snake_case")] pub enum DoctorStatus { Ok, Missing, TooOld, Unknown }
pub struct DoctorRow { pub tool: String,        // "herdr" | "git" | "claude" | "codex"
    pub path: Option<String>, pub version: Option<String>,
    pub floor: Option<String>,                    // "0.9.5" for herdr; None for info-only rows
    pub status: DoctorStatus, pub action: Option<DoctorAction> }
#[serde(rename_all = "snake_case")] pub enum DoctorAction { Install, Upgrade, Browse }
```

`Proc` (spawn + capture with timeout, returns exit code and stdout) and `Fs` (create dir,
rename, remove, list, write, exists, unpack) are small traits in `deps/mod.rs` with a real
impl and a test fake. The real `Proc` always spawns through
`crate::backend::herdr::configure_command` (hidden console on Windows).

### Tauri commands (`deps/commands.rs`)
All `#[tauri::command(async)]`; blocking work runs in `tauri::async_runtime::spawn_blocking`
exactly as `updater::commands::update_check` does. Registered in `host.rs`
`generate_handler!` right after the `agent_updates::commands::*` entries (`host.rs:~2509`).

| Command | Args | Returns |
|---|---|---|
| `deps_status` | none | `DepsSnapshot` |
| `deps_recheck` | none | `DepsSnapshot` |
| `deps_doctor` | none | `Vec<DoctorRow>` |
| `deps_install` | `tier: Option<Tier>` (`None` = preflight picks; `Some(Direct)` = "Try direct download") | `Result<(), String>` (returns once the run has started or been attached to; progress arrives as events) |
| `deps_snooze_upgrade` | none | `()` |
| `deps_set_herdr_bin` | `path: Option<String>` (`None` = clear the pin) | `Result<DepsSnapshot, String>` (validates `--version` and the floor before writing) |
| `deps_details` | none | `String` |

`deps_install` is used for first install and for upgrade alike; `DepsService` picks the
sequence from whether a server is running. The frontend shows the confirm dialog (pane
count from `DepsSnapshot.live_sessions`) *before* calling it for an upgrade. The Browse
picker is `open()` from `@tauri-apps/plugin-dialog` (as `AddProjectFlow.tsx:6`), then
`depsSetHerdrBin(path)`. When no server is running, a successful `deps_set_herdr_bin`
(and every successful install) then runs steps 5-6 of the upgrade sequence; when one is
running it only writes the pin, and the snapshot shows `restart_to_finish`.

### State, backend access, and the floor path
- **Construction.** `deps::startup(app)` is called in `host.rs` `setup` directly after
  `crate::updater::startup(...)` / `agent_updates::startup(...)` (`host.rs:~2382-2384`), so
  `SharedBackend` is already managed. It reads `app.state::<SharedBackend>().inner().clone()`
  (the helper in `agent_updates/commands.rs:22` is the model), builds the service, and calls
  `app.manage(service)`. Commands take `State<'_, DepsService>`. It also spawns a thread that
  deletes stray `.staging-*` dirs (like `updater::cleanup_leftovers`).
- **Reaching the backend.** `DepsService` holds a `SharedBackend` (`Arc<dyn PtyBackend>`) and
  uses only trait methods: `restart()` (the one restart per upgrade; never `shutdown()`,
  the user's explicit stop, which owes no session a resume), `recovery_snapshot()` (phase +
  `reason_code`), `live_set_snapshot()`, plus two
  **new defaulted trait methods** added to `PtyBackend` in `backend/mod.rs` next to
  `recovery()`:
  `fn floor_slot(&self) -> Option<Arc<crate::deps::floor::FloorSlot>> { None }` and
  `fn refresh_integration(&self) {}`. `HerdrBackend` overrides both; `UnavailableBackend`
  keeps the defaults (floor stays `Unknown`, nothing to install into).
- **Floor flow: a shared `Arc<FloorSlot>`** (the codebase's pattern for loop-to-consumer
  state is shared `Arc<Mutex<..>>`: `status_cache`, `exe_missing`, `explicit_stop`; no new
  channel). `HerdrBackend` gets a field `floor: Arc<FloorSlot>` built in `with_ops`
  (`backend/herdr/mod.rs:~1990`) and a matching `StatusLoopDeps.floor` field, passed in
  `HerdrBackend::status_loop()` (`:2186`). In `StatusLoop::probe` (`status_loop.rs:464`), when
  `r` is `Ok(s)` with `s.running`, call `self.deps.floor.set(floor::judge(&s))` **before**
  taking the cache lock. A failed or not-running probe leaves the slot unchanged. The first
  tick probes once immediately, so the state is known within one wake of boot; until then
  it is `Unknown` (not blocking). The status-loop test `Rig` and any other `StatusLoopDeps`
  constructor in tests add `floor: Arc::default()`.
- **Slot to service.** `DepsService::new` registers `floor.on_change(..)` with a closure that
  only updates the service's snapshot/prompt under its own mutex and calls
  `sink.emit_status` (cheap; never calls into the backend, so it cannot deadlock the loop).
  `RestartToFinish` is never stored in the slot: `snapshot()` computes it by comparing the
  slot's server version with the pinned binary's cached `--version` (`recheck()` refreshes
  that cache).
- **Prompt rules** (in `DepsService`): `TooOld { found }` raises a soft `UpgradePrompt` once per
  `found` per process (a `HashSet<String>`); `snooze_prompt()` clears it; `ProtocolMismatch`
  always yields `hard: true` and `snooze_prompt()` ignores it.
- **`herdr_installed`** is `recovery_snapshot().reason_code != Some(exe_missing::NOT_FOUND_CODE)`.
  The service does not subscribe to recovery; the frontend provider re-pulls `deps_status`
  when `useRecovery().reason_code` changes (`src/lib/sessions.ts:208`).

### How a protocol mismatch stops Keitora binding panes
Least invasive: no new backend type, no new recovery reason code, no `daemon://status`
change. Add `HerdrBackend::require_protocol(&self) -> Result<(), PtyBackendError>` that maps
`self.floor.blocks_panes()` to `PtyBackendError::Other(message)`; call it first (before
`gate.admit`, right after the `is_ready()` check) in the three pane-binding trait methods
in `backend/herdr/mod.rs`: `spawn` (`:2440`), `spawn_agent` (`:2506`), and `attach`
(`:2554`). `Other` makes the frontend fail the pane at once and show the text (see the
`UnavailableBackend::refused` doc, `backend/unavailable.rs`), instead of retrying as for
`NotConnected`. Message: "herdr speaks protocol {found}; Keitora needs protocol 22. Upgrade
herdr from Settings > Dependencies." Recovery resumes go through the same spawn path and
are refused too; that is intended in this state. `TooOld` and `Unknown` never block.

### Config write and pickup
`set_herdr_bin` writes `[pty] herdr_bin` in the **global** `keitora.toml` at
`user_config_path()` (honors `KEITORA_CONFIG_HOME`, which dev homes set, so dev runs write
their own file, not the user's). Paths are written absolute (no `~`). The running backend
does **not** hold the config: `PtyBackend::restart()` calls `HerdrOps::rediscover()`
(`real.rs:252`), which does `UserConfig::load()` and `discover_exe`, where `herdr_bin` is
step 1. So pin, then `restart()`; nothing else is needed. The first launch with the pin
already present takes the same path via `HerdrBackend::new`.

### Windows spawning
Reuse `crate::backend::herdr::configure_command(&mut Command)` (`backend/herdr/mod.rs:197`,
`pub(crate)`), for the tier 1 script, the script-host probe, every `--version` run, and
`git --version`. Do not add another `CREATE_NO_WINDOW` helper.

### Frontend files
- `src/lib/ipc.ts`: types and wrappers (above).
- `src/lib/deps.ts`: pure view-model helpers and the reducer (as `src/lib/updates.ts` is for the
  updater): `INITIAL_DEPS`, `depsReducer`, `cardModel(snapshot)`, `dialogModel(snapshot)`,
  `settingsModel(snapshot, rows)`, `stageLabel(stage)`, `failureCopy(kind)`.
  Tests in `src/lib/deps.test.ts` (vitest, no DOM).
- `src/components/DepsProvider.tsx`: context + `useDeps()` hook (the `UpdatesProvider.tsx` /
  `useUpdates()` pattern; a provider rather than a bare `src/lib/useDeps.ts` because the
  onboarding step, rail card, dialog and Settings must share one state and one listener).
  Mounted in `App.tsx` `App()` (`:1398`) inside `ToastProvider`, around `UpdatesProvider`.
- Components in `src/components/`: `DepsStep.tsx` (the onboarding step, rendered by
  `Onboarding.tsx` before `AddProjectFlow`), `DepsUpgradeDialog.tsx` (Radix Dialog like
  `RestartDaemonDialog.tsx`; mounted once in `AppInner`), `DepsSection.tsx` (exported
  `DepsSection`, rendered in `SettingsDrawer.tsx` after `AgentUpdatesSection`; may also live
  in `SettingsDrawer.tsx` as `function DepsSection()` beside `DaemonSection` if smaller than
  ~120 lines). The rail card changes are inside `OfflineCard` (`Rail.tsx:166`).
- Settings section title: "Dependencies".

### Harness scenarios (`.harness/app.tsx`, not committed)
One query param, `?deps=` with values `ok` (default, current behavior), `missing`,
`too_old`, `mismatch`, `restart_to_finish`, `installing`, `failed`. It drives the mocked
`deps_status` snapshot, `deps_doctor` rows, and (for `installing`) a timed series of
`deps://progress` events through the same `emit` helper the transfer mock uses
(`app.tsx:22-40`). `missing` also makes `daemon_recovery` return `reason_code:
"herdr_not_found"` and `daemon_status` return `state: "Disconnected"`. Add `deps_*` cases to
the `invoke` switch (`app.tsx:~386-547`); unknown commands must keep falling through as
today.

### Externally verified facts (2026-11-23)
- `herdr --version` prints `herdr 0.9.3` (format `herdr <x.y.z>`, no protocol). Use it
  as the binary-version fixture; the parser still accepts a bare `x.y.z`.
- Windows standalone layout: `~/.herdr/packages/standalone/{current, install.lock,
  releases/}`; `current` is a **junction** to the active `releases/<ver>-<triple>/`
  dir. After tier 1, resolve via the newest existing `herdr_release_paths()` entry (as
  planned); don't write the `current` junction path to `herdr_bin`, because a later vendor
  install would silently retarget it.
- `install.ps1` reads `HERDR_CHANNEL`, `HERDR_MANIFEST_URL`, `HERDR_INSTALL_DIR`,
  `HERDR_EXPECTED_BUILD_ID` as parameter defaults and verifies SHA-256 itself; it
  prepends its dir to the HKCU `Path` (which Keitora's process doesn't see until relaunch,
  hence pinning `herdr_bin`).
- A developer machine on 0.9.3 is below the 0.9.5 floor and will see the upgrade prompt.

## Testing

Rust (`#[cfg(test)]`), fakes behind small traits (`Net`, `Fs`, `Proc`) in the
style of `backend/herdr/fake.rs` (`FakeHerdr`; its `rediscover` / `set_exe_missing` knobs
serve the upgrade tests). **No test starts powershell or sh, touches the
real `~/.herdr`, or contacts a running herdr.**
- `herdr_release_paths` (`config.rs:2398`) reads `dirs::home_dir()` directly. Inject
  the home so tests stay under a temp dir.
- Manifest: snapshot fixture of the real `latest.json` (trimmed); release
  selection incl. top-level latest at protocol 23 → picks the newest protocol-22
  entry; never downgrades; missing platform; http URL rejected; bad/uppercase sha.
- Tier choice table (dev env, script host blocked, selected ≠ latest, offline).
- Tier 1: exact command line per OS; env scrub list; exit 0 without the binary →
  Incomplete.
- Tier 2: checksum mismatch leaves no final dir; zip with `..` / absolute entry
  rejected; size cap; version dir reuse; prune keeps in-use + previous; staging
  cleanup.
- Floor parsing (bare + prefixed), compare, protocol mismatch, RestartToFinish.
- Prompt: once per (version, session); snooze; protocol mismatch not snoozable.
- Upgrade orchestration with the herdr fake (`FakeHerdr` behind `HerdrBackend::with_ops`): an
  upgrade with live sessions never calls `shutdown()`, restarts exactly once and resumes
  every session; restart while recovery busy → "restart pending", and a deferred restart
  never fires after a direct one; a restart failure restores the previous pin; a tier-1
  failure leaves the running herdr alone.
- Install mutex: concurrent requests share one run; exactly one terminal event.
- `after_failure` table.
- One `#[ignore]` end-to-end test: local HTTP server serving a fake zip + manifest.

Frontend: vitest has no DOM environment (no jsdom/testing-library, no `.test.tsx`),
so keep vitest on pure helpers (`src/lib/deps.ts`: state → card/dialog view model)
and verify the UI in `.harness/` with new query-param scenarios (missing,
too-old, installing, failed, restart-to-finish); `daemon_status` /
`daemon_recovery` mocks are hardwired healthy today (`.harness/app.tsx:404-409`). Harness
changes are not committed (`.harness/` is git-excluded locally); scenarios are listed
under Implementation notes.

## Dependencies
- Reuse: `reqwest` 0.13 blocking + `http_client()` (`updater/source.rs`),
  `semver` 1 (direct), `rand` (direct), updater sink/snapshot pattern.
- Add as direct deps (already in `Cargo.lock` transitively, no new downloads):
  `sha2` 0.10, `zip` (9.x, matching the lock).
- External, unversioned: herdr's `latest.json` shape and install-script URLs/env.
  Parse defensively.

## Risks
- **Trust model**: the manifest (herdr.dev) and assets (github.com) are separate
  origins, so the sha256 guards against a swapped or corrupted GitHub asset but not
  a compromised herdr.dev; tier 1 trusts herdr.dev outright. Same trust a manual
  install has. Mitigations: https only, host allowlist after redirects, no downgrade.
- **Vendor script drift** (paths, env, channel fallback to preview on missing
  stable assets). Mitigated by checking the expected path + version after the run and
  pinning `HERDR_CHANNEL=stable`.
- **Corporate policy** (Constrained Language Mode, AppLocker, TLS interception,
  AV). The probe and failure classification route to tier 2 or Manual with copyable details.
- **Integration install side effect**: boot's `herdr integration install` writes
  into the real `~/.claude` / `~/.codex` (unchanged behavior, but now it can happen on
  first run and after upgrades).
- **Pinning `herdr_bin`** stops later user-side herdr upgrades (brew, manual) from
  being picked up automatically. Settings shows the pinned path with a "Clear (use
  discovery)" action.

## Complexity
**Medium-large**: one new Rust module (~10 files), small changes in backend/herdr,
config, updater client sharing, and four frontend surfaces + harness. Plan: skip
Fracture (one document), keep Singletonize and a light Loss check.
