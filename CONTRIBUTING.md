# Contributing to Keitora

This covers building Keitora from source, running a development build safely, and the checks a
change has to pass. If you use an AI coding agent, point it at [AGENTS.md](AGENTS.md) as well.

## Setup

You need a stable Rust toolchain, Node.js and Yarn. On macOS, also install the Xcode Command Line
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

It builds the app and the CLI and runs the app against a Vite server on port 1420, with its own
herdr server, its own state (wiped on every run) and a throwaway git project. It doesn't install
agent hooks into your real `~/.claude` or `~/.codex`. Its home is
`~/.keitora-dev/<checkout>-<hash>-herdr`, keyed per checkout so several worktrees can each run one.
Set `KEITORA_DEV_HOME` to change the base.

The throwaway project gets a `keitora.toml` that keeps its task database under the dev home
(`tasks/`) and points its session hooks at the CLI built from this checkout, so sessions report
idle and busy as they do for real. The herdr binary is still shared with your real setup.

The dev agents use your real Claude Code config unless you give them a credential that needs no
login: set `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`) or `ANTHROPIC_API_KEY` before
`yarn dev:isolated`. Then the launcher points `CLAUDE_CONFIG_DIR` at `<dev home>/claude` and
pre-trusts the throwaway repos there, so Claude doesn't stop at "trust this folder?". Without one,
a new dev repo asks once, and the answer is saved in your real config.

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

`yarn release` builds the bundle for the current platform and uploads it to the GitHub release for
the current version. `--patch`, `--minor` or `--major` bump the version first, and `--dry-run`
prints what it would do without doing it.

### Signed macOS builds

`scripts/build-dmg.sh` produces a signed, notarized `.dmg` in
`src-tauri/target/release/bundle/dmg/`. Set `APPLE_SIGNING_IDENTITY`, plus one of these two sets of
notarization variables:

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

Windows installers are unsigned for now, so SmartScreen warns on first install.
