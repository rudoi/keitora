//! Cross-worktree driving: one worktree's agent (the *driver*) steers another's (the
//! *driven*). This module holds the drive tables' DDL, the model types and the pure
//! helpers; the store methods live on [`LocalStore`](crate::LocalStore) in
//! `drive_store.rs`.
//!
//! Drive state lives in its own tables, created on a read-write open. `init_schema`
//! upgrades older tables in place: a missing `drive_requests` column, a `kind` CHECK
//! without `wake`, and the delivery columns on `drives` (`delivery_id`,
//! `baseline_json`, `delivered_at`). Read-only handles never see a column added after
//! their DB was written, so the delivery columns are read by their own methods and
//! never through the shared `drives` column list.

use serde::{Deserialize, Serialize};

/// Kinds of brokered prompt: a tool permission request, or an `AskUserQuestion`.
pub const DRIVE_PROMPT_KINDS: [&str; 2] = ["permission", "question"];
/// Every prompt status. `pending` is the only non-terminal one.
pub const DRIVE_PROMPT_STATUSES: [&str; 7] = [
    "pending",
    "allowed",
    "denied",
    "answered",
    "escalated",
    "expired",
    "withdrawn",
];
/// Pane-affecting request kinds the CLI asks the running app to apply. `wake` is its
/// own kind (not a flavour of `reset`) so an app that predates it refuses the row as
/// unknown instead of running a reset.
pub const DRIVE_REQUEST_KINDS: [&str; 5] = ["send", "read", "reset", "swap", "wake"];
/// Every request status; see [`DRIVE_REQUEST_OUTCOMES`] for the terminal subset.
pub const DRIVE_REQUEST_STATUSES: [&str; 5] = [
    "pending",
    "running",
    "pending_confirm",
    "applied",
    "refused",
];
/// The terminal request statuses.
pub const DRIVE_REQUEST_OUTCOMES: [&str; 2] = ["applied", "refused"];
/// How long a brokered prompt waits for the driver before escalating to the human.
pub const DRIVE_DEFAULT_ESCALATE_AFTER_S: i64 = 300;
pub const DRIVE_ESCALATE_AFTER_MIN_S: i64 = 1;
/// The gate's cap; keeps a brokered wait inside the fixed hook timeout.
pub const DRIVE_ESCALATE_AFTER_MAX_S: i64 = 540;
/// The fixed hook `timeout` both providers are configured with (documentation only).
pub const DRIVE_HOOK_TIMEOUT_S: i64 = 600;
/// A pending prompt is stale (no live hook is polling it) once it is older than
/// `timeout_s` plus this slack. Always below [`DRIVE_HOOK_TIMEOUT_S`].
pub const DRIVE_PROMPT_STALE_SLACK_S: i64 = 30;
/// Chars of `tool_input` kept for a permission prompt.
pub const DRIVE_PERMISSION_INPUT_MAX: usize = 4096;
/// Chars of `tool_input` kept for a question prompt (the questions are the payload).
pub const DRIVE_QUESTION_INPUT_MAX: usize = 16384;
/// Chars of a prompt summary.
pub const DRIVE_SUMMARY_MAX: usize = 200;
/// Bytes of a finished turn's last message kept in `session_replies`.
pub const SESSION_REPLY_MAX: usize = 32 * 1024;
/// Bytes of a `send` payload.
pub const DRIVE_SEND_TEXT_MAX: usize = 32 * 1024;
/// Bytes of a `read` result.
pub const DRIVE_READ_RESULT_MAX: usize = 64 * 1024;
pub const DRIVE_READ_LINES_DEFAULT: i64 = 200;
pub const DRIVE_READ_LINES_MAX: i64 = 2000;
/// Matches the app's request max age: an unclaimed request older than this is refused.
pub const DRIVE_REQUEST_MAX_AGE_SECS: i64 = 600;
/// A `pending_confirm` request whose confirm started longer ago than this is refused.
/// Above the app's swap-confirm timeout (300 s) so the app's own deadline always
/// fires first and records the more specific reason.
pub const DRIVE_CONFIRM_MAX_AGE_SECS: i64 = 360;
/// How long resolved requests and decided prompts are kept before purge.
pub const DRIVE_RETENTION_SECS: i64 = 86_400;

// A stale prompt must still be younger than the hook timeout at the longest wait.
const _: () =
    assert!(DRIVE_ESCALATE_AFTER_MAX_S + DRIVE_PROMPT_STALE_SLACK_S < DRIVE_HOOK_TIMEOUT_S);

/// The drive tables other than `drive_requests` (see [`drive_requests_ddl`]). Executed
/// by `init_schema` on every read-write open.
pub const DRIVE_DDL: &str = r#"
-- Who drives whom. One row per driven worktree: a worktree has at most one driver.
CREATE TABLE IF NOT EXISTS drives (
  driven           TEXT PRIMARY KEY,
  driver           TEXT NOT NULL,
  since            TEXT NOT NULL DEFAULT (datetime('now')),
  forced_from      TEXT,
  paused           INTEGER NOT NULL DEFAULT 0 CHECK(paused IN (0, 1)),
  escalate_after_s INTEGER NOT NULL DEFAULT 300 CHECK(escalate_after_s BETWEEN 1 AND 540),
  awaiting_turn    INTEGER,
  CHECK(driver <> driven)
);
CREATE INDEX IF NOT EXISTS idx_drives_driver ON drives(driver);

-- One row per brokered prompt (permission request or AskUserQuestion).
CREATE TABLE IF NOT EXISTS drive_prompts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  driven        TEXT NOT NULL,
  driver        TEXT NOT NULL,
  provider      TEXT NOT NULL,
  kind          TEXT NOT NULL CHECK(kind IN ('permission', 'question')),
  tool_name     TEXT NOT NULL DEFAULT '',
  tool_input    TEXT NOT NULL DEFAULT '',
  summary       TEXT NOT NULL DEFAULT '',
  status        TEXT NOT NULL DEFAULT 'pending'
                CHECK(status IN ('pending','allowed','denied','answered','escalated','expired','withdrawn')),
  decision_json TEXT,
  decided_by    TEXT,
  reason        TEXT NOT NULL DEFAULT '',
  timeout_s     INTEGER NOT NULL,
  hook_pid      INTEGER,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  decided_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_drive_prompts_driven_status ON drive_prompts(driven, status);
CREATE INDEX IF NOT EXISTS idx_drive_prompts_driver_status ON drive_prompts(driver, status);

-- Last finished turn per worktree. A side table, not columns on worktree_sessions.
CREATE TABLE IF NOT EXISTS session_replies (
  worktree     TEXT PRIMARY KEY,
  turn         INTEGER NOT NULL DEFAULT 0,
  last_message TEXT NOT NULL DEFAULT '',
  updated_at   TEXT NOT NULL DEFAULT (datetime('now')),
  last_turn_id TEXT,
  last_session_id TEXT
);
"#;

/// Turn completions of driven worktrees, written by the app (the `status.db` lease
/// holder) and acked by `wait`. Executed by `init_schema` right after [`DRIVE_DDL`].
/// Not one of [`DRIVE_TABLES`]: an older DB without it must still read as driven.
pub const DRIVE_TURNS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS drive_turns (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  driven           TEXT    NOT NULL,
  kind             TEXT    NOT NULL CHECK (kind IN ('driver','human')),
  delivery_id      INTEGER,
  outcome          TEXT    NOT NULL DEFAULT 'finished' CHECK (outcome IN ('finished','restarted')),
  signal           TEXT    NOT NULL CHECK (signal IN ('herdr','hook')),
  clause           TEXT    NOT NULL CHECK (clause IN
                     ('completion_seq','state_change_seq','observed_active','stop_hook','occupant_changed')),
  completion_seq   INTEGER,
  state_change_seq INTEGER,
  completed_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now')),
  reply            TEXT    NOT NULL DEFAULT '',
  reply_source     TEXT    NOT NULL CHECK (reply_source IN ('hook','transcript','none')),
  caveat           TEXT,
  acked_at         TEXT,
  CHECK ((kind = 'driver') = (delivery_id IS NOT NULL))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_drive_turns_delivery
  ON drive_turns(delivery_id) WHERE delivery_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_drive_turns_driven_acked ON drive_turns(driven, acked_at);
"#;

/// The delivery columns `init_schema` adds to an older `drives` table, with their types.
pub(crate) const DRIVES_DELIVERY_COLUMNS: [(&str, &str); 3] = [
    ("delivery_id", "INTEGER"),
    ("baseline_json", "TEXT"),
    ("delivered_at", "TEXT"),
];

/// `drive_turns.kind`: the driver's delivery finished, or the human typed a turn.
pub const DRIVE_TURN_KINDS: [&str; 2] = ["driver", "human"];
/// `drive_turns.outcome`.
pub const DRIVE_TURN_OUTCOMES: [&str; 2] = ["finished", "restarted"];
/// `drive_turns.signal`: what detected the completion.
pub const DRIVE_TURN_SIGNALS: [&str; 2] = ["herdr", "hook"];
/// `drive_turns.clause`: which rule finished the turn.
pub const DRIVE_TURN_CLAUSES: [&str; 5] = [
    "completion_seq",
    "state_change_seq",
    "observed_active",
    "stop_hook",
    "occupant_changed",
];
/// `drive_turns.reply_source`: where the reply text came from.
pub const DRIVE_TURN_REPLY_SOURCES: [&str; 3] = ["hook", "transcript", "none"];
/// Turn rows kept per worktree regardless of age.
pub const DRIVE_TURNS_KEEP_PER_WORKTREE: i64 = 50;
/// Turn rows older than this (after ack, or after completion if never acked) are pruned.
pub const DRIVE_TURNS_RETENTION_DAYS: i64 = 7;
/// A delivery whose target stays settled idle at its baseline this long, with no turn
/// recorded, saw no activity: the prompt may not have been submitted.
pub const DRIVE_STALL_NO_ACTIVITY_S: i64 = 30;

/// `drive_requests` under `name`: pane-affecting requests from the CLI that the app
/// applies. Separate from `dispatch_requests` because that table's `task_id` is NOT
/// NULL. Kept apart from [`DRIVE_DDL`] so a table with an older `kind` CHECK can be
/// rebuilt from the same text under a temporary name.
pub(crate) fn drive_requests_ddl(name: &str) -> String {
    DRIVE_REQUESTS_DDL.replace("{name}", name)
}

const DRIVE_REQUESTS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS {name} (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  kind         TEXT NOT NULL CHECK(kind IN ('send', 'read', 'reset', 'swap', 'wake')),
  worktree     TEXT NOT NULL,
  requested_by TEXT NOT NULL,
  payload      TEXT NOT NULL DEFAULT '{}',
  queue        INTEGER NOT NULL DEFAULT 0 CHECK(queue IN (0, 1)),
  status       TEXT NOT NULL DEFAULT 'pending'
               CHECK(status IN ('pending','running','pending_confirm','applied','refused')),
  reason       TEXT NOT NULL DEFAULT '',
  result       TEXT NOT NULL DEFAULT '',
  created_at   TEXT NOT NULL DEFAULT (datetime('now')),
  resolved_at  TEXT,
  -- When the request last entered pending_confirm (the human's confirm clock).
  confirm_started_at TEXT,
  -- First pass that examined the request; NULL until then.
  seen_at            TEXT,
  -- Why the app left it pending, as of the last pass; '' when it has no reason.
  wait_reason        TEXT NOT NULL DEFAULT '',
  wait_reason_at     TEXT
);
"#;

/// The `drive_requests` columns it first shipped with, in table order.
const DRIVE_REQUESTS_BASE_COLUMNS: [&str; 11] = [
    "id",
    "kind",
    "worktree",
    "requested_by",
    "payload",
    "queue",
    "status",
    "reason",
    "result",
    "created_at",
    "resolved_at",
];

/// Every `drive_requests` column, in table order (the rebuild copies them all): the
/// base ones, then [`DRIVE_REQUESTS_ADDED_COLUMNS`].
pub(crate) fn drive_requests_columns() -> String {
    DRIVE_REQUESTS_BASE_COLUMNS
        .into_iter()
        .chain(DRIVE_REQUESTS_ADDED_COLUMNS.map(|(col, _)| col))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `drive_requests` index, created once the table exists (and again after a rebuild).
pub(crate) const DRIVE_REQUESTS_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS idx_drive_requests_status ON drive_requests(status);";

/// The drive tables, created by [`DRIVE_DDL`] and [`drive_requests_ddl`].
pub(crate) const DRIVE_TABLES: [&str; 4] = [
    "drives",
    "drive_prompts",
    "drive_requests",
    "session_replies",
];

/// A binding: `driver` steers `driven`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Drive {
    pub driven: String,
    pub driver: String,
    pub since: String,
    /// The previous driver after a forced take-over.
    pub forced_from: Option<String>,
    pub paused: bool,
    pub escalate_after_s: i64,
    /// The turn a `send` is waiting for; `None` when no send is outstanding.
    pub awaiting_turn: Option<i64>,
}

/// One brokered prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrivePrompt {
    pub id: i64,
    pub driven: String,
    pub driver: String,
    pub provider: String,
    pub kind: String,
    pub tool_name: String,
    pub tool_input: String,
    pub summary: String,
    pub status: String,
    pub decision_json: Option<String>,
    pub decided_by: Option<String>,
    pub reason: String,
    pub timeout_s: i64,
    pub hook_pid: Option<i64>,
    pub created_at: String,
    pub decided_at: Option<String>,
    /// Seconds since `created_at`, computed by the store query; not serialized.
    #[serde(default, skip_serializing)]
    pub age_s: i64,
}

/// Input to `drive_prompt_insert`. `tool_input` and `summary` are given in full; the
/// store truncates them.
#[derive(Debug, Clone)]
pub struct NewDrivePrompt {
    pub driven: String,
    pub provider: String,
    pub kind: String,
    pub tool_name: String,
    pub tool_input: String,
    pub summary: String,
    pub hook_pid: Option<i64>,
}

/// A pane-affecting request for the running app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveRequest {
    pub id: i64,
    pub kind: String,
    pub worktree: String,
    pub requested_by: String,
    pub payload: String,
    pub queue: bool,
    pub status: String,
    pub reason: String,
    pub result: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
    /// When a pass first examined the request; `None` until then.
    pub seen_at: Option<String>,
    /// Why the app left it pending, as of its last pass; empty when it has none.
    pub wait_reason: String,
    /// When `wait_reason` was last set.
    pub wait_reason_at: Option<String>,
}

/// The `drive_requests` columns added after the table first shipped, with their
/// declarations, in table order. An older table gains them in place.
pub(crate) const DRIVE_REQUESTS_ADDED_COLUMNS: [(&str, &str); 4] = [
    ("confirm_started_at", "TEXT"),
    ("seen_at", "TEXT"),
    ("wait_reason", "TEXT NOT NULL DEFAULT ''"),
    ("wait_reason_at", "TEXT"),
];

impl DriveRequest {
    /// Whether the app has finished with this request.
    pub fn is_resolved(&self) -> bool {
        DRIVE_REQUEST_OUTCOMES.contains(&self.status.as_str())
    }
}

/// A worktree's last finished turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReply {
    pub worktree: String,
    pub turn: i64,
    pub last_message: String,
    pub updated_at: String,
}

/// What herdr reported for the target right before a prompt was delivered. A later
/// snapshot is compared against it to tell when the delivered turn finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// herdr's agent label at delivery (`claude`, `codex`, `agy`).
    pub agent: Option<String>,
    pub pane_id: String,
    pub terminal_id: String,
    pub agent_session: Option<String>,
    pub state_change_seq: u64,
    /// herdr's stamp of the last completed-work idle; nullable and server-wide.
    pub completion_seq: Option<u64>,
    /// `idle` or `done`.
    pub status: String,
    /// Unix ms of the delivery.
    pub delivered_at_ms: i64,
}

/// A drive's outstanding delivery, as stored on its `drives` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub driven: String,
    pub driver: String,
    pub awaiting_turn: Option<i64>,
    /// `None` for a delivery an older app made.
    pub delivery_id: Option<i64>,
    /// `None` when absent or unparsable.
    pub baseline: Option<Baseline>,
    /// UTC text, `%Y-%m-%d %H:%M:%f`.
    pub delivered_at: Option<String>,
}

/// One recorded turn completion of a driven worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveTurn {
    pub id: i64,
    pub driven: String,
    /// `driver` or `human`.
    pub kind: String,
    pub delivery_id: Option<i64>,
    /// `finished` or `restarted`.
    pub outcome: String,
    /// `herdr` or `hook`.
    pub signal: String,
    pub clause: String,
    pub completed_at: String,
    pub reply: String,
    /// `hook`, `transcript` or `none`.
    pub reply_source: String,
    pub caveat: Option<String>,
    pub acked_at: Option<String>,
}

/// Input to `drive_turn_insert`. The reply is truncated to [`SESSION_REPLY_MAX`] bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewDriveTurn<'a> {
    pub driven: &'a str,
    pub kind: &'a str,
    pub delivery_id: Option<i64>,
    pub outcome: &'a str,
    pub signal: &'a str,
    pub clause: &'a str,
    pub completion_seq: Option<u64>,
    pub state_change_seq: Option<u64>,
    pub reply: &'a str,
    pub reply_source: &'a str,
    pub caveat: Option<&'a str>,
}

/// The outcome of `drive_bind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriveBind {
    pub drive: Drive,
    /// The driver that was replaced; `Some` only on a forced take-over.
    pub previous_driver: Option<String>,
    /// Pending prompts escalated to the human by a forced take-over.
    pub escalated_prompts: usize,
}

/// A driver's decision on a brokered prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptDecision {
    Allow,
    /// An empty reason is stored as "denied by driver".
    Deny {
        reason: String,
    },
    Answer {
        answers: std::collections::BTreeMap<String, String>,
    },
    /// Hand the prompt to the human; the reason must be non-empty.
    Escalate {
        reason: String,
    },
}

/// The first `max` chars of `s` (cut on a char boundary, no marker).
pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// `s` cut at the last char boundary at or below `max` bytes (no marker).
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_helpers_respect_char_boundaries() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello", 2), "he");
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("日本語", 0), "");
        assert_eq!(truncate_chars("日本語", 2), "日本");

        assert_eq!(truncate_bytes("hello", 10), "hello");
        assert_eq!(truncate_bytes("hello", 3), "hel");
        // "é" is 2 bytes at offsets 1..3: a cut at 2 falls inside it.
        assert_eq!(truncate_bytes("héllo", 2), "h");
        assert_eq!(truncate_bytes("héllo", 3), "hé");
        // Each "日" is 3 bytes.
        assert_eq!(truncate_bytes("日本語", 5), "日");
        assert_eq!(truncate_bytes("日本語", 6), "日本");
        assert_eq!(truncate_bytes("日本語", 0), "");
    }

    #[test]
    fn request_is_resolved_only_for_outcomes() {
        let mut r = DriveRequest {
            id: 1,
            kind: "send".into(),
            worktree: "bravo".into(),
            requested_by: "alpha".into(),
            payload: "{}".into(),
            queue: false,
            status: "pending".into(),
            reason: String::new(),
            result: String::new(),
            created_at: String::new(),
            resolved_at: None,
            seen_at: None,
            wait_reason: String::new(),
            wait_reason_at: None,
        };
        for s in ["pending", "running", "pending_confirm"] {
            r.status = s.into();
            assert!(!r.is_resolved(), "{s}");
        }
        for s in DRIVE_REQUEST_OUTCOMES {
            r.status = s.into();
            assert!(r.is_resolved(), "{s}");
        }
    }

    #[test]
    fn ddl_checks_match_the_constants() {
        for s in DRIVE_PROMPT_STATUSES {
            assert!(DRIVE_DDL.contains(&format!("'{s}'")), "{s}");
        }
        let requests = drive_requests_ddl("drive_requests");
        for s in DRIVE_REQUEST_STATUSES
            .iter()
            .chain(DRIVE_REQUEST_KINDS.iter())
        {
            assert!(requests.contains(&format!("'{s}'")), "{s}");
        }
        assert!(!DRIVE_DDL.contains("drive_requests"));
        assert!(DRIVE_DDL.contains(&format!(
            "BETWEEN {DRIVE_ESCALATE_AFTER_MIN_S} AND {DRIVE_ESCALATE_AFTER_MAX_S}"
        )));
        assert!(DRIVE_DDL.contains(&format!("DEFAULT {DRIVE_DEFAULT_ESCALATE_AFTER_S} ")));
    }

    #[test]
    fn baseline_round_trips_through_json() {
        let b = Baseline {
            agent: Some("codex".into()),
            pane_id: "p-1".into(),
            terminal_id: "t-1".into(),
            agent_session: None,
            state_change_seq: 11,
            completion_seq: Some(4),
            status: "idle".into(),
            delivered_at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(serde_json::from_str::<Baseline>(&json).unwrap(), b);
    }
}
