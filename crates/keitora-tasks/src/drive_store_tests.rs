//! Tests for the drive API on `LocalStore`. Every test uses its own temp-file DB.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

use rusqlite::params;

use crate::drive::{
    NewDrivePrompt, PromptDecision, DRIVE_CONFIRM_MAX_AGE_SECS, DRIVE_PERMISSION_INPUT_MAX,
    DRIVE_QUESTION_INPUT_MAX, DRIVE_RETENTION_SECS, DRIVE_SUMMARY_MAX, SESSION_REPLY_MAX,
};
use crate::local_store::LocalStore;
use crate::Error;

/// A throwaway on-disk DB (not `:memory:`, which is per-connection, so the
/// two-connection tests need a file).
struct TempDb {
    _dir: tempfile::TempDir,
    path: String,
}

impl TempDb {
    fn new() -> TempDb {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db").to_str().unwrap().to_string();
        TempDb { _dir: dir, path }
    }
    fn open(&self) -> LocalStore {
        LocalStore::open(&self.path).unwrap()
    }
}

/// The message of a `Conflict`, panicking on anything else.
fn conflict_msg<T: std::fmt::Debug>(r: crate::Result<T>) -> String {
    match r {
        Err(Error::Conflict(m)) => m,
        other => panic!("expected Conflict, got {other:?}"),
    }
}

fn new_prompt(driven: &str, kind: &str) -> NewDrivePrompt {
    NewDrivePrompt {
        driven: driven.into(),
        provider: "claude".into(),
        kind: kind.into(),
        tool_name: "Bash".into(),
        tool_input: r#"{"command":"ls"}"#.into(),
        summary: "Bash: ls".into(),
        hook_pid: Some(42),
    }
}

/// Bind alpha -> bravo and insert one pending permission prompt; returns its id.
fn bound_with_prompt(ls: &LocalStore) -> i64 {
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id
}

/// Push a prompt's `created_at` back by `secs`.
fn backdate_prompt(ls: &LocalStore, id: i64, secs: i64) {
    ls.conn
        .execute(
            "UPDATE drive_prompts SET created_at = datetime('now', ?2) WHERE id = ?1",
            params![id, format!("-{secs} seconds")],
        )
        .unwrap();
}

fn backdate_request(ls: &LocalStore, id: i64, secs: i64) {
    ls.conn
        .execute(
            "UPDATE drive_requests SET created_at = datetime('now', ?2) WHERE id = ?1",
            params![id, format!("-{secs} seconds")],
        )
        .unwrap();
}

fn backdate_confirm(ls: &LocalStore, id: i64, secs: i64) {
    ls.conn
        .execute(
            "UPDATE drive_requests SET confirm_started_at = datetime('now', ?2) WHERE id = ?1",
            params![id, format!("-{secs} seconds")],
        )
        .unwrap();
}

fn status_of(ls: &LocalStore, id: i64) -> (String, Option<String>, String) {
    let p = ls.drive_prompt_get(id).unwrap().unwrap();
    (p.status, p.decided_by, p.reason)
}

fn send(text: &str) -> serde_json::Value {
    serde_json::json!({ "text": text })
}

/// The pre-existing tables `sessions_list` / `session_revision` read, as an older
/// schema had them (no drive tables).
const OLD_SCHEMA: &str = r#"
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'open'
    CHECK(status IN ('open', 'active', 'backlog', 'done', 'cancelled')),
  priority TEXT NOT NULL DEFAULT 'normal'
    CHECK(priority IN ('low', 'normal', 'high', 'urgent')),
  assigned_to TEXT DEFAULT NULL,
  created_by TEXT DEFAULT NULL,
  tags TEXT NOT NULL DEFAULT '[]',
  branch TEXT DEFAULT NULL,
  pr_url TEXT DEFAULT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE worktree_sessions (
  worktree        TEXT PRIMARY KEY,
  state           TEXT NOT NULL DEFAULT 'idle'
                  CHECK(state IN ('idle','busy','waiting','offline')),
  current_task_id INTEGER,
  pid             INTEGER,
  last_heartbeat  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE dispatch_requests (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  task_id      INTEGER NOT NULL,
  worktree     TEXT NOT NULL,
  requested_by TEXT NOT NULL DEFAULT '',
  status       TEXT NOT NULL DEFAULT 'pending'
               CHECK(status IN ('pending','running','applied','refused')),
  reason       TEXT NOT NULL DEFAULT '',
  created_at   TEXT NOT NULL DEFAULT (datetime('now')),
  resolved_at  TEXT
);
INSERT INTO worktree_sessions (worktree, state) VALUES ('bravo', 'busy');
"#;

fn table_names(conn: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

// ---- Drives ----

#[test]
fn drive_bind_inserts_with_defaults() {
    let db = TempDb::new();
    let ls = db.open();
    let b = ls.drive_bind("alpha", "bravo", None, false).unwrap();
    assert_eq!(b.previous_driver, None);
    assert_eq!(b.escalated_prompts, 0);
    let d = b.drive;
    assert_eq!(d.driven, "bravo");
    assert_eq!(d.driver, "alpha");
    assert_eq!(d.escalate_after_s, 300);
    assert!(!d.paused);
    assert_eq!(d.awaiting_turn, None);
    assert_eq!(d.forced_from, None);
    assert_eq!(ls.drive_get("bravo").unwrap(), Some(d));
    assert_eq!(ls.drives_list().unwrap().len(), 1);
    assert_eq!(ls.drives_by_driver("alpha").unwrap().len(), 1);
    assert!(ls.drives_by_driver("bravo").unwrap().is_empty());
}

#[test]
fn drive_bind_rejects_self() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(
        conflict_msg(ls.drive_bind("alpha", "alpha", None, false)),
        "a worktree can't drive itself"
    );
    assert!(ls.drives_list().unwrap().is_empty());
}

#[test]
fn drive_bind_rejects_driver_that_is_driven() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let before = ls.drives_list().unwrap();
    assert_eq!(
        conflict_msg(ls.drive_bind("bravo", "charlie", None, false)),
        "@bravo is driven by @alpha; drives are one level only"
    );
    assert_eq!(ls.drives_list().unwrap(), before);
}

#[test]
fn drive_bind_rejects_driven_that_drives() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("bravo", "charlie", None, false).unwrap();
    let before = ls.drives_list().unwrap();
    assert_eq!(
        conflict_msg(ls.drive_bind("alpha", "bravo", None, true)),
        "@bravo drives other worktrees; drives are one level only"
    );
    assert_eq!(ls.drives_list().unwrap(), before);
}

#[test]
fn drive_bind_second_driver_refused_without_force() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    assert_eq!(
        conflict_msg(ls.drive_bind("charlie", "bravo", None, false)),
        "@bravo is already driven by @alpha (use --force to take over)"
    );
    assert_eq!(ls.drive_get("bravo").unwrap().unwrap().driver, "alpha");
}

#[test]
fn drive_bind_force_steals_and_escalates() {
    let db = TempDb::new();
    let ls = db.open();
    let id = bound_with_prompt(&ls);
    ls.drive_set_awaiting_turn("bravo").unwrap();
    ls.conn
        .execute("UPDATE drives SET paused = 1 WHERE driven = 'bravo'", [])
        .unwrap();
    let b = ls.drive_bind("charlie", "bravo", Some(60), true).unwrap();
    assert_eq!(b.previous_driver.as_deref(), Some("alpha"));
    assert_eq!(b.escalated_prompts, 1);
    assert_eq!(b.drive.driver, "charlie");
    assert_eq!(b.drive.forced_from.as_deref(), Some("alpha"));
    assert!(!b.drive.paused);
    assert_eq!(b.drive.awaiting_turn, None);
    assert_eq!(b.drive.escalate_after_s, 60);
    assert_eq!(
        status_of(&ls, id),
        (
            "escalated".into(),
            Some("system".into()),
            "drive taken over by @charlie".into()
        )
    );
}

#[test]
fn drive_bind_same_driver_is_idempotent_and_keeps_state() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", Some(120), false).unwrap();
    ls.conn
        .execute(
            "UPDATE drives SET since = '2000-01-01 00:00:00' WHERE driven = 'bravo'",
            [],
        )
        .unwrap();
    ls.drive_set_awaiting_turn("bravo").unwrap();
    let id = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;

    let b = ls.drive_bind("alpha", "bravo", None, false).unwrap();
    assert_eq!(b.escalated_prompts, 0);
    assert_eq!(b.previous_driver, None);
    assert_eq!(b.drive.escalate_after_s, 120, "kept when not given");
    assert_eq!(b.drive.awaiting_turn, Some(1));
    assert_eq!(b.drive.since, "2000-01-01 00:00:00");
    assert_eq!(status_of(&ls, id).0, "pending");

    let b = ls.drive_bind("alpha", "bravo", Some(45), false).unwrap();
    assert_eq!(b.drive.escalate_after_s, 45, "updated when given");
    assert_eq!(b.drive.awaiting_turn, Some(1));
    assert_eq!(b.drive.since, "2000-01-01 00:00:00");
}

#[test]
fn drive_bind_validates_escalate_after_range() {
    let db = TempDb::new();
    let ls = db.open();
    for bad in [0, 541, -1] {
        assert_eq!(
            conflict_msg(ls.drive_bind("alpha", "bravo", Some(bad), false)),
            "escalate-after must be 1..=540 seconds"
        );
    }
    assert!(ls.drives_list().unwrap().is_empty());
    assert_eq!(
        ls.drive_bind("alpha", "bravo", Some(1), false)
            .unwrap()
            .drive
            .escalate_after_s,
        1
    );
    assert_eq!(
        ls.drive_bind("alpha", "bravo", Some(540), false)
            .unwrap()
            .drive
            .escalate_after_s,
        540
    );
}

#[test]
fn drive_release_requires_matching_driver_and_escalates_prompts() {
    let db = TempDb::new();
    let ls = db.open();
    let id = bound_with_prompt(&ls);
    assert_eq!(
        conflict_msg(ls.drive_release("charlie", "bravo")),
        "@bravo isn't driven by @charlie"
    );
    assert!(ls.drive_get("bravo").unwrap().is_some());
    assert_eq!(status_of(&ls, id).0, "pending");

    assert_eq!(ls.drive_release("alpha", "bravo").unwrap(), 1);
    assert!(ls.drive_get("bravo").unwrap().is_none());
    assert_eq!(
        status_of(&ls, id),
        (
            "escalated".into(),
            Some("system".into()),
            "released by @alpha".into()
        )
    );
    assert_eq!(
        conflict_msg(ls.drive_release("alpha", "bravo")),
        "@bravo isn't driven by @alpha"
    );
}

#[test]
fn drive_release_all_returns_each_worktree_and_empty_is_ok() {
    let db = TempDb::new();
    let ls = db.open();
    assert!(ls.drive_release_all("alpha").unwrap().is_empty());
    ls.drive_bind("alpha", "charlie", None, false).unwrap();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_bind("delta", "echo", None, false).unwrap();
    ls.drive_prompt_insert(&new_prompt("charlie", "permission"))
        .unwrap();
    ls.drive_prompt_insert(&new_prompt("charlie", "question"))
        .unwrap();
    assert_eq!(
        ls.drive_release_all("alpha").unwrap(),
        vec![("bravo".to_string(), 0), ("charlie".to_string(), 2)]
    );
    let left = ls.drives_list().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].driven, "echo");
}

#[test]
fn drive_end_ignores_driver_and_errors_when_missing() {
    let db = TempDb::new();
    let ls = db.open();
    let id = bound_with_prompt(&ls);
    assert_eq!(ls.drive_end("bravo").unwrap(), 1);
    assert!(ls.drive_get("bravo").unwrap().is_none());
    assert_eq!(status_of(&ls, id).2, "released by you");
    assert_eq!(conflict_msg(ls.drive_end("bravo")), "@bravo isn't driven");
}

#[test]
fn drive_set_paused_escalates_pending_once_and_resume_returns_zero() {
    let db = TempDb::new();
    let ls = db.open();
    assert!(matches!(
        ls.drive_set_paused("bravo", true),
        Err(Error::Conflict(_))
    ));
    let id = bound_with_prompt(&ls);
    assert_eq!(ls.drive_set_paused("bravo", true).unwrap(), 1);
    assert!(ls.drive_get("bravo").unwrap().unwrap().paused);
    assert_eq!(
        status_of(&ls, id),
        (
            "escalated".into(),
            Some("system".into()),
            "driver paused".into()
        )
    );
    assert_eq!(
        ls.drive_set_paused("bravo", true).unwrap(),
        0,
        "same value is a no-op"
    );
    assert_eq!(
        conflict_msg(ls.drive_prompt_insert(&new_prompt("bravo", "permission"))),
        "drive is paused"
    );
    assert_eq!(ls.drive_set_paused("bravo", false).unwrap(), 0);
    assert!(!ls.drive_get("bravo").unwrap().unwrap().paused);
    assert_eq!(ls.drive_set_paused("bravo", false).unwrap(), 0);
}

#[test]
fn drive_take_over_marks_escalated_human_and_keeps_binding() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(ls.drive_take_over("bravo").unwrap(), 0, "unbound is 0");
    let id = bound_with_prompt(&ls);
    assert_eq!(ls.drive_take_over("bravo").unwrap(), 1);
    assert_eq!(
        status_of(&ls, id),
        (
            "escalated".into(),
            Some("human".into()),
            "taken over by you".into()
        )
    );
    assert_eq!(ls.drive_get("bravo").unwrap().unwrap().driver, "alpha");
    assert_eq!(ls.drive_take_over("bravo").unwrap(), 0);
}

#[test]
fn drive_set_awaiting_turn_is_turn_plus_one_and_none_when_released() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(ls.drive_set_awaiting_turn("bravo").unwrap(), None);
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    assert_eq!(ls.drive_set_awaiting_turn("bravo").unwrap(), Some(1));
    ls.session_reply_record("bravo", "done").unwrap();
    ls.session_reply_record("bravo", "done again").unwrap();
    assert_eq!(ls.drive_set_awaiting_turn("bravo").unwrap(), Some(3));
    assert_eq!(
        ls.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        Some(3)
    );
    ls.drive_release("alpha", "bravo").unwrap();
    assert_eq!(ls.drive_set_awaiting_turn("bravo").unwrap(), None);
}

#[test]
fn drive_ack_turn_only_clears_matching_value() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_set_awaiting_turn("bravo").unwrap(); // 1
    ls.session_reply_record("bravo", "x").unwrap();
    ls.drive_set_awaiting_turn("bravo").unwrap(); // 2
    assert!(!ls.drive_ack_turn("bravo", 1).unwrap(), "stale ack");
    assert_eq!(
        ls.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        Some(2)
    );
    assert!(ls.drive_ack_turn("bravo", 2).unwrap());
    assert_eq!(ls.drive_get("bravo").unwrap().unwrap().awaiting_turn, None);
    assert!(!ls.drive_ack_turn("bravo", 2).unwrap());
}

#[test]
fn drive_restore_awaiting_turn_sets_exact_value_or_none() {
    let db = TempDb::new();
    let ls = db.open();
    assert!(!ls.drive_restore_awaiting_turn("bravo", Some(1)).unwrap());
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let prev = ls.drive_get("bravo").unwrap().unwrap().awaiting_turn;
    ls.drive_set_awaiting_turn("bravo").unwrap();
    assert!(ls.drive_restore_awaiting_turn("bravo", prev).unwrap());
    assert_eq!(ls.drive_get("bravo").unwrap().unwrap().awaiting_turn, None);
    assert!(ls.drive_restore_awaiting_turn("bravo", Some(7)).unwrap());
    assert_eq!(
        ls.drive_get("bravo").unwrap().unwrap().awaiting_turn,
        Some(7)
    );
    assert!(ls.drive_restore_awaiting_turn("bravo", None).unwrap());
    assert_eq!(ls.drive_get("bravo").unwrap().unwrap().awaiting_turn, None);
}

#[test]
fn drive_two_connections_racing_bind_one_wins() {
    let db = TempDb::new();
    drop(db.open()); // create the schema up front
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["alpha", "charlie"]
        .into_iter()
        .map(|driver| {
            let path = db.path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let ls = LocalStore::open(&path).unwrap();
                barrier.wait();
                ls.drive_bind(driver, "bravo", None, false).is_ok()
            })
        })
        .collect();
    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(wins, 1);
    assert_eq!(db.open().drives_list().unwrap().len(), 1);
}

// ---- Prompts ----

#[test]
fn prompt_insert_copies_driver_and_timeout_and_truncates() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", Some(90), false).unwrap();
    let long = "x".repeat(DRIVE_QUESTION_INPUT_MAX + 100);
    let mut perm = new_prompt("bravo", "permission");
    perm.tool_input = long.clone();
    perm.summary = "s".repeat(DRIVE_SUMMARY_MAX + 50);
    let p = ls.drive_prompt_insert(&perm).unwrap();
    assert_eq!(p.driver, "alpha");
    assert_eq!(p.timeout_s, 90);
    assert_eq!(p.status, "pending");
    assert_eq!(p.hook_pid, Some(42));
    assert_eq!(p.provider, "claude");
    assert_eq!(p.decision_json, None);
    assert_eq!(p.tool_input.chars().count(), DRIVE_PERMISSION_INPUT_MAX);
    assert_eq!(p.summary.chars().count(), DRIVE_SUMMARY_MAX);

    let mut q = new_prompt("bravo", "question");
    q.tool_input = long;
    let q = ls.drive_prompt_insert(&q).unwrap();
    assert_eq!(q.tool_input.chars().count(), DRIVE_QUESTION_INPUT_MAX);
    assert!(q.tool_input.len() > p.tool_input.len());

    assert!(matches!(
        ls.drive_prompt_insert(&new_prompt("bravo", "other")),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn prompt_insert_refused_when_not_driven_or_paused() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(
        conflict_msg(ls.drive_prompt_insert(&new_prompt("bravo", "permission"))),
        "@bravo is not driven"
    );
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_set_paused("bravo", true).unwrap();
    assert_eq!(
        conflict_msg(ls.drive_prompt_insert(&new_prompt("bravo", "permission"))),
        "drive is paused"
    );
    assert!(ls.drive_prompts_pending(None, None).unwrap().is_empty());
}

#[test]
fn prompt_decide_each_decision_sets_status_decision_json_decided_by() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let perm = || {
        ls.drive_prompt_insert(&new_prompt("bravo", "permission"))
            .unwrap()
            .id
    };
    let json = |p: &crate::DrivePrompt| -> Option<serde_json::Value> {
        p.decision_json
            .as_deref()
            .map(|s| serde_json::from_str(s).unwrap())
    };

    let p = ls
        .drive_prompt_decide(perm(), "alpha", &PromptDecision::Allow)
        .unwrap();
    assert_eq!(p.status, "allowed");
    assert_eq!(json(&p), Some(serde_json::json!({"behavior": "allow"})));
    assert_eq!(p.decided_by.as_deref(), Some("driver:alpha"));
    assert!(p.decided_at.is_some());

    let p = ls
        .drive_prompt_decide(
            perm(),
            "alpha",
            &PromptDecision::Deny {
                reason: "not that file".into(),
            },
        )
        .unwrap();
    assert_eq!(p.status, "denied");
    assert_eq!(p.reason, "not that file");
    assert_eq!(
        json(&p),
        Some(serde_json::json!({"behavior": "deny", "message": "not that file"}))
    );

    let p = ls
        .drive_prompt_decide(
            perm(),
            "alpha",
            &PromptDecision::Deny {
                reason: String::new(),
            },
        )
        .unwrap();
    assert_eq!(p.reason, "denied by driver");
    assert_eq!(
        json(&p),
        Some(serde_json::json!({"behavior": "deny", "message": "denied by driver"}))
    );

    let q = ls
        .drive_prompt_insert(&new_prompt("bravo", "question"))
        .unwrap()
        .id;
    let mut answers = BTreeMap::new();
    answers.insert("Which branch?".to_string(), "main".to_string());
    let p = ls
        .drive_prompt_decide(q, "alpha", &PromptDecision::Answer { answers })
        .unwrap();
    assert_eq!(p.status, "answered");
    assert_eq!(
        json(&p),
        Some(serde_json::json!({"answers": {"Which branch?": "main"}}))
    );

    let p = ls
        .drive_prompt_decide(
            perm(),
            "alpha",
            &PromptDecision::Escalate {
                reason: "needs a human".into(),
            },
        )
        .unwrap();
    assert_eq!(p.status, "escalated");
    assert_eq!(p.decision_json, None);
    assert_eq!(p.reason, "needs a human");
    assert_eq!(p.decided_by.as_deref(), Some("driver:alpha"));
}

#[test]
fn prompt_decide_rejects_wrong_driver_paused_already_decided_and_kind_mismatch() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(99, "alpha", &PromptDecision::Allow)),
        "no prompt 99"
    );
    let id = bound_with_prompt(&ls);
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(id, "charlie", &PromptDecision::Allow)),
        format!("prompt {id} belongs to @alpha's drive")
    );
    let mut answers = BTreeMap::new();
    answers.insert("q".to_string(), "a".to_string());
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(
            id,
            "alpha",
            &PromptDecision::Answer {
                answers: answers.clone()
            }
        )),
        "answer applies to question prompts only"
    );
    let q = ls
        .drive_prompt_insert(&new_prompt("bravo", "question"))
        .unwrap()
        .id;
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(q, "alpha", &PromptDecision::Allow)),
        "allow applies to permission prompts; use answer for questions"
    );
    ls.drive_prompt_decide(id, "alpha", &PromptDecision::Allow)
        .unwrap();
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(id, "alpha", &PromptDecision::Allow)),
        format!("prompt {id} is already allowed")
    );

    // Paused: a fresh prompt inserted before the pause is escalated by it, so pause
    // with the row still pending by flipping the flag directly.
    let p = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    ls.conn
        .execute("UPDATE drives SET paused = 1 WHERE driven = 'bravo'", [])
        .unwrap();
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(p, "alpha", &PromptDecision::Allow)),
        "drive is paused"
    );
    assert_eq!(status_of(&ls, p).0, "pending");
}

#[test]
fn prompt_decide_escalate_requires_reason() {
    let db = TempDb::new();
    let ls = db.open();
    let id = bound_with_prompt(&ls);
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(
            id,
            "alpha",
            &PromptDecision::Escalate {
                reason: "  ".into()
            }
        )),
        "escalate needs a reason"
    );
    assert_eq!(status_of(&ls, id).0, "pending");
}

#[test]
fn prompt_decide_race_second_decision_conflicts() {
    let db = TempDb::new();
    let id = bound_with_prompt(&db.open());
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|i| {
            let path = db.path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let ls = LocalStore::open(&path).unwrap();
                let decision = if i == 0 {
                    PromptDecision::Allow
                } else {
                    PromptDecision::Deny {
                        reason: "no".into(),
                    }
                };
                barrier.wait();
                ls.drive_prompt_decide(id, "alpha", &decision)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let loser = results.into_iter().find(|r| r.is_err()).unwrap();
    assert!(conflict_msg(loser).starts_with(&format!("prompt {id} is already ")));
}

#[test]
fn prompt_stale_pending_reads_as_expired() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", Some(60), false).unwrap();
    ls.session_upsert("bravo", "waiting", None, None).unwrap();
    let id = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    assert!(ls.sessions_list().unwrap()[0].brokered);

    // Just inside the window it is still live.
    backdate_prompt(&ls, id, 60 + 29);
    assert_eq!(ls.drive_prompts_pending(None, None).unwrap().len(), 1);

    backdate_prompt(&ls, id, 60 + 31);
    assert!(ls.drive_prompts_pending(None, None).unwrap().is_empty());
    assert!(!ls.sessions_list().unwrap()[0].brokered);
    assert_eq!(status_of(&ls, id).0, "pending", "readers don't rewrite");
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(id, "alpha", &PromptDecision::Allow)),
        format!("prompt {id} expired")
    );
    assert_eq!(
        status_of(&ls, id),
        (
            "expired".into(),
            Some("timeout".into()),
            "hook gone: no decision recorded".into()
        )
    );

    // The writer rewrites a second stale row.
    let id2 = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    backdate_prompt(&ls, id2, 60 + 31);
    assert_eq!(ls.drive_prompts_expire_stale().unwrap(), 1);
    assert_eq!(status_of(&ls, id2).0, "expired");
    assert_eq!(status_of(&ls, id2).1.as_deref(), Some("timeout"));
}

#[test]
fn prompt_close_rejects_allowed_status() {
    let db = TempDb::new();
    let ls = db.open();
    let id = bound_with_prompt(&ls);
    for bad in ["allowed", "denied", "answered", "pending"] {
        assert!(matches!(
            ls.drive_prompt_close(id, bad, "timeout", "x"),
            Err(Error::Conflict(_))
        ));
    }
    assert_eq!(status_of(&ls, id).0, "pending");
    assert!(ls
        .drive_prompt_close(id, "escalated", "timeout", "no decision in time")
        .unwrap());
    let p = ls.drive_prompt_get(id).unwrap().unwrap();
    assert_eq!(p.status, "escalated");
    assert_eq!(p.decision_json, None);
    assert_eq!(p.decided_by.as_deref(), Some("timeout"));
}

#[test]
fn prompt_withdrawn_by_busy_idle_offline_but_not_waiting() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let insert = || {
        ls.drive_prompt_insert(&new_prompt("bravo", "permission"))
            .unwrap()
            .id
    };

    let id = insert();
    ls.session_upsert("bravo", "waiting", None, None).unwrap();
    assert_eq!(status_of(&ls, id).0, "pending", "waiting never withdraws");
    // Another worktree's state change doesn't touch it.
    ls.session_upsert("charlie", "busy", None, None).unwrap();
    assert_eq!(status_of(&ls, id).0, "pending");

    ls.session_upsert("bravo", "busy", None, None).unwrap();
    assert_eq!(
        status_of(&ls, id),
        (
            "withdrawn".into(),
            Some("system".into()),
            "session state changed".into()
        )
    );

    let id = insert();
    ls.session_upsert("bravo", "idle", None, None).unwrap();
    assert_eq!(status_of(&ls, id).0, "withdrawn");

    let id = insert();
    ls.session_set_offline("bravo").unwrap();
    assert_eq!(status_of(&ls, id).0, "withdrawn");
}

#[test]
fn resume_after_decision_keeps_other_pending_prompts() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    // Two parallel tool calls, each with its own gate and prompt.
    let a = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    let b = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    ls.session_upsert("bravo", "waiting", None, None).unwrap();

    ls.drive_prompt_decide(a, "alpha", &PromptDecision::Allow)
        .unwrap();
    assert!(!ls.session_resume_after_decision("bravo", None).unwrap());
    assert_eq!(
        status_of(&ls, b).0,
        "pending",
        "the other gate keeps its prompt"
    );
    assert_eq!(
        ls.session_state_get("bravo").unwrap().as_deref(),
        Some("waiting")
    );

    ls.drive_prompt_decide(b, "alpha", &PromptDecision::Allow)
        .unwrap();
    assert!(ls.session_resume_after_decision("bravo", Some(3)).unwrap());
    assert_eq!(
        ls.session_state_get("bravo").unwrap().as_deref(),
        Some("busy")
    );
    // Another worktree's pending prompt doesn't hold this one in `waiting`.
    ls.drive_bind("alpha", "charlie", None, false).unwrap();
    ls.drive_prompt_insert(&new_prompt("charlie", "permission"))
        .unwrap();
    assert!(ls.session_resume_after_decision("bravo", None).unwrap());
}

#[test]
fn terminal_prompt_statuses_are_final() {
    let db = TempDb::new();
    let ls = db.open();
    let id = bound_with_prompt(&ls);
    assert!(ls
        .drive_prompt_close(id, "withdrawn", "system", "x")
        .unwrap());
    assert!(!ls
        .drive_prompt_close(id, "escalated", "system", "y")
        .unwrap());
    assert!(!ls
        .drive_prompt_close(id, "expired", "timeout", "z")
        .unwrap());
    assert_eq!(
        conflict_msg(ls.drive_prompt_decide(id, "alpha", &PromptDecision::Allow)),
        format!("prompt {id} is already withdrawn")
    );
    assert_eq!(ls.drive_prompts_withdraw("bravo").unwrap(), 0);
    assert_eq!(ls.drive_take_over("bravo").unwrap(), 0);
    assert_eq!(status_of(&ls, id).2, "x");
}

#[test]
fn prompts_pending_filters_by_driver_and_driven() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_bind("delta", "charlie", None, false).unwrap();
    let a = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    let b = ls
        .drive_prompt_insert(&new_prompt("charlie", "permission"))
        .unwrap()
        .id;
    let ids = |v: Vec<crate::DrivePrompt>| v.into_iter().map(|p| p.id).collect::<Vec<_>>();
    assert_eq!(
        ids(ls.drive_prompts_pending(None, None).unwrap()),
        vec![a, b]
    );
    assert_eq!(
        ids(ls.drive_prompts_pending(Some("alpha"), None).unwrap()),
        vec![a]
    );
    assert_eq!(
        ids(ls.drive_prompts_pending(None, Some("charlie")).unwrap()),
        vec![b]
    );
    assert!(ls
        .drive_prompts_pending(Some("alpha"), Some("charlie"))
        .unwrap()
        .is_empty());
}

// ---- Requests ----

#[test]
fn request_add_validates_payload_per_kind() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let add = |kind: &str, payload: serde_json::Value, queue: bool| {
        ls.drive_request_add(kind, "bravo", "alpha", &payload, queue)
    };

    assert!(conflict_msg(add("send", serde_json::json!({}), false)).contains("\"text\""));
    assert!(conflict_msg(add("send", serde_json::json!({"text": ""}), false)).contains("\"text\""));
    assert!(conflict_msg(add("send", serde_json::json!({"text": 5}), false)).contains("\"text\""));
    let big = "x".repeat(crate::DRIVE_SEND_TEXT_MAX + 1);
    assert!(
        conflict_msg(add("send", serde_json::json!({ "text": big }), false)).contains("\"text\"")
    );
    assert!(
        conflict_msg(add("read", serde_json::json!({"lines": 0}), false)).contains("\"lines\"")
    );
    assert!(
        conflict_msg(add("read", serde_json::json!({"lines": 2001}), false)).contains("\"lines\"")
    );
    assert!(
        conflict_msg(add("read", serde_json::json!({"lines": "ten"}), false)).contains("\"lines\"")
    );
    assert!(conflict_msg(add("swap", serde_json::json!({}), false)).contains("\"to\""));
    assert!(conflict_msg(add("swap", serde_json::json!({"to": ""}), false)).contains("\"to\""));
    assert!(conflict_msg(add("reset", serde_json::json!({"to": 1}), false)).contains("\"to\""));
    assert!(conflict_msg(add("read", serde_json::json!({}), true)).contains("queue"));
    assert!(conflict_msg(add("send", serde_json::json!("hi"), false)).contains("JSON object"));
    assert!(conflict_msg(add("paint", serde_json::json!({}), false)).contains("kind"));
    assert!(ls.drive_requests_list(10).unwrap().is_empty());

    let max = "x".repeat(crate::DRIVE_SEND_TEXT_MAX);
    let r = add("send", serde_json::json!({ "text": max }), true).unwrap();
    assert!(r.queue);
    assert_eq!(r.status, "pending");
    assert_eq!(r.requested_by, "alpha");
    add("read", serde_json::json!({}), false).unwrap();
    add("read", serde_json::json!({"lines": 1}), false).unwrap();
    add(
        "read",
        serde_json::json!({"lines": 2000, "extra": true}),
        false,
    )
    .unwrap();
    add("reset", serde_json::json!({}), false).unwrap();
    add("reset", serde_json::json!({"to": "codex"}), false).unwrap();
    let s = add("swap", serde_json::json!({"to": "codex"}), false).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&s.payload).unwrap(),
        serde_json::json!({"to": "codex"})
    );
}

#[test]
fn request_add_validates_the_wake_flags() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let add = |kind: &str, payload: serde_json::Value| {
        ls.drive_request_add(kind, "bravo", "alpha", &payload, false)
    };

    // A send's `wake` and a wake's `fresh` are booleans.
    assert!(conflict_msg(add(
        "send",
        serde_json::json!({"text": "hi", "wake": "yes"})
    ))
    .contains("\"wake\""));
    assert!(conflict_msg(add("wake", serde_json::json!({"fresh": 1}))).contains("\"fresh\""));
    assert!(conflict_msg(add("wake", serde_json::json!({"to": ""}))).contains("\"to\""));
    assert!(conflict_msg(add("reset", serde_json::json!({"mode": "nap"}))).contains("\"mode\""));
    // A wake is no longer filed as a reset: an older app would run it as one.
    assert!(
        conflict_msg(add("reset", serde_json::json!({"mode": "wake"}))).contains("kind \"wake\"")
    );
    assert!(ls.drive_requests_list(10).unwrap().is_empty());

    add("send", serde_json::json!({"text": "hi", "wake": true})).unwrap();
    add("send", serde_json::json!({"text": "hi", "wake": false})).unwrap();
    add("wake", serde_json::json!({})).unwrap();
    add("reset", serde_json::json!({"mode": "reset"})).unwrap();
    let w = add("wake", serde_json::json!({"fresh": true, "to": "codex"})).unwrap();
    assert_eq!(w.kind, "wake");
    // A queue flag is still send-only.
    assert!(ls
        .drive_request_add("wake", "bravo", "alpha", &serde_json::json!({}), true)
        .is_err());
}

#[test]
fn request_add_refused_for_non_driver_unknown_or_paused() {
    let db = TempDb::new();
    let ls = db.open();
    let p = send("hi");
    assert_eq!(
        conflict_msg(ls.drive_request_add("send", "bravo", "alpha", &p, false)),
        "@bravo is not driven"
    );
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    assert_eq!(
        conflict_msg(ls.drive_request_add("send", "bravo", "charlie", &p, false)),
        "@charlie is not the driver of @bravo"
    );
    ls.drive_set_paused("bravo", true).unwrap();
    assert_eq!(
        conflict_msg(ls.drive_request_add("send", "bravo", "alpha", &p, false)),
        "drive is paused"
    );
    assert!(ls.drive_requests_list(10).unwrap().is_empty());
    // A read changes nothing in the session, so a paused drive still allows it.
    let r = ls
        .drive_request_add(
            "read",
            "bravo",
            "alpha",
            &serde_json::json!({"lines": 5}),
            false,
        )
        .unwrap();
    assert_eq!(r.status, "pending");
}

#[test]
fn request_lifecycle_pending_begin_resolve() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let r = ls
        .drive_request_add("send", "bravo", "alpha", &send("hi"), false)
        .unwrap();
    assert!(!r.is_resolved());
    assert!(ls.drive_request_begin(r.id).unwrap());
    assert!(!ls.drive_request_begin(r.id).unwrap(), "begin wins once");
    assert!(matches!(
        ls.drive_request_resolve(r.id, "running", "", ""),
        Err(Error::Conflict(_))
    ));
    ls.drive_request_resolve(r.id, "applied", "", r#"{"awaiting_turn":1}"#)
        .unwrap();
    let got = ls.drive_request_get(r.id).unwrap().unwrap();
    assert!(got.is_resolved());
    assert_eq!(got.result, r#"{"awaiting_turn":1}"#);
    assert!(got.resolved_at.is_some());
    assert_eq!(
        conflict_msg(ls.drive_request_resolve(r.id, "refused", "late", "")),
        format!("drive request {} is already applied", r.id)
    );
    assert_eq!(
        conflict_msg(ls.drive_request_resolve(999, "applied", "", "")),
        "no drive request 999"
    );
    assert_eq!(ls.drive_requests_list(10).unwrap()[0].id, r.id);
}

#[test]
fn request_swap_pending_confirm_then_applied_and_refused() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let to = serde_json::json!({"to": "codex"});
    for outcome in ["applied", "refused"] {
        let r = ls
            .drive_request_add("swap", "bravo", "alpha", &to, false)
            .unwrap();
        assert!(ls.drive_request_begin(r.id).unwrap());
        assert!(ls.drive_request_await_confirm(r.id).unwrap());
        assert_eq!(
            ls.drive_request_get(r.id).unwrap().unwrap().status,
            "pending_confirm"
        );
        ls.drive_request_resolve(r.id, outcome, "", "").unwrap();
        assert_eq!(ls.drive_request_get(r.id).unwrap().unwrap().status, outcome);
    }
}

#[test]
fn request_cancel_only_from_pending() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let a = ls
        .drive_request_add("read", "bravo", "alpha", &serde_json::json!({}), false)
        .unwrap();
    let b = ls
        .drive_request_add("read", "bravo", "alpha", &serde_json::json!({}), false)
        .unwrap();
    assert!(ls.drive_request_cancel(a.id, "stopped waiting").unwrap());
    let got = ls.drive_request_get(a.id).unwrap().unwrap();
    assert_eq!(
        (got.status.as_str(), got.reason.as_str()),
        ("refused", "stopped waiting")
    );
    assert!(!ls.drive_request_cancel(a.id, "again").unwrap());
    ls.drive_request_begin(b.id).unwrap();
    assert!(!ls.drive_request_cancel(b.id, "x").unwrap());
    assert_eq!(
        ls.drive_request_get(b.id).unwrap().unwrap().status,
        "running"
    );
}

#[test]
fn queued_send_stays_pending_until_expired() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let queued = ls
        .drive_request_add("send", "bravo", "alpha", &send("later"), true)
        .unwrap();
    let running = ls
        .drive_request_add("read", "bravo", "alpha", &serde_json::json!({}), false)
        .unwrap();
    let confirm = ls
        .drive_request_add(
            "swap",
            "bravo",
            "alpha",
            &serde_json::json!({"to": "codex"}),
            false,
        )
        .unwrap();
    ls.drive_request_begin(running.id).unwrap();
    ls.drive_request_begin(confirm.id).unwrap();
    ls.drive_request_await_confirm(confirm.id).unwrap();

    assert_eq!(ls.drive_requests_expire(600).unwrap(), 0, "fresh rows stay");
    for id in [queued.id, running.id, confirm.id] {
        backdate_request(&ls, id, 601);
    }
    backdate_confirm(&ls, confirm.id, DRIVE_CONFIRM_MAX_AGE_SECS + 1);
    assert_eq!(ls.drive_requests_expire(600).unwrap(), 2);
    let q = ls.drive_request_get(queued.id).unwrap().unwrap();
    assert_eq!(q.status, "refused");
    assert!(q.reason.starts_with("expired"), "{}", q.reason);
    let c = ls.drive_request_get(confirm.id).unwrap().unwrap();
    assert_eq!(c.status, "refused");
    assert_eq!(c.reason, "expired: the confirm wasn't answered in time");
    assert_eq!(
        ls.drive_request_get(running.id).unwrap().unwrap().status,
        "running"
    );
}

#[test]
fn pending_confirm_ages_from_its_confirm_start_not_creation() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let r = ls
        .drive_request_add(
            "swap",
            "bravo",
            "alpha",
            &serde_json::json!({"to": "codex"}),
            false,
        )
        .unwrap();
    ls.drive_request_begin(r.id).unwrap();
    // Queued and ran for most of the request max age before the confirm opened.
    backdate_request(&ls, r.id, 590);
    assert!(ls.drive_request_await_confirm(r.id).unwrap());
    backdate_request(&ls, r.id, 900);
    assert_eq!(
        ls.drive_requests_expire(600).unwrap(),
        0,
        "the human still has the confirm window"
    );
    assert_eq!(
        ls.drive_request_get(r.id).unwrap().unwrap().status,
        "pending_confirm"
    );
    // Just inside the bound: kept; past it: refused.
    backdate_confirm(&ls, r.id, DRIVE_CONFIRM_MAX_AGE_SECS - 5);
    assert_eq!(ls.drive_requests_expire(600).unwrap(), 0);
    backdate_confirm(&ls, r.id, DRIVE_CONFIRM_MAX_AGE_SECS + 1);
    assert_eq!(ls.drive_requests_expire(600).unwrap(), 1);
    let got = ls.drive_request_get(r.id).unwrap().unwrap();
    assert_eq!(got.status, "refused");
    assert_eq!(got.reason, "expired: the confirm wasn't answered in time");
}

#[test]
fn requests_open_and_latest_filter_by_kind_worktree_and_requester() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_bind("alpha", "charlie", None, false).unwrap();
    assert!(ls
        .drive_request_latest("send", "bravo", "alpha")
        .unwrap()
        .is_none());
    let a = ls
        .drive_request_add("send", "bravo", "alpha", &send("a"), false)
        .unwrap();
    let b = ls
        .drive_request_add("send", "bravo", "alpha", &send("b"), true)
        .unwrap();
    let c = ls
        .drive_request_add("send", "bravo", "alpha", &send("c"), false)
        .unwrap();
    ls.drive_request_add("read", "bravo", "alpha", &serde_json::json!({}), false)
        .unwrap();
    ls.drive_request_add("send", "charlie", "alpha", &send("x"), false)
        .unwrap();
    ls.drive_request_begin(b.id).unwrap();
    ls.drive_request_resolve(c.id, "refused", "busy", "")
        .unwrap();
    let open: Vec<i64> = ls
        .drive_requests_open("send", "bravo", "alpha")
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(open, vec![a.id, b.id]);
    assert!(ls
        .drive_requests_open("send", "bravo", "delta")
        .unwrap()
        .is_empty());
    let latest = ls
        .drive_request_latest("send", "bravo", "alpha")
        .unwrap()
        .unwrap();
    assert_eq!((latest.id, latest.status.as_str()), (c.id, "refused"));
}

#[test]
fn requests_pending_oldest_first() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let ids: Vec<i64> = (0..3)
        .map(|i| {
            ls.drive_request_add("send", "bravo", "alpha", &send(&format!("m{i}")), false)
                .unwrap()
                .id
        })
        .collect();
    ls.drive_request_begin(ids[1]).unwrap();
    let pending: Vec<i64> = ls
        .drive_requests_pending()
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(pending, vec![ids[0], ids[2]]);
    let newest: Vec<i64> = ls
        .drive_requests_list(2)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(newest, vec![ids[2], ids[1]]);
}

#[test]
fn request_swap_confirm_round_trip_running_pending_confirm_running() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let r = ls
        .drive_request_add(
            "swap",
            "bravo",
            "alpha",
            &serde_json::json!({"to": "codex"}),
            false,
        )
        .unwrap();
    assert!(
        !ls.drive_request_resume(r.id).unwrap(),
        "pending can't resume"
    );
    assert!(
        !ls.drive_request_await_confirm(r.id).unwrap(),
        "pending can't await confirm"
    );
    ls.drive_request_begin(r.id).unwrap();
    assert!(
        !ls.drive_request_resume(r.id).unwrap(),
        "running can't resume"
    );
    assert!(ls.drive_request_await_confirm(r.id).unwrap());
    assert!(!ls.drive_request_await_confirm(r.id).unwrap());
    assert!(ls.drive_request_resume(r.id).unwrap());
    assert!(!ls.drive_request_resume(r.id).unwrap());
    assert_eq!(
        ls.drive_request_get(r.id).unwrap().unwrap().status,
        "running"
    );
    ls.drive_request_resolve(r.id, "applied", "", r#"{"provider":"codex"}"#)
        .unwrap();
    assert!(
        !ls.drive_request_resume(r.id).unwrap(),
        "applied can't resume"
    );
}

#[test]
fn requests_in_flight_lists_running_and_pending_confirm_only() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let mk = || {
        ls.drive_request_add(
            "swap",
            "bravo",
            "alpha",
            &serde_json::json!({"to": "codex"}),
            false,
        )
        .unwrap()
        .id
    };
    let _pending = mk();
    let running = mk();
    let confirm = mk();
    let done = mk();
    ls.drive_request_begin(running).unwrap();
    ls.drive_request_begin(confirm).unwrap();
    ls.drive_request_await_confirm(confirm).unwrap();
    ls.drive_request_begin(done).unwrap();
    ls.drive_request_resolve(done, "refused", "cancelled", "")
        .unwrap();
    let ids: Vec<i64> = ls
        .drive_requests_in_flight()
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, vec![running, confirm]);
}

// ---- Replies / sessions / misc ----

#[test]
fn session_reply_record_bumps_turn_and_truncates_on_char_boundary() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(ls.session_reply_get("bravo").unwrap(), None);
    assert_eq!(ls.session_reply_record("bravo", "first").unwrap(), 1);
    assert_eq!(ls.session_reply_record("bravo", "second").unwrap(), 2);
    let r = ls.session_reply_get("bravo").unwrap().unwrap();
    assert_eq!((r.turn, r.last_message.as_str()), (2, "second"));

    // One ASCII byte then 3-byte chars: the cap falls inside a char.
    let text = format!("a{}", "日".repeat(SESSION_REPLY_MAX / 3 + 10));
    assert_eq!(ls.session_reply_record("bravo", &text).unwrap(), 3);
    let kept = ls.session_reply_get("bravo").unwrap().unwrap().last_message;
    assert!(kept.len() <= SESSION_REPLY_MAX);
    assert!(kept.len() > SESSION_REPLY_MAX - 3);
    assert!(text.starts_with(&kept));
    assert_eq!(ls.session_reply_record("charlie", "x").unwrap(), 1);
}

#[test]
fn session_idle_secs_none_without_row_and_counts_up() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(ls.session_idle_secs("bravo").unwrap(), None);
    ls.session_upsert("bravo", "idle", None, None).unwrap();
    let fresh = ls.session_idle_secs("bravo").unwrap().unwrap();
    assert!((0..=2).contains(&fresh), "{fresh}");
    ls.conn
        .execute(
            "UPDATE worktree_sessions SET last_heartbeat = datetime('now', '-90 seconds') \
             WHERE worktree = 'bravo'",
            [],
        )
        .unwrap();
    let later = ls.session_idle_secs("bravo").unwrap().unwrap();
    assert!((90..=92).contains(&later), "{later}");
}

#[test]
fn pending_prompt_counts_group_by_worktree_and_skip_stale_and_decided() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.drive_bind("alpha", "charlie", None, false).unwrap();
    ls.drive_bind("delta", "echo", None, false).unwrap();
    for _ in 0..2 {
        ls.drive_prompt_insert(&new_prompt("bravo", "permission"))
            .unwrap();
    }
    let stale = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    backdate_prompt(&ls, stale, 300 + 31);
    ls.drive_prompt_insert(&new_prompt("charlie", "permission"))
        .unwrap();
    let decided = ls
        .drive_prompt_insert(&new_prompt("echo", "permission"))
        .unwrap()
        .id;
    ls.drive_prompt_decide(
        decided,
        "delta",
        &PromptDecision::Escalate {
            reason: "unsure".into(),
        },
    )
    .unwrap();
    let counts = ls.drive_pending_prompt_counts().unwrap();
    assert_eq!(counts.len(), 2);
    assert_eq!(counts["bravo"], 2);
    assert_eq!(counts["charlie"], 1);
    assert!(!counts.contains_key("echo"));
}

#[test]
fn read_paths_tolerate_missing_drive_tables() {
    let db = TempDb::new();
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        conn.execute_batch(OLD_SCHEMA).unwrap();
    }
    let ro = LocalStore::open_read_only(&db.path).unwrap();
    let sessions = ro.sessions_list().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].worktree, "bravo");
    assert_eq!(sessions[0].driven_by, None);
    assert!(!sessions[0].brokered);
    assert_eq!(sessions[0].escalated_reason, None);
    assert_eq!(ro.session_revision().unwrap(), "bravo=busy:");
    assert_eq!(ro.drive_get("bravo").unwrap(), None);
    assert!(ro.drives_list().unwrap().is_empty());
    assert!(ro.drives_by_driver("alpha").unwrap().is_empty());
    assert!(ro.drive_prompts_pending(None, None).unwrap().is_empty());
    assert!(ro.drive_requests_pending().unwrap().is_empty());
    assert!(ro.drive_requests_in_flight().unwrap().is_empty());
    assert!(ro.drive_pending_prompt_counts().unwrap().is_empty());
    let names = table_names(&ro.conn);
    for t in [
        "drives",
        "drive_prompts",
        "drive_requests",
        "session_replies",
    ] {
        assert!(!names.contains(&t.to_string()), "{t} was created");
    }
}

#[test]
fn session_revision_changes_with_driver_pending_flag_and_newest_prompt_id_not_heartbeat() {
    let db = TempDb::new();
    let ls = db.open();
    ls.session_upsert("bravo", "waiting", None, None).unwrap();
    let r0 = ls.session_revision().unwrap();
    assert_eq!(r0, "bravo=waiting:::0:0");

    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let r1 = ls.session_revision().unwrap();
    assert_eq!(r1, "bravo=waiting::alpha:0:0", "became driven");

    let id = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    let r2 = ls.session_revision().unwrap();
    assert_eq!(r2, format!("bravo=waiting::alpha:1:{id}"), "prompt pending");

    ls.session_upsert("bravo", "waiting", None, None).unwrap(); // heartbeat
    assert_eq!(ls.session_revision().unwrap(), r2, "heartbeat is ignored");

    ls.drive_prompt_close(id, "escalated", "timeout", "no answer")
        .unwrap();
    let r3 = ls.session_revision().unwrap();
    assert_eq!(
        r3,
        format!("bravo=waiting::alpha:0:{id}"),
        "flipped to escalated"
    );

    let id2 = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    ls.drive_prompt_close(id2, "escalated", "timeout", "no answer")
        .unwrap();
    let r4 = ls.session_revision().unwrap();
    assert_ne!(r4, r3, "a new prompt changes the newest id");
}

#[test]
fn data_version_changes_after_other_connection_writes() {
    let db = TempDb::new();
    let a = db.open();
    let b = db.open();
    let v0 = a.data_version().unwrap();
    a.session_upsert("bravo", "idle", None, None).unwrap();
    assert_eq!(a.data_version().unwrap(), v0, "own writes don't count");
    b.session_upsert("bravo", "busy", None, None).unwrap();
    assert_ne!(a.data_version().unwrap(), v0);
}

#[test]
fn sessions_list_fills_driven_by_brokered_escalated_reason() {
    let db = TempDb::new();
    let ls = db.open();
    ls.session_upsert("charlie", "idle", None, None).unwrap();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.session_upsert("bravo", "waiting", None, None).unwrap();
    let id = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    let find = |w: &str| {
        ls.sessions_list()
            .unwrap()
            .into_iter()
            .find(|s| s.worktree == w)
            .unwrap()
    };
    let b = find("bravo");
    assert_eq!(b.driven_by.as_deref(), Some("alpha"));
    assert!(b.brokered);
    assert_eq!(b.escalated_reason, None);
    let c = find("charlie");
    assert_eq!(
        (c.driven_by, c.brokered, c.escalated_reason),
        (None, false, None)
    );

    ls.drive_prompt_decide(
        id,
        "alpha",
        &PromptDecision::Escalate {
            reason: "needs a human".into(),
        },
    )
    .unwrap();
    let b = find("bravo");
    assert!(!b.brokered);
    assert_eq!(b.escalated_reason.as_deref(), Some("needs a human"));

    // A newer, non-escalated prompt hides the older reason.
    let id2 = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    assert_eq!(find("bravo").escalated_reason, None);
    ls.drive_prompt_close(id2, "escalated", "timeout", "no answer in time")
        .unwrap();
    assert_eq!(
        find("bravo").escalated_reason.as_deref(),
        Some("no answer in time")
    );

    ls.session_upsert("bravo", "busy", None, None).unwrap();
    let b = find("bravo");
    assert_eq!(b.escalated_reason, None, "only while waiting");
    assert_eq!(b.driven_by.as_deref(), Some("alpha"));
}

#[test]
fn drive_purge_deletes_only_old_terminal_rows() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    ls.session_reply_record("bravo", "x").unwrap();
    let old_prompt = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    ls.drive_prompt_close(old_prompt, "escalated", "timeout", "x")
        .unwrap();
    let new_prompt_id = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    ls.drive_prompt_close(new_prompt_id, "escalated", "timeout", "x")
        .unwrap();
    let pending_prompt = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    let old_req = ls
        .drive_request_add("send", "bravo", "alpha", &send("a"), false)
        .unwrap()
        .id;
    ls.drive_request_resolve(old_req, "applied", "", "")
        .unwrap();
    let new_req = ls
        .drive_request_add("send", "bravo", "alpha", &send("b"), false)
        .unwrap()
        .id;
    ls.drive_request_resolve(new_req, "refused", "", "")
        .unwrap();
    let open_req = ls
        .drive_request_add("send", "bravo", "alpha", &send("c"), true)
        .unwrap()
        .id;

    let past = format!("-{} seconds", DRIVE_RETENTION_SECS + 60);
    ls.conn
        .execute(
            "UPDATE drive_prompts SET decided_at = datetime('now', ?2), \
               created_at = datetime('now', ?2) WHERE id = ?1",
            params![old_prompt, past],
        )
        .unwrap();
    ls.conn
        .execute(
            "UPDATE drive_requests SET resolved_at = datetime('now', ?2), \
               created_at = datetime('now', ?2) WHERE id IN (?1, ?3)",
            params![old_req, past, open_req],
        )
        .unwrap();

    assert_eq!(ls.drive_purge(DRIVE_RETENTION_SECS).unwrap(), 2);
    assert!(ls.drive_prompt_get(old_prompt).unwrap().is_none());
    assert!(ls.drive_prompt_get(new_prompt_id).unwrap().is_some());
    assert!(ls.drive_prompt_get(pending_prompt).unwrap().is_some());
    assert!(ls.drive_request_get(old_req).unwrap().is_none());
    assert!(ls.drive_request_get(new_req).unwrap().is_some());
    assert!(ls.drive_request_get(open_req).unwrap().is_some());
    assert!(ls.drive_get("bravo").unwrap().is_some());
    assert!(ls.session_reply_get("bravo").unwrap().is_some());
}

#[test]
fn opening_old_schema_db_adds_drive_tables() {
    let db = TempDb::new();
    {
        let conn = rusqlite::Connection::open(&db.path).unwrap();
        conn.execute_batch(OLD_SCHEMA).unwrap();
    }
    let ls = db.open();
    let names = table_names(&ls.conn);
    for t in [
        "drives",
        "drive_prompts",
        "drive_requests",
        "session_replies",
    ] {
        assert!(names.contains(&t.to_string()), "{t} missing");
    }
    assert!(ls.drives_list().unwrap().is_empty());
    assert_eq!(ls.sessions_list().unwrap()[0].worktree, "bravo");
}

#[test]
fn ddl_rejects_self_drive_and_out_of_range_rows() {
    let db = TempDb::new();
    let ls = db.open();
    assert!(ls
        .conn
        .execute(
            "INSERT INTO drives (driven, driver) VALUES ('alpha', 'alpha')",
            []
        )
        .is_err());
    assert!(ls
        .conn
        .execute(
            "INSERT INTO drives (driven, driver, escalate_after_s) VALUES ('bravo', 'alpha', 541)",
            []
        )
        .is_err());
}

#[test]
fn session_state_get_reads_one_row() {
    let db = TempDb::new();
    let ls = db.open();
    assert_eq!(ls.session_state_get("alpha").unwrap(), None);
    ls.session_upsert("alpha", "busy", None, None).unwrap();
    ls.session_upsert("bravo", "idle", None, None).unwrap();
    assert_eq!(
        ls.session_state_get("alpha").unwrap().as_deref(),
        Some("busy")
    );
    assert_eq!(
        ls.session_state_get("bravo").unwrap().as_deref(),
        Some("idle")
    );
    assert_eq!(ls.session_state_get("charlie").unwrap(), None);
}

#[test]
fn prompt_age_is_computed_by_the_store() {
    let db = TempDb::new();
    let ls = db.open();
    ls.drive_bind("alpha", "bravo", None, false).unwrap();
    let id = ls
        .drive_prompt_insert(&new_prompt("bravo", "permission"))
        .unwrap()
        .id;
    let fresh = ls.drive_prompt_get(id).unwrap().unwrap();
    assert!((0..5).contains(&fresh.age_s), "{}", fresh.age_s);
}

#[test]
fn drive_tables_probe_is_cached_once_seen() {
    let db = TempDb::new();
    let ls = db.open();
    assert!(ls.drive_tables_exist().unwrap());
    assert!(ls.drive_tables_seen.get());
}
