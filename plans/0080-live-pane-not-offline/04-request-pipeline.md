# Phase 3: request pipeline

Part of 0080; see `00-overview.md`. Uses engine view changes (`02-engine-core.md`) and
`status.db` presence.

## Purpose

Every request (send, read, dispatch) is picked up promptly, or fails with a true reason. Today
passes run only on `tasks.db*` file events, a window open-set change or one post-wake recheck,
watchers die silently, and expiry prints "no running Keitora app picked it up" even when an app
is running.

## Verified current code (what this phase changes)

All paths are under `D:/repos/keitora`. Line numbers are from the tree at the start of this
phase and drift; search by symbol.

| Symbol | Where | Fact that matters |
|---|---|---|
| `run_pass(env, gate)` | `src-tauri/src/drive.rs` | Runs `housekeeping`, then returns early when `env.runtime.open_set()` is `None` (rows stay pending, nothing recorded). Walks `drive_requests_pending()` in id order. A queued or non-queued send takes `gate.0.lock().await` inside the loop, so every later row (including a `read`) waits behind a held gate. |
| `housekeeping(local, env, now)` | `drive.rs` | Calls `drive_requests_expire`, refuses stuck `running`/`pending_confirm` rows this process doesn't track (`STUCK_AFTER_SECS` = 600), then `drive_purge`. Runs inside a pass, so expiry only runs when an app is running and a pass started. |
| `run_drive_requests(app, project)` | `drive.rs` | Builds `DriveEnv` and calls `run_pass`; spawns returned jobs. |
| `DriveHost::schedule_pass(project, after)` | `drive.rs` (trait, `TauriHost`) | Sleeps `after`, then `run_drive_requests`. No dedupe. `FakeHost` in `drive_tests.rs` records `(project, Duration)`. |
| `DriveEnv::recheck_after_settle` + `DriveRuntime::claim_recheck` | `drive.rs` | The only automatic recheck: per send id, once, after the wake settle window (`SEND_SETTLE_TIMEOUT` 30 s + `WAKE_RECHECK_SLACK` 500 ms). |
| `DriveRuntime` (`RuntimeInner`: `open`, `waiters`, `in_flight`, `tracked`, `sent`, `woken`, `rechecks`) | `drive.rs` | `set_open(projects) -> bool` returns false when the set is unchanged. |
| `drive_set_open_projects` -> `retrigger_pending` | `drive.rs` | Re-triggers only when `set_open` returned true. Opens each registered project's DB read-only and calls `spawn_pending_requests`. The window sends the set from an effect keyed on `openKey` (`src/App.tsx`, `driveSetOpenProjects` in `src/lib/ipc.ts`) and swallows errors (`.catch(() => {})`). |
| `spawn_pending_requests(app, local, project)` | `src-tauri/src/dispatch.rs` | Calls `dispatch_requests_pending()` and `drive_requests_pending()` (both load full rows) and spawns `run_dispatch_requests` / `run_drive_requests`. With a read-only handle on a DB that predates a column, a `SELECT` of new columns fails and `is_ok_and` hides it as "nothing pending". |
| `watch_dir(dir, file_prefix, debounce, stop_rx, on_event)` | `src-tauri/src/db_watch.rs` | Returns `()`. Returns silently when `notify::recommended_watcher` or `watcher.watch` fails, and when the event channel disconnects. The `notify` callback does `if let Ok(ev) = res`, so error events are dropped. Loop polls `stop_rx` every 500 ms. |
| `start_tasks_watcher(app)` | `src-tauri/src/host.rs` | Active project's `tasks.db*` watcher. Opens `SqliteBackend` and `LocalStore` with `.ok()`: a failed open leaves `None` and every event then does nothing for requests. Superseded through `replace_stop(&TASKS_WATCHER_STOP, ..)`. Recreated only from `restart_project_watchers` (activation paths, registry change). |
| `start_bg_notifier(app)`, `spawn_project_watcher(...)`, `BG_WATCHERS: Mutex<Vec<SyncSender<()>>>`, `replace_bg_watchers` | `src-tauri/src/bg_notifier.rs` | One watcher per registered project (including the active one). `LocalStore::open` failure inside the thread is a bare `return`. Recreated only when `start_bg_notifier` is called again. |
| `drive_requests` DDL, `DRIVE_REQUESTS_COLUMNS`, `drive_requests_ddl` | `crates/keitora-tasks/src/drive.rs` | Table exists in real DBs. |
| `init_schema` + `confirm_started_at` guard + `rebuild_for_check` | `crates/keitora-tasks/src/sqlite_backend.rs` | The guard uses `pragma_table_info('drive_requests')` then `ALTER TABLE ... ADD COLUMN`. `rebuild_for_check` copies `DRIVE_REQUESTS_COLUMNS` from the old table, so the ALTER guards must run before it. |
| `drive_requests_expire(max_age_secs)` | `crates/keitora-tasks/src/drive_store.rs` | Hard-codes `reason = 'expired: no running Keitora app picked it up in time'` for `pending`. `pending_confirm` ages from `COALESCE(confirm_started_at, created_at)` against `DRIVE_CONFIRM_MAX_AGE_SECS` (360). `DRIVE_REQUEST_MAX_AGE_SECS` = 600 (`drive.rs`). `local_store.rs` has a second, dispatch-request expiry with the same wording (out of scope). |
| `REQUEST_COLS`, `row_to_request`, `DriveRequest` | `drive_store.rs`, `drive.rs` | Explicit column list; `DriveRequest` literals exist at `crates/keitora-tasks/src/drive.rs` (test helper) and `drive_store.rs` `row_to_request`. |
| `poll_request`, `timeout_line`, `Polled` | `crates/keitora-cli/src/drive.rs` | At the deadline a still-`pending` request is cancelled by the CLI with `"no running Keitora app picked it up within {N}s"`, and `timeout_line` prints `"  timed out after {N}s: no running Keitora app picked up the {what}"` (`{what}` = `"{kind} of @{wt} (request {id})"`). Asserted by `crates/keitora-cli/src/drive_tests.rs` (about line 1256). `task dispatch --wait` in `crates/keitora-cli/src/main.rs` has its own copy of the cancel text. |
| `Ctx` | `crates/keitora-cli/src/drive.rs` | Has `local: &LocalStore` (writable) but no project name and no status reader. |
| Status loop | `src-tauri/src/backend/herdr/status_loop.rs` (not `src-tauri/src/status_loop.rs`) | `StatusLoop::tick(now)` returns early when `explicit_stop` is set and when `gate.admit(None)` fails; it exists only when the herdr backend was constructed. `due(now, last, interval)` is a public helper. |
| `choose_backend` | `src-tauri/src/host.rs` | Falls back to `UnavailableBackend` (no status loop) when herdr can't be built. |

Corrections to earlier plan text:
- The sweep, supervisor and health monitor cannot be ordinary `StatusLoop` due items: that loop
  returns early while recovery owns the server or the user stopped herdr, and doesn't exist at
  all under `UnavailableBackend`. They run on their own `keitora-pipeline` thread (Decision P1,
  `02-engine-core.md` S14).
- A request for a project outside the open set is not left pending today. It is begun and
  refused with `"project not open in Keitora"` (`common_refusal`); that text is asserted in
  `drive_tests.rs`, `crates/keitora-cli/src/drive_tests.rs` and `src/lib/driveEvents.test.ts`
  and is kept. `"project not open in the window"` is therefore not a wait reason.

## Design

### Pass triggers and scheduling

A **pass** is one `run_drive_requests` (and, for the same project, one `run_dispatch_requests`)
over a project's pending rows. Add one scheduling entry point, owned by `DriveRuntime`:

```rust
pub(crate) enum Trigger { Watcher, Sweep, Recheck, Engine, OpenSet, Turn, Startup }

impl DriveRuntime {
    /// Ask for a pass over `project` no sooner than `after`. `false` when an already
    /// scheduled pass fires at or before the new due time (deduped).
    pub(crate) fn request_pass(&self, project: &str, after: Duration, why: Trigger) -> bool;
}
```

- **Dedupe key: `project`.** State is `queued: Mutex<HashMap<String, Instant>>` (project ->
  earliest scheduled due time of a pass that hasn't started). `request_pass` computes
  `due = now + max(after, PASS_COALESCE)` where `PASS_COALESCE` = 150 ms, so a burst (five
  worktrees changing in one engine tick, two watchers firing on one write) collapses into one
  pass. If `queued[project] <= due`, return false. Otherwise store `due` and spawn a timer task
  (`tauri::async_runtime::spawn`) that sleeps until `due`, then removes the entry only if it
  still equals its own `due` (if not, a sooner timer already ran it: exit), then runs the pass.
- Passes are **not** single-flight: two passes for one project may overlap exactly as today.
  Each request is taken with `drive_request_begin`'s compare-and-set, so a request is applied
  once. This keeps a slow send from delaying a read filed in the next pass.
- `DriveHost::schedule_pass(project, after)` keeps its signature and now calls
  `request_pass(project, after, Trigger::Recheck)`; the real host's body is replaced, the fake
  host keeps recording. `recheck_after_settle`/`claim_recheck` stay (they pin a precise due
  time per send id) and call `schedule_pass`.
- **Every trigger goes through `request_pass`:**

| Trigger | Where | Call |
|---|---|---|
| `Watcher` | tasks watcher and bg watcher `on_event` (`host.rs`, `bg_notifier.rs`) | `pipeline::on_db_event(app, project)`: when `has_pending` (below) is true for drive or dispatch rows, `request_pass(project, 0, Watcher)`. Replaces the direct `spawn_pending_requests` call in both. |
| `Sweep` | `Pipeline::tick`, every 10 s (below) | `request_pass(project, 0, Sweep)` for a project with pending rows |
| `Recheck` | end of a pass | `request_pass(project, 2 s, Recheck)` while any row is still pending and the oldest pending row is younger than 60 s; older rows rely on the sweep and the engine trigger (so a send queued for an hour costs a pass every 10 s, not every 2 s) |
| `Engine` | one `engine.subscribe` callback (`02-engine-core.md` S6) | `request_pass(project, 0, Engine)` when `DriveRuntime::has_pending_for(project, worktree)` and the `ViewChange` could unblock it: `settled` turned true, `agent` went `absent`/`starting` -> `present`, `state` left `busy`/`waiting`, or `stale` went false (this covers the link returning to `up`). Also once per project with pending rows on the first snapshot (a change with `prev == None`). |
| `OpenSet` | `drive_set_open_projects` | `request_pass(project, 0, OpenSet)` for every registered project with pending rows, **on every call, not only when the set changed** (see window reopen below) |
| `Turn` | a `drive_turns` write (`03-turns-and-cli.md`) is a `tasks.db` write, so the watcher fires; no extra call |
| `Startup` | once after the first window report and once after the engine's first snapshot | `request_pass` for every registered project with pending rows |

- `DriveRuntime::has_pending_for(project, worktree)` reads `pending_by_worktree:
  Mutex<HashMap<String, HashMap<String, u32>>>`, refreshed at the end of every pass and by every
  sweep. It is a hint only; a miss costs at most one 10 s sweep.

### Safety sweep

`Pipeline::sweep(now)` runs every `SWEEP_EVERY` = 10 s.

1. `UserConfig::load()`; for each registered project whose root `is_dir()`, resolve
   `effective_config(pref)` (as `retrigger_pending` does).
2. Open a cached read-only `LocalStore` per project (`SweepState.handles: HashMap<String,
   LocalStore>`; drop and reopen on any error; a missing DB file is "nothing pending", not an
   error).
3. Run `drive_requests_has_pending()` and `dispatch_requests_has_pending()` (two indexed
   `SELECT 1 ... WHERE status = 'pending' LIMIT 1` queries that read only `status`, so they work
   on a DB of any schema age). If either is true: `request_pass(project, 0, Sweep)`, and refresh
   `pending_by_worktree` for the project from `drive_requests_pending()`.
4. Update `app_projects.pending_count` / `oldest_pending_at` (write only when changed).

Nothing else runs while nothing is pending. The sweep never opens a project's DB writable.

### Watcher supervision

**State.** New managed state `WatcherRegistry` (`src-tauri/src/watchers.rs`), `Arc` shared by
`host.rs`, `bg_notifier.rs` and `Pipeline`:

```rust
pub(crate) enum WatcherKind { Tasks /* active project, host.rs */, Bg /* per project */ }
pub(crate) struct WatcherId { pub kind: WatcherKind, pub project: String /* resolved name */ }
pub(crate) struct WatcherHealth {                 // one per spawned watcher thread
    pub state: AtomicU8,                          // Starting=0, Running=1, Exited=2, Stopped=3
    pub started_at_ms: AtomicU64,                 // unix ms
    pub alive_at_ms: AtomicU64,                   // set every loop iteration (<= 500 ms)
    pub last_event_at_ms: AtomicU64,              // a notify event passed the prefix filter
    pub last_pass_at_ms: AtomicU64,               // on_event body ran
    pub last_error: Mutex<Option<(String, u64)>>, // (message, unix ms)
    pub exit_reason: Mutex<Option<String>>,       // set with state Exited
}
```

`WatcherRegistry` holds `HashMap<WatcherId, Entry { health: Arc<WatcherHealth>, restarts:
VecDeque<u64> /* unix ms of restarts in the last 10 min */, restarted_for_request:
Option<i64>, next_restart_not_before_ms: u64 }>` plus, per kind, a `restart: Arc<dyn Fn() ->
bool + Send + Sync>` closure that respawns exactly that watcher.

**`watch_dir` changes** (`db_watch.rs`; keep it the single implementation, also used by the
config and registry watchers):
- New signature: `watch_dir(dir, file_prefix, debounce, stop_rx, health: Arc<WatcherHealth>,
  on_event) -> WatchExit`, with `enum WatchExit { Stopped, StartFailed(String), Disconnected }`.
  The config and registry watchers pass `WatcherHealth::detached()` and log a non-`Stopped` exit.
- Set `state = Running` and `started_at_ms` once `watcher.watch` succeeded; set `StartFailed`
  (and `state = Exited`, `exit_reason`) when `recommended_watcher` or `watch` fails, with
  `log::warn!("watcher {dir}: failed to start: {e}")`.
- The `notify` callback no longer drops `Err(e)`: record `last_error`, `log::warn!("watcher
  {dir}: notify error: {e}")`, and send on the channel (a `Rescan`/overflow means events were
  lost, so treat it as an event and run a pass).
- Update `alive_at_ms` at the top of every loop iteration, `last_event_at_ms` when an event is
  received, `last_pass_at_ms` right after `on_event()`.
- Return `Disconnected` (and set `Exited`) when the event channel disconnects; `Stopped` (state
  `Stopped`) on stop. A `Stopped` watcher is never restarted by the supervisor.

**Callers.**
- `start_tasks_watcher`: on a failed `SqliteBackend::open` / `LocalStore::open`, record
  `last_error` and retry the open on the next event instead of keeping `None` forever; the
  request trigger (`pipeline::on_db_event`) must run even when both opens failed (it opens its
  own store).
- `spawn_project_watcher` (`bg_notifier.rs`): a `LocalStore::open` failure sets `Exited` with
  `exit_reason = "store open failed: {e}"` rather than a bare `return`.
- `BG_WATCHERS` becomes `Mutex<HashMap<String, SyncSender<()>>>` keyed by resolved project
  name, with `replace_bg_watchers(HashMap)` and a new `restart_bg_watcher(app, project)` that
  replaces one entry. The existing swap tests in `bg_notifier.rs` change to the map form. A
  per-watcher restart resets that project's toast latch (`state_map`); it re-seeds silently.

**Supervisor.** `Pipeline::supervise(now)` runs every `SUPERVISE_EVERY` = 10 s:
1. **Exited:** a watcher with `state == Exited` is restarted. `next_restart_not_before_ms` is
   `now + min(10 s * 2^(n-1), 120 s)` where `n` = restarts in the last 10 min (so: immediate,
   +10 s, +20 s, +40 s ...). Restart = call its `restart` closure; the new thread replaces the
   entry's `health`. Finding `watcher_restarted` (see `07-self-diagnostics.md`); severity `error`
   once `n >= 3`.
2. **Hung:** `state == Running` and `now - alive_at_ms > 5 s` (the loop wakes every 500 ms): mark
   `Exited` with `exit_reason = "loop stalled"`, then as 1 (the old sender is dropped by the
   restart, so the old thread exits if it ever wakes).
3. **Silent while a request ages:** `Pipeline` knows each project's oldest pending request
   (`app_projects.oldest_pending_at`, id in memory). If the oldest pending row is older than
   `SILENT_AGE` = 15 s **and** its watcher's `last_event_at_ms` is before that row's
   `created_at`, restart that project's watchers **once per request id**
   (`restarted_for_request`), and record finding `watcher_silent`. The sweep is already
   claiming the row meanwhile.
4. Reset `restarts` entries older than 10 min.

**Registration.** On every spawn, a watcher registers (or replaces) its entry. On supersede
(`replace_stop`, `replace_bg_watchers`) the old entry's state is set `Stopped` before its stop
sender is dropped. Projects removed from the registry are dropped from `WatcherRegistry` and
from `app_projects` by the registry-change handler (`on_registry_change`).

### Pipeline driver and where it runs

`src-tauri/src/pipeline.rs` defines `struct Pipeline` with `tick(&mut self, now: Instant)` that
runs `sweep` (10 s), `supervise` (10 s), the health monitor (30 s, plus its 2 s debounce flush;
`07-self-diagnostics.md`) using the existing `due()` helper from `status_loop.rs`. It is pure
over injected deps (`Clock`, registry lister, store opener, `DriveRuntime`, `WatcherRegistry`,
`PresenceSink`) so tests run on a fake clock.

**One driver.** `host.rs` setup spawns one `std::thread` named `keitora-pipeline` (right after
the engine and `StatusPublisher` are created, before `choose_backend`) that calls
`pipeline.tick(Instant::now())` every 1 s until the app exits, whatever the backend. It does not
depend on the herdr status loop, so it keeps running in recovery, after an explicit stop and
under `UnavailableBackend`. `StatusLoopDeps` gets no pipeline field.

`app_projects` columns are not flushed by the pipeline: the `PresenceSink` (backed by
`StatusPublisher`) writes immediate changes at once and queues the throttled columns for the 5 s
`keitora-status-heartbeat` beat (`02-engine-core.md` S10, S14).

### Per-request recording: `seen_at` and `wait_reason`

- Columns on `drive_requests` (DDL and migration below): `seen_at`, `wait_reason`,
  `wait_reason_at`.
- At the start of every pass, after `housekeeping`, **every pending row** is noted, including
  when the window hasn't reported: the early `return` on `open_set() == None` moves after the
  loop that records `"window hasn't reported open projects"`.
- `LocalStore::drive_request_note_wait(id, reason) -> Result<bool>`:
  ```sql
  UPDATE drive_requests
     SET seen_at = COALESCE(seen_at, datetime('now')),
         wait_reason = ?2,
         wait_reason_at = datetime('now')
   WHERE id = ?1 AND status = 'pending'
     AND (seen_at IS NULL OR wait_reason <> ?2)
  ```
  Returns `true` when a row changed. An `UPDATE` that matches no row writes nothing, so a pass
  with no change leaves `data_version` and the WAL untouched. A request that is applied or
  refused in its first pass (never waits) gets `seen_at` only if the reason update ran; a
  request that begins immediately is not noted (it has an outcome instead).
- **Vocabulary** (exact strings; `{wt}` is the driven worktree without `@`; `{status}` as
  below). One pure function `wait_reason_for(facts) -> Option<String>` in `drive.rs` produces
  them, and a test pins every string:

| When | `wait_reason` |
|---|---|
| `open_set()` is `None` | `window hasn't reported open projects` |
| engine has no snapshot yet (link `starting` or `up`) | `waiting for first herdr snapshot` |
| herdr link `recovering` (`ServerGate` refuses), `down` (explicit stop) or `unavailable` (`UnavailableBackend`, exe missing) | `waiting for herdr ({link})`, i.e. `waiting for herdr (recovering)` / `(down)` / `(unavailable)` |
| link `up` but the view is stale (snapshot older than 3 intervals) | `waiting for herdr (stale snapshot)` |
| queued/woken send, view not sendable, `herdr_status` known | `waiting for @{wt} to be idle (herdr: {status})`, `{status}` = `working`, `blocked`, `unknown`, `starting` (agent `starting`/`launch_pending`/`interactive_ready == false`), `no agent` (agent absent), `idle` is never printed here |
| view idle but `settled == false` | `waiting for @{wt} to settle` |
| `send_settling` hold active (previous send's turn not seen starting) | `waiting for @{wt} to start its turn (send settling)` |
| queued send **deferred** at delivery: the fresh `agent_snapshot` read failed the last-moment check, the row went back to `pending` (`drive_request_requeue`, `Outcome::deferred`; `03-turns-and-cli.md` T4) | the same strings, from the fresh read: `waiting for @{wt} to be idle (herdr: {status})` when its status is not `idle`/`done` (or `launch_pending`/`interactive_ready == false` -> `starting`), else `waiting for @{wt} to settle` (the fresh read's `state_change_seq` differs from the settled view's) |
| send woke its target (own wake mark) and the live set doesn't show it yet | `waiting for @{wt}'s agent to come up (woken)` |
| another reset/swap/wake holds the worktree slot (`mark_slot_wait`) | `waiting on the wake slot` |
| gate held by a dispatch (`gate.0.try_lock()` failed) | `waiting for the dispatch gate` |

  The `SendGate::Wait` texts in `02-engine-core.md` S9 are exactly the herdr/engine rows above
  (one function, `wait_reason_for`, builds both). Non-queued sends and `wake` wait only on the
  transient rows (no or stale snapshot, herdr link, agent `starting`, idle not yet settled,
  settling hold; `wake` on `Awake::Unknown`); a non-queued send to a `busy`/`waiting`/`offline`
  view is refused as today. `reset` and `swap` are never "waiting" for state. A request outside
  the open set is **refused** (`project not open in Keitora`), never waiting.
- Expiry carries the reason. Change `drive_requests_expire` (`drive_store.rs`):
  ```sql
  reason = 'expired after ' || ?1 || 's: ' ||
           CASE WHEN wait_reason = '' THEN 'the app never examined it' ELSE wait_reason END
  ```
  with `?1 = max_age_secs`. The `pending_confirm` text is unchanged. The old text
  `"expired: no running Keitora app picked it up in time"` is gone from drive requests; update
  `plans/0078-cross-worktree-driving/*` references only if a reviewer asks (plans are history),
  and the README paragraph near "picked up within ten minutes" (`README.md`).

### Reads never queue behind gated sends

In `run_pass`, after loading `pending`, partition it into reads and the rest, keep id order in
each, and run **all reads first** (no gate), then the rest:

```rust
let (reads, rest): (Vec<_>, Vec<_>) = pending.into_iter().partition(|r| r.kind == "read");
for req in reads.into_iter().chain(rest) { /* existing body */ }
```

Inside the body, a send takes the gate with `try_lock` first: on success hold the guard through
`deliver_send`; on failure record `waiting for the dispatch gate` and then `lock().await` (the
pass still waits, but only for sends, and the reason is on the row). Consequence, accepted and
documented: a `read` filed after a `send` in the same pass observes the pane **before** that
send is typed. `send --wait` then `read` is unaffected (the send resolves first).

### Window report and reopen

- `DriveRuntime::set_open(projects) -> OpenChange` replaces the bool: `First`, `Changed`,
  `Same`. It always stamps `open_reported_at` (unix ms) in `RuntimeInner`.
- `drive_set_open_projects` calls `request_pass` for every registered project with pending rows
  on **every** call (`First`, `Changed` and `Same`), then writes `app_projects.open` for each
  registered project (write only when it changed; `open_reported_at` is written once per
  changed set and otherwise at most every 5 s with the flush).
- `DriveRuntime::clear_open()` sets the open set back to `None` and is called from the main
  window's `WindowEvent::Destroyed` handler (`host.rs`, next to the `CloseRequested` handler)
  so a closed-then-reopened window (macOS) re-reports. A re-report of the same set after a
  clear is `First`.
- The frontend effect keeps its dependency on `openKey` but also reports on window `focus` and
  on app visibility change (`visibilitychange` to visible), and logs a failed invoke
  (`console.warn`) instead of swallowing it. Reports are idempotent.

### Presence in `status.db`

`02-engine-core.md` owns `status.db`, its whole DDL (S10, including `app_projects`), the
`writer` row, the lease and the heartbeat thread. This phase owns the **semantics** of
`app_projects` (a registered project's pipeline state): which columns mean what and when they
are written. Columns used here: `project`, `registry_key`, `db_path`, `open`,
`open_reported_at`, `watcher_state`, `watcher_started_at`, `watcher_last_event_at`,
`last_pass_at`, `last_pass_trigger`, `pending_count`, `oldest_pending_at`, `restarts`,
`last_error`, `last_error_at`. All times are INTEGER unix ms (02 S10); `oldest_pending_at` is
the oldest pending request's `created_at` converted with `keitora_tasks::sqlite_utc_to_ms`.

- One row per registered project, created at startup and by `on_registry_change`, deleted when
  the project is removed. `watcher_*` describe the **best** watcher for the project (a running
  one wins; for the active project, the tasks watcher or the bg watcher, whichever fired last).
- Written through `StatusPublisher` (02 S10): immediately (`app_project_set`) on a change of
  `open`, `open_reported_at`, `watcher_state`, `last_error`, `pending_count == 0 <-> >0`;
  `last_pass_at`, `last_pass_trigger`, `watcher_last_event_at`, `pending_count` and
  `oldest_pending_at` otherwise through `app_project_touch`, flushed at most once per 5 s inside
  the `keitora-status-heartbeat` transaction. Zero writes when nothing changed; nothing in
  standby.
- Request tables stay in `tasks.db`; `status.db` is never watched by the `tasks.db*` watchers
  (different directory or filename prefix; `status.db` does not start with `tasks.db`).

### Per-pass logging

Fixed formats (`log::info!` unless noted). `pass_id` is a process-wide `AtomicU64`.

```
drive pass {pass_id} start project={p} trigger={watcher|sweep|recheck|engine|open_set|turn|startup} open={a,b|none|unreported} db={resolved_path} pending=[12,13]
drive pass {pass_id} request {id} ({kind} @{wt} by @{by}): {outcome}
drive pass {pass_id} done project={p} took={ms}ms begun={n} left={n}
```

- `{outcome}` is one of `begun`, `applied`, `refused: {reason}`, `waiting: {wait_reason}`,
  `deferred: {wait_reason}` (a queued send requeued at delivery, `03-turns-and-cli.md` T4),
  `skipped: taken by another pass`, `job: spawned`.
- `open=` prints the sorted set, `none` for an empty set, `unreported` for `None`.
- A pass with `pending=[]` logs only at `debug`. A `waiting:` line is logged only when the
  `(project, id)` pair's last logged outcome differs (in-memory `last_logged` map, pruned with
  `retain_woken`), so a 2 s recheck loop doesn't spam. The existing `resolve()` line stays.
- Sweep and supervisor log only on action: `pipeline sweep: project={p} pending, pass requested`
  (info), `pipeline: watcher {kind} {p} exited ({reason}); restarted (n={n})` (warn),
  `pipeline: watcher {kind} {p} silent while request {id} aged {s}s; restarted` (warn).

### CLI timeout message (decision table)

`crates/keitora-cli/src/drive.rs` gains a pure function

```rust
pub struct TimeoutDiag { pub code: &'static str, pub cause: String }
fn diagnose_timeout(req: &DriveRequest, p: &PresenceReading, now_ms: i64) -> TimeoutDiag
```

using the `Ctx.project` and `Ctx.status_db` fields that `03-turns-and-cli.md` T6 adds (no
separate field here). A helper `fn presence(ctx: &Ctx) -> PresenceReading` opens
`StatusReader::open(path)` (02 S10; D7: the CLI never talks to herdr). `PresenceReading` is
`Unavailable` (`status_db` `None`, file missing, schema mismatch, locked past the busy
timeout), `Stale { age_s: Option<u64> }` (`keitora_tasks::freshness` returns `Stale`: no
`writer` row, or `heartbeat_at` older than `STATUS_FRESH_MS` = 15 s) or `Fresh { project:
Option<AppProject> }` (`freshness` returns `Fresh` **or** `NotReady`: presence does not depend
on herdr; `AppProject` from `StatusReader::app_project(&ctx.project)`). The diagnosis reads the
request row as fetched **before** the cancel. Comparisons with the request's `created_at` (UTC
text) use `keitora_tasks::sqlite_utc_to_ms`.

Rules in priority order; `N` = `--timeout`, `T` = `HH:MM:SS` local time of a unix-ms value,
`age` = `{s}s` under 120 s, `{m}m` under 2 h, else `{h}h`:

| # | Condition | `code` | `cause` (exact) |
|---|---|---|---|
| 1 | `Unavailable` | `no_status` | `no running Keitora app picked up the {what}` (legacy wording, so old CLI tests keep passing) |
| 2 | `Stale { age_s: Some(a) }` | `app_stale` | `no Keitora app is updating status (last update {age(a)} ago); start Keitora, then retry` |
| 2b | `Stale { age_s: None }` | `app_stale` | `no Keitora app has reported status on this machine; start Keitora, then retry` |
| 3 | `Fresh`, no `app_projects` row for `project` | `project_unknown` | `Keitora is running but project {project} is not registered with it` |
| 4 | row `open_reported_at IS NULL` | `window_unreported` | `Keitora is running but its window hasn't reported open projects` |
| 5 | row `open = 0` | `project_not_open` | `Keitora is running but project {project} is not open in its window; open it in Keitora, then retry` |
| 6a | `seen_at IS NULL` and row `watcher_state <> 'running'` | `watcher_not_running` | `Keitora is running but its watcher for {project} is not running (it restarts every 10s)` |
| 6b | `seen_at IS NULL` and `watcher_last_event_at` is NULL or before `created_at` | `watcher_silent` | `Keitora is running but its watcher for {project} hasn't fired since {T or "it started"}` |
| 6c | `seen_at IS NULL` otherwise | `not_examined` | `Keitora is running but has not examined the request (last pass {T or "never"})` |
| 7 | `seen_at` set and `wait_reason <> ''` | `waiting` | `Keitora saw it: {wait_reason}` |
| 8 | `seen_at` set and `wait_reason = ''` | `no_reason` | `Keitora examined it but recorded no reason` |

- **Stored cancel reason** (the `reason` the CLI writes with `drive_request_cancel`):
  `"no running Keitora app picked it up within {N}s"` for rule 1 (unchanged text, unchanged
  test); for every other rule `"timed out after {N}s: {cause}"`. The same function feeds
  `task dispatch --wait` in `main.rs` for rules 1-5 only (dispatch rows have no `seen_at`).
- **Printed line** (`timeout_line`): rule 1 keeps today's exact text; otherwise
  `  timed out after {N}s: {cause} ({what})`. A request already `running` or `pending_confirm`
  keeps `  timed out after {N}s: the {what} is still {status}`.
- **JSON** (`--json`) adds `"diagnosis": {"code": "{code}", "cause": "{cause}"}` next to
  `timeout_s`; exit code stays 2.
- A request that **expired** in the app (not cancelled by the CLI) reaches the CLI as a refusal
  with the reason from `drive_requests_expire`; it prints through the normal `Refused ...`
  line and carries the `wait_reason`.

### Reports and the Fixes

- **C** (read/send timed out while the app ran with the project open): the sweep claims the row
  within 10 s even if every watcher is dead; the engine trigger applies a queued send on a
  herdr-only idle flip; reads run first; a dead watcher is restarted; the row says why it is
  still pending; the CLI says which of the cases above applies.
- **R** (request 25, no trace): per-pass lines, `seen_at`/`wait_reason`, expiry reason, and
  `app_projects` rows make the next report self-explaining.

## Files touched (as referenced)

- `src-tauri/src/drive.rs`: `RuntimeInner` (`queued`, `pending_by_worktree`, `open_reported_at`,
  `last_logged`, pass counter), `request_pass`, `Trigger`, `set_open -> OpenChange`,
  `clear_open`, `run_pass` (note pending, reads-first partition, `try_lock` gate, per-pass
  logs, `Recheck`), `wait_reason_for`, `housekeeping` unchanged except it feeds expiry,
  `drive_set_open_projects`, `retrigger_pending` (now `request_pass`).
- `src-tauri/src/dispatch.rs`: `spawn_pending_requests` replaced by `pipeline::on_db_event`
  (keeps a thin wrapper while callers migrate); `has_pending` uses the new store methods.
- `src-tauri/src/db_watch.rs`: `WatchExit`, `WatcherHealth` plumbing, `notify` error events.
- `src-tauri/src/host.rs`: `start_tasks_watcher` (register, reopen on failure),
  `start_registry_watcher`/`start_config_watcher` (detached health), the `keitora-pipeline`
  thread spawn in setup, `WindowEvent::Destroyed -> clear_open`, `on_registry_change`
  (add/remove `app_projects` rows and registry entries).
- `src-tauri/src/bg_notifier.rs`: `BG_WATCHERS` map, `restart_bg_watcher`, store-open failure
  recorded.
- new `src-tauri/src/watchers.rs` (`WatcherRegistry`, `WatcherHealth`) and
  `src-tauri/src/pipeline.rs` (`Pipeline`).
- `src-tauri/src/backend/herdr/status_loop.rs`: not changed by this phase (only its public
  `due()` helper is reused).
- `crates/keitora-tasks/src/drive.rs` (DDL, `DRIVE_REQUESTS_COLUMNS`, `DriveRequest` fields),
  `sqlite_backend.rs` (ALTER guards), `drive_store.rs` (`REQUEST_COLS`, `row_to_request`,
  `drive_request_note_wait`, `drive_requests_has_pending`, `dispatch_requests_has_pending` in
  `local_store.rs`, `drive_requests_expire`), status reader (02).
- `crates/keitora-cli/src/drive.rs` (`presence`, `diagnose_timeout`, `poll_request`,
  `timeout_line`, JSON; `Ctx.project`/`Ctx.status_db` come from Phase 2),
  `crates/keitora-cli/src/main.rs` (`task dispatch --wait` text).
- `src/App.tsx`, `src/lib/ipc.ts` (report on focus/visibility, log failures).
- `README.md` (the "picked up within ten minutes" paragraph).

## `drive_requests` migration

DDL (`crates/keitora-tasks/src/drive.rs`, in `DRIVE_REQUESTS_DDL` after `confirm_started_at`):

```sql
  confirm_started_at TEXT,
  -- First pass that examined the request; NULL until then.
  seen_at            TEXT,
  -- Why the app left it pending, as of the last pass; '' when it has no reason.
  wait_reason        TEXT NOT NULL DEFAULT '',
  wait_reason_at     TEXT
```

- Append `, seen_at, wait_reason, wait_reason_at` to `DRIVE_REQUESTS_COLUMNS` (the rebuild copies
  every listed column).
- In `init_schema` (`sqlite_backend.rs`) add a helper
  `ensure_column(conn, "drive_requests", name, decl) -> rusqlite::Result<()>` using the same
  `pragma_table_info` check as the `confirm_started_at` guard, and call it for
  `seen_at TEXT`, `wait_reason TEXT NOT NULL DEFAULT ''`, `wait_reason_at TEXT`. **Place the
  calls after the `confirm_started_at` guard and before `rebuild_for_check`**: the rebuild
  selects the new columns from the old table, so they must already exist. (SQLite accepts
  `ADD COLUMN ... NOT NULL DEFAULT ''`.)
- `REQUEST_COLS` and `row_to_request` append the three columns (indexes 11-13);
  `DriveRequest` gains `seen_at: Option<String>`, `wait_reason: String`, `wait_reason_at:
  Option<String>`; fix the literal in `crates/keitora-tasks/src/drive.rs`.
- Older CLI binaries on PATH (explicit column lists, defaults on insert) keep working against a
  migrated DB. A new CLI opens the DB writable (`LocalStore::open`), which migrates it. Read-only
  handles never select the new columns: `drive_requests_has_pending` and
  `dispatch_requests_has_pending` read only `status`.
- A request filed by an older CLI has `seen_at NULL` until the first pass; that is correct.

## Regression tests

Fixture: `Fx` in `src-tauri/src/drive_tests.rs` (temp DB, `FakeHost`, fake backend) extended
with a fake clock for `Pipeline` (ticked directly; the `keitora-pipeline` thread is not started
in tests), a fake `WatcherRegistry` and a fake presence sink.

- **Store (`drive_store_tests.rs`, `sqlite_backend.rs` tests):** migration of an old-shape
  table gains the three columns, is idempotent, and survives the `wake` rebuild with values
  preserved (extend the two existing tests near `init_schema_*`); `drive_request_note_wait`
  returns true once, false on the same reason, true on a changed reason, never touches
  non-pending rows, and a false return leaves `data_version` unchanged; expiry reason covers
  "has reason" and "never examined"; `drive_requests_has_pending` works on a table without the
  new columns.
- **Repros (written RED first; steps in `00-overview.md` "Reproductions"):**
  `repro_c_herdr_only_idle_flip_applies_a_queued_send_without_a_db_write` and
  `repro_r_ready_lane_request_in_an_open_project_is_claimed` (both in `drive_tests.rs`, pipeline
  `Fx`). Each is run against the pre-phase code first and must fail on the assertion named
  there (request still `pending`; no `wait_reason`, no pass-log line).
- **C:** a herdr-only idle flip (engine view change on the fake backend, no DB write) with a
  queued send pending produces one `Engine`-triggered pass that applies it. A `read` resolves
  while an older queued send is blocked on a held `DispatchGate`. A watcher marked `Exited` is
  restarted (closure called once, backoff honoured: immediate, then 10 s, then 20 s) and the
  sweep requests a pass meanwhile. Expiry text carries `wait_reason`. `request_pass` dedupe:
  three calls inside 150 ms produce one pass; a sooner call replaces a later one; the later
  timer is a no-op. `wait_reason_for_pins_every_string` covers every vocabulary row, including
  the deferred row (a queued send requeued at delivery is back to `pending` with
  `waiting for @bravo to be idle (herdr: working)` and logs `deferred: ...`) and asserts the
  `SendGate::Wait` texts of 02 S9 come from the same function.
- **CLI timeout text:** one test per row of the decision table (1, 2, 2b, 3, 4, 5, 6a, 6b, 6c,
  7, 8) asserting `cause`, stored cancel reason, printed line and `--json` `diagnosis.code`;
  the existing legacy assertions keep passing for row 1.
- **R:** an open project claims a ready lane's request exactly once (two concurrent passes,
  one `applied`, one `skipped: taken by another pass`). A window close/reopen (`clear_open`,
  then `set_open` with the same set) and a project switch away and back (`set_open` with a
  different set, then the original) each trigger a pass that picks up a request filed during
  the gap. A request for a project outside the open set is refused with the unchanged
  `"project not open in Keitora"` **and** the pass log names the open set and the project
  (assert the formatted line from the pure log-line function). Zero writes: N passes
  with no external change leave `data_version` and `app_projects` untouched.
- **Watcher health (`db_watch.rs`):** `watch_dir` returns `StartFailed` for a missing
  directory and records `exit_reason`; `alive_at_ms` advances while idle; a `notify` error event
  (inject through the callback seam) records `last_error` and still runs `on_event`.
- **Supervisor:** the "silent while a request ages" rule restarts once per request id and not
  again; hung loop detected after 5 s of stale `alive_at_ms`.
- **macOS WAL (runs on all three OSes in `rust-os`, no `cfg` skip):** in `db_watch.rs` tests,
  open a WAL-mode SQLite file in a temp dir, start `watch_dir` on its directory with prefix
  `tasks.db`, write from a **second** connection and assert `on_event` fires within 2 s; then
  run `PRAGMA wal_checkpoint(TRUNCATE)` from a third connection, write again, and assert it
  fires again within 2 s.

## Dependencies

- `02-engine-core.md`: view-change subscription for `request_pass`, `status.db` (whole DDL incl.
  `app_projects`, `writer`, lease, the heartbeat thread that flushes throttled `app_projects`
  columns, `StatusReader::app_project`), `link_state()`, the thread table (S14), the `due()`
  helper.
- `03-turns-and-cli.md`: `drive_turns` writes wake passes for queued sends; `Ctx.project` and
  `Ctx.status_db` (T6); `drive_request_requeue` / `Outcome::deferred` (T4).
- `07-self-diagnostics.md` surfaces the watcher/pipeline findings this phase generates
  (`watcher_restarted`, `watcher_silent`, `pipeline_stalled`).

## Singletonize decisions

- **P1. Pipeline driver.** One `Pipeline::tick` object on its own `keitora-pipeline` thread
  (1 s wake), started in app setup for every backend; never driven by the status loop. Earlier
  text ("status-loop due items", "StatusLoop::tick plus a 1 s thread under
  `UnavailableBackend`") is superseded: the status loop returns early in recovery/stop and
  doesn't exist without herdr. The health monitor shares the driver; the lease heartbeat and the
  throttled `app_projects` flush are on 02's `keitora-status-heartbeat` thread. Full thread table:
  `02-engine-core.md` S14.
- **P2. Dedupe.** Key = project, earliest due wins, 150 ms coalesce window, passes not
  single-flight (CAS gives exactly-once). Sweep 10 s; supervisor 10 s; recheck 2 s while the
  oldest pending row is under 60 s.
- **P3. Reads first.** Partition reads before the rest; sends `try_lock` the gate and record
  `waiting for the dispatch gate`. A read filed after a send in one pass sees the pre-send pane.
- **P4. "Project not open" stays a refusal** with its current text; it is not a wait reason.
  The open-set diagnostic is in the pass log and the CLI diagnosis (rules 4 and 5).
- **P5. Window report.** `set_open` always re-triggers; `clear_open` on window destroy; the
  frontend also reports on focus/visibility and logs failures.
- **P6. Schema.** Three nullable/defaulted columns via ALTER guards placed before the
  `rebuild_for_check`; `DRIVE_REQUESTS_COLUMNS` extended; read-only probes read only `status`.
- **P7. Presence table.** `app_projects` DDL lives in `02-engine-core.md` S10 (unix-ms
  integers, one row per registered project, writer-owned); this doc owns its semantics. The
  global "window reported" fact is carried by `open_reported_at IS NULL` on every row (no
  `writer.window_reported_at` column).
- **P8. CLI messages.** Legacy wording only for "status unavailable" (rule 1) so existing
  tests and old-app compatibility hold; every other case gets a specific cause and a JSON
  `diagnosis.code`.
- **P9. Dispatch requests.** `dispatch_requests` get no new columns in this phase; they share
  the sweep, the watcher triggers and the CLI diagnosis rules 1-5.
