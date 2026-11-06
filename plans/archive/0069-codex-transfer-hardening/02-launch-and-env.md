# 0069 / 02 — Launch, persistence and Codex environment

Part of 0069 "Harden Claude → Codex transfer" (proposal: `../0069-codex-transfer-hardening.md`).
Shared interfaces: **`00-contracts.md`**. Siblings: `01-import.md` (produces the outcome that drives the
persist intents here), `03-frontend.md` (passes `transfer`/`fresh`, renders failures),
`04-scoping-locking-diagnostics.md` (project scoping, registry, trail).

Paths are under `src-tauri/src/` unless noted. Line numbers were re-verified against the tree at `4178d29`
(only `plans/` changed since `e9c5d27`, so they equal the older numbers except where the "Reference
corrections" table says otherwise).

## Goal

"Transfer to Codex" must always end with a running Codex session in the pane: the imported thread if the
import worked, otherwise a fresh Codex conversation. Never a bare shell reporting as a running agent, never
a misleading "Kill & Restart" screen. This document covers: persisting the transfer result correctly,
launching Codex in the pane with verification and a fallback chain **for transfers only**, and resolving
the Codex executable/environment consistently for both the import child and the pane.

## Reference corrections (verified against code)

Fixes to references in the earlier draft of this document and in the proposal. Use the right-hand column.

| Earlier reference | Reality |
|---|---|
| `resume_locked` at `mod.rs:996` | `backend/herdr/mod.rs:952` (`resume_session` is `:941`) |
| `BackendResumeRunner` `mod.rs:1444-1510` | `mod.rs:1439-1507` (`impl ResumeRunner` at `:1443`) |
| "`Continue`" return at `resume.rs:293` | `ResumeStore::resolve` (`resume.rs:245`) returns `Continue` for Codex at **`:300`** (`:293-297` is the Claude branch) |
| `AppServer::spawn` `transfer.rs:1170`, `codex_command` `:1411-1440` | `transfer.rs:1165` and `:1417-1439` |
| `host.rs:1291-1294` bare `"codex"` fallback | `host.rs:1289-1293` (inside `agent_launch_with`, `:1276`) |
| `unix_spawn_path` `config.rs:1940` | `crates/keitora-tasks/src/config.rs:1944` (`cached_login_path` `:1874`, `extra_path_dirs` `:1906`) |
| `agent.rs:239` "fresh variants carry the flag" | the flag is pushed unconditionally at `agent.rs:73`; `:239` is only the matrix test row |
| `observe()` stores presence "`resume.rs:666-679`" | `resume.rs:191` (`present = pane.agent.is_some()`), stored via `resume_observe` `:204-214`; `:666` is a test |
| `PaneInfo`/`PaneIdentity` | `PaneInfo` is a type alias of `PaneIdentity` (`cli.rs:18`, `:46`); relevant fields: `agent: Option<String>`, `agent_status: Option<String>`, `agent_session: Option<AgentSession>`; `AgentSession { source: String, agent: String, kind: AgentSessionKind::{Id,Path}, value: String }` (`cli.rs:50-65`) |
| "`agent_not_ready` errors are re-issued" | `agent_not_ready` and "blocked during startup" are folded to **`Ok`** inside `RealHerdr::agent_start` (`real.rs:375-378`, `cli.rs:219-228`), so they never reach the spawner as errors. Re-issue applies to *other* errors (see C3) |
| `discover_cli(Codex)` is cheap | unix `discover_cli` (`config.rs:2253`) calls the **uncached** `login_path()` (runs the user's login+interactive shell each call). It must not be called per spawn without a cache (see D) |
| `set_project_worktrees_by_root` can persist `Clear` | **It cannot.** It runs `merge_worktree_registry_fields` (`config.rs:621-654`) which carries a missing `codex_thread_id`/`claude_session_id`/`cli` forward from the existing entry, so a `Clear` is silently undone. A new keitora-tasks writer is required (C1) |
| resume-store "delete" row | There is no delete and no overwrite API in `LocalStore`. Existing: `resume_observe` (newest-wins + cleared-id guard, `local_store.rs:237`), `resume_get` (`:288`), `resume_clear` (`:304`: sets `session_id=NULL`, `cleared_id=old id`). `ResumeStore::clear` (`resume.rs:306`) wraps `resume_clear`. "Fresh deletes the row" therefore means **`ResumeStore::clear`**; "Imported overwrites" needs one new method (C1) |
| `ptySpawn` gains `fresh` | `fresh` **already exists** end to end: `ptySpawn(..., fresh?)` (`src/lib/ipc.ts:582`), `pty_spawn(.., fresh: Option<bool>)` (`host.rs:1381`), `spawn_pty(.., fresh: bool)` (`host.rs:1218`), `AgentLaunch { fresh, ..launch }` (`host.rs:1260`), `ResolveCtx.fresh`. Only `transfer` is new |

## Root causes covered

### R3 — A bare shell is reported as a running agent (medium-high) → symptom: "successful" transfer lands in a fresh terminal with no `codex` process

Launch path:

1. `spawn_agent_locked` (`backend/herdr/mod.rs:816`) creates a shell pane via `workspace_create`.
2. It sleeps `SHELL_READY_DELAY_MS` = 300 ms (`mod.rs:111`, used at `:851`).
3. It calls `herdr agent start --kind codex … -- resume <thread> …` (`:853-856`, timeout
   `AGENT_START_TIMEOUT_MS` = 10 000, `mod.rs:103`).

`agent_start_result_from` (`backend/herdr/cli.rs:219-228`) treats `agent_not_ready` and "blocked during
startup" as success. If `codex` then exits, the pane falls back to its shell prompt. Codex can exit for:
an unknown thread; a rejected flag; no `node` for an npm shim; `agent.start` racing a slow zsh rc file
(300 ms unverified on macOS). `observe()` records `agent_present = pane.agent.is_some()` (`resume.rs:191`).
The UI only sees a live pane.

### R4 — Launch failure → misleading screen (high) → symptom: stuck "Kill & Restart" screen (3b)

Any `ptySpawn` error other than `Recovering`/`Terminated` lands in `spawn-failed` (`TerminalPane.tsx:593`),
the error text is dropped, and the copy is hard-coded "Failed to launch claude — is it on PATH?" (`:1785`)
(UI part is `03-frontend.md`). Backend causes:

- Only `agent_start_failed`/`agent_name_not_found` advance the fallback chain (`agent.rs:91-96`,
  `is_early_exit`). Every other error, including the 10 s `AGENT_START_TIMEOUT_MS`, closes the pane and
  returns `Err` (`mod.rs:885-889`).
- Every variant, including the fresh ones, carries `--dangerously-bypass-hook-trust` (`agent.rs:73`).
- When no path is configured, the launch falls back to the bare string `"codex"` (`host.rs:1289-1293`); it
  never calls `discover_cli`.

History note: the repo was re-rooted at open-sourcing and the first commit already uses herdr, so
pre/post-herdr behaviour can't be diffed. Under herdr's `agent.start`, fallbacks became a Rust loop
(`agent.rs:28-32`, `next_fallback` `:80`) covering early exits only — not "started then vanished",
timeouts or flag rejection. That gap (R3/R4) is the herdr-shaped part of this bug. #29 is treated as a
likely symptom of R3; recount after landing before closing it.

### R6 — No fallback (certain)

On error, `onTransfer` only toasts (`App.tsx:587`). `persist_worktree_cli` keeps the old thread id when
passed `None` (`transfer.rs:1042-1048`). Clearing the id still would not give a fresh session: with no
thread id and no stored row, `ResumeStore::resolve` returns `Continue` (`resume.rs:300`), which for Codex
is `resume --last` (`agent.rs:66-67`) and can attach an *unrelated* thread in the same cwd. The transfer
branch of the restart effect always calls `restart(false)` (`TerminalPane.tsx:904`) (UI part is `03`).
Also: the resume store outranks the worktree registry (`resume.rs:276-282`), so a stale stored Codex row
can beat a freshly imported thread.

### R7 — Environment on macOS GUI launches (medium)

The import child (`AppServer::spawn`, `transfer.rs:1165`, through `codex_command` `:1417-1439`) inherits
Keitora's own environment. A Finder/Dock-launched app has launchd's minimal PATH, so a bare `"codex"`
(`transfer.rs:236`) may not resolve, and an npm `#!/usr/bin/env node` shim can fail even by absolute path.
This is the likelier macOS environment cause. Launch panes are better off: `env::pane_env`
(`backend/herdr/env.rs:750`) builds on herdr's own PATH (on unix the login-shell PATH composed by
`unix_spawn_path`, `config.rs:1944`) plus the exe dir, applied at `workspace_create`. `CODEX_HOME` is read
from Keitora's own env (`transfer.rs:643-653`); the reporter has no override so drift is unlikely, but
nothing guarantees the import and the pane agree.

## Scope and non-goals

In: sections C and D below and their tests. Out: the import protocol (`01`), frontend (`03`), registry and
scoping (`04`). **Out of the whole effort: changing default (non-transfer) launch, boot-resume or recovery
behaviour**, and upstream herdr patches. No herdr changes are required.

## Dependencies on siblings

- `01-import.md` decides the outcome and calls `persist_worktree_cli` with the intents below (under the
  dispatch gate, after `mark_committed` returned `true`, and followed by `settle` if persisting fails), and
  consumes `CodexEnv` (D) for `AppServer::spawn(env: &CodexEnv, cwd, cancel, deadline)`. 01's persist table
  says "clear" for the resume-store row on `Fresh` (`ResumeStore::clear`, C1) and "replace" on `Imported`
  (`ResumeStore::replace`).
- `03-frontend.md` calls `ptySpawn(project, id, cwd, size, command?, fresh?, transfer?)` (positional, the
  trailing `transfer` argument) and uses `RestartSignal.fresh`; it renders the final
  failure string (format in `00-contracts.md` section 6). It must **omit** the `transfer` key from the
  `invoke` payload unless it is `true` (the existing `ipc.test.ts` cases assert the exact payload and must
  stay green).
- `04-scoping-locking-diagnostics.md` supplies `resolve_project(&user, name)`, the `DispatchGate`
  placement (held only around persist), and the `trail(project, worktree, stage, detail)` helper (writes
  `log::` and `TransferRegistry::push_trail`, computing `at_ms` itself). Launch/verify entries reach it via
  C3 "Trail hand-off".

## Design

### C0. New types (exact definitions)

```rust
// backend/mod.rs, next to AgentLaunch (:300)
/// How a spawn behaves when the agent does not come up cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchPolicy {
    /// Every existing caller: boot-resume, recovery, plain restarts. Behaviour unchanged.
    #[default]
    Default,
    /// A spawn that follows a Claude->Codex transfer: verified, never `Continue`, polls.
    TransferStrict,
}

pub struct AgentLaunch {            // existing fields unchanged, one added
    pub cli: keitora_tasks::CliKind,
    pub path: String,
    pub autonomy: bool,
    pub resume_target: Option<String>,
    pub fresh: bool,
    pub policy: LaunchPolicy,       // NEW, last field
}
```

```rust
// crates/keitora-tasks/src/config.rs, re-exported from lib.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldIntent<T> { Set(T), Clear, Keep }
```

```rust
// backend/mod.rs (PtyBackend trait gains two default-implemented methods)
pub enum ResumeRecord { Codex { thread_id: String }, Cleared }
fn set_resume_record(&self, key: &SessionKey, record: ResumeRecord) -> Result<(), PtyBackendError> {
    let _ = (key, record); Ok(())
}
// stage is `TrailStage::Launch` or `TrailStage::Verify` (crate::transfer_registry::TrailStage, `00-contracts.md` section 7)
pub struct LaunchTrailEntry { pub stage: crate::transfer_registry::TrailStage, pub detail: serde_json::Value }
fn take_launch_trail(&self, key: &SessionKey) -> Vec<LaunchTrailEntry> { let _ = key; Vec::new() }
```

#### Every `AgentLaunch` construction site (grep `AgentLaunch\b`, verified)

Full struct literals (each must gain `policy: LaunchPolicy::Default`; the compiler will list them):

| Site | Kind | Policy |
|---|---|---|
| `host.rs:1299` `agent_launch_with` | production | `LaunchPolicy::Default` |
| `backend/unavailable.rs:236` | test | `LaunchPolicy::Default` |
| `backend/fake.rs:388` (`FakeBackend` test) | test | `LaunchPolicy::Default` |
| `backend/herdr/mod.rs:4974` `claude_launch()` helper | test | `LaunchPolicy::Default` (all struct-update sites below inherit it) |
| `backend/herdr/mod.rs:7814` (gated real-herdr test) | test, gated | `LaunchPolicy::Default` |

Struct-update sites (`..launch` / `..claude_launch()`), no edit needed except the first:

| Site | Policy |
|---|---|
| `host.rs:1260` `AgentLaunch { fresh, ..launch }` in `spawn_pty` | becomes `AgentLaunch { fresh, policy, ..launch }` where `policy = TransferStrict` iff the `transfer` argument is true **and** `launch.cli == CliKind::Codex`, else `Default` (C2) |
| `backend/herdr/mod.rs:5901`, `:6852`, `:7034` (tests) | inherit `Default` from `claude_launch()` |

Resolver-produced launches (`resolve_agent_launch` `host.rs:1311`, `launch_for` `:1319`, used by
`LaunchResolver` at `mod.rs:1724` for boot-resume and recovery) go through `agent_launch_with` and are
therefore always `Default`. Nothing else constructs one (no matches under `crates/`).

### C1. Persistence

**`persist_worktree_cli` (new signature, `transfer.rs:1030`)**

```rust
fn persist_worktree_cli(
    project: &ProjectConfig,                 // already resolved by the caller via 04's resolve_project(&user, name); never resolve_active()
    worktree: &str,
    cli: CliKind,
    codex_thread_id: FieldIntent<String>,
    claude_session_id: FieldIntent<String>,
) -> Result<(), String>
```

Body, in order:

1. `let _w = keitora_tasks::UserConfig::write_lock();` (the existing code does not take it; a load-modify-save
   must hold it; the new keitora-tasks method below must NOT take it).
2. `let mut user = UserConfig::load().map_err(|e| e.to_string())?;`
3. `let current = project.worktree_entries();`
4. `let matched = user.set_worktree_provider_state_by_root(&project.root, worktree, cli, codex_thread_id,
   claude_session_id, &current).map_err(|e| e.to_string())?;`
5. `if !matched { return Err(format!("project {} is not registered; cannot persist transfer state",
   project.project.name)); }`

**New writer in `crates/keitora-tasks/src/config.rs`** (pattern: `set_worktree_diff_base_by_root`,
`config.rs:888`, which also bypasses the merge):

```rust
impl UserConfig {
    pub fn set_worktree_provider_state_by_root(
        &mut self, root: &Path, worktree: &str, cli: CliKind,
        codex_thread_id: FieldIntent<String>, claude_session_id: FieldIntent<String>,
        current: &[WorktreeEntry],
    ) -> Result<bool, ResolveError>
}
```

Semantics: for each registered project whose `canonical_key(&pref.path) == canonical_key(root)`:
`list = pref.worktrees.clone().unwrap_or_else(|| current.to_vec())`; find the entry named `worktree`, or push
`current`'s entry of that name, or `WorktreeEntry::name_only(worktree)`; set `entry.cli = Some(cli)`; apply
each intent (`Set(v)` → `Some(v)`, `Clear` → `None`, `Keep` → untouched); `pref.worktrees = Some(list)`;
`matched = true`. If matched, `self.save()?`. Return `matched`. It never calls
`merge_worktree_registry_fields`, so `Clear` sticks, and `diff_base`/`path` of the entry are untouched.

Intents per outcome (01 chooses; this is what is executed):

| Outcome | `cli` | `codex_thread_id` | `claude_session_id` | resume-store record |
|---|---|---|---|---|
| `Imported` | Codex | `Set(id)` | `Set(id)` if the JSONL id is known else `Keep` | `ResumeRecord::Codex { thread_id: id }` |
| `Fresh` | Codex | `Clear` | `Set(id)` if known else `Keep` | `ResumeRecord::Cleared` |
| Codex→Claude (`Done`) | Claude | `Keep` | `Keep` | none (unchanged from today) |

The two `persist_worktree_cli(&worktree, …, None, None)`-style callers (`transfer.rs:257`, `:347`) are
rewritten by 01 to the table above.

**Resume-store record (belt and braces, best-effort).** The store outranks the registry in
`ResumeStore::resolve` (`resume.rs:276`), so a stale row would beat the new state for any later *Default*
launch (boot-resume, plain restart). After `persist_worktree_cli` succeeds, still inside the `DispatchGate`
window, the caller runs
`backend.set_resume_record(&SessionKey { project: <name>, worktree }, record)`; a failure is logged and
pushed to the trail (stage `persist`) but is **not** an error (the strict launch below does not consult
the store). The caller needs `State<SharedBackend>` (as `transfer_codex_to_claude` already takes at
`transfer.rs:278`). Implementation:

- `PtyBackend::set_resume_record` default is the no-op above (so `FakeBackend` and `unavailable.rs` need no
  edit).
- `HerdrBackend` overrides it (near `set_terminated`, `mod.rs:2645`):
  `Codex { thread_id }` → `self.store.replace(key, CliKind::Codex, &thread_id)`; `Cleared` →
  `self.store.clear(key)` (existing; leaves `session_id=NULL`, `cleared_id=<old id>`, which resolve treats as
  "no usable stored id").
- New `ResumeStore::replace(&self, key: &SessionKey, cli: CliKind, session_id: &str)` in `resume.rs`:
  `with_db_do("replace", |db| db.resume_replace(&key.project, &key.worktree, cli, session_id,
  "keitora:transfer", (self.now_ms)()))`.
- New `LocalStore::resume_replace(&self, project, worktree, cli, session_id: &str, source: &str,
  observed_at_ms: i64) -> Result<()>` in `crates/keitora-tasks/src/local_store.rs`, an **unconditional**
  upsert (no newest-wins, no cleared-id guard):
  `INSERT INTO resume_sessions (project, worktree, cli, session_id, source, agent_present, cleared_id,
  observed_at) VALUES (?1,?2,?3,?4,?5,1,NULL,?6) ON CONFLICT(project, worktree) DO UPDATE SET cli =
  excluded.cli, session_id = excluded.session_id, source = excluded.source, agent_present = 1, cleared_id =
  NULL, observed_at = excluded.observed_at`. (`agent_present=1` so recovery's `skip_resume`,
  `resume.rs:157`, does not treat the key as quit.)
- Known race (accepted): herdr may observe the dying Claude pane after the replace and flip the row back
  (`resume_observe`, different cli resets the row). That only affects a later *Default* launch and is
  corrected by the observation of the new Codex pane after a successful strict launch.

### C2. How `transfer` and `fresh` reach the host and the policy

- **TS** (`src/lib/ipc.ts:582`, owned by `03`): `ptySpawn(project, id, cwd, size, command?, fresh?,
  transfer?)`; the invoke payload gets `transfer: true` **only when true** (spread `...(transfer ? {
  transfer: true } : {})`), keeping the existing exact-payload tests green.
- **Tauri command** (`host.rs:1381`): add `transfer: Option<bool>` after `fresh`; pass
  `transfer.unwrap_or(false)`.
- **`spawn_pty`** (`host.rs:1218`) gains `transfer: bool` as the last parameter. Update its three call sites:
  `pty_spawn` (`:1391`), `session_launch` (`:1772`, passes `false`), and any test that calls it. After the
  `command` early return (`:1240-1244`, which ignores `transfer`):
  ```rust
  let policy = if transfer && launch.cli == keitora_tasks::CliKind::Codex {
      LaunchPolicy::TransferStrict
  } else {
      LaunchPolicy::Default
  };
  let launch = AgentLaunch { fresh, policy, ..launch };
  ```
  A `transfer: true` for a Claude worktree (the Codex→Claude restart) is deliberately `Default` (log
  `transfer: strict launch applies to codex only` at info). The launch is re-derived from persisted state on
  every spawn (`host.rs:1254`), so the thread id persisted by C1 is what `launch.resume_target` carries.
- **Meaning of the two flags to the strict policy** (decided in `HerdrBackend::spawn_agent`, C3):
  - `fresh: true` → chain starts at Fresh.
  - `fresh: false` and `resume_target = Some(safe id)` → chain starts at `Resume(thread)`.
  - `fresh: false` and no (or unsafe) `resume_target` → chain starts at Fresh (logged as a warning).
    **Never `Continue`.**
- **Trail hand-off.** In the `pty_spawn` command wrapper (not in `spawn_pty`, which has no registry access),
  when `transfer` is true: clone `project`/`id` first; after `spawn_pty` returns (on **both** `Ok` and
  `Err`), drain `client.take_launch_trail(&key)` and call `crate::transfer_registry::trail(&project, &id,
  entry.stage, entry.detail)` (04's helper; it stamps `at_ms` itself, so entries carry no timestamp) for each
  entry. `project` is the resolved `[project].name` (the same string `pty_spawn` receives). The backend buffers entries in
  `Spawner.trails: Arc<Mutex<HashMap<SessionKey, Vec<LaunchTrailEntry>>>>` (new `HerdrBackend` field,
  plumbed through `spawner()` at `mod.rs:1899` and every `HerdrBackend { .. }` construction), cleared at the
  start of each strict spawn, capped at 200 entries per key, and drained by `take_launch_trail`.

### C3. The transfer-strict launch

`spawn_agent_locked` (`mod.rs:816`) is shared with boot-resume and recovery (`resume_locked` `:952`,
`BackendResumeRunner` `:1439`), which depend on timeouts surfacing as `Err`/`Failed` for the memory-gated
backoff (`is_load_related_error` `:1424`, `fell_back || timed_out` `:1488`). **`spawn_agent_locked`,
`spawn_agent_with`, `resume_locked` and `agent::next_fallback` are not edited.** The strict path is a
separate function.

**Where it lives.**

- New file `backend/herdr/strict.rs` (declare `mod strict;` in `mod.rs`) holding the pure parts:
  `StrictTiming`, `StrictStep`, `strict_chain`, `Verdict`, `classify_observation`, `sanitize_ui_tokens`,
  `format_final_error`.
- `impl Spawner { pub(super) fn spawn_agent_transfer_strict(&self, key: &SessionKey, cwd: String, cols: u16,
  rows: u16, launch: &AgentLaunch) -> Result<(u64, ResumeResult, bool), PtyBackendError> }` in the same file
  (a child module may use `Spawner`'s private fields).
- In `HerdrBackend::spawn_agent` (`mod.rs:2272`) only the last three lines change, after the shared gates
  (`is_ready`, `gate.admit`, `is_terminated`, `require_running`, `ensure_ready` stay as they are):
  ```rust
  let spawner = self.spawner();
  let gen = match launch.policy {
      LaunchPolicy::Default => {
          let decision = spawner.resolve(&key, &cwd, &launch);
          let (gen, _, _) = spawner.spawn_agent_with(&key, cwd, cols, rows, &launch, decision)?;
          gen
      }
      LaunchPolicy::TransferStrict => {
          spawner.spawn_agent_transfer_strict(&key, cwd, cols, rows, &launch)?.0
      }
  };
  Ok(gen)
  ```
  The `Default` arm is the existing code verbatim. The strict function takes the key's lock itself
  (`let lock = self.lock_for(key); let _held = lock.lock().unwrap();`), like `spawn_agent_with`
  (`mod.rs:797-809`). It does not call `ResumeStore::resolve`.

**Flag control.** Add to `agent.rs`:
```rust
pub fn build_agent_args_opts(cli: CliKind, autonomy: bool, resume: &ResumeDecision, hook_trust: bool) -> Vec<String>
pub fn build_agent_args(cli: CliKind, autonomy: bool, resume: &ResumeDecision) -> Vec<String> {
    build_agent_args_opts(cli, autonomy, resume, true)
}
```
`build_agent_args_opts` is the current body of `build_agent_args` with `args.push("--dangerously-bypass-hook-trust")`
(`agent.rs:73`) guarded by `if hook_trust`. Only that one flag is "optional". The autonomy flag
`--dangerously-bypass-approvals-and-sandbox` is user policy and is never dropped. **No version gating:** the
Codex release that introduced `--dangerously-bypass-hook-trust` is unknown to this repo, so flags are not
chosen by version. Instead the flag is dropped on the last chain step (flag-rejection fallback).

**Chain (pure, `strict.rs`).**
```rust
pub(super) struct StrictStep { pub decision: ResumeDecision, pub hook_trust: bool, pub label: String }
pub(super) fn strict_chain(launch: &AgentLaunch) -> Vec<StrictStep>
```
- `resume_id = if launch.fresh { None } else { launch.resume_target.as_deref().filter(|id| is_safe_claude_session_id(id)) }`
  (an unsafe id would make `build_agent_args` emit `resume --last`, i.e. `Continue`; filtering it out is
  what keeps "never Continue" true).
- `Some(id)` → `[Resume{cli: Codex, id} hook_trust=true label="resume <id>", Fresh hook_trust=true
  label="fresh", Fresh hook_trust=false label="fresh without optional flags"]`.
- `None` → `[Fresh true "fresh", Fresh false "fresh without optional flags"]`.
- The result never contains `ResumeDecision::Continue` (unit-tested).

**Timing (`strict.rs`).** One struct, one test override following the repo pattern (`cfg!(test)` ⇒ tiny
values, as `reconcile_current` does at `mod.rs:1010`):
```rust
pub(super) struct StrictTiming {
    pub shell_ready: Duration,        // prod 300 ms (= SHELL_READY_DELAY_MS, mod.rs:111)
    pub poll: Duration,               // prod 250 ms
    pub verify_window: Duration,      // prod 10 s   after agent.start returned Ok
    pub step_cap: Duration,           // prod 45 s   hard cap per step, incl. agent.start + timeout extension
    pub chain_budget: Duration,       // prod 100 s  hard total budget: each step's cap is min(step_cap, budget remaining); no step starts with < 1 poll of budget left
    pub reissue_backoffs: [Duration; 2], // prod [300 ms, 800 ms]
}
pub(super) fn strict_timing() -> StrictTiming   // cfg!(test) -> fast(): 0 / 2 ms / 60 ms / 200 ms / 1 s / [1 ms, 2 ms]
```
`AGENT_START_TIMEOUT_MS` (10 000) is reused unchanged for `agent_start`.

**Algorithm (`spawn_agent_transfer_strict`).** `chain = strict_chain(launch)`; `first = chain[0].decision`;
`chain_start = Instant::now()`; `extra_env = strict_pane_extra_env(std::env::var("CODEX_HOME").ok())` (D);
`pane_env = env::pane_env_with(&launch.path, &self.ops.env().original_xdg, self.ops.env().path(),
&extra_env)`; `tried: Vec<String> = []`; `last_reason = String::new()`. Clear this key's trail buffer.
For each `(index, step)`:

1. If `index > 0 && chain_start.elapsed() >= chain_budget` → stop (fall to "exhausted").
2. Replace any live same-key pane exactly as `mod.rs:842-847` does (stop bridge, `pane_close`, drop keymap
   and live entries).
3. `let id = self.ops.workspace_create(&cwd, &pane_env)?;` — an error here is returned immediately
   (`Err(e.into())`), as in the default path. `sleep(shell_ready)`.
4. `tried.push(step.label)`; trail `launch` `{index, label, args, paneId}`.
5. `args = agent::build_agent_args_opts(Codex, launch.autonomy, &step.decision, step.hook_trust)`.
   `step_start = Instant::now()`; `step_deadline = step_start + step_cap`.
6. **agent.start with re-issue.** Loop: `r = self.ops.agent_start(&name, "codex", &id.pane_id, &args,
   AGENT_START_TIMEOUT_MS)`.
   - `Ok(())` → go to verify (window = `now + verify_window`, capped at `step_deadline`).
   - `Err(e)` where `agent::is_early_exit(&e)` → step failure, reason `format!("agent exited early ({}): {}",
     code, message)`; no re-issue.
   - `Err(e)` where `is_agent_start_timeout(&e)` (`HerdrError::Timeout`, or `e.code() == Some("timeout")`, or
     the message contains `"timed out waiting for agent startup"`, same text as `is_timeout_text`,
     `mod.rs:1415`) → **not a failure**: go to verify with window = `step_deadline` (up to 45 s from
     `step_start`).
   - `Err(e)` otherwise, with `!e.is_not_found() && !e.is_exe_missing()` and fewer than 2 re-issues so far
     → `sleep(reissue_backoffs[n])`, trail `launch` `{reissue: n+1, error: e.to_string()}`, loop again **on
     the same pane**.
   - `Err(e)` otherwise → step failure, reason `e.to_string()`.
7. **Verify** (below). `Confirmed` / `PresentUnknown` → success.
8. On step failure: capture `self.ops.pane_read(&id.pane_id, "recent", "text", Some(20))` (errors ignored,
   result truncated to 1 KiB) into trail `verify` `{paneTail}`; `pane_close(&id.pane_id)`; record
   `lifecycle::record(LifecycleEvent::ResumeOutcome { key, outcome: ResumeResult::Failed { reason } })` as
   `mod.rs:874-882` does; `last_reason = reason`; continue the loop.

On success: **copy** the post-launch tail of `spawn_agent_locked` (`mod.rs:858-866` keymap `record`, `:892`
`live.insert`, `:896-912` `start_bridge_in`, `:914` `resolve_recovered`, `:915-932` lifecycle record + the
`"resume"` session event) into a private `fn finish_strict_launch(...)` in `strict.rs`. Do **not** refactor
`spawn_agent_locked` to share it (that would touch the `Default` path); a follow-up may dedupe. `fell_back =
step.decision != first`. The event payload is exactly the default one: `{ "outcome": word, "fell_back":
fell_back }`, emitted under the same condition (`fell_back || decision != Fresh`). Note `fell_back` is "the
decision got weaker than requested" (Resume→Fresh), which for a chain that began at Resume equals "advanced
past the first step" (`00-contracts.md` section 6); dropping only the hook-trust flag on a Fresh→Fresh step
does not set it (the flag drop is visible in the trail). The event is emitted inside this function, before it
returns, hence before `pty_spawn` resolves (`03` relies on that order).

If the loop ends without success ("exhausted"): `Err(PtyBackendError::Other(format_final_error(&last_reason,
&tried)))` — nothing is live, every pane was closed. `format_final_error` returns
`"codex did not start: <last_reason> (tried: <tried joined with ' -> '>)"` (e.g. `"codex did not start:
agent exited early (agent_start_failed): unexpected argument (tried: resume thr-1 -> fresh -> fresh without
optional flags)"`). `last_reason` is whitespace-collapsed (newlines → space) and capped at 300 chars, and
passed through `sanitize_ui_tokens`: the pane decides on `msg.includes("Recovering")` /
`msg.includes("Terminated")` (`TerminalPane.tsx:588-589`), so those two exact, case-sensitive substrings are
replaced by their lowercase forms. `tried` lists only steps actually attempted.

**Verify (`strict.rs`).** Poll `self.ops.pane_get(&id.pane_id)` — first poll immediately, then every `poll`
(250 ms) — until the window ends. State kept: `seen_agent: bool`, `consecutive_present: u32`,
`last_mismatch: Option<String>`, `transient_errors: u32`, `last_logged: Option<(agent, status, session)>`.
Per observation `p: Option<PaneInfo>` the loop pushes a trail `Verify` entry `{agent: p.agent, status:
p.agent_status, session: {agent, kind, value}|null}` **only when that triple differs from `last_logged`** (an
unchanged poll writes nothing, so one transfer stays far below the registry's 300-entry cap; the entry has no
timestamp field, the registry stamps `at_ms`):

- `Err` with `e.is_not_found()` or `Ok(None)` → pane gone: `Verdict::Gone`.
- Any other `Err` → `transient_errors += 1`, keep polling (logged).
- `p.agent == Some(..)`: `seen_agent = true; consecutive_present += 1`.
  - Expectation `Resume(thread)`: `p.agent == Some("codex")` and `p.agent_session` is `Some(s)` with
    `s.agent == "codex"`, `s.kind == AgentSessionKind::Id`, `s.value == thread` → `Confirmed` (stop
    immediately). A `Some(s)` with `kind == Id` and a different value → `last_mismatch = Some(s.value)`.
    (`s.source` is not checked.)
  - Expectation `Fresh`: `p.agent == Some("codex")` and `consecutive_present >= 2` → `Confirmed`.
- `p.agent == None` → `consecutive_present = 0`; if `seen_agent` → `Verdict::Vanished` (stop immediately).

When the window ends with no verdict: `last_mismatch.is_some()` and the expectation is Resume →
`Mismatch(value)` (failure: `"session mismatch: pane reports <value>"`); else `seen_agent` →
`PresentUnknown` (**success**, trail `present_unknown`: the agent is up but herdr never reported the
session id; do not orphan the thread); else (agent never seen) → **before failing, query the pane's process
info**: `self.ops.pane_process_info(&id.pane_id)` (`PaneProcessInfo { shell_pid, foreground_pids }`,
`cli.rs:101-110`; `foreground_pids` never contains `shell_pid`; the fake answers what
`FakeHerdr::set_process_info` scripted and `pane_not_found` otherwise, `fake.rs:857`). `Ok(info)` with a
non-empty `foreground_pids` means a non-shell foreground process exists (Codex is running but herdr never
labelled the pane): `PresentUnknown` (**success**, trail `Verify` `{present_unknown: true, reason:
"foreground_process", foregroundProcesses: <count>}`). `Ok` with an empty `foreground_pids`, or any `Err`
(including pane not found), means the agent really never appeared: failure `"agent never appeared"`, and the
chain advances. The decision is the pure `classify_never_appeared(&Result<PaneProcessInfo, HerdrError>) ->
Verdict` in `strict.rs`.
`Gone` → failure `"pane closed during startup"`; `Vanished` → failure `"agent vanished after start"`.

Window rules: after `agent_start` returned `Ok`, window end = `min(now + verify_window, step_deadline)`; after
a start timeout, window end = `step_deadline`. The 45 s `step_cap` is also the longest a slow-loading large
thread is given before it is abandoned to fresh.

**Fake herdr extension (`backend/herdr/fake.rs`).** Today `FakeHerdr::agent_start` (`:866-907`) with `Ok`/
`NotReady` immediately sets `agent`, `agent_status` and an `agent_session` (value from `resume_id_from(args)`,
`:179`), `pane_get` (`:849`) just returns the live pane, and there is no way to script `pane_get`, so
vanish/delay/mismatch cannot be tested. Minimal extension, nothing sleeps:

```rust
/// Scripted answer of one `pane_get` (global FIFO; `Live` once exhausted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneGetScript {
    /// Answer from the fake's live pane state (today's behaviour).
    Live,
    /// The pane exists but reports no agent, status or session (a bare shell). Does not mutate state.
    NoAgent,
    /// Report this agent (`agent`, status "idle") and session whatever the state says. Does not mutate state.
    Agent { agent: String, session: Option<AgentSession> },
    /// Fail with this error.
    Err(HerdrError),
}
```
- `FakeState` gains `pane_get_scripts: VecDeque<PaneGetScript>` (initialise in the `FakeState { .. }` literal
  at `fake.rs:318`); `FakeHerdr::script_pane_get(&self, results: Vec<PaneGetScript>)` extends it.
  `pane_get` pops one per call after `st.control(Op::PaneGet, ..)?` and answers per variant (`Live` =
  today's body; `NoAgent`/`Agent` clone `pane_mut(pane_id)?` and override those fields on the clone).
- `AgentStartScript` gains `TimedOutLaunched`: applies the same launch effects as `Ok` (agent + session set)
  and then returns `Err(api("timeout", "timed out waiting for agent startup"))`.
- Existing `AgentStartScript::{Ok, EarlyExit, NameLost, NotReady, Err}` are reused. A helper
  `pub fn repeat_pane_get(script: PaneGetScript, n: usize) -> Vec<PaneGetScript>` avoids 100-entry literals.
- Because the fake already records every call (`Op::PaneGet`, `Op::AgentStart`, `Op::PaneClose`,
  `Op::WorkspaceCreate`), tests assert call order and counts with `fake.calls()` and
  `agent_start_args(&fake)` (`mod.rs:6806`).

**Present-unknown / orphaning.** A timeout and a slow verify never advance the chain before `step_cap`
(45 s). `PresentUnknown` is success. This stops a large imported thread from being abandoned to fresh just
because it loads slowly, which would orphan it.

**Final failure.** The structured error above, `cli=codex` stays persisted, the pane shows spawn-failed with
that text and the worktree's actual CLI name (03). **Kill & Restart after such a failure** is decided by `03`
and must never reach the Default `Continue` (`resume --last`): after a transfer whose outcome was `Fresh` (or
any spawn-failed following a transfer restart to Codex that had no thread id) it restarts with `fresh=true`
and `transfer=true` (this strict chain starting at Fresh); after an `Imported` outcome the persisted thread id
makes it `Resume(<thread>)`, also with `transfer=true`. The Default launch is therefore not used for this
button during a transfer's aftermath (`00-contracts.md` section 5).

### D. Codex environment, shared

**Location.** keitora-tasks gets only the pure discovery pieces; the resolver lives in a new
`src-tauri/src/codex_env.rs` (`mod codex_env;` in `main.rs`/`lib.rs` next to `mod transfer;`), used by
`transfer.rs`, `host.rs` and (for the pane extra env) `backend/herdr/strict.rs`.

```rust
// src-tauri/src/codex_env.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExeSource { Configured, Discovered, BareName }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHomeSource { Initialize, EnvVar, Default }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexEnv {
    pub exe: String,                 // absolute path when configured/discovered, else "codex"
    pub exe_source: ExeSource,
    pub path: Option<String>,        // value to set as PATH on the import child; None = leave inherited
    pub codex_home: std::path::PathBuf,   // resolved BEFORE spawn (env var, else ~/.codex); see with_initialize_home
    pub codex_home_source: CodexHomeSource,
    pub version: Option<String>,     // "0.166.0"; None if the probe failed
}

/// Everything resolution reads from the machine, injectable for tests.
pub struct CodexEnvProbe<'a> {
    pub configured: Option<String>,                       // keitora_tasks::resolve_provider_path(Codex, Some(&project), &user)
    pub discover: &'a dyn Fn() -> Option<String>,         // prod: keitora_tasks::discover_cli_cached(Codex)
    pub base_path: Option<String>,                        // unix: keitora_tasks::unix_spawn_path(); windows: process PATH
    pub env_codex_home: Option<String>,                   // std::env::var("CODEX_HOME").ok()
    pub home_dir: Option<std::path::PathBuf>,             // dirs::home_dir()
    pub path_sep: char,                                   // ':' unix, ';' windows
}

pub fn resolve_codex_env(p: &CodexEnvProbe) -> Result<CodexEnv, String>;      // pure; version = None
pub fn probe_codex_env_from_machine(project: &ProjectConfig, user: &UserConfig) -> CodexEnvProbe<'static>; // thin, cfg-gated
impl CodexEnv {
    pub fn with_version(self, v: Option<String>) -> CodexEnv;
    /// 01 calls this after `initialize` with the result's `codexHome`; empty/None leaves it unchanged. The
    /// result is used for the ledger lookup ONLY: the child was already spawned with the pre-initialize value.
    pub fn with_initialize_home(self, home: Option<&str>) -> CodexEnv;
}
pub fn codex_version(exe: &str, path: Option<&str>) -> Option<String>;        // cached, 5 s bound
pub fn parse_codex_version_output(stdout: &str) -> Option<String>;            // pure
pub fn compose_path(exe_dir: Option<&str>, base: Option<&str>, sep: char) -> Option<String>; // pure
pub fn strict_pane_extra_env(env_codex_home: Option<String>) -> Vec<(String, String)>; // pure
```

**`resolve_codex_env` rules (pure):**
- `exe`, in order: (1) `configured` (`ExeSource::Configured`); (2) `(discover)()` (`Discovered`); (3) bare
  `"codex"` (`BareName`). The Homebrew location is step 2's responsibility: `discover_cli(Codex)` gains it
  (below), so there is no separate step 3 in the resolver.
- `path`: `compose_path(exe_dir(exe), base_path, sep)`: `exe_dir` is `Path::new(exe).parent()` as a string
  when it is non-empty (a bare name has none), placed **first**, then the entries of `base_path`, order kept,
  duplicates and empty entries dropped, joined with `sep`; `None` when both are empty. On unix `base_path` is
  `unix_spawn_path()` (login PATH, then Homebrew/`/usr/local/bin`/`~/.local/bin`, then the process PATH,
  `config.rs:1944`; it already includes `/opt/homebrew/bin`). On Windows `base_path` is the process `PATH`
  (`unix_spawn_path` is `#[cfg(unix)]`).
- `codex_home` (resolved **before spawn**): `env_codex_home` when non-empty (`EnvVar`); else
  `home_dir.join(".codex")` (`Default`); `Err("could not resolve home directory for CODEX_HOME")` only when
  `env_codex_home` is empty/None and `home_dir` is None. The source goes to the trail (stage `Env`).
- `initialize` runs after spawn, hence the two-step: `resolve_codex_env` (pre-spawn value) then, after the
  handshake, `with_initialize_home(result.codexHome)`, which replaces `codex_home` and sets the source to
  `Initialize` **for the ledger lookup only**. 01 sets `CODEX_HOME` on the import child (and the strict pane
  gets it, below) **only when the pre-spawn source is `EnvVar`** (explicit); otherwise Codex applies its own
  default. `with_initialize_home` never changes what the child was started with.

**keitora-tasks changes (`crates/keitora-tasks/src/config.rs`).**
- Pure, un-gated (so they test on any OS): `pub(crate) fn codex_unix_candidates(home: Option<&Path>) ->
  Vec<PathBuf>` returning, in order, `/opt/homebrew/bin/codex`, `/usr/local/bin/codex`,
  `<home>/.local/bin/codex`; and `pub(crate) fn first_executable(cands: &[PathBuf], is_exec: &dyn Fn(&Path) ->
  bool) -> Option<PathBuf>`.
- Unix `discover_cli(Codex)` (`config.rs:2253`): after the login-PATH search, replace the current
  `~/.local/bin/codex`-only tail with `first_executable(&codex_unix_candidates(dirs::home_dir().as_deref()),
  &is_executable_file)`; delete the stale "Homebrew locations are Claude-installer-specific" comment.
  Windows `discover_cli` (`:2276`) is unchanged.
- New, cached: `pub fn discover_cli_cached(cli: CliKind) -> Option<String>` (re-exported from `lib.rs`): a
  process-wide `Mutex<HashMap<CliKind, (Instant, Option<String>)>>`; a `Some` is cached for the process, a
  `None` for 30 s (so an install mid-session is found within 30 s without running the login shell on every
  spawn). `discover_cli` itself keeps its uncached behaviour for other callers. Never call it on the app's
  setup thread (it may run the login shell once).

**Launch-side use (`host.rs`).** In `agent_launch_with` (`:1276`) replace the bare fallback (`:1289-1293`)
for **Codex only**:
```rust
let path = keitora_tasks::resolve_provider_path(cli, project, user)
    .or_else(|| match cli { CliKind::Codex => discover_for_launch(cli), CliKind::Claude => None })
    .unwrap_or_else(|| /* existing bare "claude" / "codex" */);
```
with
```rust
fn discover_for_launch(cli: CliKind) -> Option<String> {
    #[cfg(test)] { let _ = cli; None }                       // tests must not run a login shell / read the real fs
    #[cfg(not(test))] { keitora_tasks::discover_cli_cached(cli) }
}
```
(the `cfg(test)` split follows `keitora_alias_dir`, `env.rs:804-814`). For tests of the discovery branch,
`agent_launch_with` delegates to `agent_launch_with_discovery(user, project, worktree, discover: &dyn
Fn(CliKind) -> Option<String>)`, which tests call directly. This is the only change to `Default` launch
resolution and it only replaces the bare-string fallback; Claude launches are untouched. Because the
resolved absolute path reaches `env::pane_env` (`:750`) via `launch.path`, the pane's PATH then gets the exe's
dir prepended (`pane_path_with`), which also covers a Homebrew `node` next to an npm shim.

**Pane `CODEX_HOME`.** `herdr agent start` takes no env (`real.rs:351-379`); env is applied at
`workspace_create` through `pane_env`. Add `pub fn pane_env_with(exe, original, herdr_path, extra:
&[(String, String)]) -> Vec<(String, String)>` in `env.rs` (the current `pane_env` body, with `extra`
appended last); `pane_env` becomes `pane_env_with(.., &[])` so `Default` is byte-identical. The strict spawn
passes `strict_pane_extra_env(std::env::var("CODEX_HOME").ok())`, which is `[("CODEX_HOME", v)]` for a
non-empty `v` and `[]` otherwise, so the pane agrees with the import child (which got the same value) only
when the user explicitly set it; with no override both sides use Codex's default.

**Version.** `codex_version(exe, path)`: cache `static VERSIONS: OnceLock<Mutex<HashMap<String,
Option<String>>>>` keyed by the exe string; a `Some` is cached for the process, a `None` is not (the next
transfer retries). Probe: `transfer::codex_command(exe, &["--version"])` (make it `pub(crate)`; it already
handles the Windows `cmd /C` + short path) with `stdin(null)`, stdout piped, `apply_no_window`, `PATH` set to
`path` when `Some`; poll `try_wait` every 50 ms for at most 5 s, `kill()` + `None` on expiry or a non-zero
exit. `parse_codex_version_output`: first non-empty line, last whitespace-separated token that matches
`^\d+\.\d+\.\d+` (so `"codex-cli 0.166.0"` → `"0.166.0"`, a pre-release suffix is kept). The version feeds
the trail and fills `CodexTooOld.version` (≥ 0.149 for `importId`). **`CodexTooOld` itself is decided
immediately by 01 when Codex answers the import method with `-32601`; it never consults the ledger** (a ledger
record could only be a stale earlier snapshot) and never waits on the version probe. The version never selects
launch flags.

## Tests

All Rust, mocked through `backend/herdr/fake.rs` for herdr; **never the real herdr** and never an `#[ignore]`
gated herdr test. Names are the function names to create. Strict tests live in the `mod tests` of
`backend/herdr/mod.rs` (they need its private helpers `fake_backend` `:3245`, `skey` `:3275`, `worktree_dir`
`:6774`, `agent_start_args` `:6806`); pure tests live in `strict.rs`/`agent.rs`/`codex_env.rs`/keitora-tasks.
Strict tests build a Codex launch: `AgentLaunch { cli: Codex, path: "codex".into(), autonomy: false,
resume_target: Some("thr-1".into()), fresh: false, policy: TransferStrict }`.

### Persistence (keitora-tasks, `config.rs` tests, using `TEST_ENV_LOCK` + `KEITORA_CONFIG_HOME` like `:5746`)

- `provider_state_set_writes_cli_and_thread` — `Set("thr-1")`/`Keep`: entry `cli=Codex`, `codex_thread_id=thr-1`, `claude_session_id` unchanged.
- `provider_state_clear_survives_the_merge` — entry has `codex_thread_id=old`; `Clear` ⇒ reloaded registry has `None` (the regression `set_project_worktrees_by_root` would fail).
- `provider_state_keep_leaves_both_ids` — `Keep`/`Keep` with `cli=Claude`: both ids intact (Codex→Claude).
- `provider_state_fresh_leaves_claude_session_id` — `Clear` + `Keep`: `claude_session_id` intact.
- `provider_state_creates_missing_entry_from_current` — worktree absent from `pref.worktrees`: entry created with `current`'s path.
- `provider_state_preserves_path_and_diff_base`.
- `provider_state_unknown_root_returns_false_and_does_not_save`.
- `provider_state_updates_only_the_named_worktree`.
- `provider_state_non_active_project` — two registered projects; writing project B changes only B.

`transfer.rs` (own `static CONFIG_ENV_LOCK: Mutex<()>` + `KEITORA_CONFIG_HOME` temp dir; src-tauri tests have none today):
- `persist_worktree_cli_resolves_by_project_not_active` — active = A, persist for B; B changed, A untouched.
- `persist_worktree_cli_unregistered_project_errors_without_writing`.

### Resume store (`local_store.rs`, `resume.rs`)

- `resume_replace_overwrites_a_claude_row` — row `claude/A`, replace with `codex/thr-1` ⇒ `cli=Codex, session_id=thr-1, source="keitora:transfer", agent_present=true, cleared_id=None`.
- `resume_replace_ignores_newest_wins_and_cleared_id` — older `observed_at`, and a `cleared_id == thr-1`, still replaced.
- `resume_replace_creates_a_missing_row`.
- `store_replace_then_resolve_returns_resume_codex` — `ResumeStore::replace` then `resolve(ctx{cli: Codex, registry_id: Some("other"), fresh:false})` ⇒ `Resume{Codex, "thr-1"}`.
- `store_clear_then_resolve_never_returns_the_cleared_claude_id` — after `clear`, `resolve` with `cli=Codex` and no registry id ⇒ `Continue` (documents the existing result; the strict policy does not call it), and an `observe` of the cleared Claude id is still ignored (cleared-id guard).
- `herdr_set_resume_record_codex_and_cleared` (mod.rs tests, fake backend) — `set_resume_record` writes/clears the row visible via `LocalStore::resume_get(fake.env().resume_db_path())`.
- `default_trait_set_resume_record_is_a_noop` — `FakeBackend` returns `Ok`.

### Chain and flags (pure)

- `strict_chain_resume_has_three_steps` — labels `["resume thr-1","fresh","fresh without optional flags"]`, `hook_trust` `[true,true,false]`.
- `strict_chain_fresh_starts_at_fresh` — `fresh:true` with a `resume_target` ⇒ 2 steps, first is `Fresh`.
- `strict_chain_without_target_starts_at_fresh`.
- `strict_chain_skips_an_unsafe_thread_id` — `resume_target = Some("a b; --last")` ⇒ no `Resume`.
- `strict_chain_never_contains_continue` — property over `{fresh}×{None, safe, unsafe}`.
- `build_agent_args_opts_true_equals_legacy` — for every row of the existing matrix (`agent.rs:179-240`), `build_agent_args == build_agent_args_opts(.., true)`.
- `build_agent_args_opts_false_drops_only_hook_trust` — codex with/without autonomy and each decision: only `--dangerously-bypass-hook-trust` removed; claude args identical for both values.
- `sanitize_ui_tokens_lowercases_recovering_and_terminated`; `format_final_error_shape` (exact string for `("exited early", ["resume thr-1","fresh"])`, whitespace collapse, 300-char cap).
- `is_agent_start_timeout_matches_code_message_and_transport_timeout`.
- `classify_observation_*` for the verify state machine, driven with synthetic observation lists and no ops: `_confirms_resume_on_matching_id`, `_ignores_source`, `_resume_mismatch_only_at_window_end`, `_fresh_needs_two_consecutive_polls`, `_vanished_after_seen`, `_never_seen_defers_to_the_process_check`, `_agent_without_session_is_present_unknown`, `_gone_on_not_found`, `_transient_errors_do_not_fail`, `_trail_entry_only_when_the_observation_changes` (a repeated identical observation yields no second entry). And for the never-appeared decision: `classify_never_appeared_foreground_process_is_present_unknown` (`foreground_pids: [4200]`), `classify_never_appeared_empty_foreground_is_failure`, `classify_never_appeared_process_info_error_is_failure` (pane-not-found and a generic error).

### `TransferStrict` end to end (fake herdr, fast timing)

- `strict_resume_confirmed_by_matching_session` — no scripts. Assert: 1 `AgentStart`, args `["resume","thr-1","--dangerously-bypass-hook-trust"]`, `fell_back == false`, resume event `{"outcome":"resumed","fell_back":false}`, key live, pane not closed.
- `strict_fresh_launch_confirmed_by_agent_label` — `fresh:true`; args `["--dangerously-bypass-hook-trust"]`; event `outcome "fresh"`, `fell_back false`.
- `strict_vanished_agent_advances_to_fresh` — `script_pane_get([Live, NoAgent])`. Assert: 2 `AgentStart` (first with `resume`, second without), first pane in `closed_pane_ids()`, `fell_back == true`, event `outcome "fresh", fell_back true`.
- `strict_start_timeout_keeps_polling_then_confirms` — `AgentStartScript::TimedOutLaunched` + `script_pane_get([NoAgent, NoAgent, Live])`. Assert: exactly 1 `AgentStart`, no fallback, outcome resumed.
- `strict_start_timeout_is_bounded_by_the_step_cap` — `TimedOutLaunched` + endless `NoAgent`. Assert: step fails after ≈ `step_cap` (test asserts elapsed < 2 s), chain advances to fresh.
- `strict_session_mismatch_advances` — endless `Agent{codex, Some(Id "other")}` for step 1. Assert: advanced to fresh, `fell_back true`.
- `strict_late_session_match_wins_over_earlier_mismatch` — `[Agent{.. "other"}, Agent{.. "thr-1"}]` ⇒ confirmed, no fallback.
- `strict_present_unknown_is_accepted` — endless `Agent{codex, None}`. Assert: 1 `AgentStart`, success, trail contains `present_unknown`.
- `strict_agent_never_appeared_with_foreground_process_is_accepted` — endless `NoAgent` plus `set_process_info(<pane>, PaneProcessInfo { shell_pid: 1, foreground_pids: vec![2] })` for the created pane (the fake allocates deterministic ids, as the `fake.rs:1412` test relies on). Assert: 1 `AgentStart`, 1 `PaneProcessInfo` call, success (`PresentUnknown`), no fallback, `fell_back == false`, trail contains `present_unknown` with reason `foreground_process`.
- `strict_agent_never_appeared_with_empty_foreground_still_fails` — endless `NoAgent` plus `set_process_info(.., foreground_pids: vec![])` for every pane. Assert: same exhaustion as the next test (3 `AgentStart`, structured error), and one `PaneProcessInfo` call per step.
- `strict_agent_never_appeared_exhausts_with_structured_error` — endless `NoAgent` and **no process info scripted** (the fake answers pane-not-found, which counts as "no foreground process"). Assert: `Err(PtyBackendError::Other(s))` with `s == "codex did not start: agent never appeared (tried: resume thr-1 -> fresh -> fresh without optional flags)"`, 3 `AgentStart`, the third without `--dangerously-bypass-hook-trust`, all 3 panes closed, `!backend.is_live(&key)`, keymap has no entry.
- `strict_early_exit_flag_rejection_drops_the_flag` — `script_agent_start([EarlyExit, EarlyExit, Ok])`. Assert: third call's args lack the hook-trust flag, success, `fell_back true`.
- `strict_reissues_agent_start_on_other_errors` — `[Err(api("pane_busy","busy")), Err(api("pane_busy","busy")), Ok]`. Assert: 3 `AgentStart` on the **same** pane id, 1 `WorkspaceCreate`, success, `fell_back false`.
- `strict_reissue_gives_up_after_two_retries` — 3 such errors ⇒ step failure, next step runs on a **new** pane (2 `WorkspaceCreate`).
- `strict_does_not_reissue_early_exit_or_not_found` — `EarlyExit` ⇒ exactly 1 `AgentStart` for that step.
- `strict_never_issues_continue` — run the above scenarios plus `fresh:true`, `resume_target None`, unsafe id; assert no recorded `AgentStart` args contain `--last`.
- `strict_resume_target_missing_without_fresh_starts_fresh` — args without `resume`.
- `strict_workspace_create_failure_returns_err_immediately` — `fail_next(Op::WorkspaceCreate, ..)` ⇒ `Err`, no `AgentStart`.
- `strict_replaces_a_live_same_key_pane_each_step` — pre-existing live pane for the key is closed once; second step closes the first step's pane.
- `strict_gates_are_unchanged` — terminated key ⇒ `Err(Terminated)`; server not ready ⇒ `Err(NotConnected)`; no `WorkspaceCreate` recorded.
- `strict_passes_codex_home_to_the_pane_only_when_set` — `strict_pane_extra_env(Some("/x/codex-home"))` appears in the recorded `WorkspaceCreate` env; `None` ⇒ no `CODEX_HOME`.
- `strict_trail_records_every_step_and_observation` — `take_launch_trail(&key)`: one `Launch` entry per step (with `label`, `args`), ≥ 1 `Verify` entry per step, **but not one per poll** (an endless identical `NoAgent` run yields exactly one observation entry per step), a `paneTail` entry on a failed step; a second `take_launch_trail` call returns empty; a new strict spawn clears stale entries; capped at 200.
- `strict_failure_still_hands_over_the_trail` — after the exhausted error, `take_launch_trail` is non-empty (the `pty_spawn` wrapper drains on `Err`).

### `Default` regressions (must pass with the production code untouched)

Write these **first**, before editing `spawn_agent`, to prove the baseline:

- `default_policy_timeout_still_errs_and_closes_the_pane` — Codex launch, `Default`, `script_agent_start([Err(api("timeout","timed out waiting for agent startup"))])`. Assert: `spawn_agent` is `Err(Other(m))` with `is_timeout_text(&m)`, `is_load_related_error(&e, false) == true`, exactly 1 `AgentStart`, the pane closed, no `PaneGet` recorded.
- `default_policy_resume_exit_falls_back_continue_then_fresh` — Codex launch with `resume_target`, `[EarlyExit, EarlyExit, Ok]`. Assert the three `AgentStart` args are exactly `["resume","thr-1",hook]`, `["resume","--last",hook]`, `[hook]` (pins that Default still uses `Continue` and still passes the flag).
- `default_policy_never_polls_pane_get` — a successful Default spawn records zero `Op::PaneGet`.
- `default_policy_no_silent_fresh_downgrade_on_other_errors` — `Err(api("pane_busy","busy"))` ⇒ `Err`, 1 `AgentStart`, no fallback (the strict re-issue must not leak into Default).
- `default_policy_fresh_variants_keep_the_hook_trust_flag` — `fresh:true` Codex Default ⇒ args contain the flag.
- `default_policy_event_payload_unchanged` — `{"outcome","fell_back"}` for resumed/continued/fresh/fallback.
- `launch_policy_default_is_default` — `LaunchPolicy::default() == LaunchPolicy::Default`; `host::launch_for(..)` (extend the existing assertions at `host.rs:2590-2622`) returns `policy == Default`.
- `spawn_pty_transfer_false_is_default_policy` / `spawn_pty_transfer_true_codex_is_strict` / `spawn_pty_transfer_true_claude_stays_default` / `spawn_pty_transfer_with_command_ignores_policy` — via `FakeBackend`, which records the launch it was given.
- `boot_resume_launcher_is_always_default` — `resume_locked` with a launcher built from `host::launch_for` spawns through `spawn_agent_locked` (no `PaneGet`).
- All existing tests around `is_load_related_error`, `BackendResumeRunner`, `next_fallback` and the `build_agent_args` matrix pass **unmodified**.

### `CodexEnv` (`codex_env.rs` tests; pure inputs, run on any OS)

- `exe_prefers_configured_over_discovered`; `exe_discovered_when_unconfigured`; `exe_falls_back_to_bare_codex` (`ExeSource::BareName`, `path` has no exe dir).
- `path_puts_exe_dir_first_then_base_deduped` — `compose_path(Some("/opt/x/bin"), Some("/usr/bin:/opt/x/bin:/bin"), ':')` ⇒ `"/opt/x/bin:/usr/bin:/bin"`; `compose_path(None, None, ':') == None`; empty segments dropped; a `';'` variant for Windows-style input.
- `path_uses_base_path_verbatim_when_exe_is_bare`.
- `codex_home_precedence_initialize_then_env_then_default` — three sources and `CodexHomeSource` for each; empty env var is ignored; `with_initialize_home(Some(""))` unchanged.
- `codex_home_errors_without_home_and_env`.
- `strict_pane_extra_env_empty_and_set`.
- `parse_codex_version_output_variants` — `"codex-cli 0.166.0\n"`, `"codex 0.149.0-alpha.1"`, garbage ⇒ `None`.
- `codex_version_caches_some_per_exe_and_retries_none` — injectable probe closure variant counts invocations (`codex_version_with(exe, path, probe)` backs `codex_version`).
- keitora-tasks: `codex_unix_candidates_order_and_home` (homebrew, `/usr/local`, `~/.local/bin`; `None` home drops the last), `first_executable_picks_first_match` (fake predicate), `discover_cli_cached_caches_some_and_expires_none` (injectable clock/probe).
- `host.rs`: `agent_launch_with_discovery_uses_discovered_path_for_codex_when_unconfigured`, `..._keeps_configured_path`, `..._claude_unchanged_bare_fallback`, `..._bare_codex_when_discovery_finds_nothing`.
- `env.rs`: `pane_env_with_appends_extra_and_pane_env_is_unchanged` (`pane_env(a,b,c) == pane_env_with(a,b,c,&[])`).

## Implementation order within this doc

Each step is independently buildable and testable; run only targeted test filters (`cargo test -p
keitora-tasks <filter>`, `cargo test -p keitora --lib <filter>` or the repo's equivalent); never gated herdr
tests, never the app.

1. **keitora-tasks storage and discovery** (no src-tauri dependency): `FieldIntent` +
   `set_worktree_provider_state_by_root` (+ tests); `LocalStore::resume_replace` (+ tests);
   `codex_unix_candidates` / `first_executable` / the `discover_cli(Codex)` tail / `discover_cli_cached`
   (+ tests); re-exports in `lib.rs`.
2. **Baseline for `Default`**: add the `default_policy_*` regression tests above against the **unchanged**
   herdr code (they only need the fake) and run them green.
3. **Types**: `LaunchPolicy`, `AgentLaunch.policy`, the five literal sites, `ResumeRecord`,
   `set_resume_record`, `LaunchTrailEntry`, `take_launch_trail` defaults. Re-run step 2 tests.
4. **`agent.rs`**: `build_agent_args_opts` + wrapper (+ tests; the legacy matrix must still pass).
5. **`env.rs`**: `pane_env_with` + wrapper (+ test).
6. **`codex_env.rs`**: pure resolution, `compose_path`, version parse/probe/cache, `strict_pane_extra_env`
   (+ tests); make `transfer::codex_command` `pub(crate)`.
7. **`fake.rs`** extension: `PaneGetScript`, `script_pane_get`, `repeat_pane_get`, `TimedOutLaunched`
   (+ a test of the fake itself, e.g. `script_pane_get_is_fifo_then_live`).
8. **`strict.rs`**: pure parts first (`strict_chain`, `StrictTiming`, `classify_observation`, `format_final_error`,
   `sanitize_ui_tokens`, `is_agent_start_timeout`) with their tests; then `spawn_agent_transfer_strict` and
   `finish_strict_launch`; the `HerdrBackend::spawn_agent` match; the trail buffer and
   `take_launch_trail`/`set_resume_record` overrides plus `ResumeStore::replace`; the `strict_*` end-to-end
   tests. Re-run step 2 tests.
9. **`host.rs`**: `spawn_pty(.., transfer)`, the `pty_spawn` argument, trail drain, `discover_for_launch` /
   `agent_launch_with_discovery` (+ tests).
10. **`transfer.rs`**: new `persist_worktree_cli` wrapper (needs 04's `resolve_project`; use a local shim with
    the same signature until 04 lands) and the `set_resume_record` call; hand the final outcome table to 01.
    `CodexEnv` is consumed by 01 from step 6 onward (a stub is acceptable for 01 until then).
11. **Grep gates before commit**: `git grep -n "AgentLaunch {" -- src-tauri` shows `policy` at every full
    literal; `git grep -n "build_agent_args(" -- src-tauri` only in `agent.rs` and Default callers;
    `git diff` shows no edits inside `spawn_agent_locked`, `spawn_agent_with`, `resume_locked`,
    `BackendResumeRunner`, `next_fallback`.

## Assumptions (explicit)

1. **herdr `agent` / `agent_session` latency is unobserved.** Nothing in this repo has measured how soon
   after `agent.start` returns `pane get` reports `agent` or `agent_session`, nor whether `agent_session.value`
   for `codex resume <id>` equals `<id>` (the fake models it that way, `fake.rs:883-898`). The 10 s verify
   window, the 45 s step cap and the 250 ms poll are guesses, chosen defensively and logged on every poll.
   The design is **unproven until a user smoke test** on the reporting machine; every observation goes to the
   trail so the smoke test can tune the constants.
2. `agent` is `None` once the agent process exits and the pane is a bare shell (documented at `cli.rs:28-30`,
   relied on by `observe`), so "seen then gone" is a valid vanish signal.
3. A Codex that rejects a flag or an unknown thread exits quickly: either `agent.start` returns
   `agent_start_failed` (early exit) or the agent vanishes shortly after. A Codex that stays up but never
   reports (herdr integration not installed for codex) is treated as "agent never appeared" only if `agent`
   itself is never reported **and** the pane's process info shows no non-shell foreground process; `agent`
   present with no session, or a foreground process without an `agent` label, is accepted as
   `PresentUnknown`.
4. herdr's `agent.start` error for a shell that is not at its prompt yet is not known (`mod.rs:105-110` says
   a gated test was meant to record it). Re-issuing on "any error that is not early-exit, timeout, not-found
   or exe-missing" is a deliberate superset.
5. The `codex --version` output contains a `major.minor.patch` token (Codex 0.166.0 reported).
6. `resume_sessions` has a unique key on `(project, worktree)` (the existing `ON CONFLICT` at
   `local_store.rs:253` relies on it).

## Risks and mitigations

- **herdr signal reliability.** Mitigation: `PresentUnknown` is success; every observation is logged and
  trailed; the macOS smoke test is the proof.
- **Regressing boot-resume and recovery.** Mitigation: opt-in `LaunchPolicy`, `Default` pinned by regression
  tests written first, no shared-chain edits, a copy (not a refactor) of the launch tail.
- **Orphaning a valid imported thread.** Mitigation: a timeout is not failure under the strict policy; 45 s
  step cap; `PresentUnknown` accepted.
- **Strict chain longer than the frontend backstop.** Resolved: each step's effective cap is
  `min(step_cap, chain_budget - elapsed)`, so the whole chain is bounded by `chain_budget` (100 s), strictly
  under the frontend's 120 s `restarting` backstop; the 180 s registry marker outlives both. Test
  `strict_chain_never_exceeds_budget` asserts total elapsed <= chain_budget + one poll.
- **Windows.** `exe`/`path` composition keeps the Windows process PATH plus exe dir; `.cmd` shims go through
  `codex_command`'s existing quoting; `unix_spawn_path` is unix-only and `CodexEnvProbe.base_path` abstracts it.
- **Login-shell cost.** `discover_cli_cached` avoids running the login shell per spawn; `None` is retried
  every 30 s at most.
- **Manual smoke owed on macOS** (proves this document): transfer an idle session; transfer with `codex`
  absent from the GUI PATH; transfer an already-imported session; transfer where Codex rejects the resume
  (rename the rollout) and confirm the fresh fallback and toast.

## Decisions this document depends on

- Launch changes are opt-in through `LaunchPolicy::TransferStrict`, so boot-resume and recovery are
  untouched; the strict policy applies to Codex launches only.
- Fresh travels as the existing `fresh` argument; never inferred from a cleared thread id (which means
  `resume --last`). The strict chain never contains `Continue` and does not consult `ResumeStore::resolve`.
- `Clear` is persisted through a new keitora-tasks writer that bypasses the registry merge.
- A timeout is not failure under the strict policy. No prompt-ready check, because herdr has no such
  signal; other early errors re-issue `agent.start` on the same pane up to twice (300 ms, 800 ms).
- The Imported path also overwrites the resume-store row (`ResumeStore::replace`); the Fresh path clears it
  (`ResumeStore::clear`).
- No version gating of launch flags: the optional flag is dropped on the last chain step.
- "Agent never appeared" is a **failure** (a Codex that exits before the first poll looks exactly like this,
  which is R3) **unless a non-shell foreground process exists in the pane** (`pane_process_info`), which is
  accepted as present-unknown; "agent present, session unreported" is also present-unknown.
- Environment resolution reuses `discover_cli`/`unix_spawn_path`/`pane_env`, adds a cached discovery, and the
  import child gets an explicit env; the pane only gets `CODEX_HOME` when the user set it.
- A failed import always switches to Codex; the Fresh fallback starts Codex blank.
- The reporter runs Codex 0.166.0 with no env overrides: PATH for the import child is the likely cause (R7),
  not `CODEX_HOME` drift.
- #29 is treated as a likely symptom of R3.
- Dependencies: Codex CLI ≥ 0.149 for the `importId` path (older uses the ledger fallback); no herdr changes.
  Touches `backend/{mod,fake}.rs`, `backend/herdr/{mod,strict (new),agent,resume,env,fake}.rs`, `host.rs`,
  `transfer.rs`, `codex_env.rs` (new), `crates/keitora-tasks/src/{config,local_store,lib}.rs`.
