# 04 — Session state

## Goal

Give Antigravity worktrees busy, waiting and idle state, via herdr's integration plus a Keitora-owned global
hook, and make herdr's `blocked` status count as `waiting` for every provider.

## Context

- Decision 1: hooks go in the **global** `~/.gemini/config/hooks.json` and only act inside Keitora panes.
- Decision 2: herdr's `blocked` agent status **counts as `waiting` for every provider**, Claude and Codex
  included.
- Verified hook facts (`facts.md`):
  - Load locations: `~/.gemini/config/hooks.json` (global) and `<workspace>/.agents/hooks.json`.
  - File shape: a JSON object keyed by *hook name*; each name maps to an object keyed by event plus an optional
    `"enabled": false`. Non-tool events (`SessionStart`, `PreInvocation`, `PostInvocation`, `Stop`) take an
    array of handlers directly: `"Stop": [ {"type":"command","command":"…","timeout":10} ]`. Tool events take
    matcher groups. Wrapping non-tool events in `{"hooks":[…]}` fails the parse (`command hook must specify
    'command'`) and the whole file is ignored (agy keeps running).
  - Windows hook commands run under **cmd.exe** (`%COMSPEC%` expands; `$0` stays literal).
  - Order for one prompt with one tool call: `SessionStart` -> `PreInvocation` -> `PreToolUse` ->
    `PostToolUse` -> `PostInvocation` -> `PreInvocation` -> `PostInvocation` -> `Stop`.
  - Payload on stdin, camelCase JSON: `conversationId`, `cwd`, `hookEventName`, `workspacePaths`,
    `transcriptPath`, `artifactDirectoryPath`, `modelName`, `lastUserInput`; `Stop` adds `executionNum`,
    `terminationReason`, `error`, `fullyIdle` (seen `true`), `continuationActive`.
  - There are no `UserPromptSubmit`, `Notification`, `PermissionRequest` or `SessionEnd` events.
  - A silent hook (no stdout) lets `Stop` finish normally.
  - Whether the interactive TUI gates workspace hooks is unverified (a global hook avoids the question).
- From doc 01: `CliKind::Antigravity`. From doc 03: `src-tauri/src/global_writes.rs`
  (`allowed()` / `allowed_with(is_test, env_val)`) and `antigravity_trust::install_trust_for`.
- Step 4 of the implementation outline.
- Verified code state (before this work):
  - `HINT_BUSY` `src-tauri/src/backend/mod.rs:109`; `apply_reconcile` `:792-820`; `background_set` `:825-826`;
    `busy_keys` `:828-836`; `cached_background_set` `:841`.
  - `agent_status_to_state_hint` `backend/herdr/agent.rs:152-160` (maps `working` and `blocked` to busy).
  - Callers of the busy set: `tasks.rs:99,115` (`reconciled_sessions` parameter `bg:
    &HashSet<SessionKey>`), `tasks.rs:136` (calls `apply_reconcile`), `tasks.rs:161` (`sessions_list`),
    `dispatch.rs:321` (`background_set`) and `dispatch.rs:804-832` (a test), `host.rs:204,211-215`
    (`projects_overview` / `project_overviews` with `bg: &HashSet<SessionKey>`),
    `backend/herdr/mod.rs:3411,3429,3446` (tests), `activity/mod.rs:206-210` (working dot),
    `backend/herdr/status_loop.rs:245` (`Observation.busy`).
  - Hook installers: `hooks.rs` `resolve_keitora_bin` `:161`, `install_hooks_for` `:202`,
    `install_session_hooks` `:245`, `install_active_session_hooks` `:258`; `codex_hooks.rs` (pure
    `merged_codex_hooks`, I/O `install_codex_hooks_for`); triggers in `host.rs`: `set_worktrees` spawns
    `hooks::install_active_session_hooks` at `:513`, `worktree_create` `:544-566` (Codex gate at `:562`),
    `on_project_activated` `:1134`, app setup `:2403` (`thread::spawn(hooks::install_session_hooks)`).
  - Pane env: default launch `backend/herdr/mod.rs:974` calls `env::pane_env`; `env::pane_env_with` (extra
    variables appended last) is at `env.rs:759`; strict launch `strict.rs:470` already calls `pane_env_with`.
  - CLI: `crates/keitora-cli/src/main.rs` `Cmd::Session { state }` (`:35-38`), handler `:268-275`, `run()`
    `:240-` calls `resolve()` at `:248` before the match.

## Files to touch

- new `src-tauri/src/antigravity_hooks.rs`; `src-tauri/src/lib.rs` (`mod antigravity_hooks;`)
- `src-tauri/src/hooks.rs` (`keitora_bin_extra_env`)
- `src-tauri/src/backend/herdr/mod.rs` (`:974` launch env), `src-tauri/src/backend/herdr/strict.rs` (`:470`
  extra env)
- `src-tauri/src/host.rs` (triggers at `:513`, `:544-566`, `:1134`, `:2403`, and `save_user_settings`)
- `crates/keitora-cli/src/main.rs`, new `crates/keitora-cli/src/hook_payload.rs`
- `src-tauri/src/backend/mod.rs`, `backend/herdr/agent.rs`, `tasks.rs`, `dispatch.rs`, `host.rs`
  (`project_overviews`), `activity/mod.rs`, `backend/herdr/status_loop.rs`, `backend/herdr/mod.rs` (tests)

## Design

There are two layers.

**herdr layer.** herdr's `antigravity-cli` integration hook drives the pane's `agent_status` and captures
session ids. Nothing beyond the integration target is needed (doc 03).

**Keitora layer.** `src-tauri/src/antigravity_hooks.rs`, modelled on `codex_hooks.rs`.

### Hook file merge (pure) and I/O

Exact public surface:

```rust
pub const HOOK_NAME: &str = "keitora";
/// (agy event, arguments after `keitora session`) — the three events Keitora registers.
const HOOK_EVENTS: [(&str, &str); 3] = [
    ("SessionStart",   "idle --stdin-payload"),
    ("PreInvocation",  "busy --stdin-payload"),
    ("Stop",           "idle --stdin-payload --if-fully-idle"),
];

/// The command string for one registration; `windows` selects cmd.exe vs POSIX shell syntax.
pub fn hook_command(args: &str, windows: bool) -> String;

/// Return `existing` with the top-level `keitora` hook ensured. `None` when `existing` is not a JSON object
/// (the caller must then skip the file). Every other top-level name is left untouched, including herdr's
/// entry and any of the user's.
pub fn merged_antigravity_hooks(existing: serde_json::Value, windows: bool) -> Option<serde_json::Value>;

pub fn hooks_file(home: &Path) -> PathBuf; // <home>/.gemini/config/hooks.json

/// Merge into `file`; returns whether it was written. Creates the parent dir and the file when missing;
/// a file that fails to parse is skipped (`Ok(false)`), never clobbered; writes only when the merged value
/// differs; never writes when `!allowed`.
pub fn apply_hooks_at(file: &Path, windows: bool, allowed: bool) -> std::io::Result<bool>;

/// Install into the real home; a no-op unless `global_writes::allowed()`.
pub fn install_antigravity_hooks();

/// Worktree roots whose effective cli is Antigravity (explicit entry cli, else the user default_cli).
pub fn antigravity_worktrees(user: &keitora_tasks::UserConfig) -> Vec<PathBuf>;

/// Registry-wide: if `antigravity_worktrees` is non-empty, install the hook and seed trust
/// (`antigravity_trust::install_trust_for`) for each of them.
pub fn sync_registry();

/// One worktree: install the hook and seed trust for `worktree_root`.
pub fn sync_worktree(worktree_root: &Path);
```

Hook JSON that `merged_antigravity_hooks` writes under the key `keitora` (handler arrays directly, never
wrapped in `{"hooks":[…]}`):

```json
"keitora": {
  "SessionStart":  [ {"type":"command","command":"<hook_command(idle --stdin-payload)>","timeout":10} ],
  "PreInvocation": [ {"type":"command","command":"<hook_command(busy --stdin-payload)>","timeout":10} ],
  "Stop":          [ {"type":"command","command":"<hook_command(idle --stdin-payload --if-fully-idle)>","timeout":10} ]
}
```

The merge replaces the whole `keitora` object (so it is idempotent and picks up a changed shape) and never adds
an `"enabled"` key. `antigravity_worktrees` iterates `user.projects.values()`, skips refs whose
`effective_config` fails, and for each `entry in pc.worktree_entries()` keeps `pc.worktree_path(&entry.name)`
when `resolve_cli(Some(&entry), user) == CliKind::Antigravity`.

### Install triggers

Today's triggers only scan the cwd or active project, which is not enough. The Antigravity hook and trust
seeding run in exactly these places (each off the UI thread: `std::thread::spawn`):

1. App setup, `host.rs:2403`: add `thread::spawn(antigravity_hooks::sync_registry);` next to
   `thread::spawn(hooks::install_session_hooks)` (this is the registry-wide startup scan, including the user
   default).
2. `set_worktrees` (`host.rs:513`) and `on_project_activated` (`host.rs:1134`): add the same
   `thread::spawn(antigravity_hooks::sync_registry);` after the existing `install_active_session_hooks` spawn
   (project activation).
3. `worktree_create` (`host.rs:562`): next to the Codex gate add
   `if wt.cli == keitora_tasks::CliKind::Antigravity { antigravity_hooks::sync_worktree(&wt.path); }`. This sits
   inside the existing `if let Some(bin) = hooks::resolve_keitora_bin(…)` block only for placement symmetry;
   the Antigravity call does not use `bin`, so place it after that block instead (so it runs even when no
   `keitora` binary resolves).
4. `save_user_settings` (`host.rs:896`): after the registry write, when `default_cli ==
   CliKind::Antigravity` or any path changed, `thread::spawn(antigravity_hooks::sync_registry);`. When
   `default_cli == CliKind::Antigravity`, also call the backend's existing `refresh_integration()` (README Open
   issue 5 resolution), so a late `agy` install gets herdr's `antigravity-cli` integration without a restart.
5. Right after `switch_worktree_cli` persists an Antigravity target (doc 06):
   `antigravity_hooks::sync_worktree(&worktree_path)`, then the same `refresh_integration()` call as item 4.

**Write guard.** `antigravity_hooks::install_antigravity_hooks` and `antigravity_trust::install_trust_for`
refuse unless `global_writes::allowed()` (doc 03): never under `cfg!(test)`, never when
`KEITORA_HERDR_NO_INTEGRATION` is set. The pure merge and the `_at` functions are what tests exercise.

### `KEITORA_BIN` pane environment

The pane environment gains `KEITORA_BIN=<resolved keitora binary>`, set for every provider (harmless for the
others). New function in `src-tauri/src/hooks.rs`:

```rust
/// `[("KEITORA_BIN", bin)]` when a `keitora` binary resolves, else empty. Resolution is
/// `resolve_keitora_bin(cwd, user_override, cached_keitora_on_path())` with
/// `user_override = resolve_keitora_path(None, &UserConfig::load().unwrap_or_default())`.
/// Empty under `cfg!(test)` (no login shell in unit tests).
pub fn keitora_bin_extra_env(cwd: &Path) -> Vec<(String, String)>;

/// Pure: the pair list for an already-resolved binary.
pub fn keitora_bin_pair(bin: Option<String>) -> Vec<(String, String)>;
```

`keitora_on_path()` runs a login shell on unix, so its result is cached for the life of the process in a
`static ON_PATH: OnceLock<Option<String>>` inside `keitora_bin_extra_env`, instead of being called per spawn.

Wiring:

- Default launch, `backend/herdr/mod.rs:974`: `env::pane_env(&launch.path, &self.ops.env().original_xdg,
  self.ops.env().path())` becomes `env::pane_env_with(&launch.path, &self.ops.env().original_xdg,
  self.ops.env().path(), &crate::hooks::keitora_bin_extra_env(Path::new(&cwd)))`.
- Strict launch, `strict.rs:470`: `extra` (from `crate::codex_env::strict_pane_extra_env(codex_home)`) is
  extended with `crate::hooks::keitora_bin_extra_env(Path::new(&cwd))` before the `pane_env_with` call.

The hook command is a no-op unless the variable is set, so `agy` outside Keitora is unaffected, and no single
binary path is baked into the global file shared by every worktree.

- **Windows (cmd.exe):** `(if defined KEITORA_BIN "%KEITORA_BIN%" session <args> >nul 2>&1) & exit /b 0` (wrapped and forced to exit 0 so a missing or old binary never fails the agent; live-verified against agy 1.5.18)
- **POSIX:** `[ -n "$KEITORA_BIN" ] && "$KEITORA_BIN" session <args> >/dev/null 2>&1 || true`

`<args>` is the second column of `HOOK_EVENTS`. The hook prints nothing on stdout, so `agy` treats it as having
no decision (verified: a silent hook lets `Stop` finish normally).

### Event map

| agy event | `keitora session …` | Note |
|---|---|---|
| `SessionStart` | `idle --stdin-payload` | |
| `PreInvocation` | `busy --stdin-payload` | Fires on every model call; idempotent. The closest event to prompt submit. |
| `Stop` | `idle --stdin-payload --if-fully-idle` | Ignored unless the payload has `fullyIdle: true`. |
| pane gone | offline | Already handled by `apply_reconcile`. |

### `keitora session <state> --stdin-payload [--if-fully-idle]` (`crates/keitora-cli`)

`keitora session` today resolves the project and worktree from the process cwd (`resolve()` ->
`current_worktree`, `config.rs:2944`). Nothing guarantees the hook's working directory, so the new flag
changes into the payload's directory before `resolve()`.

1. `Cmd::Session` (`main.rs:35`) becomes
   `Session { state: String, #[arg(long)] stdin_payload: bool, #[arg(long, requires = "stdin_payload")]
   if_fully_idle: bool }`. The parser test at `main.rs:1221-1226` becomes `Cmd::Session { state, .. }` and gains
   cases (below).
2. New `crates/keitora-cli/src/hook_payload.rs` (`mod hook_payload;` in `main.rs`):

   ```rust
   pub const MAX_PAYLOAD_BYTES: usize = 1_048_576;
   #[derive(Debug, PartialEq)]
   pub enum PayloadAction { Skip, Run { cwd: std::path::PathBuf } }
   /// Reads at most MAX_PAYLOAD_BYTES; returns None if the input is longer (treated as Skip).
   pub fn read_capped(r: &mut impl std::io::Read, cap: usize) -> std::io::Result<Option<Vec<u8>>>;
   /// Pure interpretation of the hook payload.
   pub fn interpret_payload(raw: &[u8], if_fully_idle: bool) -> PayloadAction;
   ```

   `interpret_payload` returns `Skip` when: the JSON does not parse or is not an object; `if_fully_idle` and
   `fullyIdle != true` (absent, `false` or non-bool); or no directory is found. The directory is the `cwd`
   field when it is a non-empty string, else `workspacePaths[0]` when it is a non-empty string, else `Skip`.
3. In `run()` (`main.rs:240`), **before** the `resolve()` call at `:248`, when the command is `Session {
   stdin_payload: true, if_fully_idle, .. }`: `read_capped(&mut std::io::stdin(), MAX_PAYLOAD_BYTES)`; `None`
   or an `Err` -> `return Ok(())`; `interpret_payload` -> `Skip` -> `return Ok(())`; `Run { cwd }` -> if
   `!cwd.is_dir()` `return Ok(())`, else `std::env::set_current_dir(&cwd)` (an error -> `return Ok(())`).
   Stdin is read only under the flag.
4. Under `--stdin-payload`, any later failure (for example no `keitora.toml` found) is written to stderr and the
   process still exits 0, because a hook must never break the agent. Implement by wrapping the remainder of
   `run()` for that case: `if let Err(e) = rest().await { eprintln!("error: {e}"); }`.

### Waiting, for all providers (decision 2)

1. `backend/mod.rs`: next to `HINT_BUSY` (`:109`) add `pub const HINT_WAITING: &str = "waiting";` with the doc
   comment "`SessionSummary::state_hint` value meaning the agent is blocked on the user". Update the
   `SessionSummary::state_hint` doc (`:112`).
2. `agent_status_to_state_hint` (`agent.rs:152`): `launch_pending` -> `HINT_BUSY`; `"working"` -> `HINT_BUSY`;
   `"blocked"` -> `HINT_WAITING`; everything else `None`. Update its doc comment (`:146-151`) and the comment at
   `herdr/mod.rs:2461` ("`working` / `blocked` are `Some(HINT_BUSY)`").
3. `backend/mod.rs`:

   ```rust
   /// The agent hint per session key: busy or waiting.
   pub type HintMap = std::collections::HashMap<SessionKey, String>;
   pub fn agent_hints(backend: &dyn PtyBackend) -> HintMap          // was background_set
   pub fn cached_agent_hints(backend: &dyn PtyBackend) -> HintMap   // was cached_background_set
   fn hint_map(r: Result<Vec<SessionSummary>, PtyBackendError>) -> HintMap // was busy_keys; keeps entries whose
                                                                           // state_hint is HINT_BUSY or HINT_WAITING
   pub fn apply_reconcile(sessions: &[WorktreeSession], live_set: &HashSet<SessionKey>,
                          hints: &HintMap, project: &str) -> Vec<WorktreeSession>
   ```

   `apply_reconcile` applies, per session, in this order:
   1. no live pane (`s.state != "offline"` and key not in `live_set`) -> `offline`, `current_task_id = None`,
      `pid = None` (this wins over everything below);
   2. hint `waiting` and `s.state` is `idle` or `busy` -> `waiting`;
   3. hint `busy` and `s.state == "idle"` -> `busy`.
   A session already `waiting` or `offline` is never changed by a hint.
4. Callers (every one is a mechanical signature change):
   - `tasks.rs:99,115` `bg: &HashSet<SessionKey>` -> `hints: &HintMap` (parameter renamed), `tasks.rs:136`,
     `tasks.rs:161` (`cached_agent_hints`)
   - `dispatch.rs:321` (`agent_hints`; the variable `bg` -> `hints`), and the test at `dispatch.rs:804-832`.
     Dispatch needs no other change: it reads the reconciled state, and a `waiting` session is already "not
     idle" for `decide`.
   - `host.rs:204,211-215` (`projects_overview` / `project_overviews(…, hints: &HintMap)`) and its callers/tests
   - `backend/herdr/mod.rs:3411,3429,3446` tests
   - `activity/mod.rs:206-210`: the working-dot filter becomes `matches!(s.state_hint.as_deref(),
     Some(HINT_BUSY) | Some(HINT_WAITING))` (and the import at `:28`). A blocked lane keeps its working dot as
     today; the colour of the block comes from the session state.
   - `status_loop.rs:245`: `busy` becomes `agent::agent_status_to_state_hint(status, false).is_some()` so a
     `blocked` pane still counts as busy for that observation (unchanged behaviour); update the import at `:20`
     if `HINT_BUSY` becomes unused.
5. This gives Antigravity a needs-input state without a permission event. For Claude and Codex it backfills
   `waiting` when a hook was missed.

### `.gitignore`

Unchanged. Nothing is written into worktrees.

## Tests required

`antigravity_hooks.rs` (pure and temp-dir only):

- `merge_creates_keitora_hook_in_empty_object` — `merged_antigravity_hooks(json!({}), false)` has exactly
  `keitora.SessionStart|PreInvocation|Stop`, each an array of one handler with `type == "command"` and
  `timeout == 10`.
- `merge_uses_direct_handler_arrays` — `keitora.Stop[0]` is the handler itself (it has `command`), not an object
  with a `hooks` key.
- `merge_keeps_other_hook_names` — an existing `{"herdr-agent-state": {...}, "mine": {...}}` keeps both
  entries byte-for-byte.
- `merge_replaces_only_keitora` — an old `keitora` entry with different commands is replaced, not appended to.
- `merge_is_idempotent` — merging twice equals merging once.
- `merge_rejects_non_object` — `merged_antigravity_hooks(json!([]), false) == None`.
- `hook_command_windows_shape` — `hook_command("busy --stdin-payload", true) == "if defined KEITORA_BIN
  \"%KEITORA_BIN%\" session busy --stdin-payload >nul 2>&1"`.
- `hook_command_posix_shape` — `hook_command("busy --stdin-payload", false) == "[ -n \"$KEITORA_BIN\" ] &&
  \"$KEITORA_BIN\" session busy --stdin-payload >/dev/null 2>&1 || true"`.
- `stop_command_carries_the_idle_filter` — the `Stop` command contains `--if-fully-idle`; the other two do not.
- `apply_hooks_at_writes_once` — first call `true`, second `false`; file content equals the merged value.
- `apply_hooks_at_skips_unparseable_file` — a file with `not json` is byte-identical afterwards, returns
  `false`.
- `apply_hooks_at_refuses_when_not_allowed` — `allowed=false` creates no file.
- `antigravity_worktrees_follow_effective_cli` — a registry with worktrees `a` (cli Antigravity), `b` (cli
  Claude) and `c` (no cli) with `default_cli = Antigravity` returns the paths of `a` and `c`; with
  `default_cli = None` returns only `a`.

`hooks.rs`:

- `keitora_bin_pair_sets_the_variable` — `keitora_bin_pair(Some("/x/keitora")) == [("KEITORA_BIN","/x/keitora")]`;
  `None` gives `[]`.
- `keitora_bin_extra_env_is_empty_under_test` — returns `[]` in the test binary.

`herdr/mod.rs` / `strict.rs`:

- `default_launch_env_comes_from_pane_env_with` — with the fake herdr, the `workspace_create` env equals
  `pane_env_with(…, &[])` under test (no `KEITORA_BIN`, because the extra list is empty under `cfg!(test)`);
  add an `env.rs` test `pane_env_with_carries_keitora_bin_extra` asserting a supplied
  `[("KEITORA_BIN","/x")]` appears last.

`crates/keitora-cli` (`hook_payload.rs` tests plus parser tests in `main.rs`):

- `interpret_prefers_cwd` — `{"cwd":"/a","workspacePaths":["/b"]}` -> `Run { cwd: "/a" }`.
- `interpret_falls_back_to_first_workspace_path` — `{"workspacePaths":["/b","/c"]}` -> `Run { cwd: "/b" }`.
- `interpret_skips_invalid_json` — `b"nope"` -> `Skip`; `b"[]"` -> `Skip`.
- `interpret_skips_when_no_directory` — `{}` and `{"cwd":""}` -> `Skip`.
- `interpret_if_fully_idle_requires_true` — `{"cwd":"/a","fullyIdle":true}` runs; `false`, absent and
  `"true"` (string) skip.
- `interpret_without_the_flag_ignores_fully_idle` — `if_fully_idle=false` runs even with `fullyIdle:false`.
- `read_capped_rejects_oversize` — input of `cap + 1` bytes -> `Ok(None)`; exactly `cap` bytes -> `Ok(Some)`.
- `session_parses_stdin_flags` — `["keitora","session","idle","--stdin-payload","--if-fully-idle"]` parses with
  both flags true; `["keitora","session","idle","--if-fully-idle"]` is an error (`requires`).

Reconcile (`backend/mod.rs` tests): `reconcile_table` iterates the full product and asserts the expected
output state:

| live | session state | hint | result |
|---|---|---|---|
| no | idle / busy / waiting / offline | none / busy / waiting | `offline` (all 12 rows) |
| yes | idle | none | idle |
| yes | idle | busy | busy |
| yes | idle | waiting | waiting |
| yes | busy | none | busy |
| yes | busy | busy | busy |
| yes | busy | waiting | waiting |
| yes | waiting | none / busy / waiting | waiting |
| yes | offline | none / busy / waiting | offline |

Other:

- `agent_status_maps_blocked_to_waiting` (`agent.rs`) — `blocked` -> `Some("waiting")`; `working` ->
  `Some("busy")`; `launch_pending` true with any status -> `Some("busy")`; `idle`/`done`/`bogus` -> `None`.
- `hint_map_keeps_busy_and_waiting_only` — summaries with hints `busy`, `waiting`, `None` produce a map with
  the first two.
- `dispatch_refuses_a_blocked_lane` (`dispatch.rs`, next to `:804-832`) — a session `idle` in the DB with a
  `waiting` hint reconciles to `waiting` and `decide` treats it as not idle.
- `activity_working_dot_counts_waiting` (`activity/mod.rs`) — a `waiting` hint lights the working dot like
  `busy`.
- `status_loop_observation_counts_blocked_as_busy` (`status_loop.rs`) — a pane with `agent_status: "blocked"`
  yields `Observation.busy == true`.

## Cross-references

- 01 (providers table), 03 (trust seeding shares the triggers; `global_writes`; the herdr integration gate),
  06 (`switch_worktree_cli` triggers `sync_worktree`), 08 (isolation marker; guard tests).
