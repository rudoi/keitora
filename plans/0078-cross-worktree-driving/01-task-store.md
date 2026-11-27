# 0078-01 task store (`crates/keitora-tasks`)

Part of `plans/0078-cross-worktree-driving/`. See `00-overview.md` for goal and glossary.

**Dependencies:** none; this is the foundation. Consumed by 02 (CLI), 03 (hook broker), 04 (app
runner reads/writes the same rows, adds `session_revision`), 05 (model fields).

Placeholder names only in code, tests and docs: driver `alpha`, driven `bravo`/`charlie`. No task or
plan identifiers in source or commit messages (`AGENTS.md`).

## Principle

Extend `crates/keitora-tasks`; do not fork the logic. `init_schema`
(`crates/keitora-tasks/src/sqlite_backend.rs:67`) has no versioning or `ALTER` precedent: every table
is `CREATE TABLE IF NOT EXISTS`, run by every CLI and hook open (`LocalStore::open` and
`SqliteBackend::open` both call it). So all new state is **new tables**, which existing DBs gain on
next open with no migration step. Avoiding new columns on `worktree_sessions` also keeps
`LocalStore::session_upsert`'s upsert from clobbering them.

Do **not** change any existing table's DDL.

## Where the code lives (mirror `dispatch_requests`)

| Concern | Existing home | Drive home |
|---|---|---|
| DDL | inline in `init_schema` (`sqlite_backend.rs:150-161`) | new `const DRIVE_DDL: &str` in new `crates/keitora-tasks/src/drive.rs`, executed from `init_schema` right after the `dispatch_requests` block: `conn.execute_batch(crate::drive::DRIVE_DDL)?;` |
| Model structs and status constants | `model.rs` (`DispatchRequest`, `DISPATCH_REQUEST_STATUSES`, `DISPATCH_REQUEST_OUTCOMES`) | `drive.rs` (types, constants, pure helpers); re-exported from `lib.rs` next to `pub use model::{...}` |
| Store methods | synchronous `impl LocalStore` in `local_store.rs` (`dispatch_request_add/get/begin/resolve/cancel`, `dispatch_requests_pending/list/expire`) returning `crate::Result<T>` (concrete `crate::Error`) | a second `impl LocalStore` block in new `crates/keitora-tasks/src/drive_store.rs`. This needs `LocalStore.conn` to become `pub(crate) conn` (one-word change at `local_store.rs:25`). Add `mod drive; mod drive_store;` to `lib.rs` |

They are **not** on the async `TaskBackend` trait: that trait is for the (possibly remote) task
backend; drive state is always local, exactly like `dispatch_requests` and `worktree_sessions`. No
change to `TaskBackend`, `SqliteBackend` or the `Store` facade.

Errors: all methods return `crate::Result<T>`. A rule violation is `Error::Conflict(String)` with a
human sentence (no new `Error` variant; the CLI maps `Conflict` to exit 4, see 02). `Error::NotFound`
is task-specific (`NotFound(i64)`) and is not used.

Timestamps: text `datetime('now')` (UTC, `YYYY-MM-DD HH:MM:SS`), same as every existing table.
Durations compare with `datetime('now', '-N seconds')`, as `dispatch_requests_expire` does.

## Constants (in `drive.rs`, all `pub`)

```rust
pub const DRIVE_PROMPT_KINDS: [&str; 2] = ["permission", "question"];
pub const DRIVE_PROMPT_STATUSES: [&str; 7] =
    ["pending", "allowed", "denied", "answered", "escalated", "expired", "withdrawn"];
pub const DRIVE_REQUEST_KINDS: [&str; 4] = ["send", "read", "reset", "swap"];
pub const DRIVE_REQUEST_STATUSES: [&str; 5] =
    ["pending", "running", "pending_confirm", "applied", "refused"];
pub const DRIVE_REQUEST_OUTCOMES: [&str; 2] = ["applied", "refused"];
pub const DRIVE_DEFAULT_ESCALATE_AFTER_S: i64 = 300;
pub const DRIVE_ESCALATE_AFTER_MIN_S: i64 = 1;
pub const DRIVE_ESCALATE_AFTER_MAX_S: i64 = 540;     // the gate's cap; keeps it inside the fixed 600 s hook timeout (03)
pub const DRIVE_HOOK_TIMEOUT_S: i64 = 600;           // fixed hook `timeout` for both providers (03: GATE_HOOK_TIMEOUT_S); documentation only
pub const DRIVE_PROMPT_STALE_SLACK_S: i64 = 30;      // a pending prompt is stale after timeout_s + this (always < DRIVE_HOOK_TIMEOUT_S)
pub const DRIVE_PERMISSION_INPUT_MAX: usize = 4096;  // chars of tool_input kept, permission kind
pub const DRIVE_QUESTION_INPUT_MAX: usize = 16384;   // chars of tool_input kept, question kind
pub const DRIVE_SUMMARY_MAX: usize = 200;            // chars
pub const SESSION_REPLY_MAX: usize = 32 * 1024;      // bytes of last_message kept
pub const DRIVE_SEND_TEXT_MAX: usize = 32 * 1024;    // bytes of a send payload
pub const DRIVE_READ_RESULT_MAX: usize = 64 * 1024;  // bytes of a read result
pub const DRIVE_READ_LINES_DEFAULT: i64 = 200;
pub const DRIVE_READ_LINES_MAX: i64 = 2000;
pub const DRIVE_REQUEST_MAX_AGE_SECS: i64 = 600;     // = the app's REQUEST_MAX_AGE_SECS (dispatch.rs:512)
pub const DRIVE_RETENTION_SECS: i64 = 86_400;
```

Pure helpers: `pub fn truncate_chars(s: &str, max: usize) -> String` (cut on a char boundary, no
marker) and `pub fn truncate_bytes(s: &str, max: usize) -> String` (cut at the last char boundary at
or below `max` bytes).

## Tables (exact DDL, in `DRIVE_DDL`)

```sql
-- Who drives whom. One row per driven worktree: a worktree has at most one driver.
CREATE TABLE IF NOT EXISTS drives (
  driven           TEXT PRIMARY KEY,
  driver           TEXT NOT NULL,
  since            TEXT NOT NULL DEFAULT (datetime('now')),
  forced_from      TEXT,                                   -- previous driver after a --force steal
  paused           INTEGER NOT NULL DEFAULT 0 CHECK(paused IN (0, 1)),
  escalate_after_s INTEGER NOT NULL DEFAULT 300 CHECK(escalate_after_s BETWEEN 1 AND 540),
  awaiting_turn    INTEGER,                                -- NULL: no send outstanding
  CHECK(driver <> driven)
);
CREATE INDEX IF NOT EXISTS idx_drives_driver ON drives(driver);

-- One row per brokered prompt (permission request or AskUserQuestion).
CREATE TABLE IF NOT EXISTS drive_prompts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  driven        TEXT NOT NULL,
  driver        TEXT NOT NULL,                             -- copied from drives at insert
  provider      TEXT NOT NULL,                             -- free text ("claude", "codex", ...): no CHECK
  kind          TEXT NOT NULL CHECK(kind IN ('permission', 'question')),
  tool_name     TEXT NOT NULL DEFAULT '',
  tool_input    TEXT NOT NULL DEFAULT '',                  -- JSON text, truncated (see constants)
  summary       TEXT NOT NULL DEFAULT '',                  -- one line for humans, <= 200 chars
  status        TEXT NOT NULL DEFAULT 'pending'
                CHECK(status IN ('pending','allowed','denied','answered','escalated','expired','withdrawn')),
  decision_json TEXT,                                      -- decision payload (below); NULL when none
  decided_by    TEXT,                                      -- 'driver:<wt>' | 'human' | 'timeout' | 'system'
  reason        TEXT NOT NULL DEFAULT '',
  timeout_s     INTEGER NOT NULL,                          -- drives.escalate_after_s at insert time
  hook_pid      INTEGER,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  decided_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_drive_prompts_driven_status ON drive_prompts(driven, status);
CREATE INDEX IF NOT EXISTS idx_drive_prompts_driver_status ON drive_prompts(driver, status);

-- Pane-affecting requests from the CLI that the app applies. Separate from dispatch_requests
-- because that table's task_id is NOT NULL.
CREATE TABLE IF NOT EXISTS drive_requests (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  kind         TEXT NOT NULL CHECK(kind IN ('send', 'read', 'reset', 'swap')),
  worktree     TEXT NOT NULL,                              -- the driven worktree
  requested_by TEXT NOT NULL,                              -- the driver
  payload      TEXT NOT NULL DEFAULT '{}',                 -- JSON object, shape per kind (below)
  queue        INTEGER NOT NULL DEFAULT 0 CHECK(queue IN (0, 1)),
  status       TEXT NOT NULL DEFAULT 'pending'
               CHECK(status IN ('pending','running','pending_confirm','applied','refused')),
  reason       TEXT NOT NULL DEFAULT '',
  result       TEXT NOT NULL DEFAULT '',                   -- JSON or text, set at resolve (below)
  created_at   TEXT NOT NULL DEFAULT (datetime('now')),
  resolved_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_drive_requests_status ON drive_requests(status);

-- Last finished turn per worktree. A side table: not columns on worktree_sessions.
CREATE TABLE IF NOT EXISTS session_replies (
  worktree     TEXT PRIMARY KEY,
  turn         INTEGER NOT NULL DEFAULT 0,
  last_message TEXT NOT NULL DEFAULT '',
  updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
```

`drive_request` payload shapes (validated by `drive_request_add`; JSON object required; unknown keys
ignored) and `result` contents:

| kind | `payload` | `result` on `applied` |
|---|---|---|
| `send` | `{"text": "<1..=32768 bytes>"}` | `{"awaiting_turn": <int>}` |
| `read` | `{"lines": <1..=2000>}` (200 when absent) | the pane tail as plain text, at most 64 KB; the app truncates from the **start**, keeping the end |
| `reset` | `{"to": "<provider>"}` or `{}` (no provider change) | `{"provider": "<provider the pane now runs>"}` |
| `swap` | `{"to": "<provider>"}` (required) | `{"provider": "<provider the pane now runs>"}` |

`decision_json` (what the gate (03) reads back):

| status | `decision_json` |
|---|---|
| `allowed` | `{"behavior":"allow"}` |
| `denied` | `{"behavior":"deny","message":"<reason>"}` |
| `answered` | `{"answers":{"<question text>":"<value>", ...}}` |
| `escalated`, `expired`, `withdrawn` | `NULL` (the `reason` column carries the text) |

## Model types (in `drive.rs`)

All derive `Debug, Clone, PartialEq, Serialize, Deserialize` unless noted.

```rust
pub struct Drive {
    pub driven: String, pub driver: String, pub since: String,
    pub forced_from: Option<String>, pub paused: bool,
    pub escalate_after_s: i64, pub awaiting_turn: Option<i64>,
}
pub struct DrivePrompt {
    pub id: i64, pub driven: String, pub driver: String, pub provider: String,
    pub kind: String, pub tool_name: String, pub tool_input: String, pub summary: String,
    pub status: String, pub decision_json: Option<String>, pub decided_by: Option<String>,
    pub reason: String, pub timeout_s: i64, pub hook_pid: Option<i64>,
    pub created_at: String, pub decided_at: Option<String>,
}
pub struct NewDrivePrompt {            // input to drive_prompt_insert; derive Debug, Clone only
    pub driven: String, pub provider: String, pub kind: String, pub tool_name: String,
    pub tool_input: String,            // full text; the store truncates
    pub summary: String,               // the store truncates
    pub hook_pid: Option<i64>,
}
pub struct DriveRequest {
    pub id: i64, pub kind: String, pub worktree: String, pub requested_by: String,
    pub payload: String, pub queue: bool, pub status: String, pub reason: String,
    pub result: String, pub created_at: String, pub resolved_at: Option<String>,
}
impl DriveRequest { pub fn is_resolved(&self) -> bool } // status in DRIVE_REQUEST_OUTCOMES
pub struct SessionReply { pub worktree: String, pub turn: i64, pub last_message: String, pub updated_at: String }
pub struct DriveBind { pub drive: Drive, pub previous_driver: Option<String>, pub escalated_prompts: usize }
pub enum PromptDecision {              // derive Debug, Clone, PartialEq only
    Allow,
    Deny { reason: String },                        // empty reason -> stored as "denied by driver"
    Answer { answers: std::collections::BTreeMap<String, String> },
    Escalate { reason: String },                    // must be non-empty
}
```

## `impl LocalStore` API (in `drive_store.rs`)

All are `pub fn name(&self, ...) -> Result<...>`. "Tx" means one `BEGIN IMMEDIATE` transaction
(`rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)`; `&self` only,
like the rest of `LocalStore`).

**Drives**

| Signature | Behaviour |
|---|---|
| `drive_bind(&self, driver: &str, driven: &str, escalate_after_s: Option<i64>, force: bool) -> Result<DriveBind>` | Tx. Checks, in this order, each failure `Conflict`: (1) `driver == driven` -> "a worktree can't drive itself"; (2) `escalate_after_s` outside 1..=540 -> "escalate-after must be 1..=540 seconds"; (3) `driver` is the `driven` of any row -> "@alpha is driven by @x; drives are one level only"; (4) `driven` is the `driver` of any row -> "@bravo drives other worktrees; drives are one level only"; (5) existing row for `driven` with a **different** driver and `!force` -> "@bravo is already driven by @x (use --force to take over)". Then: no row -> insert (`escalate_after_s` default 300, `paused` 0, `awaiting_turn` NULL, `forced_from` NULL). Row with the **same** driver -> idempotent: update `escalate_after_s` only when `Some`, keep everything else, `escalated_prompts` 0. Row with a different driver and `force` -> escalate all of `driven`'s pending prompts (`decided_by='system'`, reason `"drive taken over by @alpha"`), then set `driver`, `forced_from = old driver`, `since = now`, `paused = 0`, `awaiting_turn = NULL`, `escalate_after_s` = given or 300. Returns the row, `previous_driver` (Some only on a steal) and the escalated count. |
| `drive_get(&self, driven: &str) -> Result<Option<Drive>>` | one row |
| `drives_list(&self) -> Result<Vec<Drive>>` | all rows, `ORDER BY driven` |
| `drives_by_driver(&self, driver: &str) -> Result<Vec<Drive>>` | `ORDER BY driven` |
| `drive_release(&self, driver: &str, driven: &str) -> Result<usize>` | Tx. Deletes the row only if `driver` matches, else `Conflict("@bravo isn't driven by @alpha")`. Escalates its pending prompts (`system`, reason `"released by @alpha"`). Returns how many prompts were escalated. |
| `drive_release_all(&self, driver: &str) -> Result<Vec<(String, usize)>>` | Tx. Releases every worktree driven by `driver`; returns `(driven, escalated_prompts)` ordered by name; empty when none (not an error). |
| `drive_end(&self, driven: &str) -> Result<usize>` | Human release (rail, 04): like `drive_release` without the driver check; reason `"released by you"`. `Conflict` if no row. |
| `drive_set_paused(&self, driven: &str, paused: bool) -> Result<usize>` | Human pause/resume (04/05; no CLI verb). `Conflict` if no row. Pausing escalates pending prompts (`system`, reason `"driver paused"`) and returns that count; resuming returns 0. Setting the current value again is a no-op returning 0. |
| `drive_take_over(&self, driven: &str) -> Result<usize>` | Human Take over: marks pending prompts `escalated` (`decided_by='human'`, reason `"taken over by you"`). The binding stays. Returns the count; a missing row returns 0 (no error). |
| `drive_set_awaiting_turn(&self, driven: &str) -> Result<Option<i64>>` | Called by the app (04) when it applies a `send`. One statement: `awaiting_turn = COALESCE((SELECT turn FROM session_replies WHERE worktree = ?), 0) + 1`. Returns the new value; `None` when `driven` has no row. |
| `drive_restore_awaiting_turn(&self, driven: &str, value: Option<i64>) -> Result<bool>` | Called by the app (04). Sets `awaiting_turn` to exactly `value` (`None` = `NULL`). Used to restore the previous value when a `send` fails after `drive_set_awaiting_turn`, and to clear it (`None`) when a `reset` or `swap` succeeds (the old turn no longer exists). `true` when a row was updated, `false` when `driven` has no row. |
| `drive_ack_turn(&self, driven: &str, turn: i64) -> Result<bool>` | Called by `wait` (02) after it delivers `turn_finished`: `UPDATE drives SET awaiting_turn = NULL WHERE driven = ? AND awaiting_turn = ?`. `true` if cleared. The equality guard means a newer `send` (larger value) is never cleared by a stale ack. |

**Prompts**

| Signature | Behaviour |
|---|---|
| `drive_prompt_insert(&self, new: &NewDrivePrompt) -> Result<DrivePrompt>` | Tx. First runs `drive_prompts_expire_stale`. `Conflict` when `driven` has no `drives` row ("not driven") or `paused = 1` ("drive is paused"); the gate (03) treats any error as escalate. Validates `kind`. Copies `driver` and `timeout_s = escalate_after_s` from the row. Truncates `tool_input` (4096 chars for `permission`, 16384 for `question`) and `summary` (200). Returns the inserted row. |
| `drive_prompt_get(&self, id: i64) -> Result<Option<DrivePrompt>>` | one row, raw (the gate polls this; staleness is handled as below) |
| `drive_prompts_pending(&self, driver: Option<&str>, driven: Option<&str>) -> Result<Vec<DrivePrompt>>` | `status = 'pending'` and **not stale**, optional filters, `ORDER BY id`. |
| `drive_pending_prompt_counts(&self) -> Result<std::collections::HashMap<String, usize>>` | Pending, **not stale** prompts per `driven` worktree (worktrees with none are absent). One grouped query, for the app's activity poll (04). |
| `drive_prompt_decide(&self, id: i64, by_driver: &str, decision: &PromptDecision) -> Result<DrivePrompt>` | Tx. Loads the row and its drive. `Conflict` cases (message in quotes): no such id -> "no prompt N"; `driver != by_driver` -> "prompt N belongs to @x's drive"; drive paused -> "drive is paused"; status not `pending` -> "prompt N is already <status>"; pending but stale -> mark it `expired`, then "prompt N expired"; kind/decision mismatch (`Allow` on `question`, `Answer` on `permission`) -> "allow applies to permission prompts; use answer for questions" / "answer applies to question prompts only"; `Escalate` with empty reason -> "escalate needs a reason". Otherwise CAS (`WHERE id = ? AND status = 'pending'`) and set `status` (`Allow`->`allowed`, `Deny`->`denied`, `Answer`->`answered`, `Escalate`->`escalated`), `decision_json` per the table above, `decided_by = 'driver:<by_driver>'`, `reason`, `decided_at`. Returns the updated row. |
| `drive_prompt_close(&self, id: i64, status: &str, decided_by: &str, reason: &str) -> Result<bool>` | For the gate (timeout, driver offline) and the system: CAS `pending` -> `status`, where `status` must be `escalated`, `expired` or `withdrawn` (else `Conflict`). `decision_json` stays NULL. `true` when this call changed the row. |
| `drive_prompts_withdraw(&self, worktree: &str) -> Result<usize>` | `pending` -> `withdrawn` (`decided_by='system'`, reason `"session state changed"`) for `driven = worktree`. Returns the count. |
| `drive_prompts_expire_stale(&self) -> Result<usize>` | Marks stale pending prompts `expired` (`decided_by='timeout'`, reason `"hook gone: no decision recorded"`). |

*Stale* = `status = 'pending' AND created_at < datetime('now', '-' || (timeout_s + 30) || ' seconds')`
(the gate polls only until `timeout_s` and exits, and `timeout_s <= 540` keeps it inside the fixed 600 s
hook timeout, so a pending row older than `timeout_s + 30` has no live hook). Every **reader** of pending prompts (`drive_prompts_pending`, `sessions_list`'s `brokered`
flag, `drive_prompt_decide`) applies the predicate in SQL, so a stale row never looks live even before
a writer rewrites it. `drive_prompts_expire_stale` is the writer; the app's expiry pass (04) and
`drive_prompt_insert` call it.

**Requests**

| Signature | Behaviour |
|---|---|
| `drive_request_add(&self, kind: &str, worktree: &str, requested_by: &str, payload: &serde_json::Value, queue: bool) -> Result<DriveRequest>` | Tx. `Conflict` when: `kind` unknown; payload invalid for the kind (table above; the message names the field); `queue` on a kind other than `send`; `worktree` has no drive ("not driven"); the drive's driver is not `requested_by` ("not the driver of @bravo"); the drive is paused ("drive is paused"). Returns the `pending` row. |
| `drive_request_get(&self, id: i64) -> Result<Option<DriveRequest>>` | one row |
| `drive_requests_pending(&self) -> Result<Vec<DriveRequest>>` | `status = 'pending'`, `ORDER BY id` (the app's execution order) |
| `drive_requests_list(&self, limit: i64) -> Result<Vec<DriveRequest>>` | newest first |
| `drive_request_begin(&self, id: i64) -> Result<bool>` | CAS `pending` -> `running`; `true` when this call won (copy of `dispatch_request_begin`). |
| `drive_request_await_confirm(&self, id: i64) -> Result<bool>` | CAS `running` -> `pending_confirm` (a `swap` showing its confirm to the human). |
| `drive_request_resume(&self, id: i64) -> Result<bool>` | CAS `pending_confirm` -> `running` (the human accepted the confirm; the swap now runs). |
| `drive_requests_in_flight(&self) -> Result<Vec<DriveRequest>>` | `status IN ('running','pending_confirm')`, `ORDER BY id`. For the app's housekeeping (04): rows no live task is tracking are refused with `Keitora restarted before this finished`. |
| `drive_request_resolve(&self, id: i64, status: &str, reason: &str, result: &str) -> Result<()>` | `status` must be in `DRIVE_REQUEST_OUTCOMES` else `Conflict`. `UPDATE ... WHERE id = ? AND status IN ('pending','running','pending_confirm')`; stamps `resolved_at`; stores `reason` and `result`. Already resolved -> `Conflict("drive request N is already <status>")`; missing -> `Conflict("no drive request N")` (same as `dispatch_request_resolve`). |
| `drive_request_cancel(&self, id: i64, reason: &str) -> Result<bool>` | `pending` -> `refused` only (the requester stopped waiting); `true` when withdrawn. |
| `drive_requests_expire(&self, max_age_secs: i64) -> Result<usize>` | `pending` older than `max_age_secs` -> `refused`, reason `"expired: no running Keitora app picked it up in time"`; `pending_confirm` older than `max_age_secs` -> `refused`, reason `"expired: the confirm wasn't answered in time"`. Returns the total. `running` rows are never expired by age. |

**Session replies, sessions, polling, purge**

| Signature | Behaviour |
|---|---|
| `session_reply_record(&self, worktree: &str, last_message: &str) -> Result<i64>` | Upsert: `turn = turn + 1` (first row gets 1), `last_message = truncate_bytes(last_message, 32768)`, `updated_at = now`. Returns the new turn. Called by `keitora session idle --stdin` and `StopFailure` (03; the caller chooses the fixed error text). |
| `session_reply_get(&self, worktree: &str) -> Result<Option<SessionReply>>` | one row |
| `session_idle_secs(&self, worktree: &str) -> Result<Option<i64>>` | Seconds since `worktree_sessions.last_heartbeat` (`CAST(strftime('%s','now') - strftime('%s', last_heartbeat) AS INTEGER)`); `None` if no row. For `wait`'s `stalled` check. |
| `data_version(&self) -> Result<i64>` | `PRAGMA data_version`. Changes only when **another** connection commits, which is the hook/CLI-vs-app situation. Poll loops (hook, `wait`) use it to skip the real `SELECT`. |
| `drive_purge(&self, max_age_secs: i64) -> Result<usize>` | Deletes `drive_requests` with `status IN ('applied','refused') AND resolved_at < datetime('now', '-N seconds')` and `drive_prompts` with `status <> 'pending' AND decided_at < ...`. The app passes `DRIVE_RETENTION_SECS`. `session_replies` and `drives` are never purged. Returns total rows deleted. |

**Changes to existing `LocalStore` methods**

- `session_upsert` (`local_store.rs:100`): after the upsert succeeds, if `state != "waiting"` call
  `self.drive_prompts_withdraw(worktree)?`. The gate marks `waiting` itself, so it must not withdraw
  its own prompt.
- `session_set_offline` (`local_store.rs:124`): same withdrawal after the write.
- `sessions_list` (`local_store.rs:171`): see "Model additions".
- `open_read_only` handles do not create tables (test `open_read_only_does_not_create_missing_tables`), so
  a DB that predates the drive tables has none of them. The **read paths must tolerate that**: a private
  helper `fn drive_tables_exist(&self) -> bool` checks `sqlite_master` for `drives` and `drive_prompts`
  (all four drive tables are created together). Behaviour when they are missing:
  - `sessions_list` runs its pre-existing query and fills `driven_by = None`, `brokered = false`,
    `escalated_reason = None`;
  - `session_revision` returns the pre-existing string (no driver/prompt suffix);
  - `drive_get` -> `None`; `drives_list`, `drives_by_driver`, `drive_prompts_pending`,
    `drive_requests_pending`, `drive_requests_in_flight`, `drive_pending_prompt_counts` -> empty.
  Write methods are not special-cased (they run on read-write handles, whose `open` created the tables).
  A genuine error (locked DB, corruption) still propagates; only the missing table is tolerated.

## State machines

**Prompt status.** `pending` is the only non-terminal status. All transitions are CAS on
`status = 'pending'`; terminal statuses are final.

| From | To | Who / when | `decided_by` |
|---|---|---|---|
| pending | allowed / denied / answered / escalated | `drive_prompt_decide` | `driver:<by_driver>` |
| pending | escalated | `drive_bind --force`, `drive_release*`, `drive_end`, `drive_set_paused(true)` | `system` |
| pending | escalated | `drive_take_over` | `human` |
| pending | escalated | gate on its own timeout (`drive_prompt_close`) | `timeout` |
| pending | escalated | gate seeing the driver offline (`drive_prompt_close`) | `system` |
| pending | expired | `drive_prompts_expire_stale`, or the gate seeing a stale row | `timeout` |
| pending | withdrawn | `session_upsert` with state != `waiting`; `session_set_offline` | `system` |

A timeout never resolves to `allowed`. Reader rule: any status other than `allowed`, `denied`,
`answered` means "no decision" (the native dialog appears).

**Request status.**

```
pending -> running -> applied
                   -> refused
                   -> pending_confirm -> running               (swap: confirmed, `drive_request_resume`)
                                      -> applied | refused     (swap only; refused = cancelled or expired)
pending -> refused                                              (cancel, expire, project not open)
pending_confirm -> refused                                      (expire)
```

`applied` and `refused` are terminal. `send --queue` rows stay `pending` while the agent is busy (the
app doesn't `begin` them) and are retried on each watcher pass until idle or until
`drive_requests_expire(600)` refuses them. Non-queue `send` rows are `begin`-ed at once and refused
immediately when the agent isn't idle (04).

**Awaiting turn.** `drives.awaiting_turn` is `NULL` until the app applies a `send`
(`drive_set_awaiting_turn` -> current `turn + 1`; `drive_restore_awaiting_turn` puts back a previous value
or sets `NULL`, which `reset`/`swap` do on success). "The turn finished" is
`session_replies.turn >= awaiting_turn`. `wait` (02) clears it with `drive_ack_turn` when it delivers
the event. A later `send` sets a new value, so a stale idle before the agent reacts never ends a wait.

**Pause.** `paused = 1` keeps the binding. It escalates pending prompts (once, at pause time), makes
`drive_prompt_insert` fail (so the gate escalates immediately), and makes `drive_request_add` and
`drive_prompt_decide` fail. Reading (`read`, `wait`, `prompts`, `drives`) still works.

## Model additions

`WorktreeSession` (`crates/keitora-tasks/src/model.rs:14`) gains three fields, each
`#[serde(default)]`:

```rust
pub driven_by: Option<String>,        // drives.driver for this worktree
pub brokered: bool,                   // a pending, non-stale prompt exists for this worktree
pub escalated_reason: Option<String>, // see below
```

`sessions_list` fills them by extending its query:

- `driven_by`: `LEFT JOIN drives d ON d.driven = s.worktree` -> `d.driver`.
- `brokered`: `EXISTS(SELECT 1 FROM drive_prompts p WHERE p.driven = s.worktree AND p.status = 'pending' AND <not stale>)`.
- `escalated_reason`: the `reason` of the newest `drive_prompts` row for the worktree (`ORDER BY id DESC LIMIT 1`) **iff** its `status = 'escalated'` and `s.state = 'waiting'`; else `NULL`.

Every place that builds a `WorktreeSession` literal must add the three fields. Find them with
`grep -rn "WorktreeSession {"` (at least the `local_store.rs` tests, the CLI test helper `session()`
at `crates/keitora-cli/src/main.rs:1306`, and `src-tauri`). `session_revision` hashing (so
`sessions://changed` fires) is owned by 04.

## Retention / purge

`drive_purge(DRIVE_RETENTION_SECS)` runs from the app's expiry pass (04 triggers it, next to
`dispatch_requests_expire`). Truncated `tool_input` may contain secrets (for example tokens in a shell
command); it stays in the local `tasks.db` only, is never logged, and is dropped within a day.

## Tests (same change, `#[cfg(test)]`, in `drive_store.rs` and `drive.rs`)

Use the temp-DB helper pattern from `local_store.rs` tests (`fn open(&self) -> LocalStore`).

Drives:
- `drive_bind_inserts_with_defaults`: `escalate_after_s = 300`, `paused = false`, `awaiting_turn = None`, `forced_from = None`.
- `drive_bind_rejects_self`
- `drive_bind_rejects_driver_that_is_driven` and `drive_bind_rejects_driven_that_drives`: one-level rule in both directions; existing rows unchanged after the rejection.
- `drive_bind_second_driver_refused_without_force`: message names the current driver.
- `drive_bind_force_steals_and_escalates`: pending prompt becomes `escalated`/`system`; `forced_from = Some(old)`; `paused` reset; `awaiting_turn` cleared; `previous_driver` returned.
- `drive_bind_same_driver_is_idempotent_and_keeps_state`: `awaiting_turn` and `since` unchanged; `escalate_after_s` updated only when given.
- `drive_bind_validates_escalate_after_range`: 0 and 541 refused, 1 and 540 accepted.
- `drive_release_requires_matching_driver_and_escalates_prompts`
- `drive_release_all_returns_each_worktree_and_empty_is_ok`
- `drive_end_ignores_driver_and_errors_when_missing`
- `drive_set_paused_escalates_pending_once_and_resume_returns_zero`
- `drive_take_over_marks_escalated_human_and_keeps_binding`
- `drive_set_awaiting_turn_is_turn_plus_one_and_none_when_released`
- `drive_ack_turn_only_clears_matching_value`
- `drive_restore_awaiting_turn_sets_exact_value_or_none`: restores a previous value, clears with `None`, `false` for an unbound worktree.
- `drive_two_connections_racing_bind_one_wins`: two `LocalStore`s on one file; exactly one `Ok`.

Prompts:
- `prompt_insert_copies_driver_and_timeout_and_truncates`: the question kind keeps more than the permission kind.
- `prompt_insert_refused_when_not_driven_or_paused`
- `prompt_decide_each_decision_sets_status_decision_json_decided_by`
- `prompt_decide_rejects_wrong_driver_paused_already_decided_and_kind_mismatch`: assert each message.
- `prompt_decide_escalate_requires_reason`
- `prompt_decide_race_second_decision_conflicts`: two connections; one wins.
- `prompt_stale_pending_reads_as_expired`: backdate `created_at` with raw SQL by `timeout_s + 31` seconds; `drive_prompts_pending` excludes it, `brokered` is false, `drive_prompt_decide` says expired, `drive_prompts_expire_stale` rewrites it to `expired`/`timeout`.
- `prompt_close_rejects_allowed_status`: a timeout can never produce `allowed`.
- `prompt_withdrawn_by_busy_idle_offline_but_not_waiting`: `session_upsert` with `busy` and `idle`, and `session_set_offline`, each withdraw; `waiting` leaves the prompt pending.
- `terminal_prompt_statuses_are_final`: a second close or decide returns `Conflict`/`false`.

Requests:
- `request_add_validates_payload_per_kind`: missing `text`, empty text, `lines` 0 and 2001, `swap` without `to`, `queue` on `read`.
- `request_add_refused_for_non_driver_unknown_or_paused`
- `request_lifecycle_pending_begin_resolve`: shaped like the `dispatch_request_*` tests; `begin` wins once; `resolve` writes once; a non-outcome status errors.
- `request_swap_pending_confirm_then_applied_and_refused`
- `request_cancel_only_from_pending`
- `queued_send_stays_pending_until_expired`: `drive_requests_expire(600)` on a backdated queued row gives `refused` with the "expired" reason; a `running` row is untouched; a backdated `pending_confirm` row becomes `refused`.
- `requests_pending_oldest_first`
- `request_swap_confirm_round_trip_running_pending_confirm_running`: `await_confirm` then `resume` each win once; `resume` from any other status is `false`.
- `requests_in_flight_lists_running_and_pending_confirm_only`

Replies / sessions / misc:
- `session_reply_record_bumps_turn_and_truncates_on_char_boundary`: multi-byte text at the cut.
- `session_idle_secs_none_without_row_and_counts_up`
- `pending_prompt_counts_group_by_worktree_and_skip_stale_and_decided`: two worktrees, one stale (backdated), one escalated; absent keys for zero.
- `read_paths_tolerate_missing_drive_tables`: a DB with only the pre-existing tables opened with `open_read_only` (no tables created): `sessions_list` returns rows with defaults and does not error, `session_revision` returns the pre-existing string, `drive_get` is `None`, and `drives_list`, `drives_by_driver`, `drive_prompts_pending`, `drive_requests_pending`, `drive_requests_in_flight` and `drive_pending_prompt_counts` are empty; afterwards `sqlite_master` still has no drive tables.
- `session_revision_changes_with_driver_pending_flag_and_newest_prompt_id_not_heartbeat`: the extended suffix (04) changes when a worktree becomes driven, a prompt becomes pending, flips to escalated or a new prompt is added, and is unchanged by a heartbeat.
- `data_version_changes_after_other_connection_writes`
- `sessions_list_fills_driven_by_brokered_escalated_reason`: the reason shows only while `state = waiting` and the newest prompt is `escalated`; cleared after `busy`.
- `drive_purge_deletes_only_old_terminal_rows`: pending prompts and unresolved requests survive.
- `opening_old_schema_db_adds_drive_tables`: create a DB with only the pre-existing tables (raw DDL copied into the test), open with `LocalStore::open`, assert the four new tables exist and `drives_list()` is empty.
- `truncate_helpers_respect_char_boundaries` (pure).

## Interface this doc exposes to others

- 02: `drive_bind`, `drive_release`, `drive_release_all`, `drive_get`, `drives_list`, `drives_by_driver`, `drive_prompts_pending`, `drive_prompt_get`, `drive_prompt_decide`, `drive_request_add/get/cancel`, `session_reply_get`, `drive_ack_turn`, `session_idle_secs`, `data_version`, `sessions_list`.
- 03: `drive_get`, `drive_prompt_insert`, `drive_prompt_get`, `drive_prompt_close`, `session_reply_record`, `data_version`; withdrawal happens inside `session_upsert`/`session_set_offline`. 03's `GateStore` roles map onto existing methods: `session_state(w)` is the `state` of `w`'s row in `sessions_list()` (no new method), and `set_session_state(w, s)` is `session_upsert(w, s, <current_task_id of that row>, None)`.
- 04: `drive_requests_pending`, `drive_requests_in_flight`, `drive_request_get/begin/await_confirm/resume/resolve`, `drive_set_awaiting_turn`, `drive_restore_awaiting_turn`, `drive_requests_expire`, `drive_prompts_expire_stale`, `drive_purge`, `drive_end`, `drive_set_paused`, `drive_take_over`, `drives_list`, `drive_pending_prompt_counts`, and the `sessions_list`/`session_revision` behaviour on read-only handles.
- 05: model fields only.

## Singletonize decisions

1. **Location.** Drive code is on `LocalStore` (sync, `crate::Result`), not the `TaskBackend` trait, because `dispatch_requests` and `worktree_sessions` already live there and the trait is for the swappable task backend. DDL is its own const run from `init_schema`.
2. **`provider` has no CHECK.** `resume_sessions.cli` has `CHECK(cli IN ('claude','codex'))`; a third provider would force a table rebuild. `drive_prompts.provider` and request payload `to` are free text, validated by the app at apply time.
3. **`drive_prompts.timeout_s`** is an added column so any reader can compute staleness without a drive row that may have been released.
4. **`drive_requests.queue`** is an added column so the app can tell `send --queue` (leave pending while busy) from a plain `send` (refuse while busy).
5. **`drive_ack_turn`** is new. Without it, `wait` on several worktrees would re-report an already-delivered finished turn forever. The equality guard keeps a newer send intact.
6. **Withdrawal rule is `state != 'waiting'`**, i.e. exactly the proposal's `busy`/`idle`/`offline` set. Known risk: a hook that writes `busy` while a gate is blocked (for example a parallel subagent tool) withdraws the pending prompt; the gate then sees `withdrawn` and escalates (safe, but the driver loses the decision). 03 should avoid non-gate session writes while blocked where it can.
7. **Escalation by the driver requires a reason** (store-enforced), matching the "always give a reason" contract.
8. **Pause has no CLI verb**; the store API exists for the app's human commands only.
9. **`read` truncation keeps the tail** of the pane (what a driver needs).
10. **Cross-doc reconciliation.** `escalate_after_s` is 1..=540 (default 300) because 03 fixes the hook `timeout` at 600 s and caps the broker at 540 s; stale means older than `timeout_s + 30`, which is always inside the 600 s. 04's requests are the additions `drive_requests_in_flight`, `drive_restore_awaiting_turn`, `drive_request_resume` and `drive_pending_prompt_counts`; `drive_requests.queue` is a column, so a `send` payload carries only `text`. Result JSON for `reset`/`swap` uses the key `provider`, as in the table above.
