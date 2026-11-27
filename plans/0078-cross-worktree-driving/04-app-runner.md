# 0078-04 app runner (`src-tauri`, window events)

Part of `plans/0078-cross-worktree-driving/`. See `00-overview.md` for goal and glossary.

**Dependencies:**
- 01 task store: `drive_requests` (claim/resolve/expire/purge), `drives` (incl. `paused`,
  `awaiting_turn`), `drive_prompts`, `session_replies`, and the `WorktreeSession` fields. This doc
  names the store calls by **role** (for example "begin a request"); 01 owns the exact method names.
- 02 CLI: defines the request kinds (`send|read|reset|swap`) and their payloads (restated below as the
  contract the runner parses).
- 05 UI: consumes the `drive://` events, the commands and the activity fields defined here.
- External: dispatch mailbox/watcher (`dispatch.rs`, `host.rs`, `bg_notifier.rs`), transfer engine
  (`transfer.rs`, `transfer_registry.rs`), the third-provider work (`agy`).
- 03 needs nothing from here: brokering, `answer`, release and take-over are DB-only (the hook polls the
  rows directly).

## Verified starting point (what exists today)

All of these were read in the tree; line numbers are as of this writing.

- `src-tauri/src/dispatch.rs`
  - `DispatchGate(tokio::sync::Mutex<()>)` (`:44`) serialises one apply at a time; it is `Default` and
    managed in `host.rs` `run()` (`.manage(DispatchGate::default())`).
  - `apply(...)` (`:282`) holds the gate for its whole body, calls `transfer_refusal` first (`:631`,
    text: ``a transfer is in progress for worktree `{wt}`; try again when it finishes``), reads the
    reconciled session state (`crate::tasks::reconciled_sessions`), then injects through
    `backend.write_line(key, line, submit_delay_ms)` (`inject_steps`, `:145`). `collapse` (`:185`) maps
    every control char to a space and collapses whitespace. `SUBMIT_DELAY` is 60 ms (`:176`).
  - `spawn_pending_requests` (`:534`) is the single hook point: `local.dispatch_requests_pending()` is a
    cheap indexed read; if non-empty it `tauri::async_runtime::spawn`s `run_dispatch_requests`.
  - `run_dispatch_requests` (`:556`) resolves the project config by **resolved name**
    (`UserConfig::project_config_by_name`), opens `LocalStore::open(db_path)`, expires pending rows older
    than `REQUEST_MAX_AGE_SECS = 600`, then for each pending row: CAS `dispatch_request_begin` (loses to
    a concurrent pass: `continue`), `apply`, `dispatch_request_resolve(id, status, reason)`. Statuses
    are `applied` / `refused`.
  - The loop is **sequential** (one request awaited before the next), not one task per request.
- Watchers. `host.rs::start_tasks_watcher` (`:2239`, calls `spawn_pending_requests` at `:2303` with the
  active project's resolved name) and `bg_notifier.rs::spawn_project_watcher` (`:226`, before the
  heartbeat skip). `bg_notifier::watched_projects` lists **every registered project including the
  active one**, so the active project's DB is watched by both and the runner can be entered twice for
  the same row; the CAS begin makes that safe. Both go through `db_watch::watch_dir` (any write to the
  `tasks.db*` family, 300 ms debounce, plus a 500 ms stop-poll; `on_event` runs synchronously on the
  watcher thread, so `on_event` must stay cheap).
- `host.rs`: `pty_paste` (`:1706`, backend `paste`), `pty_read_history` (`:1823`, backend
  `read_history(key, lines)`), `session_terminate` (`:1873`, `terminate_session`: sets the persisted
  flag, then `kill_live`) and `session_launch` (`:1929`). The terminated flag is read with
  `client.terminated_keys(&project)`.
- `LocalStore::open_read_only` (`local_store.rs:83`) does **not** create missing tables. Any new read on
  a read-only handle must treat a missing drive table as "no drives" (see Session rows and Activity).
- `LocalStore::session_revision` (`local_store.rs:208`) is the `sessions://changed` marker used by both
  watchers: `"{worktree}={state}:{rid};..."` (state + reconciled task id only).
- `activity/mod.rs`: `WorktreeActivity` (`:45`), `WtRef` (`:69`), `enumerate_worktrees` (`:130`),
  `ActivityState::compute` (`:168`), polled every 2.5 s by `useWorktreeActivity` for **all** registered
  projects.
- Frontend: `App.tsx` `bumpRestartNonce` (`:249`), `onTransfer` (`:643`), `runTransferEffectRef`
  (`:562`), `warm` registry (`WarmState.projects`, **keyed by the resolved `[project].name`**, see
  `lib/warmProjects.ts`; a project enters `warm.projects` only once it has been activated). Panes of every
  warm project stay mounted (`App.tsx:~1330`), keyed `${project}/${worktree}`, only for worktrees with
  `status === "available"`. `TerminalPane.tsx` restart effect (`:944-965`) honours a `RestartSignal`;
  `restartPlan` / `shouldHonourSignal` live in `lib/transferPhase.ts:353/370`; `nextRestartSignal` in
  `lib/resumeNotice.ts:83`. The pane's `spawn(fresh, via, transfer)` passes `transfer` to the backend as
  the **Codex strict-launch** flag (`host.rs launch_for_spawn`), so a drive restart must never set it.

## New module `src-tauri/src/drive.rs`

All new runner code lives here (not in `dispatch.rs`), to keep `dispatch.rs` focused on task dispatch.
`dispatch.rs` makes `collapse`, `SUBMIT_DELAY`, `transfer_refusal` and `request_refusal`'s unknown-worktree
wording `pub(crate)` for reuse, and `spawn_pending_requests` gains one more check:

```rust
let has_drive = local.drive_requests_pending().is_ok_and(|p| !p.is_empty());   // 01: pending list
if has_drive { tauri::async_runtime::spawn(crate::drive::run_drive_requests(app.clone(), project.to_string())); }
```

Both call sites (`host.rs:2303`, `bg_notifier.rs:226`) are unchanged: they already call
`spawn_pending_requests`.

`transfer.rs`: make `persist_worktree_cli` (`:2461`) `pub(crate)`; reset uses it to set and restore the
provider.

### Managed state `DriveRuntime` (register with `.manage(DriveRuntime::default())` in `host.rs run()`)

```rust
#[derive(Default)]
pub struct DriveRuntime {
    /// Resolved project names the window reports as open (warm). `None` until the first report.
    open: Mutex<Option<HashSet<String>>>,
    /// reset/swap requests awaiting the window: (project, request_id) -> report channel.
    waiters: Mutex<HashMap<(String, i64), tokio::sync::mpsc::UnboundedSender<DriveReport>>>,
    /// (project, worktree) with a reset/swap in flight; a second one is refused.
    in_flight: Mutex<HashSet<(String, String)>>,
}
enum DriveReport { Accepted, Applied(String), Refused(String) }
```

Use `crate::activity::lock` style poison-recovering locks (`unwrap_or_else(PoisonError::into_inner)`).

## Which projects can be served

A request is served only when its project is **registered** (`project_config_by_name`) **and open**
(warm in the window). "Active vs background" does not matter: the active project's watcher and every
registered project's background watcher both enter the runner, and the window's panes for every warm
project are mounted. The runner never switches the human's active project, and the window handlers
never call `setSelected`/`setView` or the pending-intent switch path (see Window listener).

How the backend learns which projects are open: the window pushes the list.
- Command `drive_set_open_projects(projects: Vec<String>) -> ()`; the frontend calls it whenever the
  sorted key list of `warm.projects` changes (a `useEffect` in `App.tsx`; resolved names). It stores
  the set in `DriveRuntime.open`. When the stored set changes (or is first set), it re-runs
  `spawn_pending_requests` for every registered project (open a `LocalStore`, call it), so requests
  that arrived before the window reported are served.
- Until the first report (`open == None`) the runner **leaves requests pending** (does not begin
  them); the 10 minute expiry still applies. Once reported, a project not in the set is refused with
  `project not open in Keitora` (no retry: refusal is final).

## Request kinds and payloads (contract with 02)

`drive_requests.kind` and `payload` (JSON). The runner parses with serde; a payload that fails to
parse is refused with `bad request payload: {serde error}`.

| kind | payload | result on `applied` |
|---|---|---|
| `send` | `{"text": string}` (the `queue` flag is the row's `queue` column) | `{"awaiting_turn": N}` |
| `read` | `{"lines": u32}` (default 200, clamp 1..=2000) | the history text, last 64 KB |
| `reset` | `{"to": string}` or `{}` (free-text provider name, validated here) | `{"provider": "<provider the pane now runs>"}` |
| `swap` | `{"to": string}` (required) | `{"provider": "<provider the pane now runs>"}` |

The row also has `worktree` (the driven one), `requested_by` (the driver worktree name), `queue` and `id`
(01).

## Runner loop

`pub(crate) async fn run_drive_requests<R: Runtime>(app, project: String)`

1. Get managed state (`SharedBackend`, `DispatchGate`, `TransferRegistry`, `DriveRuntime`); if any is
   missing return (the next DB event retries), same as `run_dispatch_requests`.
2. Resolve `config = UserConfig::load()?.project_config_by_name(&project)`; none: log warn, return.
3. `local = LocalStore::open(db_path)`; error: log warn, return.
4. Housekeeping once per pass, errors logged and ignored: expire pending rows older than
   `REQUEST_MAX_AGE_SECS` (01: expire); refuse rows stuck `running`/`pending_confirm` that this
   process is not tracking (not in `in_flight`) and older than 10 minutes, reason
   `Keitora restarted before this finished` (01: `drive_requests_in_flight`); purge `drive_requests` and decided `drive_prompts` older than 24 h (01:
   purge).
5. If `open == None`: return (leave pending). Otherwise `open_set = open.clone()`.
6. `for req in local.<pending list>()` (oldest first):
   1. Pre-begin peek, for one case only: a `send` row whose `queue` column is set. Parse the payload and
      read the target's reconciled state exactly as `send` step 3 does (under the `DispatchGate`,
      released right after). If the state is not `idle` and the row is not expired, `continue` **without
      begin** and without resolving, so the row stays `pending` and is retried on the next DB change (the
      hook's `idle` write is itself a DB change, so a queued send fires promptly). Every other case,
      including every refusal, goes through begin first so the row always ends in a final status.
   2. CAS begin (`Ok(false)` or `Err`: `continue`).
   3. Run the kind's body. `send` and `read` run inline in this loop (`await`ed, each bounded by the
      backend write/read timeout) so sends to one worktree keep their request order. `reset` and `swap`
      are `tauri::async_runtime::spawn`ed as their own task, so a long restart never blocks `send`.
   4. Resolve the row exactly once with `applied` or `refused` plus the reason/result. If resolving
      fails, log warn (the expiry pass will not touch a resolved-less running row for 10 minutes, then
      step 4 above refuses it).

### Common gates (all kinds, applied after begin, first match refuses)

Reasons are the exact strings stored in the row's `reason`.

| # | Check | Refusal reason |
|---|---|---|
| 1 | project not in `open_set` | `project not open in Keitora` |
| 2 | `config.worktrees()` non-empty and does not contain the worktree | ``unknown worktree `{wt}`; expected one of: {a, b}`` (same wording as `request_refusal`) |
| 3 | no `drives` row for `{wt}`, or its driver != `requested_by` | `@{wt} is not driven by @{requested_by}` |
| 4 | drive row `paused` (kinds `send`, `reset`, `swap`; **not** `read`) | `drive of @{wt} is paused; resume it before sending` |
| 5 | `registry.is_transferring(project, wt)` (kinds `send`, `reset`, `swap`; **not** `read`) | `transfer_refusal` text (above) |
| 6 | session terminated: `client.terminated_keys(&project).contains(wt)` (`send`, `reset`, `swap`, `read`) | `@{wt} is terminated; launch it from the rail first` |

For `reset`/`swap` additionally: `(project, wt)` already in `in_flight`: `another reset or swap is in
progress for @{wt}`. Insert on entry, remove (RAII guard) on every exit.

### `send`

Parse `{text}`; `queue` is the row's column. Then, in order:

1. Sanitise: normalise `\r\n` and `\r` to `\n`. Reject empty (after trim): `empty prompt`. Reject more than
   32 KiB of bytes: `prompt too long (max 32 KiB)`.
   - No `\n` in the text: `line = collapse(text)`; if `line` is empty after collapse refuse `empty
     prompt`.
   - Contains `\n`: `body = text` with every control char removed **except** `\n` (tab becomes a space),
     trailing whitespace trimmed.
2. Acquire `DispatchGate` (held to the end of this request).
3. Read the reconciled state: `live = client.live_set_snapshot()`, `bg = background_set(...)`,
   `reconciled_sessions(&live, &bg, &project, &config)`, the target's `state` (`"offline"` when no row).
4. Not `idle`:
   - A `queue` row that was not idle never reaches here (the loop's pre-begin peek keeps it
     pending). If it reaches here the state changed between the peek and now; treat it like any other.
   - Refuse by state: `busy`: `@{wt} is busy; send only goes to an idle session (use --queue
     to wait for it)`; `waiting`: `@{wt} is waiting on a prompt; answer it first (or use --queue)`;
     `offline`: `@{wt} is offline; start its session first`.
5. Idle: `key = SessionKey{project, worktree}`. Read the current turn from `session_replies`
   (role: "turn of worktree", 0 if none). Set `drives.awaiting_turn = turn + 1` **before** writing (01:
   `drive_set_awaiting_turn`), so a very fast Stop cannot be missed.
6. Write:
   - single line: `backend.write_line(&key, &line, SUBMIT_DELAY.as_millis() as u32)`.
   - multi-line: `backend.paste(&key, &body)`, `std::thread::sleep(SUBMIT_DELAY)`, then
     `backend.write_line(&key, "", submit_delay)` (an empty line is a lone Enter). Stop at the first
     failure.
7. Map the result with `map_write_line_result` (`Dispatched` / `Queued` / `Error`):
   - `Dispatched`: resolve `applied`, result `{"awaiting_turn": N}`. (After a multi-line paste whose
     Enter failed: refuse `pasted but could not submit; the text is in @{wt}'s input box` and leave
     `awaiting_turn` as set; the driver can `read`.)
   - `Queued`/`Error`: restore `awaiting_turn` to its previous value (01: `drive_restore_awaiting_turn`, previous
     or `None`) and refuse ``could not send to @{wt} ({detail})`` where detail is
     `injection_detail(&result, terminated)`.

No `dispatch_log`, no task reserve: `send` is free text and is not a board dispatch.

### `read`

Common gates 1, 2, 3, 6 only (reads are allowed while paused or mid-transfer). Then
`client.read_history(&key, lines)`:
- `Ok(text)`: keep at most the **last** 64 KiB (cut at a char boundary; if cut, prefix
  `[... earlier output truncated ...]\n`, and the marker counts inside the 64 KiB cap); resolve `applied` with the text as `result`.
- `Err(e)`: refuse ``could not read @{wt}: {daemon_err(e)}``.
No `DispatchGate`, no idle requirement.

### `reset`

Runs in its own task. After the common gates and the `in_flight` guard:

1. `to`: parse into `keitora_tasks::CliKind` (`serde` name). Unknown: refuse ``unknown provider `{x}`;
   expected one of: {all CliKind names}`` (build the list from the enum, never hard-code two, so `agy`
   slots in). `None` means "keep the current provider".
2. `previous = config.worktree_entries()` entry's `cli` resolved the same way `apply` does
   (`keitora_tasks::resolve_cli(entry, &UserConfig::load())`). If `to` is `Some(c)` and `c != previous`:
   `persist_worktree_cli(&config, wt, c, FieldIntent::Keep, FieldIntent::Keep)`; error: refuse
   ``could not switch @{wt} to {c}: {e}``. Then `app.emit("worktrees://changed", ())`.
3. Create the waiter channel for `(project, req.id)` and register it in `DriveRuntime.waiters`
   **before** emitting. Emit `drive://restart` (payload below).
4. `tokio::time::timeout(DRIVE_RESET_TIMEOUT = 120 s, rx.recv())`:
   - `Applied(_)`: clear `drives.awaiting_turn` (the old turn is gone; 01: `drive_restore_awaiting_turn(.., None)`),
     resolve `applied`, result `{"provider": "<provider now running>"}`.
   - `Refused(reason)` or timeout (`the Keitora window did not restart @{wt} within 120 s`) or channel
     closed: **rollback** if step 2 changed the provider: `persist_worktree_cli(&config, wt, previous,
     Keep, Keep)` and `worktrees://changed`; resolve `refused` with the reason (prefix
     `reset failed: `; when rolled back, append ` (provider restored to {previous})`).
5. Remove the waiter in every exit path.

The window decides when the spawn settled; the backend never spawns the pane itself (a restart
always goes through the mounted pane, so it knows its measured size and never races a backend
same-key respawn).

### `wake`

Its own kind: `keitora wake` files `kind = "wake"` with payload `{"fresh": bool, "to"?: provider}`.
Filing it as a `reset` with `"mode": "wake"` (an earlier draft) made an app that predates wakes run
it as a reset and kill a running agent; an unknown kind is refused instead. `init_schema` rebuilds a
`drive_requests` table whose `kind` CHECK lacks `'wake'` (copy rows, keep the AUTOINCREMENT counter,
recreate the index, in one transaction). The runner still takes a `reset` with `"mode": "wake"` from
an older CLI as a wake; the store no longer accepts one. The terminated gate doesn't apply to a wake. A target that is awake
(live pane, not terminated) is not touched: applied `{"provider", "already_awake": true}`, or refused
when `to` names a provider it isn't on (that is a reset). Otherwise the reset flow runs (provider
persist + rollback, prepare) and emits `drive://restart` with `mode: "wake"` and `fresh`; applied
`{"provider", "already_awake": false}`, and `awaiting_turn` is cleared (the dead session's turn is gone).
The event carries `replace` (a reset, or a wake that is `fresh` or switched provider): the pane must
then start a new session rather than keep or resume the one it holds.

`send` carries `"wake": bool` (absent = false, for rows from an older CLI). A waking send whose target
isn't awake and passes every other gate starts a wake job keyed by the send's own request id (the
window reports the launch for that id), and stays pending; from then on it is treated as queued
(delivered once the session is idle). It waits while any reset/swap/wake holds the worktree (a wait,
not a wake of its own: if the slot frees with the target still down, it wakes the target itself),
gives the live set `send_settle` after its own wake to show the session, and is refused when the
agent is still down after that, or when the wake fails (`could not wake @bravo: ...`). A pass is
scheduled for just after `send_settle` (once per request and deadline), since an agent that never
comes up writes nothing to trigger one.

The window's pane handles a wake by status: terminated -> launch (`session_launch`, which clears the
mark), ended/lost/spawn-failed -> spawn, suspended -> recovery focus, running/attaching/recovering ->
nothing (applied), herdr down -> refused. A `replace` wake never keeps or resumes a session: suspended
-> restart, running/attaching/detached-alive/recovering -> refused (`@bravo is attaching; retry the
wake`), so the runner rolls a switched provider back.

### `swap`

Runs in its own task. After the common gates and the `in_flight` guard:

1. `to` required, must parse as a `CliKind`; the current provider must be `claude` or `codex` and `to`
   the other one. Otherwise refuse: `to == current`: `@{wt} is already on {to}`; either side outside
   claude/codex: `swap supports only claude and codex; use reset --to {to} instead`.
2. Register the waiter. Move the row to `pending_confirm` (01: `drive_request_await_confirm`).
   Emit `drive://swap` (payload below).
3. Wait for the window (`tokio::time::timeout`):
   - confirm phase, `SWAP_CONFIRM_TIMEOUT = 300 s`: expect `Accepted`; `Refused(r)`: resolve `refused`
     with `r` (a cancelled confirm reports `swap cancelled in Keitora`); timeout: refuse `swap was not
     confirmed in Keitora within 5 minutes`.
   - on `Accepted`: move the row back to `running` (01: `drive_request_resume`). Run
     phase, `SWAP_RUN_TIMEOUT = 420 s` (the transfer's own 120 s restart backstop plus import): expect
     `Applied(msg)`: clear `awaiting_turn`, resolve `applied`, result `{"provider": "{to}"}`; `Refused(msg)`
     or timeout (`the transfer did not finish within 7 minutes; check @{wt}'s pane`): resolve
     `refused`.
4. No provider rollback here: the transfer engine owns its persistence and `transfer_worktree` returns
   `Err` only when nothing changed.

### Events (backend -> window)

Payloads are `serde` structs with **snake_case** keys, like the other events (`fell_back`, ...).

| event | payload |
|---|---|
| `drive://restart` | `{ "request_id": i64, "project": string, "worktree": string, "driver": string }` (always a fresh start; the provider is already persisted) |
| `drive://swap` | `{ "request_id": i64, "project": string, "worktree": string, "driver": string, "to": "claude"\|"codex" }` |

`project` is the resolved `[project].name` (equal to the `warm.projects` key and the session
namespace).

## Tauri commands

Register all in `host.rs` `generate_handler![...]` next to `crate::transfer::*`. All are
`#[tauri::command(async)]` (blocking SQLite on the async pool, like `sessions_list`). All resolve the
project with `UserConfig::load()?.project_config_by_name(&project)` and open
`LocalStore::open(db_path)`; unknown project: `Err("project {project} is not registered")`. They work for
any registered project (active or not) and for a closed window's DB.

| command | params | returns | effect |
|---|---|---|---|
| `drives_list` | `project: String` | `Vec<DriveView>` | the project's drives, each with its pending prompts (01: `drives_list` plus `drive_prompts_pending`, which applies the stale rule) |
| `drive_release` | `project, driven: String` | `()` | end the binding (01: `drive_end`; escalates pending prompts with `decided_by = system`). Err when not driven |
| `drive_take_over` | `project, driven: String` | `u32` (prompts escalated) | (01: `drive_take_over`) mark the worktree's pending prompts `escalated` with `decided_by = human`; binding stays. `0` is not an error |
| `drive_pause` | `project, driven: String, paused: bool` | `()` | set/clear `paused` (01: `drive_set_paused`; pausing escalates pending prompts) |
| `drive_request_resolve` | `project: String, request_id: i64, outcome: String, reason: String` | `()` | forward a window report to the runner (see below) |
| `drive_set_open_projects` | `projects: Vec<String>` | `()` | see Which projects can be served |

```rust
#[derive(Serialize)]
pub struct DriveView {
    pub driven: String, pub driver: String, pub since: String,
    pub forced_from: Option<String>, pub paused: bool,
    pub escalate_after_s: i64, pub awaiting_turn: Option<i64>,
    pub pending: Vec<PendingPromptView>,
}
#[derive(Serialize)]
pub struct PendingPromptView {
    pub id: i64, pub kind: String /* permission|question */, pub tool_name: String,
    pub summary: String, pub created_at: String,
}
```

`drive_request_resolve` (`outcome` is one of `accepted`, `applied`, `refused`; anything else:
`Err("outcome must be accepted, applied or refused")`): look up `(project, request_id)` in
`DriveRuntime.waiters`; none: `Err("request is no longer waiting for Keitora")`; otherwise send the
matching `DriveReport` (`accepted` only makes sense for `swap`; for a reset it is ignored with an
`Err("only a swap is confirmed")`). It does **not** write the row: the runner does, so a request is
resolved in exactly one place. It returns `Ok(())` once the report is queued.

Take over / release / pause write the same tables the CLI does (01) and need no waiter, no window and
no `DriveRuntime`. Take over: the blocked hook then returns and the native dialog appears. Pause keeps
the binding (see 01).

Frontend wrappers (all in `src/lib/ipc.ts`, using the existing `invoke` + `listen` style; types mirror
the Rust structs, snake_case from the wire mapped to camelCase only where `ipc.ts` already does so; the
drive types keep the wire's snake_case names because they are passed straight through):

```ts
export interface PendingPrompt { id: number; kind: "permission"|"question"; tool_name: string; summary: string; created_at: string }
export interface DriveView { driven: string; driver: string; since: string; forced_from: string|null; paused: boolean; escalate_after_s: number; awaiting_turn: number|null; pending: PendingPrompt[] }
export const drivesList = (project: string) => invoke<DriveView[]>("drives_list", { project });
export const driveRelease = (project: string, driven: string) => invoke<void>("drive_release", { project, driven });
export const driveTakeOver = (project: string, driven: string) => invoke<number>("drive_take_over", { project, driven });
export const drivePause = (project: string, driven: string, paused: boolean) => invoke<void>("drive_pause", { project, driven, paused });
export type DriveOutcome = "accepted" | "applied" | "refused";
export const driveRequestResolve = (project: string, requestId: number, outcome: DriveOutcome, reason = "") => invoke<void>("drive_request_resolve", { project, requestId, outcome, reason });
export const driveSetOpenProjects = (projects: string[]) => invoke<void>("drive_set_open_projects", { projects });
export interface DriveRestartEvent { request_id: number; project: string; worktree: string; driver: string }
export interface DriveSwapEvent extends DriveRestartEvent { to: CliKind }
export const onDriveRestart = (cb: (e: DriveRestartEvent) => void) => listen<DriveRestartEvent>("drive://restart", (ev) => cb(ev.payload));
export const onDriveSwap = (cb: (e: DriveSwapEvent) => void) => listen<DriveSwapEvent>("drive://swap", (ev) => cb(ev.payload));
```

(Tauri maps a Rust `request_id` parameter to the JS key `requestId`; `project`/`driven` are unchanged.)

## Window listener (frontend)

`App.tsx` subscribes to both events in one mount-only `useEffect` (cancel/unlisten pattern as for
`onTransferProgress`, `App.tsx:~599`) and publishes the open set in a second effect:

```ts
useEffect(() => { void driveSetOpenProjects(openKeys).catch(() => {}); }, [openKeys]); // openKeys = sorted Object.keys(warm.projects).join("\n"), split back
```

Handlers read live state through refs (`warmRef`, `transferCtl.getMap()`), never closures. **Neither handler
calls `setSelected`, `setView`, `setPendingIntent` or switches project.**

Common precheck in both handlers (any failure: `driveRequestResolve(project, request_id, "refused", reason)`
and return):
- `warmRef.current.projects[project]` missing: `project not open in Keitora`.
- the worktree is not in that project's warm `worktrees` with `status === "available"`: ``@{wt} has no pane
  in Keitora``.
- `transferCtl.getMap().has(transferKey(project, worktree))`: `a transfer is in progress for @{wt}`.

### `drive://restart` (reset)

1. `driveRestartsRef.current.set(transferKey(project, worktree), request_id)`.
2. `bumpRestartNonce(worktree, project, /*fresh*/ true, /*transfer*/ false, /*driveRequest*/ request_id)`.
   `bumpRestartNonce` (`App.tsx:249`) gains a fifth parameter `driveRequest: number | null = null`
   passed to `nextRestartSignal`.
3. `RestartSignal` (`lib/resumeNotice.ts`) gains `driveRequest: number | null`;
   `nextRestartSignal(prev, fresh, transfer = false, driveRequest: number | null = null)` returns
   `{ n, fresh, transfer, driveRequest }`. `App.tsx` passes the pane
   `restartDriveRequest={restartNonces.get(key)?.driveRequest ?? null}` and
   `onDriveSpawnSettled={(id, r) => settleDriveRestart(wp.summary.name, w.name, id, r)}`.
4. `TerminalPane` restart effect (`:944`): in addition to `restartTransfer`, read
   `restartDriveRequest`. New branch order:
   - signal ignored by `shouldHonourSignal` (a transfer owns the pane) or the pane is terminated: if
     `restartDriveRequest != null` call `onDriveSpawnSettled(id, { ok:false, error })` with `a transfer is
     in progress for @{wt}` / `The session is terminated.`, then return.
   - `restartPlan({ fresh, transfer, drive: restartDriveRequest != null }, status)`: when `drive` is true
     `confirm` is false and the result carries `drive: true`. `restartPlan`'s return type gains
     `drive: boolean` (update the four `toEqual` expectations in `transferPhase.test.ts` to include
     `drive: false`).
   - `plan.drive`: `void restart(plan.fresh).then(r => onDriveSpawnSettled?.(id, r)).catch(e =>
     onDriveSpawnSettled?.(id, { ok:false, error:String(e) }))`. **`restart(fresh)` is called without
     `{ transfer: true }`**: that flag selects the Codex strict launch and does not apply to a fresh reset.
5. `settleDriveRestart(project, worktree, id, r)`: delete the `driveRestartsRef` entry, then
   `driveRequestResolve(project, id, r.ok ? "applied" : "refused", r.ok ? "" : r.error).catch(() => {})`.
   A rejected resolve (the runner already timed out) is ignored silently.

### `drive://swap`

1. Run the precheck, then call `onTransfer(worktree, to, { project, drive: { requestId: request_id,
   driver } })`. `onTransfer` (`App.tsx:643`) gains an optional third parameter `opts?: { project?: string;
   drive?: { requestId: number; driver: string } }`:
   - `project` defaults to `warmRef.current.active`; it is captured as today.
   - **Driven mode** (`opts.drive` set): skip `setSelected`/`setView`. Skip the mid-turn note unless
     `project` equals the active project (the live-session map is the active project's only). Prefix the
     confirm body with ``{driver} asked to transfer {worktree} in {project} to {Codex|Claude}.\n\n``. Keep
     the existing title, labels and `danger` flag.
   - Cancelled confirm (`!ok`), a failing preview (`showToast(String(e))` path) or a failed
     `shouldStartTransfer` re-check each additionally call `driveRequestResolve(project, requestId,
     "refused", "swap cancelled in Keitora" | String(e) | "a transfer is already in progress")`.
   - After the confirm returns true, call `driveRequestResolve(project, requestId, "accepted")`. If it
     rejects (the runner timed out while the dialog was open) show `showToast("That swap request already
     expired.")` and **return without starting the transfer**.
   - Record `driveSwapsRef.current.set(key, requestId)` right before `transferCtl.dispatch({ type: "begin" })`.
2. Reporting the outcome: `TransferEffect` (`lib/transferPhase.ts`) gains
   `{ kind: "report"; project: string; worktree: string; ok: boolean; message: string }`, emitted **by
   `finish()`** (every terminal path goes through it) after the toast: `ok = true` only from the
   `spawn_settled` success branch (message = `resultToast(e)`); `false` for `failed`, `cancelled`,
   `spawn_settled` failure and `backstop` (message = the same text the toast shows). In
   `runTransferEffectRef`, `case "report"`: take and delete `driveSwapsRef` for the key; if present
   call `driveRequestResolve(project, id, ok ? "applied" : "refused", message)`. Non-driven transfers
   have no entry and ignore the effect. The reducer's existing tests that assert `effects` with
   `toEqual` must add the `report` effect.

`useCallback` deps for `onTransfer` stay `[showToast, confirmDialog, transferCtl, subscribeResume]`;
the new refs are stable.

## Session rows and revision

### `WorktreeSession` and `sessions_list`

`WorktreeSession` gains `driven_by: Option<String>`, `brokered: bool`, `escalated_reason:
Option<String>` (01 defines them, all `#[serde(default)]`; 01 fills them in
`LocalStore::sessions_list`). 04's requirements on that fill:
- `sessions_list` is called with **read-only handles** too (`projects_overview` goes through
  `reconciled_sessions_in` with `open_read_only`, `host.rs:225-250`, which creates no tables). The drive lookups must therefore treat a missing drive table as "no drive" (defaults), never an
  error, or the whole overview would blank for an old-schema DB. 01 owns this (its `drive_tables_exist` check in `sessions_list`, `session_revision` and the drive read methods, with the test `read_paths_tolerate_missing_drive_tables`).
- `apply_reconcile` (`backend/mod.rs:792`) clones sessions and only edits `state`/`current_task_id`/`pid`,
  so the new fields pass through unchanged. When it downgrades a session to `offline` it must also set
  `brokered = false` and `escalated_reason = None` (an offline session is not waiting on anything);
  `driven_by` stays.
- TS mirror (`src/lib/sessions.ts`): `driven_by?: string | null; brokered?: boolean; escalated_reason?:
  string | null` (optional, so existing fixtures stay valid; absent means false/null).

### `session_revision` change

`LocalStore::session_revision` (`local_store.rs:208`) must change when a drive field a session row
carries changes. New per-row suffix, added inside the existing subquery, appended after `rid`:

```
|| ':' || COALESCE((SELECT d.driver FROM drives d WHERE d.driven = s.worktree), '')
|| ':' || (EXISTS (SELECT 1 FROM drive_prompts p WHERE p.driven = s.worktree AND p.status = 'pending'))
|| ':' || COALESCE((SELECT MAX(p.id) FROM drive_prompts p WHERE p.driven = s.worktree), 0)
```

i.e. driver, whether a pending prompt exists, and the newest prompt id (so a new prompt, and the
pending -> escalated flip, both change the string). The row format becomes
`"{worktree}={state}:{rid}:{driver}:{brokered 0|1}:{max_prompt_id};..."`. It stays free of heartbeat and of
prompt `decided_at`, so heartbeats still do not fire. The pending flag uses the raw `status = 'pending'` (no stale predicate): a stale row is rewritten to `expired` by the
expiry pass, which flips the flag and the revision. If the drive tables do not exist (old schema on a
read-only handle) 01 returns the old string (see its `drive_tables_exist`).
01 owns the SQL; 04 requires the behaviour. Both watchers already call `session_revision`, and the
background watcher's heartbeat skip (`bg_notifier.rs:229`) uses the same value, so a brokered/escalated
flip also re-runs the attention pass there (05).

Consequence: each decision produces one `sessions://changed`, which is intended. The existing tests
`session_revision_*` in `local_store.rs` keep passing; add the new ones below.

## Activity

### `WorktreeActivity` fields

```rust
pub struct WorktreeActivity {
    // ... existing fields ...
    /// The driver worktree when this worktree is driven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driven_by: Option<String>,
    /// Worktrees this worktree drives (names, sorted); empty when it drives none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub drives: Vec<String>,
    /// Pending, undecided brokered prompts for this worktree (0 when not driven).
    pub pending_prompts: u32,
    /// This worktree's drive is paused.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub drive_paused: bool,
}
```

TS mirror in `src/lib/ipc.ts` (`WorktreeActivity` interface, raw interface and
`normalizeWorktreeActivity`; interface at `ipc.ts:357`):

```ts
// WorktreeActivity
drivenBy: string | null; drives: string[]; pendingPrompts: number; drivePaused: boolean;
// RawWorktreeActivity
driven_by?: string; drives?: string[]; pending_prompts?: number; drive_paused?: boolean;
// normalize
drivenBy: raw.driven_by ?? null, drives: raw.drives ?? [], pendingPrompts: raw.pending_prompts ?? 0, drivePaused: raw.drive_paused ?? false,
```

### Filling them in `compute()`

- `WtRef` gains `db_path: PathBuf` (the project's `pc.tasks.db_path`), set in `enumerate_worktrees`.
- In `compute()`, after `prepared` is built and with **no activity lock held**, group worktrees by
  `db_path` (several projects never share a DB; worktrees of one project do). For each distinct
  `db_path` open `LocalStore::open_read_only(path)` per tick (cheap, no cache, so no `Sync` concerns);
  `Err` (missing DB/locked): that project has no drive info this tick. Read once per project: drives
  drives and pending-prompt counts per driven worktree (01: `drives_list` and `drive_pending_prompt_counts`, which applies
  the stale rule). A missing drive table means "no drives" (01 tolerates it on read-only handles).
- Per worktree: `driven_by` = the drive's driver when `driven == worktree`; `drives` = sorted driven
  names where `driver == worktree`; `pending_prompts` = pending count for `worktree`; `drive_paused` =
  the worktree's own drive row's `paused`.
- Cost: one read-only open and two small queries per project per 2.5 s tick. This feeds rail badges for
  every project, active or not.

## Data flow and churn

- **Attention data flow.** Event-driven for the active project and background watchers (session rows
  carry `driven_by`, `brokered`, `escalated_reason`); the 2.5 s activity poll carries `driven_by`,
  `drives`, `pending_prompts`, `drive_paused` for every project. They can disagree for up to one poll
  interval, which is harmless.
- **Churn.** `db_watch::watch_dir` fires on any change to the `tasks.db*` family (including `-wal` and
  `-shm`), debounced 300 ms, and each pass runs a few cheap reads and emits only when a revision
  changes. `on_event` now also does one `drive_requests_pending` read. Hook and `wait` writes are one
  insert and one status update per prompt plus the state updates the hooks already make. The runner
  itself runs off the watcher thread (`spawn`), so a slow reset never blocks `watch_dir`.
- **Retention.** The runner's housekeeping step (above) purges `drive_requests` and decided
  `drive_prompts` older than 24 h (01 defines the rule).

## Scope limit

v1 drives only worktrees of a project that is open (warm) in the Keitora window, for all four request
kinds (decision below). `answer`, `wait`, `prompts`, `drives` are DB-only and work with the window
closed; a request made while the window is closed stays pending and expires (10 min) with
`expired: no running Keitora app picked it up in time`.

Why the transfer controller stays in the frontend: today the restart, handoff-summary injection and
`transfer_settled` steps of a swap live in `App.tsx` (`onTransfer` and the transfer reducer).
Moving them into the backend would mean rewriting the transfer phase machine and the pane restart
contract (restarts must never be a backend same-key respawn). That is large and risky. Driving the
existing controller through `drive://swap` / `drive://restart` costs about 100 lines. The price is
that `reset` and `swap` need the project open in the window.

## Third provider

`swap --to` and provider-keyed maps must not assume two providers
(`src-tauri/src/backend/herdr/agent.rs:174`, `src/components/Rail.tsx:393`). `reset --to` already takes
any `CliKind` and builds its error list from the enum; `swap` is explicitly claude <-> codex and says so
in its refusal. Coordinate at merge with the `agy` work.

## Tests (`#[cfg(test)]` in the same change; `cargo test --workspace` and `yarn test` stay green)

Rust, `drive.rs` (pure helpers and `FakeBackend`, `src-tauri/src/backend/fake.rs`; no live herdr):
- payload parsing: bad JSON refused with `bad request payload`; `lines` clamp; `to` unknown lists all
  `CliKind` names.
- `send`: idle writes one line (collapse applied: embedded newline/ESC in a single-line prompt cannot
  submit early); multi-line goes `paste` then an empty `write_line`; busy/waiting/offline refusal texts;
  `queue` + busy stays pending (row not begun) and is applied after the state turns idle; expiry
  refuses it; `awaiting_turn` set before write and restored on a failed write; empty and over-32 KiB
  refused; a paused drive, a transfer in flight, a terminated worktree and "not driven by requester"
  each refuse with the exact reason.
- `read`: 64 KiB tail cap on a char boundary; allowed while paused and during a transfer; refused when
  terminated.
- `reset`: refused while a transfer is in flight or terminated; `--to` persists then emits
  `worktrees://changed` then `drive://restart`; window `Refused` and timeout roll the provider back and
  refuse with the reason; `Applied` clears `awaiting_turn`; a second reset for the same worktree is
  refused; no `--to` leaves the provider untouched.
- `swap`: pending_confirm while waiting; `Refused` (cancel) refuses; confirm timeout refuses;
  `Accepted` then `Applied` resolves; unsupported pair refused; `to == current` refused.
- project-not-open refusal and "leave pending until the first open-set report"; `drive_set_open_projects`
  re-triggers pending requests.
- `drive_request_resolve`: unknown waiter errors; bad outcome errors; reaches the waiting runner.
- Commands: `drive_take_over` returns the escalated count and marks `decided_by = human`; `drive_release`
  and `drive_pause` escalate pending prompts (01 covers the store behaviour; here the command wiring).
- `activity`: `WorktreeActivity` drive fields for a bound pair, a paused drive, a project with a
  missing/old DB (defaults, no panic), and two projects with a same-named worktree (no cross-talk).
- `session_revision` (in `crates/keitora-tasks`): changes when a worktree becomes driven, when a prompt
  becomes pending, when it flips to escalated, and when a new prompt is added; **unchanged** by a
  heartbeat; unchanged format fallback on an old-schema read-only handle; `sessions_list` on a read-only
  handle without drive tables returns defaults.

Frontend (vitest, `yarn test`): `restartPlan` drive flavour (no confirm, `drive: true`, fresh honoured);
`nextRestartSignal` carries `driveRequest`; `transferReducer` emits `report` with `ok` true only for a
successful spawn and false for failed/cancelled/spawn-failed/backstop; the `drive://` handler logic
extracted to a pure `lib/driveEvents.ts` (precheck: project not warm, worktree without a pane,
transfer in flight, each returning the exact refusal reason) with tests, and a test that it never
touches selection/view/active project; `normalizeWorktreeActivity` defaults for absent drive fields.

## Singletonize decisions

1. **All four kinds require an open project.** `send`/`read` technically only need a live herdr
   session, but the overview scopes v1 to open projects, and one uniform refusal is simpler to explain and
   test. Relaxing `send`/`read` later means dropping gate 1 for those two kinds.
2. **How the backend knows "open":** the window pushes the warm project list
   (`drive_set_open_projects`) because the backend cannot see the frontend's warm registry. Requests
   wait (stay pending) until the first report so a slow window boot does not refuse them.
3. **New module `drive.rs`** rather than growing `dispatch.rs`; only a few helpers there become
   `pub(crate)` and `spawn_pending_requests` gains one check.
4. **`send`/`read` run inline, `reset`/`swap` as spawned tasks.** The plan said "each request in its
   own task"; spawning every send would lose request ordering to one worktree, so only the two long
   kinds are spawned.
5. **The runner resolves rows; `drive_request_resolve` only forwards** (a waiter channel per request).
   One writer per row, and the timeout/rollback logic lives next to the code that made the change.
6. **Timeouts:** reset 120 s (matches `RESTARTING_BACKSTOP_MS`), swap confirm 300 s, swap run 420 s.
   Stale in-flight rows are refused by the next pass after 10 minutes.
7. **Reset provider rollback** is the backend's job (it persisted the change), using
   `persist_worktree_cli` with `FieldIntent::Keep` for both session ids; the fresh spawn ignores stored
   resume ids anyway.
8. **Reset and swap clear `awaiting_turn`** on success (the old turn no longer exists). 01 must allow
   setting it to none.
9. **Reset does not require idle** (the driver wants a fresh conversation, possibly mid-turn); it only
   refuses terminated sessions, transfers and paused drives. Not gated by `DispatchGate`.
10. **Multi-line `send`** is `paste` + empty `write_line`, with the sanitising rules above (keep `\n`,
    drop other controls, 32 KiB cap); single-line stays on `collapse`.
11. **Drive restart is not a transfer restart:** a new `driveRequest` field on `RestartSignal` instead
    of reusing `transfer`, because `transfer` selects the Codex strict launch.
12. **Swap outcome reporting** adds a `report` `TransferEffect` from `finish()` so every terminal path
    reports exactly once.
13. **A confirm dialog that outlives its request stays open** (there is no programmatic dismiss on
    `confirmDialog`); confirming it afterwards toasts "That swap request already expired." and does
    nothing.
14. **Cross-doc requests of 01** (all added there): `drive_requests_in_flight` (read of `running`/`pending_confirm`
    requests); `drive_restore_awaiting_turn` (set `awaiting_turn` to none or a previous value);
    `drive_request_await_confirm` and `drive_request_resume` (running <-> pending_confirm); `sessions_list`,
    `session_revision` and the drive read methods tolerate missing drive tables on read-only handles;
    `drive_pending_prompt_counts` (per-worktree count applying the stale rule).
15. **Activity opens a read-only store per project per tick** instead of caching handles.
16. **Extra activity field `drive_paused`** (beyond `driven_by`/`drives`/`pending_prompts`) so the rail menu can
    label Pause/Resume for any project without an extra call.
