# Windows Backend Port Plan

> **Status:** Superseded in part by the Windows desktop port (task
> #0088), which ports the Rust + Tauri desktop app. Terminals now run in a
> **herdr** server (the only PTY backend; the earlier standalone PTY daemon was
> removed on 2026-10-06), so the §1 claim below ("session layer is shared via
> `portable-pty`, no rewrite") and the `pty_spawn()` references in §2 are
> **historical**: Keitora no longer opens PTYs itself, herdr does. §2b–§4
> (PATH/binary/config) remain accurate and were implemented as part of that port.

## Windows build & install (#0088)

**Prerequisites on the Windows machine:**
- **Git for Windows** on `PATH` (`git rev-parse` drives config resolution).
- **herdr** installed. Keitora looks for it via `[pty] herdr_bin` in `keitora.toml`
  (`~` expanded), then `KEITORA_HERDR_BIN`, then `herdr` on `PATH` — the first
  candidate that exists wins. herdr self-updates into versioned directories
  (`%USERPROFILE%\.herdr\packages\standalone\releases\<ver>-<triple>\herdr.exe`),
  so a stored absolute path can go stale; prefer `PATH` or re-point `herdr_bin` after
  an update. If herdr can't be found or started, the app still opens with an
  unavailable backend and the rail shows the reason.
- **`claude`** installed (npm global → `%APPDATA%\npm\claude.cmd`), or on `PATH`.
- Rust MSVC toolchain; `cargo tauri build` (Tauri v2 cannot cross-compile Windows
  from macOS — build natively on Windows).

**Sidecar binaries:** the bundle config (`tauri.bundle.conf.json`) ships only
`binaries/keitora-cli` as a Tauri `externalBin` (staged by `yarn bundle-cli`; build the
release with `yarn tauri:bundle`). Tauri strips the target triple, placing
`keitora-cli.exe` beside the app. herdr is **not** bundled.

**`keitora.exe` on PATH:** session hooks resolve the CLI via `keitora_on_path()`, which
searches the Windows `PATH` for `keitora.exe`. The NSIS installer must place a
`keitora.exe` on `PATH` (add the install dir to the user PATH, or install into a dir
already on PATH). Until that installer step exists, set `[cli].path` in `keitora.toml`
to the CLI's absolute path as a fallback.

**Signing — deferred:** no certificate yet, so the NSIS installer is **unsigned**;
Windows SmartScreen will warn on first installs ("More info → Run anyway"). Signing
stays env-var-driven (`WINDOWS_CERTIFICATE_THUMBPRINT`, or `WINDOWS_PFX_PATH` +
`WINDOWS_PFX_PASSWORD`) so a signed build works once a cert exists.

**Unsupported install locations:** herdr's state and socket live under
`<KEITORA_HOME>/herdr`; `\\wsl$` / network / some container volumes may not support
them — keep `KEITORA_HOME` on a local NTFS path.

---

## 1. Session Layer

Terminals run in a **herdr** server that Keitora starts with an isolated state
directory (`<KEITORA_HOME>/herdr`), so the session layer is herdr's; herdr owns the
ConPTY on Windows. Keitora talks to it through the `PtyBackend` trait
(`src-tauri/src/backend/`), with the herdr implementation in
`src-tauri/src/backend/herdr/` (`env.rs` resolves the executable and builds the
isolated environment). When herdr can't start, `UnavailableBackend` stands in.

**History:** this originally relied on `portable-pty` / `native_pty_system()`
(ConPTY) called from `pty_spawn()` in `src-tauri/src/lib.rs`, and later on a standalone
PTY daemon. Both are gone; the §2 notes on `pty_spawn()` describe that
earlier design and are kept for the shell/PATH reasoning.

**Agent PATH on Windows:** the bundled CLI ships as `keitora-cli.exe`, which is not
on any pane's `PATH`. Keitora mirrors it to `%LOCALAPPDATA%\keitorain\keitora.exe`
and prepends that directory (then the configured `claude`/`codex` directory) to each
agent pane's `PATH` via herdr's per-pane `--env`, so agents can run a bare `keitora`.

---

## 2. Shell and PATH Handling

Three distinct sub-problems.

### 2a. Shell selection in `pty_spawn`

`src-tauri/src/lib.rs` `pty_spawn()` currently builds:

```rust
let mut cmd = CommandBuilder::new("sh");
cmd.arg("-c");
cmd.arg(&launch);  // e.g. "claude --continue || claude"
```

`sh` is not guaranteed on Windows (it is present only with Git Bash, Cygwin, or
WSL in `PATH`). **Decision:** detect the OS at compile time and use PowerShell 7
(`pwsh`) as the default shell, falling back to `cmd.exe`:

- `pwsh -c "claude --continue; if ($LASTEXITCODE -ne 0) { claude }"` — preferred;
  PowerShell 7 ships on Windows 11 and is downloadable for Windows 10.
- `cmd.exe /c "claude --continue || claude"` — fallback; `||` works in `cmd`.

The `command` parameter of `pty_spawn` (the `Option<String>` override from the
frontend) lets a user configure their preferred shell in a future settings panel
without a backend change.

### 2b. `claude` binary resolution in `pty_spawn`

The default launch string `"claude --continue || claude"` relies on `claude`
being on `PATH`. On macOS, `claude` is typically on `$PATH` via the user's shell
profile. On Windows, `claude` is installed via
`npm install -g @anthropic-ai/claude-code` and typically lands in
`%APPDATA%\npm\claude.cmd` — a directory not on the system PATH for
GUI-launched processes.

This is the same GUI-PATH problem tracked as **task #9** for the macOS build.
The Windows doc cross-references that task: the fix (probe well-known install
locations; fall back to a user-configured path) benefits both platforms.

Windows probe order for `claude`:
1. `%APPDATA%\npm\claude.cmd`
2. `%LOCALAPPDATA%\npm\claude.cmd`
3. `claude` on the inherited system PATH (unlikely from a GUI launch)
4. User-configured path (future settings panel)

### 2d. Path separators in `pty_spawn`

`pty_spawn()` receives `cwd: String` from the frontend (line 53) and passes it
directly to `cmd.cwd(&cwd)` (line 75). If the frontend sends a POSIX-style path
on Windows (e.g. `C:/Users/...` or `/c/Users/...`), `cmd.cwd()` may reject it.

**Resolution:** the frontend must always obtain the worktree path from the
`worktrees()` IPC call, which calls `Config::resolved_worktrees()` →
`worktree_path()`, returning a `PathBuf` serialized to a native string. As long
as the frontend never constructs paths independently, the boundary is safe. This
should be audited and enforced when Windows testing begins.

---

## 3. Hook Install Paths

`src-tauri/src/hooks.rs` `install_session_hooks()` installs session-state hooks
by writing to `<worktree_path>/.claude/settings.local.json`. The hook `command`
field is the absolute path to the compiled `keitora` binary, resolved per-worktree by
`resolve_keitora_bin()`.

Current probe candidates (the worktree's own build):

```rust
for cand in ["target/release/keitora", "target/debug/keitora"] {
```

**Required changes for Windows:**

1. **`.exe` extension:** the compiled binary is `keitora.exe` on Windows.
   `resolve_keitora_bin()` must probe `target/release/keitora.exe` and
   `target/debug/keitora.exe` (and the `[cli].path` / PATH candidates likewise).
   One-line change: add a `cfg`-conditioned suffix or probe both names.

2. **Hook runner behaviour:** confirm whether the Claude CLI on Windows treats the
   `command` field as a raw `CreateProcess` string or invokes `cmd.exe /c`. If
   it is a raw `CreateProcess` call, the `.exe` extension is required and no
   shell expansion occurs. (Expected: raw CreateProcess — verify against Claude
   CLI Windows docs before shipping.)

3. **Backslashes in paths:** the path written to `settings.local.json` will
   contain Windows backslashes. Confirm that the Claude CLI on Windows accepts
   backslashes in hook `command` paths (expected: yes, since `CreateProcess`
   accepts both, but verify).

4. **Global Claude CLI config location:** `%USERPROFILE%\.claude\` is the global
   Claude CLI config dir on Windows, equivalent to `$HOME/.claude/` on macOS.
   This is distinct from the per-worktree `<worktree>/.claude/settings.local.json`
   that Keitora writes — keep this distinction clear when reading Claude CLI docs.

---

## 4. Worktree Path Handling in task-config

The config reader is `crates/keitora-tasks/src/config.rs`.

### `Config::find()` / `find_config_path()`

`Config::find()` (line 52) delegates to the private free function
`find_config_path()` (line 151). That function starts from the git root:

```rust
fn git_toplevel() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        ...
```

**Prerequisite on Windows:** `git` must be on `PATH`. Git for Windows installs
it there by default; document this as an install prerequisite.

### `worktree_path()` / `resolved_worktrees()`

`Config::worktree_path()` (line 73) and `Config::resolved_worktrees()` (line 86)
construct paths using `PathBuf::join`, which is platform-native — no POSIX
hardcoding in this code.

### `db_path` risk

`db_path` is stored as a plain `String` field in `task-config.json` (line 24 of
`config.rs`). If `task-config.json` was written on macOS and committed to the
repo, `db_path` will contain a POSIX path (e.g. `/Users/alice/...`) that is
wrong on Windows.

**Resolution:** on Windows, `db_path` must use native separators or be a path
relative to `$KEITORA_SHARED_DIR`. When initialising a Windows install, generate
`task-config.json` on Windows (or use the `KEITORA_SHARED_DIR` env var to override
the absolute path at runtime).

### `git worktree list` parsing

As of Phase 4, no code path in `crates/keitora-tasks/` parses raw `git worktree list`
output. This is a future risk: if any such parser is added, it must handle both
POSIX and Windows path formats in `git`'s output. Flag this in any future PR that
adds worktree-list parsing.

---

## 5. Packaging

### Installer format

Tauri v2 supports NSIS (`.exe` installer) and MSI (`.msi`) via `bundle.targets`
in `src-tauri/tauri.conf.json`. **Recommendation for first pass:** NSIS — simpler
to configure, more widely understood by Windows users, smaller output size.

### Code signing (Authenticode)

Windows SmartScreen will block unsigned installers. Two certificate tiers:

- **EV (Extended Validation):** silences SmartScreen immediately. Requires
  hardware token; typically from DigiCert or Sectigo.
- **OV (Organization Validation):** cheaper; SmartScreen builds reputation over
  time (may warn on first few installs).

Tauri v2 signing environment variables for Windows (see
<https://tauri.app/distribute/sign/windows/>):

- `WINDOWS_CERTIFICATE_THUMBPRINT` — preferred; signs using a certificate already
  in the Windows cert store.
- `WINDOWS_PFX_PATH` + `WINDOWS_PFX_PASSWORD` — alternative; signs from a PFX
  file.

### Build environment

Tauri v2 does not support cross-compilation for Windows from macOS or Linux. The
signed Windows build must run on a Windows host or a `windows-latest` GitHub
Actions runner. A new CI job (parallel to the existing macOS job) is needed:

```yaml
jobs:
  build-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - name: Build
        run: cargo tauri build
        env:
          WINDOWS_CERTIFICATE_THUMBPRINT: ${{ secrets.WINDOWS_CERTIFICATE_THUMBPRINT }}
```

### Bundling `keitora.exe`

`resolve_keitora_bin()` probes the worktree's `target/release/keitora` /
`target/debug/keitora` build, then the `[cli].path` override, then `keitora` on PATH.
On a production Windows install none of the dev build paths exist, and `keitora.exe`
may not be on PATH.

**Required:** the installer must place `keitora.exe` on PATH (or set `[cli].path`) so
`resolve_keitora_bin()`'s PATH/override fallbacks resolve it on a production Windows
install — the same first-run install concern as macOS (#0007), with a Windows path.

---

## 6. Reference

- `src-tauri/src/backend/herdr/env.rs` — herdr resolution (`HerdrEnv::resolve`) and per-pane env
- `crates/keitora-tasks/src/config.rs` — `PtyConfig::herdr_bin`, `herdr_bin_candidates()`
- `src-tauri/src/dispatch.rs` — `run_claude_bounded()`, headless `claude` invocation
- `src-tauri/src/hooks.rs` — `resolve_keitora_bin()`, `install_session_hooks()`
- `crates/keitora-tasks/src/config.rs` — `Config::find()`, `find_config_path()`,
  `Config::worktree_path()`, `Config::resolved_worktrees()`
- `docs/build-plan.md` §10 — Windows risk item (origin of this doc)
- Tauri Windows signing: <https://tauri.app/distribute/sign/windows/>
