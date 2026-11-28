# Phase 2: turns and CLI

Part of 0080; see `00-overview.md`. Builds on the engine view and `status.db` from
`02-engine-core.md`. The "Singletonize decisions" section at the end pins every name, column,
rule and JSON shape; where the prose above it is looser, that section wins.

## Purpose

Turn completion without depending on a Stop hook, a `wait` that always terminates, a separate
human-turn event, and CLI verbs that read the same answers as the app.

## Design

### Baseline

`deliver_send` reads a **fresh** `agent list` entry for the target (never the cached tick) right
before it types the prompt (`agent prompt` for a one-line prompt, paste + Enter for a block). Its
baseline is `B = {pane_id, terminal_id, agent_session, state_change_seq, completion_seq, status,
delivered_at}`, and the send is not delivered unless `B.status` is idle or done, settled, and
`interactive_ready`.

- `B` is persisted in `tasks.db` on the `drives` row, as new columns next to `awaiting_turn`
  along with a monotonic `delivery_id`, in the same write that sets `awaiting_turn`.
- So a turn that completes while the app restarts is still resolved: the first snapshots after
  startup compare against the persisted `B`.
- `drives` gains `delivery_id INTEGER`, `baseline_json TEXT` and `delivered_at TEXT`, additive.
  `awaiting_turn` keeps its meaning for older CLIs. The new CLI acks by `delivery_id` (CAS) and
  also clears `awaiting_turn`.

### Settled idle

`idle`/`done` counts only when seen on two consecutive snapshots at least 1 s apart with
`state_change_seq` unchanged. This damps a resize or redraw that briefly matches idle.

### Turn predicate

On a snapshot with the same occupant as `B` (same `pane_id` and `terminal_id`; the same
`agent_session`, or one newly set), the delivery's turn is **finished** when status is settled
idle and any of these holds:
1. `completion_seq` is non-null and differs from `B.completion_seq`. "Differs", not "is
   greater": this is correct whether the field is a counter or the current idle transition's id
   (research-herdr correction 6).
2. `state_change_seq >= B.state_change_seq + 2`: some status change happened and came back to
   idle.
3. The engine itself observed `working` or `blocked` after `delivered_at`.

A Stop hook (or agy Stop with `fullyIdle`) recorded for this delivery also finishes the turn.
Either source finishes it, the first one wins, and the turn is deduped by `delivery_id`.

### Outcomes other than finished

`wait` always terminates:

| Condition | `wait` event |
|---|---|
| Finished | `turn_finished {reply, reply_source, caveat?}` |
| Brokered prompt pending | `prompt` (unchanged) |
| herdr `blocked`, no brokered prompt, for 10 s | `stalled {reason: "blocked on a dialog nothing brokers"}` |
| Still idle with no seq change 30 s after delivery | `stalled {reason: "no activity observed since delivery"}` (the prompt may not have been submitted, or herdr can't read this UI) |
| Occupant changed (restart, `/clear`, recovery resume) | `stalled {reason: "agent restarted mid-turn; result unknown"}` |
| herdr not up (`starting`, `recovering`, `down`, `unavailable`) or its snapshot stale, for `--stall-after` | `stalled {reason: "herdr unavailable for Ns (<link>)"}`, `<link>` = the link word or `stale snapshot` |
| `status.db` stale (no app) for `--stall-after` | `stalled {reason: "no Keitora app is updating status"}`, after the legacy hook path below finds nothing |
| herdr `working` | never a stall: a long healthy turn is not stalled |

### Reply text (collection order)

On completion the engine collects the reply in this order:
1. A Stop-hook reply recorded for this delivery. The engine waits up to 3 s for it after a
   herdr-detected completion.
2. The transcript (Phase 5, `06-reply-fallbacks.md`), looked up from 1 s after detection, off
   the status-loop thread (T5 step 4).
3. None, with `reply_source: "none"` and a hint to use `keitora read`.

For agy, a herdr-only completion carries `caveat: "agy may still run background tasks"`.

### Turn events: `drive_turns`

The engine records each completion of a **driven** worktree as a row in a new `drive_turns` table
in the project's `tasks.db`.
- The app (the `status.db` lease holder) is the only writer.
- The write is meant to fire the `tasks.db` watchers and `wait`'s `data_version`. A completion
  means the target is idle, so queued sends should get a pass.
- Rows are pruned 7 days after ack (or after completion, if never acked), keeping the last 50 per
  worktree.

### Human turns (D5)

The user decided these surface as a separate `human_turn` event. Attribution uses a per-worktree
ledger:
- Each settled completion is identified by its `(completion_seq, state_change_seq)` at idle. The
  engine remembers the last one it recorded.
- A new completion is a **driver** turn if an outstanding delivery exists whose `B` predates it
  and which isn't yet resolved. That is the first completion past the baseline.
- Any other new completion of a driven worktree is a **human** turn. Examples:
  - the human typed while nothing was outstanding;
  - a second completion after the driver's turn resolved (a message the human queued mid-turn);
  - a completion while the driver's send is still queued and undelivered.
- Race: the human submits at the same instant a driver send is delivered. The first completion is
  attributed to the driver. This is accepted and documented, because sends require settled idle.
- Completions that happened before the engine's first snapshot produce no events. The exception
  is a persisted outstanding `B`, which resolves the driver turn.

### `wait` changes

- Event priority: `prompt` > `turn_finished` > `human_turn` > `send_refused` > `stalled`.
- Each `human_turn` is delivered once (it's acked).
- "Nothing to wait for" (exit 4) now also requires no unacked turn row.
- `wait` polls both `tasks.db` and `status.db` `data_version`: one extra cheap `data_version`
  read per 500 ms.

### Legacy path

When `status.db` is stale (no app, or an old app), `wait` falls back to today's
`session_replies.turn >= awaiting_turn` check, with `reply_source: "hook"` and attribution
unknown. With a fresh app it uses only `drive_turns`, so a turn is never delivered twice. An
older CLI on PATH keeps working through `session_replies`.

### CLI consumers move to `status.db`

`drives`, `wait`, `read --last`, `task watch` and the gate's driver-liveness check read
`status.db` while it is fresh (heartbeat within 15 s), else today's raw row. The CLI never talks
to herdr (D7).

**Gate (permission hook) driver check:** while `status.db` is fresh, it uses the driver's engine
view (`agent == present`); while stale, today's raw row; `stop_reason` uses the same function.

## Files touched (as referenced)

- `keitora-cli/src/drive.rs` (`select_event`, `gather`, `wait_with`, `cmd_drives`, `cmd_read`,
  `event_output`), `keitora-cli/src/gate.rs` (`GateStore` impl, `gate_output`),
  `keitora-cli/src/main.rs` (`run_drive`, `cmd_task_watch`)
- `src-tauri/src/drive.rs` (`deliver_send`, `send_settling`, `housekeeping`), new
  `src-tauri/src/engine/turns.rs`
- `keitora-tasks`: `drive.rs` (DDL, `Baseline`, `DriveTurn`), `drive_store.rs` (delivery and turn
  store methods), `sqlite_backend.rs` (`init_schema` column adds)

## Reports fixed

B (the hang: completion from herdr), A2 (`drives` consistency).

## Regression tests

- **B:** a Codex delivery with no Stop: `completion_seq` changes -> `wait` returns
  `turn_finished` with `reply_source: none` within one hot tick. The idle-no-change case ->
  `stalled` at 30 s. `working` for 10 minutes -> no stall. A human typing after resolution ->
  `human_turn`.
- **Repro (RED first, `00-overview.md` "Reproductions"):** `repro_b_codex_answer_without_stop_returns_turn_finished`
  (`crates/keitora-cli/src/drive_tests.rs`): written and run against the old code before the
  predicate lands; it must fail with `wait` still waiting after 10 fake minutes.
- **Turn predicate tests:** each clause alone, an occupant change, a herdr restart (seqs reset),
  a persisted baseline across an app restart, and human-turn attribution including the queued
  mid-turn message.
- **Store tests:** `drive_turns` ack and prune; zero writes with no change; `delivery_id` CAS ack.

The exact test names and assertions are in T12.

## Dependencies

- `02-engine-core.md` (view, `status.db`, fake backends with seqs).
- Spike 1 (`spikes.md`) picks the primary predicate clause and confirms latency; the 30 s
  no-activity stall and hot cadence depend on its latency result.
- `06-reply-fallbacks.md` supplies the transcript step of reply collection.
- `04-request-pipeline.md` relies on `drive_turns` writes waking passes.

## Singletonize decisions

Checked against the source at `936ad34`; line numbers are hints, symbol names are authoritative.

### T1. Verified current code

| Symbol | Where | Fact that matters |
|---|---|---|
| `drives` DDL | `crates/keitora-tasks/src/drive.rs` `DRIVE_DDL` | Columns `driven, driver, since, forced_from, paused, escalate_after_s, awaiting_turn`. The module doc says "never new columns on existing ones"; this phase is the documented exception (update that doc line). |
| `DRIVE_TABLES` / `drive_tables_exist` | `drive.rs` / `drive_store.rs:169` | Read-only handles answer "not driven" when any of the 4 tables is missing. `drive_turns` is **not** added to `DRIVE_TABLES` (old DBs would read as undriven). |
| `DRIVE_COLS` / `row_to_drive` | `drive_store.rs` | Used by read-only handles; **do not** add the new columns to it (an old DB without them would fail every read). New columns are read by separate methods (T3). |
| `init_schema` | `crates/keitora-tasks/src/sqlite_backend.rs:67` | Adds `confirm_started_at` via `pragma_table_info` check + `ALTER TABLE`; copy that pattern. |
| `drive_set_awaiting_turn` | `drive_store.rs:417` | `awaiting_turn = COALESCE(session_replies.turn, 0) + 1`. Only caller: `src-tauri/src/drive.rs:1289`. Replaced by `drive_begin_delivery` and deleted. |
| `drive_restore_awaiting_turn` | `drive_store.rs:434` | Callers `drive.rs:1296,1322` (restore after failed send) and `:1364` (`clear_awaiting_turn` after reset/swap). The first two move to `drive_restore_delivery`; `clear_awaiting_turn` keeps it. |
| `drive_ack_turn` | `drive_store.rs:444` | Only caller `keitora-cli/src/drive.rs:1544`. Kept for older callers; the new CLI's legacy path uses `drive_ack_turn_legacy` (T3). |
| `session_reply_record` / `session_reply_get` | `drive_store.rs:961` / `:976` | Stop-hook capture bumps `turn`. Unchanged; the engine only reads it. |
| `data_version` | `drive_store.rs:1012` | `PRAGMA data_version`. |
| `drive_request_begin` (CAS pending->running) | `drive_store.rs:838` | New `drive_request_requeue` is CAS running->pending. |
| `deliver_send` | `src-tauri/src/drive.rs:1282` | Line prompts go through `backend.write_line` (herdr `agent prompt`, `backend/herdr/mod.rs:771`); block prompts through `paste` + `write_line(key, "")` (Enter key). |
| `housekeeping` | `drive.rs:992` | Runs once per pass; gets the prune call. |
| `WaitArgs` | `crates/keitora-cli/src/drive.rs:73` | `--timeout` (0 = forever), `--stall-after` (default 120, min `STALL_AFTER_MIN` = 10). |
| `WaitInput` / `WaitEvent` / `select_event` / `poll_events` / `gather` / `wait_with` / `event_output` | `keitora-cli/src/drive.rs:1275` / `:1297` / `:1321` / `:1366` / `:1403` / `:1467` / `:1549` | Today: prompt > turn_finished (`session_replies.turn >= awaiting_turn`) > send_refused > stalled (raw `busy` row + `session_idle_secs`, clamped to elapsed). `WAIT_POLL` 500 ms, `WAIT_REEVAL` 5 s. Acks before printing. |
| `cmd_drives` | `keitora-cli/src/drive.rs:550` | State from raw `sessions_list`. |
| `cmd_read` `--last` / `read_last_output` | `keitora-cli/src/drive.rs:785` / `:811` | Reads `session_replies`; exit 6 when none. |
| `Ctx` / `run_drive` | `keitora-cli/src/drive.rs:336` / `keitora-cli/src/main.rs:490` | No project name or status DB today. |
| `Clock` | `keitora-cli/src/gate.rs:158` | `now() -> Instant`, `sleep`. Fakes: `drive_tests.rs:20`, `gate_tests.rs:97`. |
| `GateStore::session_state` / `impl GateStore for LocalStore` / `stop_reason` / driver check | `gate.rs:145` / `:174` / `:567` / `:677` | Both checks read `session_state(driver)`; `None`/`offline` = driver offline. |
| `gate_output` | `gate.rs:724` | Builds `Config::from_project(resolve())`, opens `LocalStore`, calls `run_gate`. |
| `cmd_task_watch` | `keitora-cli/src/main.rs:1212` | Raw `sessions_list` filtered to `config.worktrees`. |
| Project name in the CLI | `keitora_tasks::Config::project_config().project.name` | Used to key `status.db`. |
| Dependencies | `keitora-cli/Cargo.toml` | No `chrono`; new code uses `SystemTime` unix ms. |

### T2. Schema (exact)

`drives` gains three columns, each added in `init_schema` with the `confirm_started_at` pattern
(`SELECT 1 FROM pragma_table_info('drives') WHERE name = ?` then `ALTER TABLE drives ADD COLUMN
...`):

```sql
ALTER TABLE drives ADD COLUMN delivery_id   INTEGER;  -- NULL until a new app delivers
ALTER TABLE drives ADD COLUMN baseline_json TEXT;     -- serde_json of Baseline (T4)
ALTER TABLE drives ADD COLUMN delivered_at  TEXT;     -- strftime('%Y-%m-%d %H:%M:%f','now'), UTC
```

New table, in a new const `DRIVE_TURNS_DDL` in `keitora-tasks/src/drive.rs`, executed by
`init_schema` right after `DRIVE_DDL`:

```sql
CREATE TABLE IF NOT EXISTS drive_turns (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  driven           TEXT    NOT NULL,
  kind             TEXT    NOT NULL CHECK (kind IN ('driver','human')),
  delivery_id      INTEGER,
  outcome          TEXT    NOT NULL DEFAULT 'finished' CHECK (outcome IN ('finished','restarted')),
  signal           TEXT    NOT NULL CHECK (signal IN ('herdr','hook')),
  clause           TEXT    NOT NULL CHECK (clause IN
                     ('completion_seq','state_change_seq','observed_active','stop_hook','occupant_changed')),
  completion_seq   INTEGER,           -- herdr identity at the settled idle (ledger key)
  state_change_seq INTEGER,
  completed_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
  reply            TEXT    NOT NULL DEFAULT '',   -- truncated to SESSION_REPLY_MAX bytes
  reply_source     TEXT    NOT NULL CHECK (reply_source IN ('hook','transcript','none')),
  caveat           TEXT,
  acked_at         TEXT,
  CHECK ((kind = 'driver') = (delivery_id IS NOT NULL))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_drive_turns_delivery
  ON drive_turns(delivery_id) WHERE delivery_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_drive_turns_driven_acked ON drive_turns(driven, acked_at);
```

- `outcome = 'restarted'` rows have `signal = 'herdr'`, `clause = 'occupant_changed'`,
  `reply_source = 'none'`.
- `drive_purge` (`drive_store.rs:1020`) is not touched; pruning is a new method (T3).

### T3. Store API (`keitora-tasks`, exact)

Types in `keitora-tasks/src/drive.rs`, re-exported from `lib.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub agent: Option<String>,          // herdr agent label at delivery ("claude" | "codex" | "agy")
    pub pane_id: String,
    pub terminal_id: String,
    pub agent_session: Option<String>,
    pub state_change_seq: u64,
    pub completion_seq: Option<u64>,
    pub status: String,                 // "idle" | "done"
    pub delivered_at_ms: i64,           // unix ms, for Rust-side age math
}
pub struct Delivery { pub driven: String, pub driver: String, pub awaiting_turn: Option<i64>,
    pub delivery_id: Option<i64>, pub baseline: Option<Baseline>, pub delivered_at: Option<String> }
pub struct DriveTurn { pub id: i64, pub driven: String, pub kind: String, pub delivery_id: Option<i64>,
    pub outcome: String, pub signal: String, pub clause: String, pub completed_at: String,
    pub reply: String, pub reply_source: String, pub caveat: Option<String>, pub acked_at: Option<String> }
pub struct NewDriveTurn<'a> { pub driven: &'a str, pub kind: &'a str, pub delivery_id: Option<i64>,
    pub outcome: &'a str, pub signal: &'a str, pub clause: &'a str, pub completion_seq: Option<u64>,
    pub state_change_seq: Option<u64>, pub reply: &'a str, pub reply_source: &'a str,
    pub caveat: Option<&'a str> }
pub const DRIVE_TURNS_KEEP_PER_WORKTREE: i64 = 50;
pub const DRIVE_TURNS_RETENTION_DAYS: i64 = 7;
```

Methods on `LocalStore` (in `drive_store.rs`):

| Method | SQL / behaviour |
|---|---|
| `drive_delivery_columns_exist(&self) -> Result<bool>` | `pragma_table_info('drives')` has `delivery_id`; cached true in a new `Cell<bool>` like `drive_tables_seen`. |
| `drive_turns_exist(&self) -> Result<bool>` | `sqlite_master` has `drive_turns`; cached likewise. |
| `drive_delivery_get(&self, driven) -> Result<Option<Delivery>>` | `None` when drive tables or delivery columns are absent. |
| `drive_deliveries(&self) -> Result<Vec<Delivery>>` | Drives with `awaiting_turn IS NOT NULL`; empty when columns absent. |
| `drive_begin_delivery(&self, driven, b: &Baseline) -> Result<Option<(i64, i64)>>` | One `UPDATE drives SET awaiting_turn = COALESCE((SELECT turn FROM session_replies WHERE worktree = ?1), 0) + 1, delivery_id = (SELECT COALESCE(MAX(x), 0) + 1 FROM (SELECT delivery_id AS x FROM drives UNION ALL SELECT delivery_id FROM drive_turns)), baseline_json = ?2, delivered_at = strftime('%Y-%m-%d %H:%M:%f','now') WHERE driven = ?1 RETURNING awaiting_turn, delivery_id`; `None` when not driven. |
| `drive_restore_delivery(&self, prev: &Delivery) -> Result<bool>` | Sets all four columns (`awaiting_turn`, `delivery_id`, `baseline_json`, `delivered_at`) back to `prev`'s values. |
| `drive_turn_insert(&self, t: &NewDriveTurn) -> Result<Option<i64>>` | `BEGIN IMMEDIATE`; for `kind = 'driver'` insert only if `EXISTS (SELECT 1 FROM drives WHERE driven = ?driven AND delivery_id = ?delivery_id AND awaiting_turn IS NOT NULL)`; `INSERT OR IGNORE` (unique delivery index); returns the new id or `None` (already resolved / no longer outstanding). |
| `drive_turns_unacked(&self, driven) -> Result<Vec<DriveTurn>>` | `acked_at IS NULL`, `ORDER BY id`; empty when the table is absent. |
| `drive_turn_latest(&self, driven) -> Result<Option<DriveTurn>>` | Newest by id (acked or not, any outcome). |
| `drive_turn_for_delivery(&self, delivery_id) -> Result<Option<DriveTurn>>` | For `send_settling` release and the engine's resolved check. |
| `drive_turn_ack(&self, id) -> Result<bool>` | `BEGIN IMMEDIATE`; `UPDATE drive_turns SET acked_at = strftime(...) WHERE id = ?1 AND acked_at IS NULL`; if 1 row and the row is `kind = 'driver'`: `UPDATE drives SET awaiting_turn = NULL WHERE driven = ? AND delivery_id = ?` (CAS by `delivery_id`); `true` iff this call acked the row. |
| `drive_ack_turn_legacy(&self, driven, awaiting: i64) -> Result<bool>` | `BEGIN IMMEDIATE`; `drive_ack_turn(driven, awaiting)`'s update; on success also `UPDATE drive_turns SET acked_at = ... WHERE driven = ? AND delivery_id = (SELECT delivery_id FROM drives WHERE driven = ?) AND acked_at IS NULL` (columns permitting) so a row the engine wrote for the same delivery is never delivered again. Select the `delivery_id` before the first update. |
| `drive_turns_prune(&self) -> Result<usize>` | Delete rows with `COALESCE(acked_at, completed_at) < strftime('%Y-%m-%d %H:%M:%f','now','-7 days')` whose id is not among the newest `DRIVE_TURNS_KEEP_PER_WORKTREE` ids of the same `driven`. |
| `drive_request_requeue(&self, id) -> Result<bool>` | `drive_request_cas(id, "running", "pending")`. |

`drive_set_awaiting_turn` is deleted (no caller left). `drive_ack_turn` stays (older code paths and
its tests).

### T4. Delivery and baseline capture (app, exact)

`deliver_send(env, local, req: &DriveRequest, prompt)` (the request is passed so `req.queue` and
`req.id` are known):

1. `let obs = env.backend.agent_snapshot(&key)` (`02-engine-core.md` S8). `Err(e)` -> refused with
   `injection_detail`-style text. `Ok(None)` -> treat as `offline` (refused, `state_refusal`).
2. Check, against `obs` and `env.engine.view(&key)`:
   - `obs.agent_status` is `idle` or `done`, `!obs.launch_pending`,
     `obs.interactive_ready != Some(false)`;
   - the engine view is `idle`, `settled`, not `stale`, and its `state_change_seq ==
     Some(obs.state_change_seq)` (nothing changed between the settled view and this read).
   On failure: if `req.queue` -> `local.drive_request_requeue(req.id)` and return a new
   constructor `Outcome::deferred(reason)` (`status: "deferred"`, added next to `applied`/`refused`
   at `drive.rs:518`; `run_pass` skips `resolve` for it, drops the track guard, logs the
   outcome `deferred: {wait_reason}`, and Phase 3 records the reason with the "deferred" row of
   the `04-request-pipeline.md` vocabulary: `waiting for @{wt} to be idle (herdr: {status})`, or
   `waiting for @{wt} to settle` when the fresh read is idle but its `state_change_seq` differs
   from the settled view's); else refuse with
   `"@{wt} changed state just before delivery (herdr: {status}); use --queue to wait"`.
3. `let prev = local.drive_delivery_get(wt)?` (for restore). Build `Baseline` from `obs`
   (`delivered_at_ms` = now) and call `local.drive_begin_delivery(wt, &b)`; `None` -> refused
   `"@{wt} is not driven"`.
4. Type the prompt exactly as today. On any failure -> `local.drive_restore_delivery(&prev)` and
   refuse with today's texts.
5. On success: `env.runtime.mark_sent(project, wt, delivery_id, b.state_change_seq)`,
   `env.engine.note_delivery(&key)` (adds to `awaited`, forcing hot mode), log at info
   `drive: delivered to @{wt} delivery {delivery_id} (herdr {status} scs {n} cs {m})`, and return
   `Outcome::applied(json!({"awaiting_turn": turn, "delivery_id": delivery_id}).to_string())`.

`send_settling` (02 S9) additionally releases its hold when
`local.drive_turn_for_delivery(delivery_id)` is `Some`.

### T5. Engine turn tracking (`src-tauri/src/engine/turns.rs`, exact)

Evidence: `ProjectEvidence` (02 S4) gains `deliveries: Vec<Delivery>` (from
`drive_deliveries()`), `driven: HashSet<String>` (from `drives_list()`), and
`replies: HashMap<String, SessionReply>` (from `session_reply_get` per driven worktree), all read
through the same cached read-only handle.

Per driven key, in `EngineState`:

```rust
pub struct TurnMemory {
    pub ledger: Option<(Option<u64>, u64)>,         // (completion_seq, state_change_seq) of the last recorded settled completion
    pub ledger_reply_turn: i64,                      // session_replies.turn when the ledger was last moved
    pub active_since_ledger: bool,                   // working/blocked seen since the ledger moved
    pub delivery: Option<DeliveryTrack>,
    pub pending_reply: Option<PendingReply>,         // a finished turn waiting up to 3 s for the hook reply
    pub ledger_moved_ms: i64,                        // wall ms the ledger last moved (human-turn ReplyQuery bound)
    pub recent: VecDeque<CompletionNote>,            // last 10 recorded completions (Phase 6 input)
    pub no_activity: Option<(i64, i64)>,             // (delivery_id, wall ms) of a 30 s no-activity delivery (Phase 6 input)
}
pub struct DeliveryTrack { pub delivery_id: i64, pub baseline: Baseline, pub awaiting_turn: i64,
    pub active_seen: bool, pub absent_since: Option<Instant> }
pub struct PendingReply { pub kind: TurnKind, pub delivery_id: Option<i64>, pub clause: Clause,
    pub detected_at: Instant, pub identity: (Option<u64>, u64), pub reply_turn_at_detection: i64,
    pub transcript: TranscriptState }               // NotStarted | InFlight | Done(Result<TurnReply, &'static str /* miss reason */>)
pub enum TurnVerdict { Pending, Finished(Clause), Restarted }
pub enum Clause { CompletionSeq, StateChangeSeq, ObservedActive, StopHook } // SQL: completion_seq | state_change_seq | observed_active | stop_hook
pub const REPLY_WAIT: Duration = Duration::from_secs(3);
pub const TRANSCRIPT_AFTER: Duration = Duration::from_secs(1);
```

Pure predicate:

```rust
pub fn evaluate(b: &Baseline, obs: Option<&AgentObs>, settled_idle: bool, active_seen: bool,
                hook_finished: bool, absent_twice: bool) -> TurnVerdict
```

in this order:
1. `hook_finished` (`replies[wt].turn >= awaiting_turn`) -> `Finished(StopHook)`.
2. `obs` is `None` (agent absent) -> `Restarted` if `absent_twice` (absent on two consecutive
   snapshots at least 1 s apart, tracked by `absent_since`), else `Pending`.
3. Occupant check -> `Restarted` if `obs.pane_id != b.pane_id`, or `obs.terminal_id !=
   b.terminal_id`, or both `agent_session`s are `Some` and differ, or `obs.state_change_seq <
   b.state_change_seq` (herdr restarted and its seqs reset). A `None` -> `Some` session is the
   same occupant.
4. `!settled_idle` -> `Pending` (settled idle = the 02 view is `idle` and `settled`, not vetoed by
   a hook row).
5. `obs.completion_seq.is_some() && obs.completion_seq != b.completion_seq` ->
   `Finished(CompletionSeq)`.
6. `obs.state_change_seq >= b.state_change_seq + 2` -> `Finished(StateChangeSeq)`.
7. `active_seen` -> `Finished(ObservedActive)`.
8. `Pending`.

`active_seen` is set when an ingested snapshot taken after `b.delivered_at_ms` shows
`working` or `blocked` for that key. It is in-memory only (a restart loses it; clauses 1 and 2
still apply to a persisted baseline).

Per ingest, for every driven key with evidence (lease owner only writes; a standby instance runs
the same logic without writing and without logging completions):

1. **Load deliveries.** A `Delivery` with `awaiting_turn` and `delivery_id` and a parsable
   `baseline` and no `drive_turn_for_delivery` row becomes/stays `delivery`; a changed
   `delivery_id` replaces it (fresh `active_seen = false`). One without `delivery_id` (delivered by
   an older app) is ignored by the engine (the CLI's legacy path handles it). `awaited` = keys with
   a tracked delivery.
2. **Driver verdict.** `Restarted` -> `drive_turn_insert(kind 'driver', outcome 'restarted',
   signal 'herdr', clause 'occupant_changed', reply '', reply_source 'none')`; log info
   `drive: @{wt} delivery {id} ended: agent restarted mid-turn`. `Finished(StopHook)` -> insert at
   once with `signal 'hook'`, `reply = replies[wt].last_message`, `reply_source 'hook'`.
   `Finished(herdr clause)` -> `pending_reply` (unless already pending).
3. **Ledger (human turns).** On a settled-idle snapshot whose identity `(cs, scs)` differs from
   `ledger`:
   - first settled idle since engine start (`ledger == None`): set the ledger, no event;
   - "is a completion" = `cs.is_some() && cs != ledger.cs` || `scs >= ledger.scs + 2` ||
     `active_since_ledger`; if not a completion, just move the ledger;
   - a completion while a `delivery` is tracked and unresolved is the driver's (step 2 handles it
     on the same snapshot); move the ledger;
   - otherwise it is **human**: `pending_reply` with `kind Human`, `delivery_id None`, the clause
     that matched (cs -> `CompletionSeq`, scs -> `StateChangeSeq`, active -> `ObservedActive`);
     move the ledger. A `session_replies.turn` bump with no herdr completion does not create a
     human turn (hook-only human completions are not detected; documented limit).
   Moving the ledger sets `ledger_reply_turn = replies[wt].turn` and `active_since_ledger =
   false`; any `working`/`blocked` snapshot sets `active_since_ledger = true`.
4. **Reply collection** for a `pending_reply`, evaluated on every ingest (the key is hot while
   a reply is pending, so about once per second):
   - hook first: if `replies[wt].turn > reply_turn_at_detection` (or, for a driver turn,
     `>= awaiting_turn`) -> reply from the hook, `reply_source 'hook'`, insert now;
   - from `detected_at + TRANSCRIPT_AFTER` (1 s): if no transcript lookup is in flight, start one
     with `TranscriptSource::reply(&ReplyQuery)` (`06-reply-fallbacks.md`; the Phase 2 impl is a
     `NoTranscript` that returns `None`) on `tauri::async_runtime::spawn_blocking`, never on the
     status-loop thread; its result is stored in `pending_reply.transcript` and used on the next
     ingest: `Some(r)` -> `r.text`, `reply_source 'transcript'`, insert (a hook reply seen on the
     same ingest still wins);
   - at `detected_at + REPLY_WAIT` (3 s) with neither -> `''`, `'none'`, insert.
   `ReplyQuery.delivered_at_ms` is `Baseline.delivered_at_ms` for a driver turn and, for a human
   turn, the wall ms at which the ledger last moved (the previous settled idle). Insert with
   `signal 'herdr'`. `caveat = Some("agy may still run background tasks")` when the agent label
   is `agy` and `signal == 'herdr'`. Log info
   `drive: @{wt} {kind} turn finished (delivery {id|-}, clause {clause}, reply {reply_source})`.
   The engine never writes `session_replies` (T11), also not with a transcript reply; `read
   --last` finds it through `drive_turn_latest` (T10).
5. **Health record** (Phase 6 input, in memory only): every inserted row pushes
   `CompletionNote { kind, signal, reply_source, transcript_miss: Option<&'static str>, at_ms }`
   onto `TurnMemory.recent` (a `VecDeque`, capped at 10); `transcript_miss` is the
   `06-reply-fallbacks.md` failure reason (`not_found`, `not_this_turn`, `parse_failed`,
   `no_provider_support`) when the transcript step returned nothing. A tracked delivery whose
   view stays settled idle with `state_change_seq`/`completion_seq` equal to the baseline for
   `STALL_NO_ACTIVITY_S` (30 s, the same rule as the CLI stall in T7) sets
   `TurnMemory.no_activity = Some((delivery_id, at_ms))`; a later `working`/`blocked` or
   completion for the key clears it. `SessionEngine::turn_health(&self) -> Vec<(SessionKey,
   TurnHealth { recent: Vec<CompletionNote>, no_activity: Option<(i64, i64)> })>` exposes both
   to `07-self-diagnostics.md` (`completion_without_stop`, `turn_start_not_observed`).
6. Only worktrees with a `drives` row get rows. Nothing is inserted for completions before the
   engine's first snapshot (the ledger starts empty), except through a persisted baseline.

Housekeeping: `housekeeping` (`src-tauri/src/drive.rs:992`) calls `local.drive_turns_prune()`
once per pass.

### T6. CLI context changes (exact)

- `Clock` (`gate.rs:158`) gains `fn wall_ms(&self) -> i64;` (`RealClock`: `SystemTime::now()` since
  `UNIX_EPOCH` in ms; the two fake clocks return a settable base plus elapsed).
- `Ctx` gains `pub project: String` (`config.project_config().project.name`) and
  `pub status_db: Option<PathBuf>` (`Some(keitora_tasks::status_db_path())` in `run_drive`; tests
  pass a temp path or `None`).
- New helper in `keitora-cli/src/drive.rs`:

```rust
pub enum EngineRead { Fresh { view: Option<StatusSession>, writer: WriterRow }, NotReady(WriterRow), Stale { heartbeat_at: Option<i64> } }
fn engine_read(ctx: &Ctx, worktree: &str) -> EngineRead
```

  It opens `StatusReader::open(path)` per evaluation (cheap; a missing file or another schema
  version is `Stale`), applies `keitora_tasks::freshness`, and for `Fresh` reads
  `session(project, worktree)`. A `Fresh` view whose `stale` is 1 is reported as `NotReady`.

### T7. `wait` (exact)

`WaitInput` gains: `delivery: Option<Delivery>`, `unacked: Vec<DriveTurn>` (oldest first),
`engine: EngineRead`, `now_ms: i64`. `gather` fills them with `drive_delivery_get`,
`drive_turns_unacked`, `engine_read` and `ctx.clock.wall_ms()`.

`poll_events`'s `version` closure returns `(i64, i64)`: `(local.data_version(),
status_reader.map_or(0, |r| r.data_version()))`, reopening the reader when it was missing. Any
change in either re-evaluates; `WAIT_REEVAL` (5 s) still forces one (time-based stalls).

`select_event(inputs, stall_after, elapsed_s)` order (ties go to argument order):
1. **prompt**: unchanged.
2. **turn_finished**: the first input whose `unacked` has a `kind == "driver"`, `outcome ==
   "finished"` row (oldest). Else the **legacy** check, only for an input whose `engine` is not
   `Fresh` **or** whose `delivery.delivery_id` is `None`: `awaiting_turn = Some(a)` and
   `reply.turn >= a` -> `turn_finished` built from `session_replies` with `reply_source "hook"`.
3. **human_turn**: the first input whose `unacked` has a `kind == "human"` row (oldest).
4. **send_refused**: unchanged.
5. **stalled**: first match, per input with `awaiting_turn.is_some()`, with every age clamped as
   today (`age.max(0).min(elapsed_s)`):
   - an unacked `outcome == "restarted"` row -> reason `"agent restarted mid-turn; result
     unknown"` (age 0, reported at once);
   - `Fresh` with a view and a `delivery` that has a `baseline`:
     - `view.herdr_status == "working"` -> never stalled;
     - `view.herdr_status == "blocked" && !view.brokered` and `now_ms - view.last_change_at >=
       10_000` -> `"blocked on a dialog nothing brokers"` (seqs compare as `i64`: `Some(b.state_change_seq as i64)`);
     - `view.state == "idle" && view.settled && view.state_change_seq == b.state_change_seq &&
       view.completion_seq == b.completion_seq` and `now_ms - b.delivered_at_ms >= 30_000` ->
       `"no activity observed since delivery"`;
   - `NotReady(writer)`: `since` = `writer.herdr_link_since`, except with link `up` (a stale
     snapshot or a stale row) `since` = `writer.last_snapshot_at` (else `herdr_link_since`);
     `secs = (now_ms - since) / 1000`; when `secs >= stall_after` ->
     `"herdr unavailable for {secs}s ({link})"`, `{link}` = `writer.herdr_link`, or
     `stale snapshot` when it is `up` (the same labels as the wait reasons in
     `04-request-pipeline.md`);
   - `Stale` (and the legacy turn check found nothing): today's rule first (raw row `busy` and
     `session_idle_secs >= stall_after` -> `"no hook activity for {secs}s"`), then
     `"no Keitora app is updating status"` once `elapsed_s >= stall_after`.

Thresholds are constants in `keitora-cli/src/drive.rs`: `STALL_BLOCKED_S = 10`,
`STALL_NO_ACTIVITY_S = 30`.

Before printing, `wait_with` acks:
- a `drive_turns` event -> `drive_turn_ack(id)`; if it returns `false` (another `wait` took it),
  drop the event and keep polling;
- a legacy `turn_finished` -> `drive_ack_turn_legacy(worktree, awaiting)`;
- a `restarted` stall -> `drive_turn_ack(id)` (the delivery is resolved: a re-wait gets exit 4).

"Nothing to wait for" (exit 4) requires, for every listed worktree: no `awaiting_turn`, no pending
prompt for the caller, no open send by the caller, and `drive_turns_unacked` empty. The existing
"last send was refused" check is unchanged.

### T8. Event JSON shapes (exact; legacy keys kept for older drivers)

```jsonc
// exit 0
{"event":"turn_finished","worktree":"bravo","turn_id":12,"delivery_id":7,"kind":"driver",
 "reply":"...","reply_source":"hook|transcript|none","caveat":null,"signal":"herdr|hook",
 "clause":"completion_seq|state_change_seq|observed_active|stop_hook",
 "completed_at":"2026-01-01 10:00:00.123",
 "turn":3,"last_message":"...","updated_at":"2026-01-01 10:00:00.123"}
// legacy path: turn_id/delivery_id/clause null, signal "hook", reply_source "hook",
// turn/last_message/updated_at from session_replies, reply == last_message.
// Non-legacy: "turn" = session_replies.turn or null, "last_message" == "reply", "updated_at" == "completed_at".

// exit 0
{"event":"human_turn","worktree":"bravo","turn_id":13,"reply":"...","reply_source":"...",
 "caveat":null,"clause":"...","completed_at":"..."}

// exit 0
{"event":"stalled","worktree":"bravo","reason":"no activity observed since delivery",
 "state":"idle","idle_s":30,"delivery_id":7}
// "state": the view state (Fresh/NotReady) or the raw row state (Stale), "-" when unknown;
// "idle_s": the clamped condition age; "delivery_id": null when unknown.

// exit 0 (unchanged)
{"event":"prompt","worktree":"bravo","prompt":{...}}
// exit 5 (unchanged)
{"event":"send_refused","worktree":"bravo","request_id":4,"reason":"..."}
// exit 2 (unchanged)
{"event":"timeout","waited_s":500,"worktrees":["bravo"]}
```

Human lines: `@bravo finished its turn` then the reply, or
`(no reply text captured; run: keitora read bravo)` when `reply_source` is `none`, then the caveat
on its own line when set. `@bravo: the human finished a turn` + reply for `human_turn`.
`@bravo looks stalled: {reason}; try: keitora read bravo` for `stalled`.

`DRIVE_GUIDANCE` (`keitora-cli/src/drive.rs:195`) THE LOOP section gains
`human_turn -> the human typed into bravo; read it, then decide` and the `stalled` line becomes
`stalled -> the reason says why; keitora read bravo, then wait again`. The Antigravity sentence
"its turn_finished events carry no reply text" becomes "its turn_finished events may carry no
reply text and a caveat (agy replies are read from its session files when herdr reports the conversation id, see `06-reply-fallbacks.md`)".

### T9. Exit codes (unchanged meanings)

`0` turn_finished, human_turn, prompt, stalled; `2` timeout; `3` no longer the driver; `4` nothing
to wait for / invalid `--stall-after`; `5` send_refused; `6` not found (`read --last` with no turn
and no reply).

### T10. Other CLI verbs (exact)

- **`drives`** (`cmd_drives`): build the state map with
  `overlay_sessions(local.sessions_list()?, fresh_views, &ctx.worktrees, &drivers)` where
  `fresh_views` is `Some(map)` only under `Freshness::Fresh` (from `StatusReader::sessions(project)`).
  New JSON keys per drive: `agent`, `herdr_status`, `stale`, `state_source` (`"engine"` under
  `Fresh`, else `"hooks"`), `delivery_id`, `delivered_at`, `unacked_turns` (count). Human line gains
  ` [stale]` when stale. A1: a driven worktree with no row and an idle view shows `idle`.
- **`read --last`**: candidates are `drive_turn_latest(wt)` (any freshness: `drive_turns` is
  durable) and `session_reply_get(wt)`; pick the newer by timestamp (`completed_at` vs `updated_at`,
  both UTC text, compared after parsing to `(date, time)`; a tie picks the turn row). Neither ->
  exit 6 as today. Output keeps `worktree`, `source` (`"turn"` or `"last_message"`), `text`,
  `truncated`, `turn`, `updated_at`, and adds `reply_source`, `turn_id`, `kind`. A turn row with
  `reply_source "none"` prints the hint line instead of an empty body (exit 0). It never acks.
- **`task watch`** (`cmd_task_watch`): sessions = `overlay_sessions(local.sessions_list()?,
  fresh_views, &config.worktrees, &drivers)`, with `StatusReader` reopened each poll. The doc
  comment "Session states are what the agent hooks last wrote" changes to "...the app's engine
  view while it is fresh, else what the hooks last wrote".
- **Gate** (`gate.rs`): a new `pub struct GateDb { pub local: LocalStore, pub project: String,
  pub status: Option<StatusReader>, pub now_ms: i64 }` implements `GateStore` by delegating every
  method to `local` except `session_state(worktree)`: under `Fresh`, a row with `stale == 0` ->
  `Some("offline")` when `agent == "absent"`, else `Some(view.state)`; no row -> `Some("offline")`;
  otherwise (not `Fresh`, or the row is stale) -> `local.session_state_get(worktree)`.
  `gate_output` builds `GateDb` (project from `config.project_config().project.name`, status from
  `StatusReader::open(&status_db_path())`) and passes it to `run_gate`. `stop_reason` and the
  driver check at `gate.rs:677` use it unchanged through the trait. `impl GateStore for LocalStore`
  stays for tests.

### T11. Compatibility matrix

| CLI | App | Behaviour |
|---|---|---|
| old | new | `wait` uses `session_replies` (Codex without Stop still hangs there, as before); `drive_ack_turn` clears `awaiting_turn`. Engine rows it wrote stay unacked and are pruned after 7 days; a later new CLI may deliver one stale `turn_finished` for them once (accepted). |
| new | old / none | `status.db` missing or stale -> legacy path for completion and stalls; `drives`/`task watch`/gate read raw rows. |
| new | new, fresh | `drive_turns` only for deliveries with a `delivery_id`; legacy check only for a delivery an older app made (`delivery_id` NULL). Never both for one delivery (`drive_ack_turn_legacy` also acks the row; `drive_turn_insert` requires the delivery to be outstanding). |
| new | new, herdr down | `NotReady`: unacked rows still delivered; legacy hook check also applies; stall "herdr unavailable" after `--stall-after`. |

The engine never writes `session_replies` (hooks own it, D8).

### T12. Required tests (names and assertions)

`crates/keitora-tasks/src/drive_store_tests.rs`:
- `init_schema_adds_delivery_columns_to_an_old_drives_table` (rows and `awaiting_turn` kept).
- `read_only_handle_on_an_old_db_reads_drives_without_delivery_columns`
  (`drive_delivery_get` -> `None`, `drives_list` works).
- `begin_delivery_sets_awaiting_turn_and_a_monotonic_delivery_id` (1, 2; after release + rebind
  the next id is above every id in `drive_turns`).
- `restore_delivery_restores_all_four_columns`.
- `turn_insert_dedups_by_delivery_id_and_requires_an_outstanding_delivery`.
- `turn_ack_is_once_and_clears_awaiting_turn_by_delivery_id` (second ack `false`; an ack for an
  older delivery leaves a newer `awaiting_turn`).
- `legacy_ack_also_acks_the_engine_row`.
- `prune_keeps_the_newest_50_and_recent_rows` (60 old acked rows -> 50; a 6-day-old acked row
  kept; an 8-day-old unacked row outside the newest 50 deleted).
- `requeue_returns_a_running_request_to_pending`.
- `drive_turns_ddl_checks_match_the_constants`.

`src-tauri/src/engine/turns.rs` (pure `evaluate`):
- `clause1_completion_seq_change_finishes` (4 -> 5; `None` -> 3; 4 -> `None` does not).
- `clause1_any_difference_counts` (5 -> 2 finishes).
- `clause2_state_change_seq_plus_two_finishes` (+1 does not).
- `clause3_observed_active_then_settled_idle_finishes`.
- `unsettled_or_vetoed_idle_never_finishes`.
- `stop_hook_finishes_even_while_herdr_is_busy`.
- `occupant_change_is_restarted` (pane, terminal, session `Some`->other `Some`; `None`->`Some` is
  not).
- `herdr_restart_with_reset_seqs_is_restarted`.
- `absent_once_is_pending_absent_twice_is_restarted`.

`src-tauri/src/engine/tests.rs` (FakeEvidence + temp `tasks.db` as the writer target):
- `b_codex_delivery_without_stop_finishes_on_completion_seq`: baseline cs 4; ingest cs 5 idle at
  t, again at t+1 s -> after `REPLY_WAIT` one `drive_turns` row `reply_source 'none'`, `clause
  'completion_seq'`.
- `herdr_completion_waits_3s_for_the_stop_reply` (reply row bumped at t+2 s -> `reply_source
  'hook'`, text kept).
- `stop_hook_first_wins_with_signal_hook` (and no second row when herdr fires later).
- `persisted_baseline_resolves_after_an_app_restart` (fresh engine, delivery in evidence, two
  snapshots with a changed cs -> row).
- `agy_herdr_only_completion_carries_the_caveat`.
- `human_turn_when_nothing_is_outstanding`.
- `second_completion_after_the_driver_turn_is_human` (the queued mid-turn message).
- `completion_while_the_send_is_queued_undelivered_is_human`.
- `no_events_for_completions_before_the_first_snapshot`.
- `undriven_worktree_records_nothing`.
- `standby_instance_writes_no_turns`.
- `occupant_change_writes_a_restarted_row`.
- `no_change_no_turn_writes` (N ingests of the same snapshot -> 0 rows, `data_version` unchanged).
- `transcript_reply_used_after_1s_when_no_hook` (fake `TranscriptSource`; hook at the same
  ingest still wins; `reply_source 'transcript'`; `session_replies` untouched).
- `turn_health_records_completions_and_no_activity` (ring capped at 10; `no_activity` set at
  30 s, cleared by `working`).

`src-tauri/src/drive_tests.rs`:
- `send_captures_a_fresh_baseline_and_persists_it` (FakeBackend `set_agent`; `baseline_json`
  matches; `delivery_id` in the result JSON).
- `send_requeues_a_queued_send_when_the_fresh_read_is_not_idle`.
- `send_refuses_a_direct_send_when_the_fresh_read_is_not_idle`.
- `failed_typing_restores_the_previous_delivery`.
- `settling_released_by_a_turn_row`.

`crates/keitora-cli/src/drive_tests.rs` (temp `tasks.db` + `write_status_for_test` + FakeClock
with `wall_ms`):
- `repro_b_codex_answer_without_stop_returns_turn_finished` (written RED first; steps in
  `00-overview.md` "Reproductions": baseline cs 4, `working`, idle with cs 5, no Stop row ever,
  `wait` -> `turn_finished`, `reply_source: none`, one hot tick).
- `b_turn_row_is_delivered_as_turn_finished_with_reply_source_none`.
- `stall_after_30s_with_no_seq_change`; `no_stall_at_29s`.
- `working_for_10_minutes_never_stalls`.
- `blocked_unbrokered_for_10s_stalls`; `brokered_blocked_is_a_prompt_not_a_stall`.
- `restarted_row_is_a_stall_and_resolves_the_delivery` (re-wait -> exit 4).
- `herdr_unavailable_stalls_after_stall_after`.
- `no_app_uses_the_legacy_hook_turn_then_stalls_with_no_app`.
- `fresh_app_ignores_session_replies_for_a_new_delivery` (no double delivery).
- `legacy_check_applies_to_a_delivery_without_delivery_id`.
- `human_turn_is_delivered_once`.
- `event_priority_prompt_turn_human_refused_stalled`.
- `nothing_to_wait_for_requires_no_unacked_turn`.
- `a_lost_ack_race_keeps_polling`.
- `drives_reads_engine_state_when_fresh` (A1: no row + idle view -> `idle`, `state_source
  "engine"`); `drives_falls_back_to_rows_when_stale` (`state_source "hooks"`).
- `read_last_picks_the_newer_of_turn_and_reply`; `read_last_none_reply_prints_the_hint`.
- `turn_finished_json_keeps_the_legacy_keys`.

`crates/keitora-cli/src/gate_tests.rs`:
- `gate_db_driver_absent_by_engine_is_offline_when_fresh`.
- `gate_db_driver_present_by_engine_although_row_offline` (A1-like).
- `gate_db_falls_back_to_the_row_when_stale`.
- `stop_reason_uses_the_engine_view_when_fresh`.

`crates/keitora-cli/src/main.rs`:
- `task_watch_overlays_engine_state_when_fresh`.
