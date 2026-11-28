# Phase 1: engine core (app side)

Part of 0080; see `00-overview.md` for goal, glossary, decisions and failure modes. The
"Singletonize decisions" section at the end pins every name, type, number and call this phase
uses; where the prose above it is looser, that section wins.

## Purpose

One answer to "is an agent running, and is it idle, busy or blocked?" for every consumer. A
single app-side module, the session engine, owns the per-worktree view and every consumer reads
its output. Nothing else interprets raw signals.

## Source-of-truth table

| Question | Primary signal | Secondary or enrichment | Never |
|---|---|---|---|
| **Is an agent running?** | herdr `agent list`: an entry for the worktree's pane (matched by keymap `pane_id`) with `agent` set, or `launch_pending` | Keitora's launch records (it started `{kind}-{hash}`) | the pane's existence alone (a leftover shell); a hook row |
| **Idle, busy or blocked?** | herdr `agent_status`, settled (see `03-turns-and-cli.md`) | Hook rows under the most-recent-evidence rule; the gate's brokered-prompt rows (`blocked` -> "driver deciding") | a hook row older than herdr's last status change |
| **Did the turn finish?** | the turn predicate over the delivery baseline (`03-turns-and-cli.md`) | a Stop hook / agy `fullyIdle` Stop for that delivery | waiting forever on a hook |
| **What did it say?** | Stop-hook reply for that delivery | transcript (Claude located from herdr's `agent_session`, Codex rollout `task_complete`; `06-reply-fallbacks.md`); then `reply_source: "none"` and a `keitora read` hint | - |
| **Which session is this?** | herdr `agent_session` (already in `resume.db`) | hook `session_id` | worktree name alone |
| **Permission decision** | hooks (the only source) | - | - |

This phase implements the first two rows and the session identity row. Turn completion and reply
are Phase 2 and Phase 5.

## Design

### `AgentInfo` parsing

- Add `completion_seq: Option<u64>`, `state_change_seq: u64` (default 0),
  `interactive_ready: Option<bool>`, `screen_detection_skipped: bool` (default false),
  `agent_session`, `terminal_id` and `revision`.
- `launch_pending` defaults to false when absent (already true in `parse_agent_info` today).
- Match entries to worktrees by keymap `pane_id`, never by name alone. Agents Keitora didn't
  start (the user ran `claude` by hand) still count as present.
- The fake backends gain `agent list` entries with seqs, `launch_pending`/`interactive_ready`
  and occupant identity.

### Placement: pure derive plus thin driver

A pure `derive(...) -> Derived` function plus a thin driver in the status loop. All logic is in
the pure function, so the table tests need no herdr.

### Polling

The engine is a due item in the existing herdr status loop (`status_loop.rs`, 1 s wake). It
reuses that loop's server gate and headroom logic instead of adding a thread.

| Mode | When | `agent list` every |
|---|---|---|
| hot | any awaited turn, pending drive or dispatch request, launch in flight, or a view not yet settled | 1 s (2 s under low headroom) |
| warm | the window has reported a non-empty open-project set, nothing hot | 5 s (15 s under low headroom) |
| cold | otherwise | 15 s (30 s under low headroom) |

- Each tick is a single `agent list` call. `pane list` stays on its existing cadence for the live
  set and the resume store.
- Every call asks the `ServerGate` first (`admit(None)`). While recovery isn't healthy, the
  engine doesn't call herdr. It marks the link `recovering` (failure modes in `00-overview.md`).
- The engine reports its mode (hot/warm/cold) to the status loop each tick, so the loop picks
  the next due time without a new thread.

### Derived view per `(project, worktree)`

| Field | Values / meaning |
|---|---|
| `state` | `idle`, `busy`, `waiting` or `offline` (the existing vocabulary, so every consumer keeps working) |
| `agent` | `present`, `starting` or `absent` |
| `herdr_status` | the raw herdr status |
| `settled` | idle confirmed (settled-idle rule) |
| `stale` | no fresh herdr snapshot |
| `pane_id`, `terminal_id`, `agent_session` | identity of the pane occupant |
| `state_change_seq`, `completion_seq` | herdr's counters |
| `last_change_at` | when the view last changed |
| `brokered`, `escalated_reason` | from the gate's prompt rows |

**Freshness:** a view is `stale` when its snapshot is older than 3x the current cadence or the
gate refuses. Stale views never produce `offline` or a refusal; they produce "wait".

### Outputs

- **In the app:** `sessions://changed` on any view change; `sessions_list` and
  `projects_overview`; the activity working dot; `bg_notifier` toasts through an in-process
  engine subscription (its per-project watcher stays for dispatch requests).
- **The status store** `status.db` (Decisions D4), which the CLI reads: global, keyed by
  `(project, worktree)`, app is the single lease-holding writer, CLI read-only, rows written only
  on change plus a 5 s writer heartbeat on its own thread (S10, S14). This doc owns the whole
  `status.db` DDL (S10), including `app_projects` (semantics in `04-request-pipeline.md`) and
  `findings` (semantics in `07-self-diagnostics.md`).

### Consumers migrate to the engine (app side, this phase)

- the rail, lane state, attention, toasts and `bg_notifier`;
- `sessions_list`, `projects_overview` and dispatch idle gating;
- the drive runner's `send`, `wake`, `queue`, `awake()` and `send_settling`;
- agent updates and the quit prompt (agent present, not pane present);
- recovery and resume, where they ask "running?" (S12: verified they already read herdr's agent
  fields, so they stay as they are).

The send gate = engine `state == idle && settled && agent == present && interactive_ready !=
Some(false) && !stale`, plus the existing `send_settling` hold, which is now released by herdr
`working`/seq change instead of a hook turn bump.

`apply_reconcile` and its one-way rules are deleted. `reconciled_sessions_in` becomes a thin read
of the engine view, filtered to configured worktrees. Rows for unconfigured names are reported
as a finding (`07-self-diagnostics.md`), not dropped silently.

**Hook rows become inputs, not truth.** Hooks still write `worktree_sessions` and
`session_replies` (compatible with older CLIs on PATH). The engine reads them as evidence. The
stale-writer problem goes away because `offline` no longer comes from `SessionEnd`, and a
`busy`/`waiting` row written by another session is dropped by the session-id filter (S5 rule 0;
the `worktree_sessions.session_id` column and its CLI writes come from `05-hook-placement.md`).

CLI-side consumers (`drives`, `wait`, `read --last`, `task watch`, the gate's driver check) are
migrated in Phase 2 (`03-turns-and-cli.md`).

## Files touched (as referenced)

- `status_loop.rs` (due item, cadence, `ServerGate`)
- `backend/mod.rs` (`apply_reconcile` deleted), `tasks.rs` (`reconciled_sessions_in`)
- `drive.rs` (`DriveEnv`, `awake()`, `send_settling`)
- `src-tauri/src/paths.rs` -> `keitora-tasks` (`keitora_home()`)
- `sessions_list`, `projects_overview`, `bg_notifier`, activity working dot, agent updates, quit
  prompt
- the fake herdr backend and `FakeBackend`
- new: `src-tauri/src/engine/`, `crates/keitora-tasks/src/{home,status_store}.rs` (S2)

## Reports fixed

A1 (presence from herdr, rail/`sessions_list`/send), A2 (one liveness answer).

## Regression tests

- **A1:** a missing row with agy present and idle -> `idle` in the rail, `sessions_list`, send
  and `drives`. A stale `offline` row with an agent present -> `idle`. A bare shell (agent
  absent) with an `idle` row -> `offline`.
- **A2:** property test over provider x row state x herdr state: `wake` reports `already_awake`
  iff send doesn't refuse as offline. A late `SessionEnd` after reset leaves the view idle.
- **Repros (written RED first; steps in `00-overview.md` "Reproductions"):**
  `repro_a1_agy_live_without_a_row_reads_idle_and_send_not_refused` and
  `repro_a2_reset_to_codex_then_late_session_end_keeps_send_and_wake_in_step`.
- **Attention latch guard:** see S12 "Latch guard" and S13 (`attention.test.ts`,
  `bg_notifier.rs`).
- **Engine table tests:** the mapping (presence x status x hook row x age), settled-idle damping,
  the most-recent-evidence rule, and the stale/recovering view.
- **Store tests:** `status.db` lease takeover and refusal, heartbeat staleness for the CLI,
  recreate on corruption, zero writes with no change.

The exact test names and assertions are in S13.

## Dependencies

- Phase 0 for live spikes. Spike 1 (`spikes.md`) gates this phase: it confirms `agent list`
  semantics, whether `interactive_ready` is present (if absent, gate on `launch_pending` and a
  non-`unknown` status only), and latency.
- Phases 2, 3, 4 and 6 build on the view and `status.db`.

## Singletonize decisions

Everything below was checked against the source at `936ad34`. Line numbers are as of that
commit and are hints only; symbol names are authoritative.

### S1. Verified current code (what exists today)

| Symbol | Where | Fact that matters |
|---|---|---|
| `AgentInfo` | `src-tauri/src/backend/herdr/cli.rs:69` | Fields: `pane_id`, `agent`, `agent_status` (String, `"unknown"` default), `launch_pending` (already defaults false), `name`, `cwd`. |
| `parse_agent_info` / `parse_agent_list` | `cli.rs:274` / `cli.rs:299` | `agent list` payload is at `/result/agents`. |
| `AgentSession` / `parse_agent_session` | `cli.rs:50` / `cli.rs:172` (private) | `{source, agent, kind: id\|path, value}`; reuse it for `agent list`. |
| `PaneIdentity` (= `PaneInfo`) | `cli.rs:18` | Has `agent`, `agent_status`, `agent_session`; no seqs. Unchanged. |
| `HerdrOps::agent_list` | `backend/herdr/ops.rs:200` | Already exists; real impl `real.rs:410`, fake `backend/herdr/fake.rs:1082` (fake lists only panes with `agent.is_some()`, `launch_pending: false`). |
| `FakeHerdr::set_agent_present` | `backend/herdr/fake.rs:495` | Sets `agent` + `agent_status = "idle"`. |
| `agent_status_to_state_hint` | `backend/herdr/agent.rs:169` | Used by `observations_from` (recovery) and `state_hint_for_pane` (`backend/herdr/mod.rs:2474`). Kept. |
| `StatusLoop::tick` | `backend/herdr/status_loop.rs:351` | Steps: explicit stop (return), headroom, gate refusal (return), probe, sample, status push, observe (`pane list`, 30 s / 60 s low). No step 6. |
| `StatusLoopDeps` | `status_loop.rs:269` | Built in `HerdrBackend::status_loop` (`backend/herdr/mod.rs:2238`). |
| `HerdrBackend::new(app)` | `backend/herdr/mod.rs:1955` | Called from `host.rs:2524`. |
| `KeyMap::entries` / `generation` / `get_current` | `backend/herdr/keymap.rs:434` / `:292` / used at `mod.rs:2797` | `observe` filters entries to `server_generation == keymap.generation()`. |
| `SessionSummary` | `src-tauri/src/backend/mod.rs:118` | `{key, gen, state_hint}`. Kept. |
| `HintMap`, `apply_reconcile`, `agent_hints`, `hint_map`, `cached_agent_hints` | `backend/mod.rs:792`, `:801-838`, `:844`, `:848`, `:862` | Deleted in this phase (with their tests). |
| `reconciled_sessions` / `reconciled_sessions_in` / `sessions_list` | `src-tauri/src/tasks.rs:97` / `:112` / `:154` | Callers: `dispatch.rs:319`, `drive.rs:932`, `host.rs:243`, `tasks.rs:162`. |
| `projects_overview` / `project_overview` | `src-tauri/src/host.rs:199` / `:230` | Use `live_set_snapshot` + `cached_agent_hints`. |
| `daemon_list_sessions` | `host.rs:~2278` | Feeds the frontend `useDaemonSessions` (background-session count, quit prompt). |
| `LocalStore::sessions_list` | `crates/keitora-tasks/src/local_store.rs:196` | Hook rows with `driven_by`, `brokered`, `escalated_reason` computed from drive tables. `last_heartbeat` is `datetime('now')` text, UTC, 1 s resolution. |
| `LocalStore::open_read_only` | `local_store.rs:84` | `READ_ONLY \| NO_MUTEX`, busy timeout `READ_ONLY_BUSY_TIMEOUT` = 200 ms. |
| `DriveEnv::awake` / `sets` / `session_state` / `send_state` / `SessionSets` | `src-tauri/src/drive.rs:905` / `:915` / `:925` / `:948` / `:974` | `send_state` adds the `send_settling` hold (`drive.rs:240`) keyed on `session_replies.turn`. |
| `RecentSend {at, turn}` / `mark_sent` | `drive.rs:135` / `:223` | Replaced (S9). |
| `state_refusal` | `drive.rs:767` | Text kept for busy/waiting/offline. |
| `wake_gate` | `drive.rs:1215` | Uses `SessionSets::awake`. |
| dispatch idle gate | `src-tauri/src/dispatch.rs:316-324` | `live_set_snapshot` + `agent_hints` + `reconciled_sessions`. |
| `working_keys` / its call | `src-tauri/src/activity/mod.rs:301` / `:358` | Busy = `state_hint` busy or waiting from `list_sessions()`. |
| `live_sessions` (agent updates) | `src-tauri/src/agent_updates/mod.rs:628` | Uses `list_sessions()` keys. |
| `step_session`, `spawn_project_watcher` | `src-tauri/src/bg_notifier.rs:138`, `:230` | Toasts only on `tasks.db*` events. |
| `sessions://changed` emitters | `host.rs:330`, `:2097`, `:2508` | No engine emitter today. |
| `keitora_home` | `src-tauri/src/paths.rs` | Callers: `backend/herdr/env.rs:198,590`, `deps/mod.rs:201,223`, `logging.rs:34`. |
| `paneStatus.ts` | `src/lib/paneStatus.ts` | Pane attach/connection state, not agent liveness. **No change.** |
| `laneState.ts` `blockStateCls` / `attention.ts` | `src/lib/laneState.ts`, `src/lib/attention.ts` | Read `WorktreeSession.state`; the vocabulary is unchanged, so their logic is unchanged. |

### S2. Module layout (new files)

App (`src-tauri/src/engine/`, declared `mod engine;` in `lib.rs`):

| File | Contents |
|---|---|
| `engine/mod.rs` | `SessionEngine`, `SharedEngine = Arc<SessionEngine>`, `Mode`, `HerdrLink`, `KeyView`, `ProjectViews`, `ViewChange`, `Subscriber`, the `Evidence` trait, `HerdrSnapshot`, cadence constants. Driver-facing methods (`due`, `ingest`, `mark_link`, `poll_failed`) and consumer-facing methods (S6). |
| `engine/model.rs` | `AgentObs`, `View`, `Presence`, `KeyMemory`, `HookEvidence`, `ProjectEvidence`, conversions `View -> keitora_tasks::StatusSession`. |
| `engine/derive.rs` | `pub fn derive(input: &DeriveInput) -> Derived` and its table tests. No I/O, no clock reads. |
| `engine/evidence.rs` | `RegistryEvidence` (app impl of `Evidence`: registry + cached read-only `LocalStore` per `db_path`). |
| `engine/writer.rs` | `StatusPublisher`: owns `Option<keitora_tasks::StatusWriter>`, lease state, the heartbeat thread, change-only row writes. |
| `engine/turns.rs` | Phase 2 (`03-turns-and-cli.md`); not created in this phase. |
| `engine/tests.rs` | Engine-level tests (`#[cfg(test)] #[path]`). |

Shared crate (`crates/keitora-tasks/src/`):

| File | Contents |
|---|---|
| `home.rs` | `pub const KEITORA_HOME_ENV: &str = "KEITORA_HOME";` and `pub fn keitora_home() -> PathBuf` moved verbatim from `src-tauri/src/paths.rs` (with its two tests). Re-exported from `lib.rs` as `keitora_tasks::keitora_home`. |
| `status_store.rs` | DDL, `STATUS_SCHEMA_VERSION`, `status_db_path()`, `StatusWriter`, `StatusReader`, row types (`WriterRow`, `StatusSession`), `Freshness`, `freshness()`, `overlay_sessions()`. Tests in `status_store_tests.rs`. |

`src-tauri/src/paths.rs` becomes `pub use keitora_tasks::keitora_home;` (one line plus the
module doc) so the five existing callers stay unchanged.

### S3. `AgentInfo` after this phase (exact)

```rust
pub struct AgentInfo {
    pub pane_id: String,
    pub terminal_id: String,              // "terminal_id"; "" when absent
    pub agent: Option<String>,            // "agent"
    pub agent_status: String,             // "agent_status"; "unknown" when absent
    pub launch_pending: bool,             // "launch_pending"; false when absent
    pub interactive_ready: Option<bool>,  // "interactive_ready"; None when absent
    pub screen_detection_skipped: bool,   // "screen_detection_skipped"; false when absent
    pub completion_seq: Option<u64>,      // "completion_seq"; None when absent or JSON null
    pub state_change_seq: u64,            // "state_change_seq"; 0 when absent
    pub agent_session: Option<AgentSession>, // "agent_session" via parse_agent_session
    pub revision: u64,                    // "revision"; 0 when absent
    pub name: Option<String>,             // "name"
    pub cwd: Option<String>,              // "cwd"
}
```

`pane_id` stays the only required key (a missing one skips the entry, as today). All JSON keys
are the bundled-schema names listed in `research-herdr.md` "`agent.list` fields". The derives
(`Clone, Debug, PartialEq, Eq`) stay valid. Update every struct literal: `fake.rs:1082`
fills the new fields from new per-pane fake state (S11), and the two `cli.rs` tests that build an
`AgentInfo` literal get the new fields.

### S4. Engine data model (exact)

```rust
// engine/model.rs
pub struct AgentObs {                      // one agent-list entry matched to a key
    pub pane_id: String,
    pub terminal_id: String,
    pub agent: Option<String>,             // herdr agent label: "claude" | "codex" | "agy" | ...
    pub agent_status: String,
    pub launch_pending: bool,
    pub interactive_ready: Option<bool>,
    pub screen_detection_skipped: bool,
    pub completion_seq: Option<u64>,
    pub state_change_seq: u64,
    pub agent_session: Option<String>,     // AgentSession.value
    pub agent_session_kind: Option<String>,   // AgentSession.kind: "id" | "path" (Phase 5 transcript locator)
    pub agent_session_source: Option<String>, // AgentSession.source, e.g. "herdr:claude"
}
pub enum Presence { Present, Starting, Absent }       // serialized "present" | "starting" | "absent"
pub struct View {
    pub state: &'static str,               // "idle" | "busy" | "waiting" | "offline"
    pub agent: Presence,
    pub agent_kind: Option<String>,
    pub herdr_status: Option<String>,
    pub settled: bool,
    pub stale: bool,
    pub interactive_ready: Option<bool>,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session: Option<String>,
    pub state_change_seq: Option<u64>,
    pub completion_seq: Option<u64>,
    pub brokered: bool,
    pub escalated_reason: Option<String>,
    pub last_change_at_ms: i64,            // unix ms of the last change of any field above except stale
}
pub struct KeyMemory {                     // per-key state carried between snapshots
    pub view: View,
    pub idle_first: Option<(Instant, u64)>,      // first idle/done snapshot and its state_change_seq
    pub herdr_changed: (Instant, i64),           // when the current state_change_seq was first seen (mono, wall ms)
    pub last_scs: Option<u64>,
    pub unknown_since: Option<Instant>,
    pub unknown_since_ms: Option<i64>,           // wall ms twin of unknown_since (Phase 6 herdr_unknown)
    pub present_since_ms: Option<i64>,           // wall ms the agent became Present; None while Absent/Starting (Phase 6 hooks_vs_herdr)
    pub generation: Generation,                  // keymap generation the memory belongs to
}
pub struct HookEvidence {                  // one worktree_sessions row (via LocalStore::sessions_list)
    pub state: String,
    pub written_ms: Option<i64>,           // last_heartbeat via keitora_tasks::sqlite_utc_to_ms; None if unparsable
    pub brokered: bool,
    pub escalated_reason: Option<String>,
    pub session_id: Option<String>,        // worktree_sessions.session_id (05-hook-placement.md section 7);
                                           // None when the column is absent (pre-Phase-4 DB) or the writer sent no id
}
pub struct ProjectEvidence {
    pub db_path: String,
    pub configured: Vec<String>,           // ProjectConfig::worktrees()
    pub rows: HashMap<String, HookEvidence>,
}
```

```rust
// engine/mod.rs
pub trait Evidence: Send + Sync {
    /// The project's hook rows and configured worktrees, read through a cached
    /// read-only handle. None when the project is unknown or its DB can't be read.
    fn project(&self, project: &str) -> Option<ProjectEvidence>;
}
pub struct HerdrSnapshot {
    pub at: Instant,
    pub at_ms: i64,
    pub generation: Generation,
    /// One entry per current-generation keymap key (reserved keys excluded), with its
    /// matched agent-list entry (None: no entry for that pane_id).
    pub keys: Vec<(SessionKey, Option<AgentObs>)>,
}
pub enum HerdrLink { Starting, Up, Recovering, Down, Unavailable } // "starting" | "up" | "recovering" | "down" | "unavailable"
pub enum Mode { Hot, Warm, Cold }
pub enum KeyView { NotReady, Missing, View(keitora_tasks::StatusSession) }
pub struct ProjectViews { pub ready: bool, pub link: HerdrLink, pub views: HashMap<String, keitora_tasks::StatusSession> }
pub struct ViewChange { pub key: SessionKey, pub prev: Option<StatusSession>, pub next: Option<StatusSession> }
pub type Subscriber = Box<dyn Fn(&[ViewChange]) + Send + Sync>;
```

`SessionEngine` (all state behind one `Mutex<EngineState>`; subscribers called after the lock is
released):

```rust
struct EngineState {
    link: HerdrLink, link_since_ms: i64,
    last_snapshot: Option<(Instant, i64)>,     // (mono, wall ms) of the last successful agent list
    memory: HashMap<SessionKey, KeyMemory>,
    launches: HashMap<SessionKey, Instant>,    // note_launch; cleared by note_launch_done or after LAUNCH_GRACE
    pending: HashSet<String>,                  // projects with pending drive/dispatch requests
    awaited: HashSet<SessionKey>,              // outstanding deliveries (Phase 2 fills it)
    window_open: bool,
    low: bool,                                 // low headroom, from the status loop
    last_poll: Option<Instant>,
}
```

Constants (in `engine/mod.rs`): `HOT = 1 s`, `HOT_LOW = 2 s`, `WARM = 5 s`, `WARM_LOW = 15 s`,
`COLD = 15 s`, `COLD_LOW = 30 s`, `STALE_FACTOR = 3`, `SETTLE = 1 s`, `LAUNCH_GRACE = 60 s`,
`UNKNOWN_FINDING_AFTER = 30 s` (read by the Phase 6 monitor).

### S5. `derive` (exact rules, in order)

`DeriveInput { obs: Option<&AgentObs>, hook: Option<&HookEvidence>, prev: Option<&KeyMemory>,
launch_noted: Option<Instant>, generation: Generation, now: Instant, now_ms: i64 }` ->
`Derived { view: View, memory: KeyMemory }`. A `prev` whose `generation` differs is treated as
`None` (a new server resets settling and seq tracking).

`herdr_changed`: if `prev.last_scs != Some(obs.state_change_seq)` or there is no prev, it becomes
`(now, now_ms)`; otherwise it is carried over.

0. **Session-id filter (stale-writer guard).** If `hook.session_id` is `Some(h)` and
   `obs.agent_session` is `Some(a)` and `h != a`, treat `hook` as `None` for every rule below
   (the row was written by a previous occupant). A `None` on either side keeps the row (old CLIs,
   hooks without an id, herdr without a session yet). `RegistryEvidence` fills `session_id` from a
   new `LocalStore::session_ids(&self) -> Result<HashMap<String, String>>` that returns an empty
   map when `pragma_table_info('worktree_sessions')` has no `session_id` column (cached like
   `drive_tables_seen`), so read-only handles on older DBs keep working; `sessions_list`'s column
   list is not changed. The rule and its table test land in this phase; the column and the CLI
   writes land in Phase 4.

1. **Absent.** `obs` is `None`, or `obs.agent` is `None` and `!obs.launch_pending`:
   - if `launch_noted` is within `LAUNCH_GRACE` of `now`: `agent = Starting`, `state = "busy"`;
   - else `agent = Absent`, `state = "offline"`, identity and seqs `None`, `brokered = false`,
     `escalated_reason = None`. Hook rows are ignored.
2. **Starting.** `obs.launch_pending || obs.interactive_ready == Some(false)`: `Starting`,
   `"busy"`. (`interactive_ready == None` counts as ready, per Spike 1's fallback.)
3. **`working`** -> `Present`, `"busy"`. **`blocked`** -> `Present`, `"waiting"`.
4. **`unknown`** or any unrecognised status string -> `Present`; `state` = `prev.view.state` when
   `prev.view.agent != Absent`, else `"busy"`. `unknown_since` is set on entry, kept while unknown,
   cleared otherwise.
5. **`idle` / `done`** -> `Present`:
   - Settling: if `prev.idle_first == Some((t, s))` with `s == obs.state_change_seq`, keep it and
     `settled = now - t >= SETTLE`; else `idle_first = Some((now, obs.state_change_seq))`,
     `settled = false`. Any non-idle status clears `idle_first`.
   - **Hook veto (most-recent-evidence):** if `hook.state` is `busy` or `waiting` and
     `hook.written_ms / 1000 > herdr_changed.1 / 1000` (strictly newer, whole seconds), `state` =
     `hook.state`, `settled = false`.
   - Else if `settled`: `"idle"`.
   - Else (unsettled): `prev.view.state` when `prev.view.agent != Absent` and that state is
     `busy`, `waiting` or `idle`; otherwise `"idle"` (with `settled = false`).
6. A hook `idle` or `offline` row never changes the result of rules 1-5.
7. `brokered` / `escalated_reason` are copied from `hook` only when `state == "waiting"`; else
   `false` / `None`.
8. `last_change_at_ms` = `prev.view.last_change_at_ms` when every field except `stale` and
   `last_change_at_ms` is equal to prev; else `now_ms`. `stale = false` (derive only runs on a
   fresh snapshot).

### S6. Engine API (consumers and driver)

Consumer side (all take `&self`):

| Method | Returns / does |
|---|---|
| `views(&self, project: &str) -> ProjectViews` | Current views of `project`'s keys. `ready = last_snapshot.is_some() && link == Up`. Every view carries `stale` per S7. |
| `view(&self, key: &SessionKey) -> KeyView` | `NotReady` before the first snapshot; `View(..)` when a view exists (possibly `stale`); `Missing` otherwise (no keymap key: never launched in this server). |
| `all_views(&self) -> Vec<(SessionKey, StatusSession)>` | Every view (activity, quit prompt, agent updates). |
| `is_ready(&self) -> bool` | As `ProjectViews::ready`. |
| `note_launch(&self, key: &SessionKey)` / `note_launch_done(&self, key: &SessionKey)` | Called by `HerdrBackend` at the start and end of every agent launch (`spawn_agent` and the resume path). |
| `set_pending(&self, project: &str, pending: bool)` | Called at the end of every drive pass (`run_pass`) and dispatch request pass with "any rows still pending". |
| `set_window_open(&self, open: bool)` | Called from `DriveRuntime::set_open` (`drive.rs:190`) with `!projects.is_empty()`. |
| `subscribe(&self, f: Subscriber)` | Called once per subscriber at setup; receives every batch of changes. |
| `note_delivery(&self, key: &SessionKey)` | Phase 2 (`03-turns-and-cli.md` T4): adds `key` to `awaited` until the next ingest reloads deliveries from evidence. |
| `link_state(&self) -> LinkState` | `LinkState { link: HerdrLink, since_ms: i64, last_snapshot_ms: Option<i64>, ready: bool }`; read by the drive wait reasons (`04-request-pipeline.md`) and the Phase 6 `herdr_link` check. |
| `key_health(&self) -> Vec<(SessionKey, KeyHealth)>` | `KeyHealth { present_since_ms: Option<i64>, unknown_since_ms: Option<i64>, pane_id: Option<String> }` from `KeyMemory`; Phase 6 input (`hooks_vs_herdr`, `herdr_unknown`). Phase 2 adds `turn_health()` (`03-turns-and-cli.md` T5). |

Driver side (status loop only):

| Method | Does |
|---|---|
| `due(&self, now: Instant, low: bool) -> bool` | Stores `low`; `true` when `last_poll` is `None` or older than `interval(mode(), low)`. |
| `mode(&self) -> Mode` | `Hot` if `pending` non-empty, `awaited` non-empty, `launches` non-empty, or any view is `Present` with `state == "idle" && !settled`, or `Starting`; else `Warm` if `window_open`; else `Cold`. |
| `ingest(&self, snap: HerdrSnapshot)` | Sets `last_poll`, `last_snapshot`, `link = Up`; for each key reads `Evidence::project` once per project; runs `derive`; drops memory for keys not in `snap.keys`; publishes changed views (S8); calls subscribers with the changes. Logs each change at `debug`: `engine: {key} {old_state}->{new_state} agent={..} herdr={..} scs={..} cs={..}`. |
| `poll_failed(&self, now: Instant)` | Sets `last_poll` only (views go stale by age). |
| `mark_link(&self, link: HerdrLink, now_ms: i64)` | Sets the link (and `link_since_ms` on change). Any link other than `Up` makes every view `stale` at once. |

### S7. Staleness rules (exact)

- A view is stale when `link != Up`, or `now - last_snapshot.0 > STALE_FACTOR * interval(mode,
  low)`.
- Stale views keep their last `state`; nothing consumes a stale view as `offline` or as `idle`.
- Before the first snapshot there are no views (`KeyView::NotReady`); consumers fall back to the
  hook row with `stale = true` (S10 `overlay_sessions`).
- The `stale` flag flips are published to `status.db` like any other change.

### S8. Status loop wiring (exact)

- `StatusLoopDeps` gains `pub engine: Option<SharedEngine>` (None in existing tests).
- `HerdrBackend` gains a field `engine: Option<SharedEngine>` set by a new builder
  `HerdrBackend::with_engine(self, engine: SharedEngine) -> Self`; `HerdrBackend::new(app)` becomes
  `HerdrBackend::new(app, engine: SharedEngine)` and calls `.with_engine(engine)`. `host.rs:2524`
  passes the engine it created (S12). `HerdrBackend::status_loop()` passes `engine` into the deps.
- `tick` changes:
  - explicit-stop branch: `engine.mark_link(Down, now_ms)` before returning;
  - gate-refusal branch (step 2): `engine.mark_link(Recovering, now_ms)` before returning;
  - new **step 8, after observe**: if `cached_running().is_none()` -> `mark_link(Down)`; else if
    `!(boot_ready)()` -> `mark_link(Starting)`; else if `engine.due(now, self.low.is_low())` ->
    one `self.deps.ops.agent_list()`; on `Ok(agents)` build the `HerdrSnapshot` from
    `keymap.entries()` filtered to `server_generation == keymap.generation()` and
    `!key.is_reserved()`, matching each entry's `pane_id` to `AgentInfo::pane_id`, then
    `engine.ingest(snap)`; on `Err(e)` -> `engine.poll_failed(now)` and `self.report(&Err(e))`
    (feeds the watchdog exactly like a failed `pane list`).
- `now_ms` comes from a new injected `wall_ms: Arc<dyn Fn() -> i64 + Send + Sync>` in
  `StatusLoopDeps` (real: `SystemTime` since epoch; tests: fake).
- `HerdrBackend` exposes the fresh read Phase 2 needs as a `PtyBackend` method:
  `fn agent_snapshot(&self, key: &SessionKey) -> Result<Option<AgentObs>, PtyBackendError>`
  (default `Ok(None)`). Herdr impl: `gate.admit_session(key)?`, `keymap.get_current(key)` (None ->
  `Ok(None)`), one `ops.agent_list()`, match by `pane_id`, convert.
- The UnavailableBackend never ticks the engine; host setup calls
  `engine.mark_link(Unavailable, ..)` when it falls back to it.

### S9. Drive runner gate (send, wake, queue, `awake`, `send_settling`)

- `DriveEnv` gains `pub engine: SharedEngine`. `run_drive_requests` fills it from
  `app.state::<SharedEngine>()`; tests build one with `SessionEngine::new_for_test()` (S11).
- `SessionSets` becomes `{ views: ProjectViews, terminated: HashSet<String> }`; `sets()` fills
  `views` from `self.engine.views(&self.project)`.
- New enum in `drive.rs`:
  `enum Awake { Yes, No, Unknown }`. `SessionSets::awake(key)`: terminated -> `No`; `!views.ready`
  or the view is stale -> `Unknown`; view `agent` `present`/`starting` -> `Yes`; else `No`.
  `DriveEnv::awake` returns the same, from a fresh `sets()`.
- `wake_gate`: `Awake::Yes` behaves as today's `true`; `Awake::Unknown` returns `WakeGate::Wait`
  (no wake is started on stale information); `Awake::No` as today's `false`.
- `run_restart` with `RestartMode::Wake` (`drive.rs:1402`): `Awake::Yes` -> already awake;
  `Awake::Unknown` cannot happen there because `run_pass` leaves a wake request un-begun while
  `Awake::Unknown` (add that check before `drive_request_begin` for `Parsed::Restart { mode: Wake }`).
- `send_state` returns `enum SendGate { Ready, Wait(String), Refuse(String) }`. Every `Wait`
  text is a string from the wait-reason vocabulary in `04-request-pipeline.md` (one table, pinned
  by one test there); in order:
  - no view and `ready` -> `Refuse(state_refusal("offline"))`;
  - no snapshot yet (link `starting` or `up`) -> `Wait("waiting for first herdr snapshot")`;
  - link `recovering`, `down` or `unavailable` -> `Wait("waiting for herdr ({link})")`;
  - view stale with link `up` (snapshot aged) -> `Wait("waiting for herdr (stale snapshot)")`;
  - `agent == starting` -> `Wait("waiting for @{wt} to be idle (herdr: starting)")`;
  - `state == idle && !settled` -> `Wait("waiting for @{wt} to settle")`;
  - `state == idle && settled && interactive_ready != Some(false)` and the settling hold is off ->
    `Ready`; hold on -> `Wait("waiting for @{wt} to start its turn (send settling)")`;
  - `busy` / `waiting` / `offline` -> `Refuse(state_refusal(state))` (a **queued** send instead
    waits with `waiting for @{wt} to be idle (herdr: {status})`, as today's queue does).
- `run_pass`: a queued send proceeds only on `Ready` (else `continue`, un-begun). A non-queued
  send with `Wait(_)` is left un-begun too (the transient conditions above never refuse);
  `Refuse(r)` refuses as today. Phase 3 records the `Wait` text as `wait_reason`.
- `RecentSend` becomes `{ at: Instant, delivery_id: i64, baseline_scs: u64 }`; `mark_sent(project,
  worktree, delivery_id, baseline_scs)`. `send_settling(project, worktree, view: &StatusSession,
  settle)` releases (and forgets) the hold when `view.herdr_status` is `working` or `blocked`, or
  `view.state_change_seq != Some(baseline_scs)`, or `at.elapsed() >= settle`. Phase 2 also
  releases it when a `drive_turns` row for `delivery_id` exists. Until Phase 2 lands,
  `delivery_id` is 0 and `baseline_scs` is the engine view's seq at delivery.

### S10. `status.db` (exact)

Path: `keitora_tasks::status_db_path() = keitora_home().join("status.db")`.

```sql
-- applied by StatusWriter on a fresh file; then PRAGMA user_version = 1 (STATUS_SCHEMA_VERSION)
CREATE TABLE IF NOT EXISTS writer (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  instance_id      TEXT    NOT NULL,
  pid              INTEGER NOT NULL,
  version          TEXT    NOT NULL,   -- app CARGO_PKG_VERSION
  started_at       INTEGER NOT NULL,   -- unix ms
  heartbeat_at     INTEGER NOT NULL,   -- unix ms
  last_snapshot_at INTEGER,            -- unix ms of the last successful agent list; NULL before it
  herdr_link       TEXT    NOT NULL
                   CHECK (herdr_link IN ('starting','up','recovering','down','unavailable')),
  herdr_link_since INTEGER NOT NULL    -- unix ms the link entered its current value
);
CREATE TABLE IF NOT EXISTS sessions (
  project          TEXT    NOT NULL,
  worktree         TEXT    NOT NULL,
  state            TEXT    NOT NULL CHECK (state IN ('idle','busy','waiting','offline')),
  agent            TEXT    NOT NULL CHECK (agent IN ('present','starting','absent')),
  agent_kind       TEXT,
  herdr_status     TEXT,
  settled          INTEGER NOT NULL CHECK (settled IN (0,1)),
  stale            INTEGER NOT NULL CHECK (stale IN (0,1)),
  interactive_ready INTEGER CHECK (interactive_ready IN (0,1)),
  pane_id          TEXT,
  terminal_id      TEXT,
  agent_session    TEXT,
  state_change_seq INTEGER,
  completion_seq   INTEGER,
  brokered         INTEGER NOT NULL DEFAULT 0 CHECK (brokered IN (0,1)),
  escalated_reason TEXT,
  last_change_at   INTEGER NOT NULL,   -- unix ms
  written_at       INTEGER NOT NULL,   -- unix ms of this row's last write
  PRIMARY KEY (project, worktree)
);
CREATE TABLE IF NOT EXISTS app_projects (   -- semantics (when each column is written) owned by 04-request-pipeline.md
  project               TEXT PRIMARY KEY,            -- resolved [project].name
  registry_key          TEXT    NOT NULL,            -- registry key (shown in messages)
  db_path               TEXT    NOT NULL,            -- resolved tasks.db the app reads
  open                  INTEGER NOT NULL DEFAULT 0 CHECK (open IN (0,1)),
  open_reported_at      INTEGER,                     -- unix ms; NULL until the window's first report
  watcher_state         TEXT    NOT NULL DEFAULT 'absent'
                        CHECK (watcher_state IN ('absent','starting','running','exited')),
  watcher_started_at    INTEGER,                     -- unix ms
  watcher_last_event_at INTEGER,                     -- unix ms
  last_pass_at          INTEGER,                     -- unix ms
  last_pass_trigger     TEXT,                        -- watcher|sweep|recheck|engine|open_set|turn|startup
  pending_count         INTEGER NOT NULL DEFAULT 0,
  oldest_pending_at     INTEGER,                     -- unix ms (created_at of the oldest pending request)
  restarts              INTEGER NOT NULL DEFAULT 0,  -- watcher restarts in the last 10 min
  last_error            TEXT    NOT NULL DEFAULT '',
  last_error_at         INTEGER                      -- unix ms
);
CREATE TABLE IF NOT EXISTS findings (       -- semantics (checks, hysteresis, remedies) owned by 07-self-diagnostics.md
  project     TEXT    NOT NULL,             -- resolved [project].name; '' = app-level finding
  worktree    TEXT    NOT NULL DEFAULT '',  -- '' = project- or app-level finding
  check_id    TEXT    NOT NULL,             -- not "check": CHECK is an SQL keyword
  severity    TEXT    NOT NULL CHECK (severity IN ('info','warn','error')),
  message     TEXT    NOT NULL,
  detail      TEXT    NOT NULL DEFAULT '{}',-- JSON
  first_seen  INTEGER NOT NULL,             -- unix ms
  last_seen   INTEGER NOT NULL,             -- unix ms
  occurrences INTEGER NOT NULL DEFAULT 1,
  cleared_at  INTEGER,                      -- unix ms; NULL while active
  remedy      TEXT    NOT NULL DEFAULT '',
  remedy_at   INTEGER,                      -- unix ms
  PRIMARY KEY (project, worktree, check_id)
);
```

**Time unit (every `status.db` table):** INTEGER unix epoch milliseconds, never SQLite text. Both
sides compute them with `SystemTime::now()`; no chrono in `keitora-tasks` or `keitora-cli`.
`tasks.db` keeps its existing UTC text columns (`created_at`, `last_heartbeat`, `seen_at`,
`completed_at`, ...); any comparison across the two parses the text with one helper in
`status_store.rs`: `pub fn sqlite_utc_to_ms(s: &str) -> Option<i64>` (accepts
`%Y-%m-%d %H:%M:%S` with an optional `.%f`, UTC). JSON output may render ms as RFC 3339
(`keitora status`), but the stored value is always ms.

Writer connection (`StatusWriter::open(path) -> Result<StatusWriter>`): creates the parent dir,
`PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 1000;`.

Reader connection (`StatusReader::open(path) -> Result<Option<StatusReader>>`): `Ok(None)` when
the file is missing (never creates it); else `OpenFlags::SQLITE_OPEN_READ_ONLY |
SQLITE_OPEN_NO_MUTEX`, `busy_timeout(LocalStore::READ_ONLY_BUSY_TIMEOUT)`; a `user_version` other
than `STATUS_SCHEMA_VERSION` returns `Ok(None)` (treated as stale).

**Startup and recreate** (`StatusPublisher::start`):
1. Open. If open fails with anything but `SQLITE_BUSY`, or `PRAGMA quick_check` isn't `ok`, or
   `user_version` is neither 0 nor `STATUS_SCHEMA_VERSION`: best-effort read
   `SELECT pid, version, heartbeat_at FROM writer WHERE id = 1`; if that reads a heartbeat within
   15 s, go **standby** (do not touch the file). Otherwise close, delete `status.db`,
   `status.db-wal`, `status.db-shm` (ignore `NotFound`), and reopen. A delete that fails (another
   process holds the file on Windows) leaves the engine in standby and is retried on the next
   heartbeat.
2. `user_version == 0`: run the DDL, then `PRAGMA user_version = 1`.
3. Try the lease.

**Lease protocol** (`StatusWriter`):
- `try_acquire(&mut self, me: &WriterIdentity, now_ms) -> Result<LeaseOutcome>` in one
  `BEGIN IMMEDIATE`: read the `writer` row; if none, or `heartbeat_at < now_ms - 15_000`, or
  `instance_id == me.instance_id`: `INSERT OR REPLACE` the row (`herdr_link = 'starting'`,
  `last_snapshot_at = NULL`) and `UPDATE sessions SET stale = 1` -> `Owned`; else ->
  `Standby { pid, version, heartbeat_at }`.
- `heartbeat(&self, instance_id, now_ms) -> Result<bool>`: `UPDATE writer SET heartbeat_at = ?
  WHERE id = 1 AND instance_id = ?`; `false` (0 rows) means the lease was lost: the publisher
  becomes standby and logs `warn`.
- `release(&self, instance_id)`: `DELETE FROM writer WHERE id = 1 AND instance_id = ?` on clean
  shutdown (the Tauri exit path), so the CLI sees "no app" at once.
- `instance_id = format!("{pid:x}-{started_ms:x}-{:016x}", rand::random::<u64>())` (app side; `rand`
  is already a `src-tauri` dependency).
- The heartbeat runs on its own thread `keitora-status-heartbeat`, every 5 s, independent of
  herdr and of the status loop (so the CLI sees a live app even with herdr missing). In standby it
  calls `try_acquire` instead. Each beat is one transaction that sets `heartbeat_at`, writes
  `herdr_link`/`herdr_link_since`/`last_snapshot_at` when they changed, and flushes the throttled
  columns queued by other threads: `app_projects` (`last_pass_at`, `watcher_last_event_at`,
  `pending_count`, `oldest_pending_at`; `04-request-pipeline.md`) and `findings.last_seen`
  (`07-self-diagnostics.md`). It does nothing else (no sweep, no health checks, no herdr call).
  See S14 for every thread.
- Writes from other threads go through `StatusPublisher` (the one writer connection behind a
  `Mutex`): `publish` (S10 row writes), `app_project_set(&AppProjectRow)` /
  `app_project_delete(project)` / `app_project_touch(project, AppProjectTouch)` (the throttled
  columns, queued for the next beat), and `findings_apply(&[FindingWrite])` (insert / update /
  clear / delete; `last_seen`-only changes are queued for the beat). Every one is a no-op in
  standby.
- `StatusPublisher::lease(&self) -> LeaseState` (`Owned` | `Standby { pid, version, heartbeat_at }`)
  feeds Phase 6's `status_writer` check.
- Standby: views are still derived and served in-process; nothing is written to `status.db` (and,
  in Phase 2, nothing to `drive_turns`). Phase 6 shows the finding.

**Row writes:** after each `ingest` or link change, `StatusPublisher::publish(&[(SessionKey,
StatusSession)])` compares each row with the last written copy (all fields except `written_at`)
and writes only the changed ones, plus `DELETE` for keys whose view disappeared, in one
transaction. No change -> no transaction.

**Read API** (`keitora-tasks`, used by the CLI):

```rust
pub struct WriterRow { pub instance_id: String, pub pid: i64, pub version: String, pub started_at: i64,
    pub heartbeat_at: i64, pub last_snapshot_at: Option<i64>, pub herdr_link: String, pub herdr_link_since: i64 }
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StatusSession { pub project: String, pub worktree: String, pub state: String, pub agent: String,
    pub agent_kind: Option<String>, pub herdr_status: Option<String>, pub settled: bool, pub stale: bool,
    pub interactive_ready: Option<bool>, pub pane_id: Option<String>, pub terminal_id: Option<String>,
    pub agent_session: Option<String>, pub state_change_seq: Option<i64>, pub completion_seq: Option<i64>,
    pub brokered: bool, pub escalated_reason: Option<String>, pub last_change_at: i64 }
pub enum Freshness { Fresh(WriterRow), NotReady(WriterRow), Stale { heartbeat_at: Option<i64> } }
pub const STATUS_FRESH_MS: i64 = 15_000;
pub fn freshness(writer: Option<WriterRow>, now_ms: i64) -> Freshness;
impl StatusReader {
    pub fn writer(&self) -> Result<Option<WriterRow>>;
    pub fn session(&self, project: &str, worktree: &str) -> Result<Option<StatusSession>>;
    pub fn sessions(&self, project: &str) -> Result<Vec<StatusSession>>;
    pub fn app_project(&self, project: &str) -> Result<Option<AppProject>>;      // 04 CLI timeout diagnosis
    pub fn findings(&self, project: &str, worktree: Option<&str>) -> Result<Vec<FindingRow>>; // 07 `keitora status`, wait/timeout lines
    pub fn data_version(&self) -> Result<i64>;
}
```

`AppProject` and `FindingRow` mirror the `app_projects` and `findings` columns one to one (same
names, `INTEGER` times as `i64`/`Option<i64>`, 0/1 as `bool`).

`View -> StatusSession`: `u64` seqs are stored as `i64` (`i64::try_from(..).unwrap_or(i64::MAX)`),
`Presence` as its lowercase word, `bool`s as 0/1.

`freshness`: no row, or `now_ms - heartbeat_at > STATUS_FRESH_MS` -> `Stale`; fresh heartbeat with
`herdr_link != "up"` or `last_snapshot_at` NULL -> `NotReady`; else `Fresh`. The CLI uses a
session row only under `Fresh`, and only when the row's own `stale` is 0.

**Shared overlay** (`keitora_tasks::overlay_sessions`), used by the app (`reconciled_sessions_in`)
and the CLI (`task watch`, `drives`):

```rust
pub fn overlay_sessions(rows: Vec<WorktreeSession>, views: Option<&HashMap<String, StatusSession>>,
    configured: &[String], drivers: &HashMap<String, String>) -> Vec<WorktreeSession>
```

- `views == None` means "engine not ready / status stale": return `rows` (scoped to `configured`
  when non-empty) with `stale = true` on each.
- Scope: worktrees in `configured` (all when `configured` is empty) that have a row or a view.
- With a view: `state = view.state`, `agent = Some(view.agent)`, `herdr_status`, `stale =
  view.stale`; when the view state is `offline`, `current_task_id = None`, `pid = None`,
  `brokered = false`, `escalated_reason = None` (today's offline rule); when it isn't `waiting`,
  `brokered = false`, `escalated_reason = None`.
- View but no row: synthesize `WorktreeSession { project: "", worktree, state: view.state,
  current_task_id: None, pid: None, last_heartbeat: "", driven_by: drivers.get(wt), brokered: false,
  escalated_reason: None, agent, herdr_status, stale }` (A1: a missing row with an agent present).
- Row but no view (and `views` is `Some`): `state = "offline"`, `agent = Some("absent")`, offline
  rule applied (the engine is ready, so no keymap key means no agent).
- Sorted by worktree. Rows for names outside `configured` are dropped here and returned to the
  engine for the Phase 6 finding (`unconfigured_rows`).

`WorktreeSession` (`crates/keitora-tasks/src/model.rs:14`) gains three `#[serde(default)]` fields:
`agent: Option<String>`, `herdr_status: Option<String>`, `stale: bool`. `LocalStore::sessions_list`
fills them with `None`, `None`, `false`. TS `WorktreeSession` (`src/lib/sessions.ts:32`) gains
`agent?: "present" | "starting" | "absent" | null; herdr_status?: string | null; stale?: boolean;`.

### S11. Fakes and test helpers

- `FakeHerdr` (`backend/herdr/fake.rs`): per-pane fake state gains `launch_pending: bool`,
  `interactive_ready: Option<bool>`, `completion_seq: Option<u64>`, `state_change_seq: u64`,
  `screen_detection_skipped: bool`; `agent_list` fills `AgentInfo` from them plus `terminal_id`,
  `agent_session` (existing pane field) and `revision: 0`. New setters:
  `set_agent_status(pane_id, &str)` (bumps `state_change_seq` by 1 when the status changes),
  `set_completion_seq(pane_id, Option<u64>)`, `set_state_change_seq(pane_id, u64)`,
  `set_launch_pending(pane_id, bool)`, `set_interactive_ready(pane_id, Option<bool>)`.
  `set_agent_present(pane_id, true)` also bumps `state_change_seq`.
- `FakeBackend` (`backend/fake.rs`): `agents: HashMap<SessionKey, AgentObs>`, setter
  `set_agent(key, Option<AgentObs>)`, and `agent_snapshot` returns it (records a `Call`).
- `SessionEngine::new_for_test(evidence: Arc<dyn Evidence>) -> SharedEngine` (no publisher) and a
  `FakeEvidence` (`HashMap<String, ProjectEvidence>` behind a `Mutex`) in `engine/tests.rs`,
  `pub(crate)` for drive/dispatch/host tests. Helper `obs(pane, status, scs, cs) -> AgentObs`.
- `keitora_tasks::status_store` test helper `pub fn write_status_for_test(path, writer: WriterRow,
  sessions: &[StatusSession])` (`#[doc(hidden)]`, always compiled so `keitora-cli` tests can use it).

### S12. Consumer migration (exact new calls)

| Consumer | Today | After |
|---|---|---|
| App setup (`host.rs` setup, before the backend at `:2524`) | - | `let engine = SessionEngine::new(Arc::new(RegistryEvidence::new()), StatusPublisher::start(status_db_path(), app_version));` `app.manage(engine.clone());` `engine.subscribe(Box::new(move \|_\| { let _ = app.emit("sessions://changed", ()); }));` then `HerdrBackend::new(app.clone(), engine.clone())`. |
| `tasks::reconciled_sessions(live, hints, project, config)` | `apply_reconcile` | `reconciled_sessions(views: &ProjectViews, project, config)`; `reconciled_sessions_in(local, views, project, config)` = `overlay_sessions(local.sessions_list()?, views.ready.then_some(&views.views), &config.worktrees(), &drivers)` where `drivers` comes from `local.drives_list()` (`driven -> driver`). |
| `tasks::sessions_list` command | `cached_agent_hints` + `live_set_snapshot` | takes `engine: tauri::State<'_, SharedEngine>`; `reconciled_sessions(&engine.views(&project), &project, &config)`. Drops the `SharedBackend` parameter. |
| `host::projects_overview` / `project_overviews` / `project_overview` | `live`, `hints` params | replace both params with `engine: &SessionEngine`; per project `reconciled_sessions_in(local, &engine.views(&resolved), &resolved, &pc)`. |
| `dispatch.rs:316-324` | `live_set_snapshot`, `agent_hints`, `reconciled_sessions` | `let views = engine.views(&project);` (engine from `app.state::<SharedEngine>()`), `reconciled_sessions(&views, ..)`; a session with `stale == true` is gated as not idle: `state = "busy"` for `decide`/`request_refusal`, refusal text `"@{wt}: herdr status is stale ({link}); try again shortly"`. `has_live_pty = state != "offline"`. |
| `drive.rs` (`awake`, `sets`, `session_state`, `send_state`, `wake_gate`, `send_settling`) | live set + hints + hook turn | S9. `session_state` is deleted; its two callers use `send_state`. |
| `activity/mod.rs:358` `working_keys(client.list_sessions())` | pane hints | `working_keys(engine.all_views())`: busy = view `state` `busy` or `waiting`. The `list_sessions()` call stays as `let _ = client.list_sessions();` with a comment that it keeps the backend's live set fresh (its result is no longer used for busy). `ActivityEngine` gets the engine through its existing state access (`app.state::<SharedEngine>()`). |
| `bg_notifier` | toasts on `tasks.db*` events only | Factor the per-burst body into `fn evaluate_project(app, project_name, resolved, sessions: &[WorktreeSession], state_map: &mut HashMap<String,(String,bool,bool)>)`. The watcher calls it with `reconciled_sessions_in(&local, &engine.views(&resolved), ..)` (after `spawn_pending_requests`, unchanged). `start_bg_notifier` registers one `engine.subscribe` callback (once per process, guarded by a `OnceLock`) that, for each project in the batch, loads the registry entry and calls `evaluate_project` with the same overlay. `state_map` moves into a process-wide `Mutex<HashMap<String /*project*/, HashMap<..>>>` shared by both paths. `step_session` and its tests are unchanged. |
| `daemon_list_sessions` (quit prompt, background count) | `list_sessions()` keys | keys from `engine.all_views()` with `agent != "absent"`, plus, when `!engine.is_ready()`, today's `list_sessions()` keys (never under-count before the first snapshot). |
| `agent_updates::live_sessions` | `list_sessions()` keys | same rule as `daemon_list_sessions` (agent present or starting; pane keys while not ready). |
| Frontend `sessions.ts` / `projectsOverview.ts` / `drives.ts` | `sessions://changed` from DB watchers | unchanged code; the engine now also emits `sessions://changed` (payload `()`), at most once per ingest. |
| Rail lane block | `blockStateCls(attn, session, ..)` | unchanged; `Rail.tsx` adds class `is-stale` (reduced opacity) on the lane block when `session?.stale`. |
| `attention.ts` / `bg_notifier.rs` latch input | `session.state` | one-line input mapping only: `latchState(s) = s.starting ? "offline" : s.state` (see "Latch guard" below); the latch and `needs_input` bodies are unchanged. |
| `laneState.ts`, `paneStatus.ts` | - | no change (verified: same state vocabulary; pane status is attach state). |
| Recovery `observations_from` (`status_loop.rs:235`) and the resume store `observe` | `pane list` agent fields | **no change**, deliberately: they already read herdr's agent fields from `pane list`, must work while the engine is stale (during recovery), and are filtered by keymap generation. |
| `transfer.rs:1021,1035` | `list_sessions`, `is_live` | no change: they ask about the pane (kill/respawn), not the agent. |

### Latch guard: a synthesized idle never latches "finished"

`latchIdleFromTurn` (`attention.ts`) and `latch_idle_from_turn` (`bg_notifier.rs`) set the
"finished a turn, awaiting you" latch on a `busy|waiting -> idle` transition. The engine
creates new ways to produce that transition without a turn having happened, so the rule is
tightened here, with the consumer migration:

- **Rule.** The latch is set only when the previous observed state was a real `busy` or
  `waiting` of a present, started agent. These never set it, and each leaves the latch
  unchanged (false when fresh):
  1. a first-seen view (`prev` undefined): seed only, as today. This includes the first view
     after `NotReady -> ready`, an app restart and a project switch;
  2. an engine-synthesized idle for an agent with no hook row (S5 / `overlay_sessions`, "view
     but no row"): there was no turn to finish;
  3. `offline -> idle` (a presence lift, A1/A2): `offline` clears the latch (existing rule) and
     the idle after it leaves it unchanged;
  4. `starting -> idle`: the engine maps `Starting` to `busy` (S5), so without this clause every
     launch would latch "finished" when the agent becomes ready;
  5. a stale view keeps the last state, so no transition occurs while stale; the first fresh
     view after stale compares against the last fresh state.
- **How clause 4 reaches the pure functions.** `WorktreeSession` gains `starting: bool` (serde
  default false, skipped when false; set by `overlay_sessions` from `StatusSession.agent ==
  starting`). `useAttention` (`attention.ts`) and `evaluate_project` (`bg_notifier.rs`, via
  `latch_input_state`) compute `latchState(s) = s.starting ? "offline" : s.state` and pass that
  as `prev` and `next` to the existing latch function. The latch and `needsInput` / `needs_input`
  bodies are unchanged; only that input mapping is new.
- Clause 2 needs no new field (a synthesized idle row appears as `prev` undefined or `offline ->
  idle`, clauses 1 and 3). The tests pin it anyway because a future engine change (for example
  emitting `idle` from the first snapshot while an old row says `busy`) would break it silently.

### S13. Required tests (names and assertions)

`backend/herdr/cli.rs`:
- `parse_agent_info_reads_seqs_readiness_and_identity`: the existing real-shape fixture
  (`cli.rs:544`) plus `"completion_seq":4,"launch_pending":false,"screen_detection_skipped":true`
  -> `completion_seq == Some(4)`, `state_change_seq == 7`, `interactive_ready == Some(true)`,
  `screen_detection_skipped`, `terminal_id == "term_abc"`, `revision == 3`,
  `agent_session.value == "sess-1"`.
- `parse_agent_info_defaults_optional_fields`: `{"pane_id":"w1:p1","agent_status":"idle"}` ->
  `terminal_id == ""`, `completion_seq == None`, `state_change_seq == 0`, `interactive_ready ==
  None`, `!screen_detection_skipped`, `!launch_pending`, `agent_session == None`, `revision == 0`.
- `parse_agent_info_null_completion_seq_is_none`.

`engine/derive.rs` (table tests, fake `Instant` base + offsets):
- `absent_agent_is_offline_whatever_the_row_says`: obs None x hook {none, idle, busy, waiting} ->
  `offline`, `Absent`, identity None.
- `bare_shell_entry_without_agent_is_offline`: `agent: None, launch_pending: false`.
- `noted_launch_without_entry_is_starting_until_grace`: 59 s -> `Starting`/`busy`; 61 s ->
  `offline`.
- `launch_pending_or_not_ready_is_starting_busy`; `interactive_ready_none_counts_as_ready`.
- `working_is_busy_blocked_is_waiting`.
- `idle_settles_after_two_snapshots_a_second_apart`: t0 idle scs 5 -> unsettled; t0+0.5 s ->
  unsettled; t0+1 s scs 5 -> settled `idle`; then scs 6 idle -> unsettled again.
- `done_counts_as_idle`.
- `unsettled_idle_keeps_the_previous_busy`; `first_unsettled_idle_reads_idle`.
- `unknown_keeps_the_last_state_and_first_unknown_is_busy`, asserting `unknown_since`.
- `hook_busy_newer_than_the_herdr_change_vetoes_idle`; `hook_busy_in_the_same_second_loses`;
  `hook_waiting_newer_vetoes_idle_as_waiting`.
- `hook_idle_or_offline_never_overrides_presence_or_working` (table over the 4 herdr states).
- `stale_busy_after_a_missed_stop_loses_to_the_next_herdr_change`.
- `late_session_end_after_reset_leaves_the_view_idle` (A2: hook row `offline` written after the
  herdr idle -> view stays `idle`).
- `generation_change_resets_settling`.
- `brokered_and_escalated_only_while_waiting`.
- `last_change_at_moves_only_on_change`.
- `hook_row_from_another_session_is_ignored_as_evidence` (rule 0: hook `busy` with
  `session_id` `"s-old"`, obs `agent_session` `"s-new"`, idle settled -> `idle`; same ids -> veto
  applies; either side `None` -> veto applies).
- `present_since_is_set_on_present_and_cleared_on_absent`.

`engine/tests.rs`:
- `no_snapshot_means_not_ready`: `view()` -> `NotReady`; `views().ready == false`.
- `gate_refusal_marks_every_view_stale_never_offline`.
- `snapshot_older_than_three_intervals_is_stale` (warm: 15 s ok, 15.001 s stale).
- `missing_key_after_first_snapshot_is_missing`.
- `subscribers_get_changes_once_and_nothing_without_change`.
- `mode_table`: pending / awaited / launch / unsettled idle / starting -> `Hot`; window open ->
  `Warm`; else `Cold`; `interval` for each mode x low matches the table.
- `a1_missing_row_agy_present_idle_reads_idle_everywhere`: FakeEvidence with no row, obs `agy`
  idle settled -> `sessions_list` overlay `idle`, `send_state == Ready`, `project_overview` `idle`.
- `a1_stale_offline_row_with_agent_present_reads_idle`.
- `a1_bare_shell_with_idle_row_reads_offline`.
- `repro_a1_agy_live_without_a_row_reads_idle_and_send_not_refused` and
  `repro_a2_reset_to_codex_then_late_session_end_keeps_send_and_wake_in_step`: the smallest
  step lists from `00-overview.md` "Reproductions", written and run RED against the old code
  before S5 lands; the tests above and below are their broader siblings.
- `a2_wake_already_awake_iff_send_not_refused_offline`: property over provider {claude, codex,
  agy} x row {none, idle, busy, waiting, offline} x herdr {absent, starting, settled idle,
  working, blocked, unknown, stale}: `Awake::Yes` <=> `send_state` is not
  `Refuse(offline text)`; `Awake::Unknown` <=> `send_state` is `Wait(_)` for the stale case.

`backend/herdr/status_loop.rs` (existing `Rig`, `Op::AgentList` counted):
- `engine_polls_agent_list_at_the_hot_cadence`: pending set, 10 s -> 10 calls; low -> 5.
- `engine_polls_at_the_warm_and_cold_cadences`: 60 s warm -> 12; cold -> 4; low warm -> 4.
- `no_agent_list_while_the_gate_refuses` (0 calls; link `Recovering`).
- `no_agent_list_before_boot_ready` (link `Starting`).
- `a_failed_agent_list_reaches_the_watchdog`.
- `engine_snapshot_matches_by_pane_id_and_skips_other_generations`.

`crates/keitora-tasks/src/status_store_tests.rs`:
- `writer_takes_a_fresh_lease_and_heartbeats_only_the_writer_row`.
- `second_writer_is_refused_while_the_heartbeat_is_fresh` (14 s -> `Standby`).
- `second_writer_takes_over_after_15s` (16 s -> `Owned`; the first writer's `heartbeat` returns
  `false`).
- `takeover_marks_previous_rows_stale`.
- `release_deletes_only_my_writer_row`.
- `freshness_table`: missing row -> `Stale`; 15 000 ms -> `Fresh`; 15 001 ms -> `Stale`; link
  `starting` or no `last_snapshot_at` -> `NotReady`.
- `version_mismatch_is_recreated_without_a_fresh_owner` / `..._kept_with_a_fresh_owner`.
- `corrupt_file_is_recreated` (garbage bytes in `status.db`).
- `unchanged_views_cause_zero_writes` (`data_version` seen by a second connection is unchanged
  after N publishes of the same rows).
- `reader_never_creates_the_file`; `reader_rejects_another_schema_version`.
- `overlay_sessions_table`: fresh view beats row; row without view -> `offline` with task/pid
  cleared; view without row -> synthesized with `driven_by`; `None` views -> rows marked stale;
  non-waiting state clears `brokered`; unconfigured names dropped.

`crates/keitora-tasks/src/home.rs`: the two `keitora_home` tests moved from `paths.rs`.

App consumers:
- `drive_tests.rs`: `send_waits_not_refuses_while_stale`, `send_waits_while_starting`,
  `send_waits_until_idle_settles`, `send_settling_released_by_working`,
  `wake_waits_while_status_is_stale`.
- `dispatch.rs`: `dispatch_treats_a_stale_session_as_not_idle`. The two existing tests at
  `dispatch.rs:836` and `:884` that call `apply_reconcile` are rewritten over `overlay_sessions`.
- `bg_notifier.rs`: `engine_only_flip_to_waiting_fires_once` (no DB write, background project).
- Latch guard, `bg_notifier.rs` (`step_session`, `latch_input_state`):
  `first_seen_synthesized_idle_does_not_latch_finished` (prev `None`, next `idle`: no fire, latch
  false); `offline_to_idle_lift_does_not_latch_finished`; `starting_to_idle_does_not_latch_finished`
  (`starting: true` busy, then idle: latch false, no fire); `real_busy_to_idle_still_latches_finished`
  (started agent busy -> idle: latch true, fires once in the background);
  `engine_synthesized_idle_for_a_rowless_agent_never_fires` (A1 shape: no row, `idle` from the
  first snapshot, N snapshots: zero fires).
- Latch guard, `attention.test.ts`: `latchIdleFromTurn` keeps `undefined -> idle` and
  `offline -> idle` false; new `latchState` cases (`starting` maps to `offline`; started `busy`
  -> `idle` latches); a `useAttention` case: a rowless idle agent shown twice gives
  `needsInput == false` and no badge or notification.
- `activity/mod.rs`: `working_keys_come_from_engine_views`.
- `backend/mod.rs`: the `apply_reconcile` tests (`:1168-1240`) are deleted.

`status_store_tests.rs` also gets `status_ddl_matches_the_documented_columns` (every column of
`app_projects` and `findings` above exists with its default) and `sqlite_utc_to_ms_parses_both_forms`.

### S14. Threads and cadences (the one scheduling story; `04` and `07` reference this)

| Thread | Owner | Wakes | Work | Runs without herdr? |
|---|---|---|---|---|
| herdr status loop (existing) | `backend/herdr/status_loop.rs` | 1 s | headroom, probe, sample, status push, observe (`pane list` 30 s / 60 s low), **engine snapshot** (`agent list` at the S4 hot/warm/cold cadence, step 8), link marking; `ingest` publishes changed `sessions` rows and calls engine subscribers on this thread | no (absent under `UnavailableBackend`; returns early on stop / gate refusal after marking the link) |
| `keitora-status-heartbeat` | `engine/writer.rs` (`StatusPublisher`) | 5 s | lease heartbeat or `try_acquire`; writer link/snapshot columns; flush of queued `app_projects` and `findings.last_seen` columns (S10) | yes |
| `keitora-pipeline` | `src-tauri/src/pipeline.rs` (`04-request-pipeline.md`) | 1 s | `Pipeline::tick`: safety sweep (10 s), watcher supervision (10 s), health monitor (30 s, `07-self-diagnostics.md`), health debounce flush (2 s) | yes (started once in app setup for every backend) |
| tokio tasks | `DriveRuntime::request_pass` timers, pass jobs | on demand | drive / dispatch passes; Phase 5 transcript reads (`spawn_blocking`) | yes |

Event-driven work rides the thread that produced the event: engine view changes (status loop)
fan out to `sessions://changed`, `bg_notifier`, the pipeline's `Engine` trigger (which only
calls `request_pass`) and the health monitor's debounced `on_view_change` (which only marks
keys dirty for the pipeline thread). Subscribers stay cheap: only `bg_notifier`'s
`evaluate_project` (S12) reads a `tasks.db`, through the cached read-only handle.