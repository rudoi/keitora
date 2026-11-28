//! `keitora status` over temp DBs: the JSON and human shapes for a fresh, stale and
//! missing `status.db`, the worktree filter with its turns, exit codes, and the
//! health notes stalls and timeouts carry.

use std::path::PathBuf;

use keitora_tasks::status_store::{
    write_app_project_for_test, write_findings_for_test, write_status_for_test,
};
use keitora_tasks::{AppProject, CliKind, FindingRow, LocalStore, StatusSession, WriterRow};
use serde_json::json;

use super::*;

struct Rig {
    dir: tempfile::TempDir,
    local: LocalStore,
    /// Unix ms of bravo's hook write (second resolution, from SQLite's clock).
    hook_at: i64,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let local = LocalStore::open(dir.path().join("tasks.db").to_str().unwrap()).unwrap();
    local.drive_bind("alpha", "bravo", None, false).unwrap();
    local
        .session_upsert("bravo", "offline", None, None)
        .unwrap();
    let hook_at = local
        .sessions_list()
        .unwrap()
        .into_iter()
        .find(|s| s.worktree == "bravo")
        .and_then(|s| sqlite_utc_to_ms(&s.last_heartbeat))
        .unwrap();
    Rig {
        dir,
        local,
        hook_at,
    }
}

impl Rig {
    fn status_db(&self) -> PathBuf {
        self.dir.path().join("status.db")
    }
    /// Two hours after bravo's hook write.
    fn now(&self) -> i64 {
        self.hook_at + 2 * 3600 * 1000
    }
}

fn cli_of(w: &str) -> Option<CliKind> {
    match w {
        "alpha" => Some(CliKind::Claude),
        "bravo" => Some(CliKind::Codex),
        _ => None,
    }
}

fn run(
    r: &Rig,
    status_db: Option<&std::path::Path>,
    filter: Option<&str>,
) -> Result<Output, ExitError> {
    let input = StatusInput {
        local: &r.local,
        project: "my-game".into(),
        worktrees: vec!["alpha".into(), "bravo".into()],
        worktree_cli: &cli_of,
        status_db,
        now_ms: r.now(),
    };
    status(&input, filter)
}

fn writer(now: i64) -> WriterRow {
    WriterRow {
        instance_id: "app-1".into(),
        pid: 4242,
        version: "0.6.0".into(),
        started_at: 1_767_261_600_000, // 2026-01-01T10:00:00Z
        heartbeat_at: now - 2_000,
        last_snapshot_at: Some(now - 1_000),
        herdr_link: "up".into(),
        herdr_link_since: now - 60_000,
    }
}

fn view(wt: &str, status: &str, now: i64) -> StatusSession {
    StatusSession {
        project: "my-game".into(),
        worktree: wt.into(),
        state: "idle".into(),
        agent: "present".into(),
        agent_kind: Some("id".into()),
        herdr_status: Some(status.into()),
        settled: true,
        stale: false,
        interactive_ready: Some(true),
        pane_id: Some(format!("p-{wt}")),
        terminal_id: Some(format!("t-{wt}")),
        agent_session: Some("00000000-0000-4000-8000-000000000001".into()),
        state_change_seq: Some(4),
        completion_seq: Some(3),
        brokered: false,
        escalated_reason: None,
        last_change_at: now - 31_000,
    }
}

fn presence() -> AppProject {
    AppProject {
        project: "my-game".into(),
        registry_key: "my-game".into(),
        db_path: "D:/repos/my-game/tasks.db".into(),
        open: true,
        open_reported_at: Some(1),
        watcher_state: "running".into(),
        watcher_started_at: Some(1),
        watcher_last_event_at: None,
        last_pass_at: None,
        last_pass_trigger: None,
        pending_count: 0,
        oldest_pending_at: None,
        restarts: 0,
        last_error: String::new(),
        last_error_at: None,
    }
}

fn finding(project: &str, wt: &str, check: &str, severity: &str, last_seen: i64) -> FindingRow {
    FindingRow {
        project: project.into(),
        worktree: wt.into(),
        check_id: check.into(),
        severity: severity.into(),
        message: format!("{check} message"),
        detail: "{}".into(),
        first_seen: 1_767_261_900_000, // 2026-01-01T10:05:00Z
        last_seen,
        occurrences: 1,
        cleared_at: None,
        remedy: String::new(),
        remedy_at: None,
    }
}

/// A live app: views for alpha and bravo, presence, and findings.
fn publish(r: &Rig) {
    let now = r.now();
    let mut p = presence();
    p.watcher_last_event_at = Some(now - 40_000);
    p.last_pass_at = Some(now - 12_000);
    write_status_for_test(
        &r.status_db(),
        writer(now),
        &[view("alpha", "idle", now), view("bravo", "idle", now)],
    )
    .unwrap();
    write_app_project_for_test(&r.status_db(), &p).unwrap();
    let last_seen = 1_767_262_140_000; // 2026-01-01T10:09:00Z
    let cleared = FindingRow {
        cleared_at: Some(last_seen),
        ..finding("my-game", "alpha", "herdr_unknown", "warn", last_seen)
    };
    write_findings_for_test(
        &r.status_db(),
        &[
            finding("my-game", "bravo", "hooks_vs_herdr", "warn", last_seen),
            finding("other-proj", "main", "hooks_missing", "error", last_seen),
            cleared,
        ],
    )
    .unwrap();
}

#[test]
fn fresh_json_has_the_documented_shape() {
    let r = rig();
    publish(&r);
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(
        out.value,
        json!({
            "source": "status_db",
            "app": { "running": true, "pid": 4242, "version": "0.6.0",
                     "started_at": "2026-01-01T10:00:00Z", "heartbeat_age_s": 2,
                     "herdr_link": "up", "snapshot_age_s": 1, "engine_ready": true },
            "project": { "name": "my-game", "open": true,
                         "watcher": { "state": "running", "last_event_age_s": 40,
                                      "last_pass_age_s": 12, "restarts": 0 },
                         "pending_requests": 0, "oldest_pending_age_s": null },
            "worktrees": [
                { "name": "alpha", "provider": "claude", "state": "idle", "agent": "present",
                  "herdr_status": "idle", "settled": true, "stale": false, "pane_id": "p-alpha",
                  "agent_session": "00000000-0000-4000-8000-000000000001", "driven_by": null,
                  "last_change_age_s": 31, "hook": null, "findings": [] },
                { "name": "bravo", "provider": "codex", "state": "idle", "agent": "present",
                  "herdr_status": "idle", "settled": true, "stale": false, "pane_id": "p-bravo",
                  "agent_session": "00000000-0000-4000-8000-000000000001", "driven_by": "alpha",
                  "last_change_age_s": 31,
                  "hook": { "state": "offline", "last_write_age_s": 7200 },
                  "findings": ["hooks_vs_herdr"] }
            ],
            "findings": [
                { "project": "my-game", "worktree": "bravo", "check_id": "hooks_vs_herdr",
                  "severity": "warn", "message": "hooks_vs_herdr message",
                  "first_seen": "2026-01-01T10:05:00Z", "last_seen": "2026-01-01T10:09:00Z",
                  "occurrences": 1, "remedy": "" }
            ]
        })
    );
}

#[test]
fn fresh_human_output() {
    let r = rig();
    publish(&r);
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert_eq!(
        out.human,
        "keitora status (project my-game)
  app       running (pid 4242, v0.6.0), herdr up, snapshot 1s ago
  project   open in window; watcher running; 0 pending requests
  worktrees
    @alpha  idle  claude  herdr: idle (settled)
    @bravo  idle  codex   herdr: idle (settled)   driven by @alpha   hooks: offline, last write 2h ago
  findings (1)
    warn  @bravo  hooks_vs_herdr  hooks_vs_herdr message"
    );
}

#[test]
fn a_stale_status_db_shows_the_hook_rows_only() {
    let r = rig();
    let now = r.now();
    let old = WriterRow {
        heartbeat_at: now - 300_000,
        ..writer(now)
    };
    write_status_for_test(&r.status_db(), old, &[view("bravo", "idle", now)]).unwrap();
    write_findings_for_test(
        &r.status_db(),
        &[finding("my-game", "bravo", "hooks_vs_herdr", "warn", now)],
    )
    .unwrap();
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["source"], "legacy");
    assert_eq!(
        out.value["app"],
        json!({ "running": false, "heartbeat_age_s": 300 })
    );
    assert_eq!(out.value["findings"], json!([]));
    assert_eq!(
        out.value["worktrees"][1]["state"], "offline",
        "from the hook row"
    );
    assert_eq!(out.value["worktrees"][1]["agent"], Value::Null);
    assert_eq!(
        out.value["worktrees"][0]["state"], "offline",
        "no row at all"
    );
    assert_eq!(
        out.human,
        "keitora status (project my-game)
  app       not running (last update 5m ago); showing hook rows only
  worktrees
    @alpha  offline  claude
    @bravo  offline  codex    driven by @alpha   hooks: offline, last write 2h ago"
    );
}

#[test]
fn a_missing_status_db_reads_as_no_app() {
    let r = rig();
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert_eq!(out.code, 0);
    assert_eq!(out.value["source"], "legacy");
    assert_eq!(
        out.value["app"],
        json!({ "running": false, "heartbeat_age_s": null })
    );
    assert!(out.value["app"].get("engine_ready").is_none());
    assert!(out.human.contains(
        "app       not running (no status reported on this machine); showing hook rows only"
    ));
    assert!(!r.status_db().exists(), "never created");
    let out = run(&r, None, None).unwrap();
    assert_eq!(out.value["source"], "legacy");
}

#[test]
fn while_herdr_recovers_the_hook_rows_stand_in_marked_stale() {
    let r = rig();
    let now = r.now();
    r.local.session_upsert("bravo", "busy", None, None).unwrap();
    r.local.session_upsert("alpha", "idle", None, None).unwrap();
    let recovering = WriterRow {
        herdr_link: "recovering".into(),
        ..writer(now)
    };
    // Only bravo has an (old) view; alpha has just its hook row.
    write_status_for_test(&r.status_db(), recovering, &[view("bravo", "idle", now)]).unwrap();
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert_eq!(out.value["source"], "status_db");
    assert_eq!(out.value["app"]["engine_ready"], false);
    let bravo = &out.value["worktrees"][1];
    assert_eq!(bravo["state"], "busy", "the hook row, not the old view");
    assert_eq!(bravo["stale"], true);
    let alpha = &out.value["worktrees"][0];
    assert_eq!(
        alpha["state"], "idle",
        "no view is not offline while herdr is down"
    );
    assert_eq!(alpha["stale"], true);
    assert!(
        out.human.contains(
            "
    @bravo  busy  codex   herdr: idle (settled) [stale]   driven by @alpha"
        ),
        "{}",
        out.human
    );
    assert!(
        out.human.contains(
            "
    @alpha  idle  claude  herdr: not ready [stale]"
        ),
        "{}",
        out.human
    );
}

#[test]
fn a_stale_view_falls_back_to_the_hook_row() {
    let r = rig();
    let now = r.now();
    r.local.session_upsert("bravo", "busy", None, None).unwrap();
    let old = StatusSession {
        stale: true,
        ..view("bravo", "idle", now)
    };
    write_status_for_test(
        &r.status_db(),
        writer(now),
        &[view("alpha", "idle", now), old],
    )
    .unwrap();
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert_eq!(out.value["app"]["engine_ready"], true);
    assert_eq!(out.value["worktrees"][0]["state"], "idle");
    assert_eq!(out.value["worktrees"][0]["stale"], false);
    assert_eq!(out.value["worktrees"][1]["state"], "busy");
    assert_eq!(out.value["worktrees"][1]["stale"], true);
}

fn add_turns(local: &LocalStore, n: usize) {
    for i in 0..n {
        let reply = format!("reply {i}");
        local
            .drive_turn_insert(&keitora_tasks::NewDriveTurn {
                driven: "bravo",
                kind: "human",
                delivery_id: None,
                outcome: "finished",
                signal: if i % 2 == 0 { "herdr" } else { "hook" },
                clause: "completion_seq",
                completion_seq: None,
                state_change_seq: None,
                reply: &reply,
                reply_source: if i % 2 == 0 { "none" } else { "hook" },
                caveat: None,
            })
            .unwrap()
            .unwrap();
    }
}

#[test]
fn a_worktree_filter_adds_its_last_five_turns() {
    let r = rig();
    publish(&r);
    add_turns(&r.local, 6);
    let out = run(&r, Some(&r.status_db()), Some("bravo")).unwrap();
    let wts = out.value["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 1);
    assert_eq!(wts[0]["name"], "bravo");
    let turns = out.value["detail"]["turns"].as_array().unwrap();
    assert_eq!(turns.len(), STATUS_TURNS);
    assert_eq!(
        turns[0].as_object().unwrap().keys().collect::<Vec<_>>(),
        ["completed_at", "kind", "reply_source", "signal"]
    );
    assert_eq!(turns[0]["kind"], "human");
    assert_eq!(turns[0]["signal"], "hook", "newest first (the sixth turn)");
    assert!(turns[0]["completed_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        out.value["findings"].as_array().unwrap().len(),
        1,
        "its own findings"
    );
    let human = &out.human;
    assert!(human.starts_with("keitora status @bravo (project my-game)\n"));
    assert!(human.contains("\n  worktree\n    @bravo  idle  codex  herdr: idle (settled)"));
    assert!(human.contains("\n    warn  hooks_vs_herdr  hooks_vs_herdr message\n"));
    assert!(human.contains(&format!(
        "          first seen {}, 1x",
        keitora_tasks::clock_text(1_767_261_900_000)
    )));
    assert!(human.contains("\n  turns (last 5)\n"));
    assert!(human.contains("human  hook  reply: hook"));
    // Without the filter there is no detail.
    let out = run(&r, Some(&r.status_db()), None).unwrap();
    assert!(out.value.get("detail").is_none());
}

#[test]
fn exit_codes() {
    let r = rig();
    assert_eq!(run(&r, None, None).unwrap().code, 0);
    let e = run(&r, None, Some("zulu")).unwrap_err();
    assert_eq!(e.code, 4);
    assert_eq!(e.msg, "no worktree named zulu in project my-game");
    // A task DB that can't be opened is an unresolvable project: exit 1.
    let missing = r.dir.path().join("nope").join("tasks.db");
    let e = LocalStore::open_read_only(missing.to_str().unwrap())
        .map_err(crate::drive::store_err)
        .err()
        .unwrap();
    assert_eq!(e.code, 1);
}

#[test]
fn health_notes_are_the_two_worst_active_ones_while_fresh() {
    let r = rig();
    let now = r.now();
    write_status_for_test(&r.status_db(), writer(now), &[]).unwrap();
    write_findings_for_test(
        &r.status_db(),
        &[
            finding("my-game", "bravo", "a_info", "info", now),
            finding("my-game", "bravo", "b_warn_old", "warn", now - 10),
            finding("my-game", "bravo", "c_warn_new", "warn", now),
            finding("my-game", "bravo", "d_error", "error", now - 99),
            FindingRow {
                cleared_at: Some(now),
                ..finding("my-game", "bravo", "e_cleared", "error", now)
            },
            finding("my-game", "alpha", "f_other", "error", now),
        ],
    )
    .unwrap();
    let notes = health_notes(Some(&r.status_db()), "my-game", "bravo", now).unwrap();
    let ids: Vec<&str> = notes.iter().map(|f| f.check_id.as_str()).collect();
    assert_eq!(ids, ["d_error", "c_warn_new"]);
    // Stale: none at all.
    assert!(health_notes(Some(&r.status_db()), "my-game", "bravo", now + 60_000).is_none());
    assert!(health_notes(None, "my-game", "bravo", now).is_none());

    let mut out = Output {
        value: json!({"event": "stalled"}),
        human: "  @bravo looks stalled".into(),
        code: 0,
    };
    attach_health(&mut out, Some(notes));
    assert_eq!(
        out.value["findings"],
        json!([
            {"check_id": "d_error", "severity": "error", "message": "d_error message"},
            {"check_id": "c_warn_new", "severity": "warn", "message": "c_warn_new message"},
        ])
    );
    assert_eq!(
        out.human,
        "  @bravo looks stalled\n  health: d_error message\n  health: c_warn_new message"
    );
    let mut out = Output {
        value: json!({}),
        human: "x".into(),
        code: 0,
    };
    attach_health(&mut out, None);
    assert!(out.value.get("findings").is_none());
    assert_eq!(out.human, "x");
}
