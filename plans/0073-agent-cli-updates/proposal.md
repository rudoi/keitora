# 0073 — Agent CLI updates: don't fail on update prompts, guide the user through them

## Goal

When Codex (or Claude Code) has an update available, Keitora must not treat the session
as a failed launch. Instead Keitora should know an update is pending, tell the user, and
help them apply it: run the update in a visible pane, then get them back into their
sessions.

## Problem

`codex` (TUI) checks for a newer release on startup. When it finds one it shows an
interactive screen before the session starts:

> Update available! 0.160.0 -> 0.163.0 … Update now (runs `npm install -g @openai/codex`) /
> Skip / Skip until next version

It caches what it found in `$CODEX_HOME/version.json`
(`{"latest_version", "last_checked_at", "dismissed_version"}`) and honours the config key
`check_for_update_on_startup` (present in the shipped binary). A per-launch
`codex -c check_for_update_on_startup=false …` override is accepted before a subcommand,
and unknown `-c` keys are accepted silently (verified against codex-cli 0.160.0).

Every Keitora pane launch goes through herdr `agent start … --timeout 10000`, which waits
for the agent to become ready. A codex sitting on the update screen never does:

- **Fresh launch / `session_launch`:** `Spawner::spawn_agent_locked`
  (`src-tauri/src/backend/herdr/mod.rs`, the `agent_start` match). herdr's `timeout`
  error (`cli.rs` → `PtyBackendError::Other("herdr error timeout: …")`) falls into the
  generic `Err(e)` arm, the pane is **closed** (destroying the prompt), and the frontend
  shows the spawn-failed overlay "Failed to launch codex — is it on PATH?"
  (`spawnFailedCopy`, `src/lib/transferPhase.ts`). If the user answers the prompt and
  codex exits, `is_early_exit` walks the resume → continue → fresh chain, each step
  hitting the same prompt.
- **Boot / herdr-recovery resume:** the same `spawn_agent_locked` via `resume_locked`.
  The runner (`BackendResumeRunner::resume`, `backend/herdr/mod.rs`) marks a timeout as
  `load_related` (`is_load_related_error` / `timed_out || fell_back`), and
  `recovery/machine.rs::resume_result` then drops `resume_cap` to 1 and staggers.
  So one outdated CLI slows every other resume.
- **Claude → Codex transfer:** the strict launch (`backend/herdr/strict.rs`) keeps
  watching past the timeout. `classify_never_appeared` accepts any pane with a foreground
  process as `PresentUnknown`, so a codex parked on the update screen is **accepted** as
  a successful launch, after up to the 10 s verify window.
- **herdr may also report success.** `agent_start_result_from` (`cli.rs`) folds
  `agent_not_ready` / "blocked during startup" into `Ok`. If herdr classifies the update
  screen that way, the default path "succeeds" with a pane parked on the prompt and the
  agent shown as blocked.

Claude Code's native auto-updater runs in the background and does not block startup
today. Nothing in Keitora knows a Claude update exists either.

## Scope

**In**

1. **Never block a launch on a CLI's own update prompt.** Every codex launch gets
   `-c check_for_update_on_startup=false`. Claude's updater is left alone.
2. **Keitora-side update awareness** for both CLIs: installed version vs. latest,
   checked on app start and every N hours (user setting, default 6), cached.
3. **An agent-update banner** (same family as `UpdateBanner`): "Codex 0.163.0 is
   available (you have 0.160.0)" with **Update**, **Skip this version**, **Later**.
4. **Run the update in a visible terminal pane** with the right command for how the CLI was
   installed, show the result, re-probe the version, then offer a **Restart Codex
   sessions** button. Restarting is never automatic.
5. **Defensive prompt detection** in case a codex build ignores the override: recognise the
   update screen on the default and strict launch paths, keep or fail the pane with a
   distinct `AgentUpdatePrompt` error, and show "Codex wants to update" with Update / Retry
   instead of "is it on PATH?".

**Out**

- Auto-applying updates or auto-restarting sessions.
- Updating herdr itself, or Keitora (it already has its own updater).
- Taking over Claude's updater (no `DISABLE_AUTOUPDATER`).
- Pinning or downgrading CLI versions; volta/pnpm/custom-prefix install kinds beyond
  "show the command".
- Telemetry on update adoption.

## Architecture

```
 app start (after recovery settles, capped) + every agent_check_hours
        │
        ▼
 agent_updates (Rust, src-tauri/src/agent_updates/)
   ├─ installed:  codex --version / claude --version   (probe; cache invalidated after an update)
   ├─ latest:     codex → $CODEX_HOME/version.json if fresh, else npm registry
   │              claude → npm registry
   ├─ install:    classify the resolved exe path → npm | brew | native | unknown → command
   └─ skip:       [updates] agent_skip_{codex,claude} in keitora.toml
        │ emits agent-updates://status  (one entry per CLI found)
        ▼
 AgentUpdatesProvider (React) ──▶ AgentUpdateBanner ─ Update ─▶ AgentUpdateDialog
                                                                   │ TerminalPane on the reserved
                                                                   │ key  (__keitora / agent-update)
                                                                   ▼
                                     pty_spawn(command = "<install cmd>; echo <sentinel>:$?")
                                                                   │ sentinel read back
                                                                   ▼
                                         re-probe ─▶ "Codex updated to X" + Restart Codex sessions
                                                                   │
                                                                   ▼
                                     agent-updates://restart {keys} ─▶ each mounted TerminalPane:
                                     kill + sessionLaunch (existing Kill & Restart flow)

 launch path
   build_agent_args_opts ── codex: ["-c", "check_for_update_on_startup=false", …] prefix
   spawn_agent_locked  ── Err arms: pane_read → update screen? → AgentUpdatePrompt (no fallback chain)
                       ── Ok(AgentStart::NotReady): pane_read once → update screen? → agent-updates://prompt (pane kept)
   strict_step         ── before accepting Confirmed/PresentUnknown: pane_read → update screen? → fail, stop chain
```

## Technical design

### 1. Suppress the codex startup prompt

`build_agent_args_opts` (`src-tauri/src/backend/herdr/agent.rs`) **prefixes** every
`CliKind::Codex` argv with `-c`, `check_for_update_on_startup=false`. It is a global codex
flag, so it comes first, before the `resume` subcommand:
`codex -c check_for_update_on_startup=false resume <id> --dangerously-… --dangerously-bypass-hook-trust`.
`--dangerously-bypass-hook-trust` stays last, as already documented there. The value
has no characters that herdr's shell-typed argv would need to quote.

Both the default launch (`mod.rs`, via `build_agent_args`) and the strict launch (`strict.rs`,
via `build_agent_args_opts`) build their argv here, so this one change covers fresh, resume,
recovery and transfer panes. The headless `codex exec resume …` call in `transfer.rs` gets
the same prefix for consistency: `summary_args` (`transfer.rs`, the one place that builds
that argv; `summary_command` calls it) changes its return type from `[&str; 8]` to
`[&str; 10]` and starts with `"-c", "check_for_update_on_startup=false"`, then `"exec"`,
`"resume"`, …. Update its two tests, `summary_args_by_id_shape` and
`summary_args_last_shape_for_native_codex_worktree` (`transfer.rs`). `codex app-server`
does not run the TUI check and is left alone.

Exact-argv tests to update in `agent.rs`: `codex_args_always_bypass_hook_trust_plus_autonomy`,
`build_agent_args_matrix` (codex rows), `unsafe_resume_id_degrades_to_continue`. Add a
test asserting the prefix comes first and hook-trust stays last.

Side effect: with startup checks off, codex stops refreshing `version.json`, so Keitora's
own check fetches "latest" itself when that file is stale.

### 2. `agent_updates` module (new, `src-tauri/src/agent_updates/`)

Pure, testable core (`model.rs`) plus thin IO (`probe.rs`, `mod.rs` scheduler + state),
in the style of `codex_env.rs` / `updater/release.rs`:

- **Installed version**
  - codex: `codex --version` via `codex_env::codex_version(exe, path)`. Its cache is a
    function-local `static CACHE` that keeps a hit forever. Hoist it to a module-level
    `static VERSION_CACHE: OnceLock<VersionCache>` in `codex_env.rs`, have `codex_version`
    use it, and add `pub fn invalidate_codex_version(exe: &str)` that removes the `exe` key.
    Call it after an update run.
  - claude: new `claude --version` probe, parsed by `parse_claude_version_output`
    (`"2.1.328 (Claude Code)"` → `2.1.328`), 5 s bound, cached per exe with the same
    invalidation. Executables come from `discover_cli_cached(CliKind::Codex|Claude)`
    (`crates/keitora-tasks/src/config.rs`). A CLI that isn't found reports `NotInstalled`
    and gets no banner.
- **Latest version**
  - codex: parse `$CODEX_HOME/version.json` (home resolved by `codex_env`). Trust
    `latest_version` if `last_checked_at` is < 24 h old. Otherwise GET
    `https://registry.npmjs.org/@openai/codex/latest` → `.version`.
  - claude: GET `https://registry.npmjs.org/@anthropic-ai/claude-code/latest` → `.version`.
  - blocking `reqwest` with a 10 s timeout. Call `updater::source::install_crypto_provider()`
    first (the crate is built `rustls-no-provider`). No user data is sent.
  - Network failure → `latest = None` → status `Unknown`, so no banner is shown.
- **Install kind** — `install_kind(cli, exe_path, home) -> InstallKind`, a pure function
  with table tests. `exe_path` is the string `discover_cli_cached` returned (or the
  configured path), **not canonicalised**: symlinks are not followed. It is normalised to
  a `/`-separated, lower-cased string, and `home` (`dirs::home_dir()`) the same way. The
  first matching rule wins, in this order:
  1. `Npm`: the path contains any of `/node_modules/`, `/npm/` (Windows
     `%APPDATA%\npm\*.cmd` shims), `/nvm/`, `/nvm4w/`, `/.nvm/`, `/nodejs/`.
     (Example: an nvm-windows install puts codex at `C:\nvm4w\nodejs\codex.cmd`, a shim for
     `node_modules\@openai\codex\bin\codex.js`, which matches `/nvm4w/` and `/nodejs/`.)
  2. `Brew`: the path starts with `/opt/homebrew/`, or contains `/caskroom/` or
     `/cellar/`. `/usr/local/bin/*` alone is not `Brew` (npm's default unix prefix lives
     there too), so it falls through. A brew-node npm global (`/opt/homebrew/bin/codex`)
     is therefore classified `Brew`; the resulting `brew upgrade` fails visibly in the
     pane, which is the accepted cost.
  3. `Native`: the path starts with `<home>/.local/bin/<cli>` (any `.exe` suffix) or
     `<home>/.claude/local/` (claude) / `<home>/.codex/bin/` (codex).
  4. else `Unknown`.
- **Update command** — `update_command(cli, kind) -> Option<String>`:
  - codex `Npm` → `npm install -g @openai/codex@latest`
  - codex `Brew` → `brew upgrade --cask codex` *(cask name is best knowledge, not checked
    on this machine)*
  - codex `Native` → `None`. `codex update` exists in codex-cli 0.160.0 (`codex update
    --help`: "Update Codex to the latest version"), but how it behaves for a native
    install could not be verified here, so Keitora does not run it. The banner shows
    `codex update` as copyable guidance ("run this in your own terminal"), like `Unknown`.
  - claude `Native` → `claude update`
  - claude `Npm` → `npm install -g @anthropic-ai/claude-code@latest`
  - claude `Brew` → `brew upgrade --cask claude-code` *(cask name is best knowledge, not
    checked on this machine)*
  - `Unknown` → `None`. The banner shows the npm command for that CLI as a copyable
    suggestion and "Keitora couldn't tell how {cli} was installed". No Update button is
    shown whenever the command is `None`; the copyable text is shown instead.
- **Assessment** — `assess(installed, latest, skipped) -> AgentUpdateState`, using a
  semver compare (with lenient parsing of `0.149.0-alpha.1`):
  - `UpToDate`
  - `Available{installed, latest, command}`
  - `Skipped{latest}`, when `latest == skipped`. A newer latest re-arms the banner.
  - `Unknown`
  - `NotInstalled`
- **Settings** (in the user `keitora.toml`, `[updates]` table of
  `crates/keitora-tasks/src/config.rs::UpdatesConfig`, config.rs:275, alongside
  `auto_check`). Add exactly these fields, each with
  `#[serde(default, skip_serializing_if = "Option::is_none")]`:
  - `pub agent_check_hours: Option<u32>`: unset = 6; `0` = app start only; any other
    value is clamped to `1..=168`. Add `pub fn agent_check_hours(&self) -> u32` that
    returns the effective value (unset → 6, 0 → 0, else clamped).
  - `pub agent_skip_codex: Option<String>` and `pub agent_skip_claude: Option<String>`.
  - Extend `is_empty()` (config.rs:292) to `auto_check.is_none() &&
    agent_check_hours.is_none() && agent_skip_codex.is_none() &&
    agent_skip_claude.is_none()`. Add round-trip and table tests next to
    `updates_auto_check_false_round_trips` (config.rs:4143 block).
  - Exposed through `SettingsUser` (`host.rs:619`) as a new field
    `pub agent_update_check_hours: u32`, filled in `get_settings` (`host.rs:696`, next to
    `auto_check_updates`) from `user_cfg.updates.agent_check_hours()`. Mirrored in
    `src/lib/settings.ts` (`SettingsUser`, next to `auto_check_updates` at line 63) as
    `agent_update_check_hours: number`, plus the `.harness/app.tsx` mock settings
    (line 332).
  - The interval is saved by a **new command** `set_agent_update_check_hours(hours: u32)`
    in `host.rs`, written exactly like `set_update_auto_check` (`host.rs:930`,
    persist-only, no `project://changed` emit), with a wrapper
    `setAgentUpdateCheckHours(hours)` in `settings.ts` (next to `setUpdateAutoCheck`,
    line 128). It is **not** folded into `save_user_settings` (that command's signature is
    `(app, claude_path, codex_path, default_cli)` and the drawer calls it on its own
    save button; changing it would touch unrelated code).
  - "Skip this version" writes through a new `agent_update_skip(cli, version)` command.
    **The mutex already exists:** `keitora_tasks::UserConfig::write_lock()`
    (`config.rs:832`, a process-wide `REGISTRY_WRITE` guard that `save_user_settings`,
    `set_skip_permissions` and `set_update_auto_check` already take). The command does
    `let _registry = keitora_tasks::UserConfig::write_lock();`, `UserConfig::load()`, sets
    `updates.agent_skip_codex` / `agent_skip_claude`, `save()`. The lock is not
    reentrant: take it once and call no other registry writer while holding it. No new
    static is added.
- **Scheduler** — `pub fn startup(app: AppHandle)` in `agent_updates/mod.rs`, called from
  the `.setup` closure in `host.rs` immediately after `crate::updater::startup(...)`
  (`host.rs:2342`; the backend is already managed by then, `host.rs:2323-2325`). It
  spawns one named thread `agent-updates-schedule`, mirroring `updater::spawn_schedule`
  (`updater/mod.rs:516`). It re-reads `UserConfig::load()` each tick, so an interval
  change applies at the next tick. Register `mod agent_updates;` in `lib.rs` (alphabetical,
  before `mod backend;`).
  - The first check waits until recovery is settled. The thread holds the `AppHandle` and
    reads `app.state::<SharedBackend>().recovery_snapshot()` (`PtyBackend::recovery_snapshot`,
    `backend/mod.rs:716`, returning `backend::herdr::recovery::RecoveryPayload`). Settled
    means `snap.phase == crate::backend::herdr::recovery::Phase::Healthy &&
    snap.queue.is_empty()` (fields `phase: Phase`, `queue: Vec<QueueItem>`;
    `recovery/payload.rs:32`). Polled every 2 s, capped at 3 min, then it checks anyway.
  - Unlike Keitora's own updater, it also runs in debug builds. It makes only cheap
    read-only calls.
- **State + IPC**
  - `startup` also calls `app.manage(AgentUpdates::default())` before spawning the
    thread. `AgentUpdates` wraps a `Mutex<AgentUpdatesSnapshot>` holding `{codex:
    AgentUpdateEntry, claude: AgentUpdateEntry, checked_at, stopped: Vec<SessionKey>}`,
    where an entry has `state` plus `exe` and `install_kind` for the settings drawer.
  - The commands live in `agent_updates/commands.rs` (module `pub mod commands;`) and take
    `State<AgentUpdates>`: `agent_updates_status()` returns the snapshot,
    `agent_updates_check()` forces a check, `agent_update_skip(cli, version)` records a
    skip. Event `agent-updates://status` fires on every change.
  - **Event emission pattern:** a `const STATUS_EVENT: &str = "agent-updates://status"`
    (likewise `RUN_EVENT`, `PROMPT_EVENT`, `STOP_EVENT`, `RESTART_EVENT`) and
    `use tauri::Emitter;` then `let _ = app.emit(STATUS_EVENT, &payload);`, exactly as
    `updater::AppSink` does (`updater/mod.rs:123-131`, constants at line 26). Payloads
    derive `Serialize` with `#[serde(tag = "type", rename_all = "snake_case")]` for the
    state/outcome enums, like `UpdateStatus` (`updater/mod.rs:34`).
  - **Registration:** add to the `tauri::generate_handler![…]` list in `host.rs`
    (`.invoke_handler`, `host.rs:2398`): `set_agent_update_check_hours` right after
    `set_update_auto_check` (line 2453), and after
    `crate::updater::commands::update_apply` (line 2457):
    `crate::agent_updates::commands::agent_updates_status`,
    `…::agent_updates_check`, `…::agent_update_skip`, `…::agent_update_run`,
    `…::agent_update_close`, `…::agent_update_live_sessions`. Frontend wrappers and
    `onAgentUpdates*` listeners go in `src/lib/ipc.ts` next to `onUpdaterStatus`
    (line 1252), using `listen<T>("agent-updates://…", cb)`.

### 3. Run the update in a visible pane

**Surface.**
- Every herdr pane in Keitora belongs to a `SessionKey {project, worktree}` and is shown by
  `TerminalPane`. The update gets a **reserved key**: `project = "__keitora"`,
  `worktree = "agent-update"`.
  - **Collision finding (checked):** nothing in the repo rejects a project name. A project
    name is the committed `keitora.toml` `[project].name` (any string) or the root
    basename (`onboard_project`, `host.rs:362-388`); `UserConfig::register`
    (`config.rs:859`) takes any `&str`. So `__keitora` is *not* provably unregistrable by
    construction, and no name is (a hand-edited toml can set any string). Decision: keep the
    key, make it collision-proof in two ways. (1) `SessionKey::is_reserved()` compares
    **both** fields (`project == RESERVED_PROJECT && worktree == RESERVED_WORKTREE`), so a
    real project named `__keitora` is hidden only if it also has a worktree called
    `agent-update`. (2) `onboard_project` (`host.rs`, right after `project_name` is
    determined, before step 3) returns
    `Err("The project name \"__keitora\" is reserved")` when `project_name ==
    SessionKey::RESERVED_PROJECT`. Add a test for each.
  - Add to `backend/mod.rs` (next to `SessionKey`, line 73):
    `impl SessionKey { pub const RESERVED_PROJECT: &'static str = "__keitora"; pub const
    RESERVED_WORKTREE: &'static str = "agent-update"; pub fn agent_update() -> Self; pub
    fn is_reserved(&self) -> bool }`. This single helper is used at every site below.
- **Every site that must exclude the reserved key** (the central filter is inside
  `KeyMap`, so most consumers are covered at once):
  1. `KeyMap::entries()` (`backend/herdr/keymap.rs:413`): `.filter(|(k, _)| !k.is_reserved())`.
     `get`, `get_current`, `is_current_gen`, `remove` and `pane_owner` (keymap.rs:220-247,
     423) stay **unfiltered**: `attach`, `write`, `read_history`, `kill_live` and the
     "pane is mapped to a session" guard in `close_name_holder` (`mod.rs:877`) must still
     see the reserved pane. This one change covers all of:
     - `HerdrBackend::mark_owed` (`mod.rs:1290`, the owed queue seed);
     - the `entries()` check in `mod.rs:1347` (does anything need recovery);
     - `summaries_from` (`mod.rs:2304`), which is the source of `list_sessions`
       (`mod.rs:2774`) and `cached_sessions` (`mod.rs:2794`), hence the Tauri command
       `daemon_list_sessions` (`host.rs:2029`) and the activity engine's busy-set
       (`activity/mod.rs:206`);
     - the attach-time resume-store observation (`mod.rs:2514`);
     - herdr restore reconcile: `adopt_live_panes` (`reconcile.rs:258`), `adopt_from`'s
       observe (`reconcile.rs:316`), `reconcile_lineage` (`reconcile.rs:417`);
     - the status loop's observation of live panes (`status_loop.rs:527-541`), so the
       reserved pane never writes a resume-store row or `agent_present` state (it is also
       a plain shell, with no agent);
     - the perf overlay roots: `HerdrPerfSource::panes_at` (`perf_source.rs:55`) joins
       only the workspaces an entry owns, so the update pane is not sampled.
  2. `KeyMap::begin_recovery` (keymap.rs:307): skip reserved keys, so `should_be_running`
     can never hold one (covers `note_server_death`, `mod.rs:397-400`, and the
     recovery-queue seeds).
  3. `snapshot_entries` (keymap.rs:467): skip reserved keys, so the reserved pane is never
     written to the persisted keymap file (after an app restart it is simply forgotten,
     and its stray herdr workspace is left as an unreferenced shell; acceptable).
  4. `HerdrBackend::spawn` (`mod.rs:2412`, `self.live.lock().unwrap().insert(...)`): do not
     insert the reserved key into the `live` set, so the quit-confirm guard
     (`host.rs:2379`, `live_set_snapshot().len()`) and `daemon://status` counts ignore it.
     The `kill` path's `live.remove` is a harmless no-op for it.
  5. Frontend: `useDaemonSessions` / `daemonListSessions` (`src/lib/sessions.ts:289`,
     `src/lib/ipc.ts:468`) consume only the filtered backend list, so no frontend change
     is needed; `sessionCounts.ts` takes the same list.
  - **Not affected (checked):** `hooks::install_hooks_for` / `codex_hooks::install_codex_hooks_for`
    run only from project activation and worktree setup (`host.rs:512`, `554-561`, `1122`,
    `2359`), never from `spawn`/`spawn_agent`, so the update pane writes no `.claude` /
    `.codex` files; the activity engine iterates registered worktrees, so there is no
    warning path; `worktree_exists` (`host.rs:1479`) answers `false` for the key, which
    only matters if a stale key ever reached reconcile.
  - Tests: `KeyMap::entries()` hides it while `get` returns it; `begin_recovery` ignores
    it; `snapshot_entries` omits it; `spawn` with the reserved key leaves
    `live_set_snapshot()` empty; `list_sessions` omits it.
- Recovery gate: `attach`/`write_line` for the key go through `gate.admit_session(key)`
  (`mod.rs:2486`), which refuses with the recovering token while herdr recovery is not
  `Healthy`. The banner's **Update** button is therefore disabled (tooltip "Wait for
  session recovery to finish") unless `recovery.phase === "Healthy"` (the `useRecovery()`
  value App already holds, `App.tsx:404`).

**Flow.**
1. Banner **Update** opens `AgentUpdateDialog` (modal), which mounts a `TerminalPane`
   for the reserved key. Plumbing (checked against the code):
   - `pty_spawn` (`host.rs:1518`) already has `command: Option<String>`; `spawn_pty_with`
     (`host.rs:1326`) sends an explicit command straight to `PtyBackend::spawn`, ignoring
     `transfer`/`fresh`. `HerdrBackend::spawn` (`mod.rs:2366`) calls
     `workspace_create(&cwd, &env::pane_shell_env(..))`, then
     `pane_run(&pane_id, &[command])`, records the keymap entry and starts the bridge. It
     runs no hook or worktree setup, and the env is the plain shell env (user XDG dirs, no
     agent PATH). `pty_spawn` also calls `active_project()?` for a `db_path` that `spawn`
     ignores, so an active project must exist (the app always has one past onboarding).
   - The frontend `ptySpawn(project, id, cwd, size, command?, fresh?, transfer?)`
     (`src/lib/ipc.ts:590`) already takes `command` as its 5th argument.
   - **`TerminalPane` prop addition (exact):** add one optional prop
     `command?: string` to the props type and destructuring in `TerminalPane.tsx`
     (`:169-225`), documented as "Shell line typed into a fresh pane on spawn; set only
     by `AgentUpdateDialog`". In `spawn` (`:563`) change line 582 from
     `ptySpawn(projectName, name, path, size, undefined, fresh, transfer)` to
     `ptySpawn(projectName, name, path, size, command, fresh, transfer)`, and add `command`
     to that `useCallback`'s dependency array (`:617`). Nothing else changes: the pane
     attaches first, gets `NoLiveSession`, and falls back to `spawn()` (`:747`), which is how
     the command is typed. `restart`/`launch` paths are unreachable for the dialog pane
     (it hides the restart controls).
   - **Props the dialog passes** (all other props of `TerminalPane` are required, so each
     value is fixed here): `name="agent-update"`, `projectName="__keitora"`,
     `path={run.cwd}` (the `cwd` returned by `agent_update_run`), `active={true}`,
     `daemonInfo` and `recovery` from `useDaemonStatus()` / `useRecovery()` (the same
     values `App.tsx:402-404` passes to every pane; the dialog receives them as props
     from `App`), `restartNonce={0}`, `cli={cli}` (the CLI being updated), `terminated={false}`,
     `terminateNonce={0}`, `launchNonce={0}`, `command={run.command}`. `transfer`,
     `isDropTarget`, `restartFresh`, `restartTransfer`, `onSpawnSettled`,
     `onCancelTransfer`, `launchFresh` stay at their defaults.
   - `cwd` is the user's home directory (`dirs::home_dir()`), returned by the backend.
2. The command is `<update cmd>; echo "__KEITORA_AGENT_UPDATE_DONE:$?"`. On Windows the
   herdr shell is Git Bash, so POSIX syntax holds on every OS.
3. The backend command `agent_update_run(app, cli)` (in `agent_updates/commands.rs`) does
   the following:
   - builds that command line from the assessed entry (the frontend never sends a raw
     command), and refuses (`Err`) when the entry has no command (`None`);
   - returns `{command, cwd}` to the dialog, which spawns the pane;
   - starts a watcher thread that, every 1 s, calls
     `PtyBackend::read_history(&SessionKey::agent_update(), 50)` (`backend/mod.rs:601`;
     the herdr impl is one `pane read --source recent --format text --lines 50`, and works
     for the reserved key because it uses the unfiltered `KeyMap::get`) and looks for the
     sentinel, bounded at 15 min. A `NoSuchSession` / error read before the pane exists is
     ignored (retry on the next second). The sentinel line is recognised with
     `line.trim().strip_prefix("__KEITORA_AGENT_UPDATE_DONE:")` followed by
     `rest.trim().parse::<i32>()` succeeding; the typed command line also echoes the text
     but ends in `$?"`, which does not parse as a number, so it never matches.
4. When the watcher sees the sentinel:
   - It parses the exit code, invalidates the version cache, and re-probes.
   - It emits `agent-updates://run {cli, outcome}`. `outcome` is one of:
     - `Updated{from, to}`
     - `Unchanged{version}`: exit 0 but the same version, for example a cask lagging
       behind.
     - `Failed{code, hint}`
   - `hint` is set by scanning the same 50 history lines (case-sensitive substring):
     `EBUSY` or `EPERM` → "close running {Codex} sessions first"; `EACCES` → "needs elevated
     permissions; run the command with sudo in your own terminal". Otherwise `None`.
   - It then re-runs the assessment.
5. The pane stays open in the dialog until the user closes it. Closing calls the new
   command `agent_update_close()` (frontend wrapper `agentUpdateClose()`), which runs
   `client.kill_live(&SessionKey::agent_update(), None)` (`backend/mod.rs:653`; the herdr
   override at `mod.rs:2756` kills the mapped generation without needing the pane's `gen`,
   and sets no `terminated` flag). The dialog also calls it on unmount.

**Windows file lock.** `npm install -g` cannot replace `codex.exe` (or a native
`claude.exe`) while processes hold it open.
- When the user clicks **Update** on Windows, the dialog first lists the live sessions of
  that CLI. `PtyBackend` has no `pane_list`, so these are enumerated in Rust as
  `agent_update_live_sessions(cli) -> Vec<SessionKey>`: `client.list_sessions()` (already
  free of the reserved key), keeping keys for which
  `crate::host::resolve_agent_launch(&key)` (`host.rs:1445`, `pub(crate)`) returns a
  launch with `launch.cli == cli`. A session whose agent was quit but whose pane
  remains is included; stopping it is harmless.
- If there are any, it offers **Stop {n} sessions and update** / **Update anyway** /
  **Cancel**.
- **Stop** emits `agent-updates://stop {keys}`. Each mounted `TerminalPane` with a
  matching key calls `ptyKill` and remembers that it was stopped for an update; its
  ended overlay reads "Stopped for the Codex update".
- Keys with no mounted pane are killed from Rust via `PtyBackend::kill_live(&key, None)`.
- The stopped keys are kept in `AgentUpdatesSnapshot.stopped`, in memory, cleared on
  restart.
- `kill` does not set `terminated`, but it cancels recovery for that key, so a restart
  must be explicit (below).

**Restart button.**
- After `Updated`, the banner shows "Codex updated to X. Running sessions keep the old
  version until restarted" with **Restart Codex sessions** and **Dismiss**.
- Restart targets the union of the `stopped` keys and the current live sessions of that
  CLI.
- It emits `agent-updates://restart {keys}`. Each mounted `TerminalPane` whose key
  matches runs its existing Kill & Restart flow (`ptyKill` then `sessionLaunch`, which
  resumes through `ResumeStore::resolve`).
- Keys that aren't mounted are relaunched lazily: the frontend records them in a
  `pendingRestart` set, and the next time a `TerminalPane` for one mounts it runs
  `sessionLaunch` instead of attach.

### 4. Defensive prompt detection

- **Matcher.** `agent::update_prompt_cli(screen: &str) -> Option<CliKind>` is a pure
  matcher. It returns `Codex` when the text contains `"Update available"` and either
  `"Skip until next version"` or `"Update now"`. No Claude matcher is added in this work
  (Claude's updater never shows a startup screen). It is table-tested with scrubbed
  fixtures: the codex screen from the Problem section, a normal codex TUI screen, empty
  text, and a screen with only `"Update available"`.
- **New error.** `PtyBackendError::AgentUpdatePrompt(keitora_tasks::CliKind)` in
  `backend/mod.rs:450`. The only exhaustive `match` over `PtyBackendError` is `Display`
  (`backend/mod.rs:476`); add the arm `AgentUpdatePrompt(cli) => write!(f,
  "AgentUpdatePrompt: {}", <"claude" | "codex">)` (inline `match cli`, since
  `backend/mod.rs` has no kind-to-string helper) and extend the test
  `backend_error_display_strings` (`backend/mod.rs:848`).
  - `spawn_err_kind` (`host.rs:1245`) needs **no change**: its `other =>
    daemon_err(other)` arm returns `e.to_string()`, which is already the bare token
    `AgentUpdatePrompt: codex`. Add a case to `spawn_err_kind_tokens` (`host.rs:2537`)
    asserting that. `attach_err_kind` (`host.rs:1213`) falls into its
    `Internal: …` wildcard, which is fine (an attach never produces this error).
  - `sanitize_ui_tokens` (`strict.rs:361`) gains
    `.replace("AgentUpdatePrompt", "agentupdateprompt")`, so pane text echoed into an
    error can't spoof the token. Extend `sanitize_ui_tokens_lowercases_recovering_and_terminated`
    (`strict.rs:897`).
  - `is_load_related_error` (`backend/herdr/mod.rs:1575`) already returns `false` through
    its wildcard, so recovery still staggers on the failure but does not drop concurrency
    to serial. Add a test.
  - Recovery removes the entry from the queue like any failure. The session's pane then
    falls back to its own `spawn()` (`TerminalPane.tsx:747`), which surfaces the spawn-
    failed overlay with the new copy and, per the Frontend bullets below, triggers an
    immediate agent-updates check.
- **Shared helper.** In `backend/herdr/mod.rs` add
  `impl Spawner { fn update_prompt_on_screen(&self, pane_id: &str, source: &str, lines:
  Option<u32>) -> Option<CliKind> }`: `self.ops.pane_read(pane_id, source, "text",
  lines).ok().and_then(|t| agent::update_prompt_cli(&t))`. A read error is `None`. It is
  only called when the launch's CLI is `CliKind::Codex`.
- **`AgentStart` return type (minimal churn).** Add to `backend/herdr/ops.rs`, next to
  `HerdrOps`: `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum AgentStart { Ready,
  NotReady }`. Change the trait method `HerdrOps::agent_start` (`ops.rs:182`) to return
  `Result<AgentStart, HerdrError>` (update its doc comment: `NotReady` = herdr reported
  `agent_not_ready` / "blocked during startup"). Concretely:
  - `cli.rs:224` `agent_start_result_from(err) -> Result<AgentStart, HerdrError>`: the
    not-ready branch returns `Ok(AgentStart::NotReady)` instead of `Ok(())`. Update its
    tests (`cli.rs:575-602`): the two `is_ok()` asserts become
    `assert_eq!(.., Ok(AgentStart::NotReady))`.
  - `real.rs:367-395` `RealHerdr::agent_start`: `Ok(_) => Ok(AgentStart::Ready)`; the
    `Err` arm is unchanged (it calls `agent_start_result_from`). Import `AgentStart`
    next to `agent_start_result_from` (`real.rs:21`).
  - `fake.rs:956` `FakeHerdr::agent_start`: next to `let timed_out = …` add
    `let not_ready = script == AgentStartScript::NotReady;` and replace the final
    `Ok(())` (`fake.rs:1011`) with `Ok(if not_ready { AgentStart::NotReady } else {
    AgentStart::Ready })`. The scripts that return `Err` are unchanged. The fake's own
    tests (`fake.rs:1345-1521`) only `unwrap()` / `is_ok()` / `unwrap_err()` the result,
    so they compile unchanged; add one test that `script_agent_start(vec![NotReady])`
    yields `Ok(AgentStart::NotReady)`.
  - Callers: `mod.rs:969-971` (`spawn_agent_locked`, change the `Ok(()) =>` arm to
    `Ok(started) =>`, used as described under Default path) and `strict.rs:615-617` (`strict_step`, change
    `Ok(()) =>` to `Ok(_) =>`; behaviour unchanged). These are the only two callers
    outside tests (`grep agent_start(` confirms); the test helpers in `mod.rs` call the
    backend, not `agent_start` directly.
  - `HerdrOps` has exactly two implementations: `RealHerdr` and `FakeHerdr`.
- **Default path** (`spawn_agent_locked`, `mod.rs:935`):
  - The `is_early_exit` arm (`mod.rs:997`) and the generic `Err(e)` arm (`mod.rs:1013`):
    call `self.update_prompt_on_screen(&id.pane_id, "visible", None)` **before**
    `close_quietly`. On `Some(cli)`, close the pane (as the arm does today) and
    `return Err(PtyBackendError::AgentUpdatePrompt(cli))`, skipping the rest of the
    fallback chain, since every step would hit the same screen. Otherwise the arm is
    unchanged. (The `agent_name_taken` arm is not checked.)
  - `Ok(started)` (`mod.rs:971`): after the keymap `record` and before `break`, when
    `started == AgentStart::NotReady && launch.cli == CliKind::Codex`, do one
    `update_prompt_on_screen(&id.pane_id, "visible", None)`. On a match,
    keep the pane (the user can answer it in place) and emit, through the spawner's
    existing sink (`Spawner.sink: Arc<dyn EmitSink>`, `backend/emit.rs`, which takes an
    arbitrary event name and a `serde_json::Value`):
    `self.sink.emit("agent-updates://prompt", serde_json::json!({ "project":
    key.project, "worktree": key.worktree, "cli": "codex" }))`. Then launch continues
    normally and returns `Ok`.
  - The frontend `AgentUpdatesProvider` handles `agent-updates://prompt` by (a) calling
    `agent_updates_check()` immediately and (b) recording `{project, worktree, cli}` in a
    `promptedKeys` set. `TerminalPane` shows the pane-level hint "Codex is asking to
    update. Answer in the terminal, or use Keitora's updater" while its key is in
    `promptedKeys` and its status is `running`; the hint is dropped when the key's pane
    restarts or ends.
- **Strict path** (`strict.rs`). `PresentUnknown` is accepted in two places:
  `classify_window_end` (`strict.rs:341-347`, an agent was seen but never confirmed) and
  `classify_never_appeared` (`strict.rs:352-357`, a foreground process exists); both
  results flow out of `verify` (`strict.rs:662`) into `strict_step`
  (`strict.rs:600`), where `Verdict::failure()` (`strict.rs:208`) maps `PresentUnknown`
  (and `Confirmed`) to success at `strict.rs:655`. The pane check goes in `strict_step`
  **immediately before** `match verdict.failure()` (`strict.rs:655`), so it covers both
  acceptance routes with one call site:
  - `if verdict.failure().is_none() { if let Some(cli) =
    self.update_prompt_on_screen(pane_id, "recent", Some(20)) { return
    Err(StepError::UpdatePrompt(cli)); } }`.
  - Add the variant `StepError::UpdatePrompt(CliKind)` (`strict.rs:104`) and its arm in
    `StepError::reason` (`strict.rs:114`): `"agent update prompt"`. The step-failure code in
    `spawn_agent_transfer_strict_with` (`strict.rs:554-586`) records
    `let update_prompt = match &failure { StepError::UpdatePrompt(c) => Some(*c), _ => None };`
    before `failure.reason()`, runs its existing trail / `close_quietly` / lifecycle /
    log lines unchanged, and then, like the `name_taken` `break` at `strict.rs:583`,
    `if let Some(cli) = update_prompt { return Err(PtyBackendError::AgentUpdatePrompt(cli)); }`
    so no later step runs. (The existing `paneTail` capture at `strict.rs:557` is
    unchanged; it already runs before the close, so no matcher is added there.)
  - A `Confirmed` verdict reached on the first polls (a fresh launch whose pane is
    labelled `codex` twice in a row) can precede the update screen; the screen check is
    best effort there, and the `-c` override remains the primary prevention.
  - Tests: a `FakeHerdr` strict launch whose pane text (`set_pane_text`) is the update
    screen fails with `AgentUpdatePrompt` after one step (not after the whole chain), and
    the same launch with ordinary text still succeeds.
- **Frontend.**
  - `TerminalPane.tsx`: the `spawn` catch block (`TerminalPane.tsx:593-615`) needs no new
    branch to keep working: the `AgentUpdatePrompt: codex` message is not
    `Recovering`/`Terminated`, so it already reaches `setSpawnFailure({ cli, error: msg,
    … })` and `updateStatus("spawn-failed")`. The addition is that when `msg` starts with
    `AgentUpdatePrompt` it also calls `agentUpdatesCheck()` (fire and forget, errors
    ignored) so the banner appears at once.
  - `spawnFailedCopy` (`src/lib/transferPhase.ts:306`): before the `${cli} did not start`
    test, add `if (error.startsWith("AgentUpdatePrompt")) return { message:
    `${Cli} wants to update before it starts`, detail: null };` where `Cli` is `cli` with
    its first letter upper-cased. Add a case to its vitest.
  - `lib/agentUpdates.ts` exports `isUpdatePromptError(error: string | null): boolean`
    (`error?.startsWith("AgentUpdatePrompt") ?? false`). The spawn-failed overlay in
    `TerminalPane.tsx` (the block that renders `failedCopy`, `:1873`) shows, when
    `isUpdatePromptError(spawnFailure.error)`, two extra buttons: **Update {Codex}**
    (opens `AgentUpdateDialog` via `useAgentUpdates().openDialog(cli)`) and **Retry** (the
    existing Kill & Restart handler).
  - No separate transfer copy change is needed: `spawnFailedCopy` has one consumer
    (`TerminalPane.tsx:1873`), and a transfer launch that fails goes through the same
    `spawn` catch and spawn-failed overlay.

### 5. Frontend

- `src/lib/agentUpdates.ts`: pure banner and dialog model (`bannerOf(snapshot,
  dismissedThisRun)`, `runOutcomeCopy`, `updatePromptHint`), with vitest coverage like
  `src/lib/updates.ts`.
- `src/components/AgentUpdatesProvider.tsx` + `useAgentUpdates()`. It mirrors
  `UpdatesProvider`, subscribes to `agent-updates://status|run|prompt|stop|restart`, and
  keeps the "Later" dismissals for this run, the `pendingRestart` set and the
  `promptedKeys` set. `useAgentUpdates()` returns `{ snapshot, runOutcome, dialogCli,
  openDialog(cli), closeDialog(), later(cli), skip(cli, version), check(),
  pendingRestart, promptedKeys }`. It is mounted in `App()` (`App.tsx:1375`) as
  `<AgentUpdatesProvider>` directly inside `<UpdatesProvider>` and outside
  `<ConfirmDialogProvider>`.
- `src/components/AgentUpdateBanner.tsx`, rendered in `AppInner` right after
  `<UpdateBanner />` (`App.tsx:1214`, below `<RecoveryBanner info={recovery} />`). It
  receives `recovery` as a prop (to disable **Update**, see Surface) and renders `null`
  while `bannerOf(useUpdates().state) !== null` (from `lib/updates.ts:122`), so Keitora's
  own update banner shows first and only one strip is visible at a time. With both CLIs
  available, it shows the codex entry first and the claude entry after the codex banner is
  skipped or dismissed.
- `src/components/AgentUpdateDialog.tsx`: modal hosting the reserved-key `TerminalPane`,
  the Windows stop-first step, and the outcome line. It is rendered by `AppInner` (it
  needs `daemonInfo` and `recovery`, `App.tsx:402-404`) whenever
  `useAgentUpdates().dialogCli !== null`, and receives both as props.
- `SettingsDrawer.tsx`: an "agent cli updates" section showing, per CLI, installed / latest
  / install kind, plus the check interval input (`agent_update_check_hours`, saved with
  `setAgentUpdateCheckHours`) and a "check now" button (`agentUpdatesCheck()`). Lowercase
  copy, matching the drawer. Placed directly under `UpdatesSection`
  (`SettingsDrawer.tsx:1126`).
- IPC wrappers and types in `src/lib/ipc.ts`. Harness mocks in `.harness/` so the UI can be
  screenshot without herdr.

## Dependencies

- herdr ops already present: `workspace_create`, `pane_run`, `pane_read`, `pane_list`,
  `pane_process_info`.
- `reqwest`, `semver` already in `src-tauri/Cargo.toml`; `updater::source::install_crypto_provider`.
- `codex_env` (exe + `CODEX_HOME` resolution, version probe); `discover_cli_cached`.
- The existing `TerminalPane` Kill & Restart flow and `session_launch`.

## Risks

- **Codex key drift.** OpenAI could rename `check_for_update_on_startup`. Mitigation: the
  section 4 detection turns it into a guided prompt rather than a dead pane.
- **Reserved-key leaks.** A session-listing site we miss would show a phantom
  `__keitora` entry. Mitigations: one `is_reserved()` helper, the central filters in
  `KeyMap` (section 3, "Every site"), a test per filter site, and the `onboard_project`
  name guard. The `list_sessions` consumers were enumerated (`daemon_list_sessions`,
  `activity/mod.rs:206`, `transfer.rs:969`, `backend/mod.rs` busy-set helpers); all read
  `summaries_from`, which is covered by the `KeyMap::entries()` filter.
- **Windows file locks** (section 3): handled by stop-first, with a hint when it still
  fails.
- **Install-kind misclassification** (custom npm prefix, volta, pnpm). These fall back to
  `Unknown` / show the command, and the pane makes failures visible.
- **Network.** Offline or behind a proxy the result is `Unknown` and nothing is shown.
  The fetch is bounded and never blocks a launch.
- **Restart interrupts work.** That is why restart is a button and is never automatic.
- **herdr classification unknown.** Both outcomes (timeout and not-ready) are handled.
- **Claude auto-updater racing a Keitora-run `claude update`:** low; both are idempotent
  and the post-run re-probe reports whatever version ended up installed.
- **Boot load:** the first check waits for recovery to settle (capped), and probes are
  cached, so checks never compete with session resume.

## Decisions (refine round 1, user)

1. **Interval:** app start + every 6 h by default. It is a user setting
   (`agent_update_check_hours` → `[updates] agent_check_hours`) in the settings drawer;
   0 = app start only.
2. **Restart after update:** always a button ("Restart Codex sessions"), never automatic.
3. **Claude's updater:** left alone (no `DISABLE_AUTOUPDATER`). Keitora only surfaces
   Claude's version and offers its update command.
4. **Banner visibility:** only for CLIs Keitora discovered.

## Decisions (refine round 2, automated review)

- The `-c` override is a **prefix** (before `resume`); hook-trust stays last.
- `load_related` is set in the herdr resume runner, not `machine.rs`. `AgentUpdatePrompt`
  is non-load-related, so recovery staggers but doesn't go serial. A failed resume leaves
  the queue; the session shows the guided overlay instead of being "owed".
- Visible pane = reserved `SessionKey` + `pty_spawn` with `command`, filtered out of
  recovery and listings. Completion is detected by a sentinel line, not process polling.
- `kill` cancels recovery, so restart is explicit (event-driven Kill & Restart, plus lazy
  relaunch for unmounted sessions).
- The version cache gets explicit invalidation. The Claude probe is new.
- herdr returning `Ok(not-ready)` on the prompt is handled (pane kept, hint shown). The
  strict path checks the screen before accepting `PresentUnknown`.
- Settings and skip versions live in `keitora.toml` `[updates]`, behind the existing
  `UserConfig::write_lock()`.
- "Settled" = recovery `Healthy` with an empty queue, capped at 3 min.

## Complexity

**Medium-large.** About 20 files:
- herdr backend: `agent.rs`, `cli.rs`, `ops.rs`, `real.rs`, `mod.rs`, `strict.rs`,
  `fake.rs`, `keymap.rs` (reserved-key filters)
- `backend/mod.rs` (error, `SessionKey::is_reserved`)
- the new `agent_updates/` module (`mod.rs`, `model.rs`, `probe.rs`, `commands.rs`) and
  `lib.rs` (`mod agent_updates;`)
- `codex_env.rs`, `transfer.rs`, `host.rs`
- `config.rs`
- frontend: `ipc.ts`, `settings.ts`, `agentUpdates.ts` + test, three new components,
  `TerminalPane.tsx`, `transferPhase.ts`, `SettingsDrawer.tsx`, `App.tsx`,
  `.harness/app.tsx`

Plan: skip Fracture; Singletonize is done (see the section below); a light Loss check
remains.

## Implementation notes (singletonized)

Resolved by reading the code; every item below is a decision, not an open question.

**Order of work** (each step compiles and tests on its own):
1. `SessionKey` consts + `is_reserved()`/`agent_update()`; the three `KeyMap` filters
   (`entries`, `begin_recovery`, `snapshot_entries`); the `spawn` live-set skip; tests.
2. `AgentStart` type through `ops.rs` / `cli.rs` / `real.rs` / `fake.rs` / `mod.rs` /
   `strict.rs` (a pure refactor; no behaviour change).
3. `agent.rs`: the `-c` prefix, `update_prompt_cli`; `transfer.rs::summary_args`; tests.
4. `PtyBackendError::AgentUpdatePrompt`, `StepError::UpdatePrompt`, `update_prompt_on_screen`,
   the default-path and strict-path checks, `sanitize_ui_tokens`.
5. `config.rs` `UpdatesConfig` fields; `host.rs` `SettingsUser`, `set_agent_update_check_hours`,
   `onboard_project` guard.
6. `agent_updates/` core (`install_kind`, `update_command`, `assess`, version parsing and
   fetch), `codex_env` invalidation, scheduler, state, commands, registration.
7. Frontend: `ipc.ts`, `agentUpdates.ts`, provider, banner, dialog, `TerminalPane`
   (`command` prop, update-prompt branch and buttons), settings section, harness mocks.

**Decisions made while singletonizing** (the rest of the document is unchanged):
- Reserved key: `__keitora` / `agent-update` cannot be proven unregistrable (project names
  are unvalidated); instead `is_reserved()` matches both fields and `onboard_project`
  rejects `__keitora`.
- Reserved-key exclusion is centralised in `KeyMap` (`entries`, `begin_recovery`,
  `snapshot_entries`) plus the `spawn` live-set skip; `get`/`pane_owner` stay unfiltered so
  attach, write, history and kill keep working.
- Interval saved by a new `set_agent_update_check_hours` command (like
  `set_update_auto_check`), not through `save_user_settings`. The write mutex is the
  existing `UserConfig::write_lock()`; no new static.
- Sentinel is read with `PtyBackend::read_history` (no new backend method); live sessions
  of a CLI come from `list_sessions` + `resolve_agent_launch`; the pane is closed with
  `kill_live(key, None)` through `agent_update_close`.
- `spawn_err_kind` needs no change (its fallback arm already yields the bare token).
- Native codex has no run command (shown as guidance); `codex update` and the brew cask
  names are best knowledge and are not run against a real install in this work.
- The Update button is disabled unless recovery is `Healthy`.
