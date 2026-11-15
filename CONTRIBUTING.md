# Contributing to Keitora

This covers building Keitora from source, running a development build safely, and the checks a
change has to pass. If you use an AI coding agent, point it at [AGENTS.md](AGENTS.md) as well.

## Setup

You need Rust (`rust-toolchain.toml` pins the version, and rustup installs it automatically), Node.js and Yarn. On macOS, also install the Xcode Command Line
Tools (`xcode-select --install`). On Windows you need Git for Windows and the MSVC Rust toolchain;
[docs/windows-backend.md](docs/windows-backend.md) has the Windows specifics.

```sh
yarn install    # frontend deps, and installs the pre-commit hook
```

## How it fits together

Keitora is a [Tauri](https://tauri.app/) app. The Rust backend in `src-tauri/` talks to herdr,
manages the task database and session state, and runs the dispatcher. The React + Vite frontend
lives in `src/`. Task logic lives in one place, `crates/keitora-tasks`, which both the app and the
`keitora` CLI (`crates/keitora-cli`) use, so the two always agree on how tasks behave.
[docs/build-plan.md](docs/build-plan.md) has the full design.

## Running a development build

Don't use plain `yarn tauri dev` if you also use Keitora day to day. It runs from inside this repo,
registers it as a project, and starts agents in every one of its worktrees, which takes over
sessions you may be using.

Use the isolated launcher instead:

```sh
yarn dev:isolated
```

It builds the app and runs it against a Vite server on port 1420, with its own herdr server, its own
state (wiped on every run) and a throwaway git project. It doesn't install agent hooks into your
real `~/.claude` or `~/.codex`. Its home is `~/.keitora-dev/<checkout>-<hash>-herdr`, keyed per
checkout so several worktrees can each run one. Set `KEITORA_DEV_HOME` to change the base.

Two things are still shared with your real setup: the herdr binary, and the task database. A dev
build can write to your real tasks.

herdr doesn't honour the isolation environment on every platform; on Windows it can reach your real
server regardless. The launcher checks for this before starting and refuses to run if it can't
confirm the herdr server is its own. It also only stops a herdr server on exit when that server is
its own. If it refuses, don't work around it: the dev app would attach to your real sessions, and
stopping it would end them.

If you ever have to stop a stray Keitora or herdr process, stop it by PID. Killing by image name
also takes down any copy of Keitora you're actually using.

## How Keitora finds herdr

Keitora uses the first of these that is an executable file:

1. `[pty] herdr_bin` in `keitora.toml`
2. `KEITORA_HERDR_BIN`
3. `herdr` (`herdr.exe`) on Keitora's own process `PATH`
4. macOS and Linux: `/opt/homebrew/bin/herdr`, `/usr/local/bin/herdr`, then `~/.local/bin/herdr`
5. the newest `~/.herdr/packages/standalone/releases/<version>-<triple>/herdr`, chosen by version
6. macOS and Linux: the first `herdr` on your login-shell `PATH`. An app launched from the Dock or
   Finder gets a minimal `PATH`, so this is how installs in places like `~/.cargo/bin` are found.
   It only runs when steps 1 to 5 found nothing, and it runs in the background so a slow shell
   doesn't delay the window.

If none of these exist, Keitora still tries a bare `herdr` when it starts the server. A herdr server
that's already running is recognised by name, not path, so it stays recognised when herdr updates
itself into a new directory.

Panes get your login-shell `PATH` first (so asdf, mise, nvm or pyenv shims still win), then the
step 4 directories, then Keitora's own `PATH`.

If herdr can't be found, the app still opens and the sidebar shows where it looked (the full list is
in `keitora.log`). Install herdr or set `herdr_bin`, then press **Restart** on that card.

On macOS and Linux herdr's sockets live under `$KEITORA_HOME/herdr/config/herdr/`, and unix socket
paths are capped at 103 bytes on macOS (107 on Linux). A `KEITORA_HOME` or `KEITORA_DEV_HOME` long
enough to exceed that is rejected at startup with a message naming the path and the limit.

### Upgrading from ptyd

Older builds ran terminals in a bundled `keitora-ptyd` daemon, which is gone. You can delete
`~/.keitora/ptyd.sock`, `ptyd.lock`, `ptyd-state.json` and `ptyd.log`. If an old `keitora-ptyd` is
still running, stop it by PID.

## Commands

| Command | What it does |
| --- | --- |
| `yarn dev:isolated` | Isolated development build (see above). `yarn dev:herdr` is the same thing |
| `yarn build` | Typecheck and build the frontend |
| `yarn tauri:bundle` | Build the packaged app, including the `keitora` CLI |
| `yarn test` | Frontend tests (Vitest) |
| `cargo test --workspace` | Rust tests |
| `yarn lint` | Biome lint and format check on `src` |
| `yarn format` | Format `src` with Biome |
| `yarn lint:rust` | `cargo fmt --check` and Clippy across the workspace |

`./keitora` at the repo root runs the CLI from source. It uses a built binary if there is one, and
falls back to `cargo run -p keitora-cli`.

## Checks

- New Rust logic comes with `#[cfg(test)]` tests in the same change. `cargo test --workspace` and
  `yarn test` must pass.
- The build has zero warnings. TypeScript is checked by Biome (`noExplicitAny` is an error) and
  `tsc --noEmit`. Rust uses `rustfmt` and Clippy with `clippy::all` and rustc `warnings` denied
  workspace-wide.
- CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, Clippy with warnings as errors and
  `cargo test` on Windows, macOS and Linux, plus Biome, `yarn build` and `yarn test`.
  `rust-toolchain.toml` pins the Rust version; rustup installs it automatically.
- The pre-commit hook (husky + lint-staged) formats and lints staged files, runs Clippy when Rust
  changed, and `tsc` when TypeScript changed. It installs on `yarn install` and does nothing in a
  worktree that hasn't run it. It formats the staged version of a file, so stage the whole file
  before committing.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/).
- This repo is public. Use placeholder names in tests, fixtures, docs and commit messages, never
  real projects, people or machines. [AGENTS.md](AGENTS.md) has the list of placeholders.

Recommended editor setup: VS Code with the
[Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) and
[rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
extensions.

## Releases

Releases are cut by tagging. `yarn release` bumps the version, commits, tags and pushes; the tag
push runs `.github/workflows/release.yml`, which builds and publishes the installers. The script
never builds or uploads anything itself.

```sh
yarn release --patch        # or --minor, --major, or --set X.Y.Z
yarn release --dry-run      # print every step and change nothing (works from any branch)
```

`--set X.Y.Z` must be greater than the current version. `--skip-ci-check` releases without a passing
CI run for `HEAD`, for example while Actions is down.

The script refuses to run unless:

- you are on `main`, with a clean working tree that isn't behind `origin/main`;
- the tag doesn't exist yet and the version files agree with each other;
- the latest `ci.yml` run for `HEAD` passed (unless `--skip-ci-check`).

It then commits `chore(release): vX.Y.Z`, tags it and pushes `main` and the tag in one atomic push.
If the push fails nothing reaches the remote, and the script undoes the local commit and tag. Only
admins can run it: `main`'s ruleset requires pull requests from everyone else.

`release.yml` builds a Windows NSIS `setup.exe` and an Apple Silicon `.dmg` signed with a
Developer ID Application identity (not notarized yet). Tag builds also publish the in-app
updater's files: `Keitora_<v>_x64-setup.exe.sig`, `Keitora.app.tar.gz` and `Keitora.app.tar.gz.sig`
(see [Updater signing](#updater-signing)). The result appears on the repo's
[Releases page](https://github.com/rudoi/keitora/releases). Pull requests that touch the release
files run the same builds without publishing, and a weekly scheduled run keeps the build cache warm
and catches runner drift.

If a tag's build fails, the tag exists but there is no release. Fix the cause and re-run the failed
jobs on that tag; publishing replaces any assets already uploaded. Or fix forward and release a new
patch version.

### One-time signing setup

Tag builds sign the app with the same identity every time. macOS ties a Screen Recording grant to
the app's designated requirement. For a Developer ID identity that is the bundle id plus the team
id, so a renewed certificate keeps everyone's grant, while switching to a self-signed or ad-hoc
signature resets it. The workflow also accepts a self-signed identity (it trusts that certificate on
the runner) and pins whichever one you upload by its SHA-1.

Upload the identity from the Mac that holds it, in your own terminal:

```sh
security find-identity -v -p codesigning           # copy the identity's 40-character SHA-1
bash scripts/macos-signing-secrets.sh <SHA-1>
```

The script puts a random export password on the clipboard and walks you through exporting that one
identity from Keychain Access to `~/keitora-signing-export/signing.p12`. The GUI exports only that
identity; `security export -t identities` would export every identity in the keychain. The script
then checks that the `.p12` holds the certificate you named and uploads it with `gh secret set` over
stdin, so neither the key nor the password is printed or passed as an argument. It sets:

| Name | Kind | Value |
| --- | --- | --- |
| `MACOS_CERTIFICATE_P12` | `release` environment secret | base64 of the `.p12` |
| `MACOS_CERTIFICATE_PASSWORD` | `release` environment secret | its export password |
| `MACOS_SIGNING_SHA1` | repository variable | the certificate's SHA-1 |
| `MACOS_SIGNING_IDENTITY` | repository variable | the identity's full name |

Finally it shreds the `.p12` and clears the clipboard. Re-run it whenever the certificate is renewed.

Tag runs fail with a clear message while any of these is missing. The `release` environment must
exist first, restricted to `v*` tags so no other ref can read the key:

```sh
gh api -X PUT repos/{owner}/{repo}/environments/release --input - <<'EOF'
{"deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}
EOF
gh api -X POST repos/{owner}/{repo}/environments/release/deployment-branch-policies -f name='v*' -f type=tag
```

### Updater signing

Keitora checks `https://api.github.com/repos/rudoi/keitora/releases/latest` (30 s after launch, then
every 6 h, unless *Automatically check for updates* is off in Settings) and installs a newer release
in place. herdr is left running, so sessions survive. The app installs only files signed with the
Tauri updater key whose **public** half is `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`.

The private key and its password are secrets in the `release` environment:

```sh
yarn tauri signer generate -w ~/keitora-updater.key        # once; prompts for a password
gh secret set TAURI_SIGNING_PRIVATE_KEY --env release < ~/keitora-updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --env release   # prompts for the password
```

Put the contents of `~/keitora-updater.key.pub` in `tauri.conf.json`. Keep an offline backup of the
key and password, then delete the local copy.

- **Losing the key strands every installed copy.** Installed apps can only verify updates signed
  with the key they trust, so a release signed by a new key must be installed by hand.
- **Rotating** means shipping a release, signed with the old key, whose `tauri.conf.json` holds the
  new public key. Every release after that is signed with the new key.
- Tag builds fail without the key (`UPDATER_KEY_MISSING`) and fail if the pubkey is still the
  `REPLACE_WITH_UPDATER_PUBKEY` placeholder. `scripts/check-updater-sig.ts` also fails a tag build
  whose `.sig` was made by a key other than the configured pubkey. Tauri only warns about that.
- Local `yarn tauri dev` and `yarn tauri:bundle` need no key and produce no updater files. The
  `createUpdaterArtifacts` flag comes from `scripts/bundle-overlay.ts` on CI.
- `KEITORA_UPDATE_REPO=owner/repo` points the updater at another repo (a fork or a test repo).
  Downloads still have to verify against the embedded public key. Development builds can check
  and download but refuse to install.
- The first release that includes the updater has to be installed by hand. Updates work from that
  release onwards.

### Signed macOS builds

`scripts/build-dmg.sh` is the local path for signed and notarized builds. It produces a `.dmg` in
`target/release/bundle/dmg/` and requires `APPLE_SIGNING_IDENTITY`; `--sign-only` skips
notarization. CI uses `--sign-only`. To notarize, also set one of these two sets of variables:

**Apple ID** (simplest for one developer)

| Variable | Value |
| --- | --- |
| `APPLE_ID` | The Apple ID email used for notarization |
| `APPLE_PASSWORD` | An app-specific password from appleid.apple.com, not your Apple ID password |
| `APPLE_TEAM_ID` | Your 10-character team ID, e.g. `AB12CD34EF` |

**API key** (better for CI, no 2FA prompts)

| Variable | Value |
| --- | --- |
| `APPLE_API_KEY` | The key ID from App Store Connect, e.g. `ABCD123456` |
| `APPLE_API_ISSUER` | The issuer UUID from App Store Connect |
| `APPLE_API_KEY_PATH` | Absolute path to the `.p8` key file |

`APPLE_SIGNING_IDENTITY` is the full certificate name, e.g.
`Developer ID Application: Jane Smith (AB12CD34EF)`. List yours with
`security find-identity -v -p codesigning`. Never commit any of these values.

### macOS permissions (Screen Recording)

Agents take screenshots with `screencapture`. macOS doesn't check that tool's own permission.
It checks the **responsible app**: the Keitora instance that started the `herdr server` your
panes run in. So the entry to allow under System Settings → Privacy & Security →
Screen & System Audio Recording is **Keitora**. Entries for herdr or your terminal don't matter
for agent sessions.

macOS saves that permission against the app's code signature. A plain `yarn tauri:bundle` with
no signing identity gives an ad-hoc signature, which is tied to that exact build. Every rebuild
looks like a new app: System Settings still shows Keitora as allowed, but each capture prompts
again and fails. To fix that, build with a stable identity:

```sh
security find-identity -v -p codesigning          # any Apple Development / Developer ID works
export APPLE_SIGNING_IDENTITY="Apple Development: Jane Smith (AB12CD34EF)"
yarn tauri:bundle
```

If you have no Apple certificate, `scripts/macos-local-signing-cert.sh` creates a self-signed
"Keitora Local Signing" identity in your login keychain. macOS may ask you to authorize trusting
it. The certificate only works on your own machine. `tauri:bundle` prints a warning on macOS
whenever it builds without a stable identity.

After you first install a signed build, grant the permission once:

1. Quit Keitora and run `herdr server stop`. The server outlives Keitora, and panes stay charged
   to whichever build started it. Stopping it ends live panes; Keitora resumes them on its next
   launch.
2. `tccutil reset ScreenCapture com.keitora.dispatch` clears the stale entry.
3. Launch Keitora and allow it when asked, or switch it on in System Settings.

`bash scripts/macos-capture-doctor.sh` checks all of this without changing anything. It reports
how the installed app is signed, which signing identities you have, and whether the running
herdr server is older than the installed build. Development builds (`yarn tauri dev`) are always
ad-hoc signed, so screenshots from agents in a dev build won't keep their permission.

Windows installers are unsigned for now, so SmartScreen warns on first install.
