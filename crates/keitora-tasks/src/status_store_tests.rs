use super::*;
use tempfile::TempDir;

const T0: i64 = 1_800_000_000_000;

fn db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.db");
    (dir, path)
}

fn me(id: &str) -> WriterIdentity {
    WriterIdentity {
        instance_id: id.to_string(),
        pid: 42,
        version: "0.0.0-test".to_string(),
        started_at: T0,
    }
}

fn writer_row(heartbeat_at: i64, link: &str, snapshot: Option<i64>) -> WriterRow {
    WriterRow {
        instance_id: "a".to_string(),
        pid: 7,
        version: "0.0.0-test".to_string(),
        started_at: T0,
        heartbeat_at,
        last_snapshot_at: snapshot,
        herdr_link: link.to_string(),
        herdr_link_since: T0,
    }
}

fn view(project: &str, worktree: &str, state: &str, agent: &str) -> StatusSession {
    StatusSession {
        project: project.to_string(),
        worktree: worktree.to_string(),
        state: state.to_string(),
        agent: agent.to_string(),
        agent_kind: Some("claude".to_string()),
        herdr_status: Some("idle".to_string()),
        settled: true,
        stale: false,
        interactive_ready: Some(true),
        pane_id: Some("w1:p1".to_string()),
        terminal_id: Some("term_1".to_string()),
        agent_session: None,
        state_change_seq: Some(3),
        completion_seq: None,
        brokered: false,
        escalated_reason: None,
        last_change_at: T0,
    }
}

fn reader(path: &Path) -> StatusReader {
    StatusReader::open(path).unwrap().expect("reader")
}

fn owned(path: &Path, id: &str, now: i64) -> StatusWriter {
    let mut w = StatusWriter::open(path).unwrap();
    assert_eq!(w.try_acquire(&me(id), now).unwrap(), LeaseOutcome::Owned);
    w
}

// ── Lease ────────────────────────────────────────────────────────────────────

#[test]
fn writer_takes_a_fresh_lease_and_heartbeats_only_the_writer_row() {
    let (_d, path) = db();
    let mut w = owned(&path, "a", T0);
    assert_eq!(w.owner(), Some("a"));
    w.publish_sessions(&[view("proj", "alpha", "idle", "present")], T0)
        .unwrap();
    let r = reader(&path);
    let row = r.writer().unwrap().unwrap();
    assert_eq!(row.instance_id, "a");
    assert_eq!(row.heartbeat_at, T0);
    assert_eq!(row.herdr_link, "starting");
    assert_eq!(row.last_snapshot_at, None);
    let before = r.sessions("proj").unwrap();

    assert!(w.heartbeat("a", T0 + 5_000).unwrap());
    assert_eq!(r.writer().unwrap().unwrap().heartbeat_at, T0 + 5_000);
    assert_eq!(r.sessions("proj").unwrap(), before);
    let written_at: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT written_at FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(written_at, T0, "a heartbeat never touches session rows");
}

#[test]
fn second_writer_is_refused_while_the_heartbeat_is_fresh() {
    let (_d, path) = db();
    let _a = owned(&path, "a", T0);
    let mut b = StatusWriter::open(&path).unwrap();
    let got = b.try_acquire(&me("b"), T0 + 14_000).unwrap();
    assert_eq!(
        got,
        LeaseOutcome::Standby {
            pid: 42,
            version: "0.0.0-test".to_string(),
            heartbeat_at: T0
        }
    );
    assert_eq!(b.owner(), None);
    // The loser stays read-only: publishing writes nothing.
    assert_eq!(
        b.publish_sessions(&[view("proj", "alpha", "idle", "present")], T0)
            .unwrap(),
        0
    );
    assert!(reader(&path).sessions("proj").unwrap().is_empty());
    assert_eq!(reader(&path).writer().unwrap().unwrap().instance_id, "a");
}

#[test]
fn second_writer_takes_over_after_15s() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    let mut b = StatusWriter::open(&path).unwrap();
    assert_eq!(
        b.try_acquire(&me("b"), T0 + 16_000).unwrap(),
        LeaseOutcome::Owned
    );
    assert!(!a.heartbeat("a", T0 + 16_500).unwrap());
    assert_eq!(a.owner(), None);
    assert!(b.heartbeat("b", T0 + 17_000).unwrap());
    // The old writer can no longer write rows.
    assert_eq!(
        a.publish_sessions(&[view("proj", "alpha", "busy", "present")], T0)
            .unwrap(),
        0
    );
}

#[test]
fn lease_lost_between_beats_is_noticed_by_publish() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    let mut b = StatusWriter::open(&path).unwrap();
    assert_eq!(
        b.try_acquire(&me("b"), T0 + 16_000).unwrap(),
        LeaseOutcome::Owned
    );
    assert_eq!(
        a.publish_sessions(&[view("proj", "alpha", "busy", "present")], T0 + 16_100)
            .unwrap(),
        0
    );
    assert_eq!(a.owner(), None);
    assert!(reader(&path).sessions("proj").unwrap().is_empty());
}

#[test]
fn the_same_instance_reacquires_its_own_lease() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    assert_eq!(
        a.try_acquire(&me("a"), T0 + 1_000).unwrap(),
        LeaseOutcome::Owned
    );
}

#[test]
fn takeover_marks_previous_rows_stale() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    a.set_herdr_link("a", "up", T0 + 100).unwrap();
    a.set_last_snapshot_at("a", T0 + 200).unwrap();
    a.publish_sessions(
        &[
            view("proj", "alpha", "idle", "present"),
            view("proj", "bravo", "busy", "present"),
        ],
        T0,
    )
    .unwrap();
    let mut b = StatusWriter::open(&path).unwrap();
    assert_eq!(
        b.try_acquire(&me("b"), T0 + 20_000).unwrap(),
        LeaseOutcome::Owned
    );
    let r = reader(&path);
    assert!(r.sessions("proj").unwrap().iter().all(|s| s.stale));
    let w = r.writer().unwrap().unwrap();
    assert_eq!(w.herdr_link, "starting");
    assert_eq!(w.last_snapshot_at, None);
    // The new owner republishing the same fresh views rewrites them (stale cleared).
    let n = b
        .publish_sessions(
            &[
                view("proj", "alpha", "idle", "present"),
                view("proj", "bravo", "busy", "present"),
            ],
            T0 + 20_000,
        )
        .unwrap();
    assert_eq!(n, 2);
    assert!(r.sessions("proj").unwrap().iter().all(|s| !s.stale));
}

#[test]
fn release_deletes_only_my_writer_row() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    let mut b = StatusWriter::open(&path).unwrap();
    assert_eq!(
        b.try_acquire(&me("b"), T0 + 16_000).unwrap(),
        LeaseOutcome::Owned
    );
    a.release("a").unwrap();
    assert_eq!(reader(&path).writer().unwrap().unwrap().instance_id, "b");
    b.release("b").unwrap();
    assert_eq!(reader(&path).writer().unwrap(), None);
    assert_eq!(b.owner(), None);
}

#[test]
fn link_and_snapshot_updates_report_a_lost_lease() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    assert!(a.set_herdr_link("a", "up", T0 + 1).unwrap());
    // Unchanged value: still the owner, still true.
    assert!(a.set_herdr_link("a", "up", T0 + 1).unwrap());
    assert!(a.set_last_snapshot_at("a", T0 + 2).unwrap());
    let w = reader(&path).writer().unwrap().unwrap();
    assert_eq!((w.herdr_link.as_str(), w.herdr_link_since), ("up", T0 + 1));
    assert_eq!(w.last_snapshot_at, Some(T0 + 2));
    let mut b = StatusWriter::open(&path).unwrap();
    b.try_acquire(&me("b"), T0 + 30_000).unwrap();
    assert!(!a.set_herdr_link("a", "down", T0 + 30_001).unwrap());
    assert!(!a.set_last_snapshot_at("a", T0 + 30_002).unwrap());
}

// ── Freshness ────────────────────────────────────────────────────────────────

#[test]
fn freshness_table() {
    assert_eq!(freshness(None, T0), Freshness::Stale { heartbeat_at: None });
    let up = writer_row(T0, "up", Some(T0));
    assert_eq!(
        freshness(Some(up.clone()), T0 + 15_000),
        Freshness::Fresh(up.clone())
    );
    assert_eq!(
        freshness(Some(up), T0 + 15_001),
        Freshness::Stale {
            heartbeat_at: Some(T0)
        }
    );
    let starting = writer_row(T0, "starting", Some(T0));
    assert_eq!(
        freshness(Some(starting.clone()), T0 + 1),
        Freshness::NotReady(starting)
    );
    let no_snapshot = writer_row(T0, "up", None);
    assert_eq!(
        freshness(Some(no_snapshot.clone()), T0 + 1),
        Freshness::NotReady(no_snapshot)
    );
}

// ── Open / recreate ──────────────────────────────────────────────────────────

fn set_user_version(path: &Path, v: i64) {
    let c = Connection::open(path).unwrap();
    c.pragma_update(None, "user_version", v).unwrap();
}

#[test]
fn version_mismatch_is_recreated_without_a_fresh_owner() {
    let (_d, path) = db();
    {
        let mut a = owned(&path, "a", T0);
        a.publish_sessions(&[view("proj", "alpha", "idle", "present")], T0)
            .unwrap();
    }
    set_user_version(&path, 99);
    assert!(StatusWriter::open(&path).is_err());
    match StatusWriter::open_or_recreate(&path, T0 + 16_000).unwrap() {
        WriterOpen::Ready(mut w) => {
            assert_eq!(
                w.try_acquire(&me("b"), T0 + 16_000).unwrap(),
                LeaseOutcome::Owned
            )
        }
        WriterOpen::Held { .. } => panic!("expected a recreated file"),
    }
    let r = reader(&path);
    assert!(r.sessions("proj").unwrap().is_empty());
    assert_eq!(r.writer().unwrap().unwrap().instance_id, "b");
}

#[test]
fn version_mismatch_is_kept_with_a_fresh_owner() {
    let (_d, path) = db();
    {
        owned(&path, "a", T0);
    }
    set_user_version(&path, 99);
    match StatusWriter::open_or_recreate(&path, T0 + 5_000).unwrap() {
        WriterOpen::Held {
            pid, heartbeat_at, ..
        } => assert_eq!((pid, heartbeat_at), (42, T0)),
        WriterOpen::Ready(_) => panic!("a live writer's file must be left alone"),
    }
    let c = Connection::open(&path).unwrap();
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 99, "file untouched");
}

#[test]
fn corrupt_file_is_recreated() {
    let (_d, path) = db();
    std::fs::write(
        &path,
        b"this is not a sqlite database, just garbage bytes......",
    )
    .unwrap();
    assert!(StatusWriter::open(&path).is_err());
    let w = match StatusWriter::open_or_recreate(&path, T0).unwrap() {
        WriterOpen::Ready(w) => w,
        WriterOpen::Held { .. } => panic!("garbage has no live writer"),
    };
    drop(w);
    let r = reader(&path);
    assert_eq!(r.writer().unwrap(), None);
}

#[test]
fn a_fresh_file_gets_the_schema_version() {
    let (_d, path) = db();
    let nested = path.parent().unwrap().join("nested").join("status.db");
    StatusWriter::open(&nested).unwrap();
    let c = Connection::open(&nested).unwrap();
    let v: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, STATUS_SCHEMA_VERSION);
    // Reopening a current file keeps it.
    StatusWriter::open(&nested).unwrap();
}

// ── Change-only writes ───────────────────────────────────────────────────────

#[test]
fn unchanged_views_cause_zero_writes() {
    let (_d, path) = db();
    let mut w = owned(&path, "a", T0);
    let rows = vec![
        view("proj", "alpha", "idle", "present"),
        view("proj", "bravo", "busy", "present"),
    ];
    assert_eq!(w.publish_sessions(&rows, T0).unwrap(), 2);
    let r = reader(&path);
    let dv = r.data_version().unwrap();
    for i in 0..5 {
        assert_eq!(w.publish_sessions(&rows, T0 + i).unwrap(), 0);
    }
    assert_eq!(
        r.data_version().unwrap(),
        dv,
        "no transaction without a change"
    );

    // One changed row is written; one disappeared view is deleted.
    let mut changed = rows.clone();
    changed[0].state = "busy".to_string();
    changed.pop();
    assert_eq!(w.publish_sessions(&changed, T0 + 10).unwrap(), 2);
    assert_ne!(r.data_version().unwrap(), dv);
    let got = r.sessions("proj").unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].state, "busy");
    assert_eq!(r.session("proj", "bravo").unwrap(), None);
    assert_eq!(r.session("proj", "alpha").unwrap().unwrap(), changed[0]);
}

// ── Reader ───────────────────────────────────────────────────────────────────

#[test]
fn reader_never_creates_the_file() {
    let (_d, path) = db();
    assert!(StatusReader::open(&path).unwrap().is_none());
    assert!(!path.exists());
}

#[test]
fn reader_rejects_another_schema_version() {
    let (_d, path) = db();
    StatusWriter::open(&path).unwrap();
    set_user_version(&path, 2);
    assert!(StatusReader::open(&path).unwrap().is_none());
}

#[test]
fn reader_reads_app_projects_and_findings() {
    let (_d, path) = db();
    StatusWriter::open(&path).unwrap();
    {
        let c = Connection::open(&path).unwrap();
        c.execute(
            "INSERT INTO app_projects (project, registry_key, db_path) VALUES ('proj', 'proj', 'D:/repos/proj/tasks.db')",
            [],
        )
        .unwrap();
        c.execute_batch(
            "INSERT INTO findings (project, worktree, check_id, severity, message, first_seen, last_seen)
               VALUES ('proj', 'alpha', 'hooks_missing', 'warn', 'm1', 1, 2);
             INSERT INTO findings (project, worktree, check_id, severity, message, first_seen, last_seen)
               VALUES ('proj', '', 'watcher', 'error', 'm2', 3, 4);
             INSERT INTO findings (project, check_id, severity, message, first_seen, last_seen)
               VALUES ('', 'status_writer', 'info', 'm3', 5, 6);",
        )
        .unwrap();
    }
    let r = reader(&path);
    let p = r.app_project("proj").unwrap().unwrap();
    assert_eq!(
        p,
        AppProject {
            project: "proj".into(),
            registry_key: "proj".into(),
            db_path: "D:/repos/proj/tasks.db".into(),
            open: false,
            open_reported_at: None,
            watcher_state: "absent".into(),
            watcher_started_at: None,
            watcher_last_event_at: None,
            last_pass_at: None,
            last_pass_trigger: None,
            pending_count: 0,
            oldest_pending_at: None,
            restarts: 0,
            last_error: String::new(),
            last_error_at: None,
        }
    );
    assert_eq!(r.app_project("other-proj").unwrap(), None);
    let all = r.findings("proj", None).unwrap();
    assert_eq!(
        all.iter().map(|f| f.check_id.as_str()).collect::<Vec<_>>(),
        ["watcher", "hooks_missing"]
    );
    assert_eq!(all[1].detail, "{}");
    assert_eq!(all[1].occurrences, 1);
    assert_eq!(r.findings("proj", Some("alpha")).unwrap().len(), 1);
    assert_eq!(r.findings("proj", Some("")).unwrap()[0].check_id, "watcher");
    assert_eq!(r.findings("", None).unwrap()[0].check_id, "status_writer");
}

#[test]
fn write_status_for_test_round_trips() {
    let (_d, path) = db();
    let w = writer_row(T0, "up", Some(T0));
    let s = view("proj", "alpha", "waiting", "present");
    write_status_for_test(&path, w.clone(), std::slice::from_ref(&s)).unwrap();
    let r = reader(&path);
    assert_eq!(r.writer().unwrap(), Some(w));
    assert_eq!(r.sessions("proj").unwrap(), vec![s]);
    assert!(r.sessions("other-proj").unwrap().is_empty());
}

#[test]
fn project_status_reads_each_worktree_from_one_read() {
    let (_d, path) = db();
    let stale_view = StatusSession {
        stale: true,
        ..view("proj", "bravo", "busy", "present")
    };
    let rows = [view("proj", "alpha", "idle", "present"), stale_view.clone()];
    write_status_for_test(&path, writer_row(T0, "up", Some(T0)), &rows).unwrap();
    let r = reader(&path);
    let fresh = r.project_status("proj", T0);
    assert!(
        matches!(fresh.engine_read("alpha"), EngineRead::Fresh { view: Some(v), .. } if v.state == "idle")
    );
    assert!(matches!(
        fresh.engine_read("charlie"),
        EngineRead::Fresh { view: None, .. }
    ));
    // A stale view reads not ready.
    assert_eq!(fresh.engine_read("bravo").view(), Some(&stale_view));
    assert!(matches!(
        fresh.engine_read("bravo"),
        EngineRead::NotReady { .. }
    ));
    let sessions = r.fresh_sessions("proj", T0).expect("fresh");
    assert_eq!(sessions.len(), 2);
    assert!(r.fresh_sessions("other-proj", T0).unwrap().is_empty());

    // No heartbeat for over 15 s: stale, no views.
    let later = T0 + STATUS_FRESH_MS + 1;
    assert_eq!(
        r.project_status("proj", later).engine_read("alpha"),
        EngineRead::Stale {
            heartbeat_at: Some(T0)
        }
    );
    assert_eq!(r.fresh_sessions("proj", later), None);

    // herdr down: not ready, the old rows still visible.
    write_status_for_test(&path, writer_row(T0, "down", Some(T0)), &rows).unwrap();
    let not_ready = r.project_status("proj", T0);
    assert!(matches!(
        not_ready.engine_read("alpha"),
        EngineRead::NotReady { view: Some(_), .. }
    ));
    assert_eq!(r.fresh_sessions("proj", T0), None);
    assert_eq!(
        EngineRead::of(None, "proj", "alpha", T0),
        EngineRead::Stale { heartbeat_at: None }
    );
}

// ── DDL ──────────────────────────────────────────────────────────────────────

fn columns(c: &Connection, table: &str) -> Vec<(String, Option<String>)> {
    let mut stmt = c
        .prepare(&format!(
            "SELECT name, dflt_value FROM pragma_table_info('{table}')"
        ))
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn names(cols: &[(String, Option<String>)]) -> Vec<&str> {
    cols.iter().map(|(n, _)| n.as_str()).collect()
}

fn default_of<'a>(cols: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
    cols.iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, d)| d.as_deref())
}

#[test]
fn status_ddl_matches_the_documented_columns() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(STATUS_DDL).unwrap();

    assert_eq!(
        names(&columns(&c, "writer")),
        [
            "id",
            "instance_id",
            "pid",
            "version",
            "started_at",
            "heartbeat_at",
            "last_snapshot_at",
            "herdr_link",
            "herdr_link_since"
        ]
    );
    assert_eq!(
        names(&columns(&c, "sessions")),
        [
            "project",
            "worktree",
            "state",
            "agent",
            "agent_kind",
            "herdr_status",
            "settled",
            "stale",
            "interactive_ready",
            "pane_id",
            "terminal_id",
            "agent_session",
            "state_change_seq",
            "completion_seq",
            "brokered",
            "escalated_reason",
            "last_change_at",
            "written_at"
        ]
    );

    let app = columns(&c, "app_projects");
    assert_eq!(
        names(&app),
        [
            "project",
            "registry_key",
            "db_path",
            "open",
            "open_reported_at",
            "watcher_state",
            "watcher_started_at",
            "watcher_last_event_at",
            "last_pass_at",
            "last_pass_trigger",
            "pending_count",
            "oldest_pending_at",
            "restarts",
            "last_error",
            "last_error_at"
        ]
    );
    assert_eq!(default_of(&app, "open"), Some("0"));
    assert_eq!(default_of(&app, "watcher_state"), Some("'absent'"));
    assert_eq!(default_of(&app, "pending_count"), Some("0"));
    assert_eq!(default_of(&app, "restarts"), Some("0"));
    assert_eq!(default_of(&app, "last_error"), Some("''"));

    let findings = columns(&c, "findings");
    assert_eq!(
        names(&findings),
        [
            "project",
            "worktree",
            "check_id",
            "severity",
            "message",
            "detail",
            "first_seen",
            "last_seen",
            "occurrences",
            "cleared_at",
            "remedy",
            "remedy_at"
        ]
    );
    assert_eq!(default_of(&findings, "worktree"), Some("''"));
    assert_eq!(default_of(&findings, "detail"), Some("'{}'"));
    assert_eq!(default_of(&findings, "occurrences"), Some("1"));
    assert_eq!(default_of(&findings, "remedy"), Some("''"));

    // findings is keyed (project, worktree, check_id).
    c.execute_batch(
        "INSERT INTO findings (project, check_id, severity, message, first_seen, last_seen)
           VALUES ('proj', 'c', 'warn', 'm', 1, 1);",
    )
    .unwrap();
    assert!(c
        .execute(
            "INSERT INTO findings (project, check_id, severity, message, first_seen, last_seen)
               VALUES ('proj', 'c', 'warn', 'm', 1, 1)",
            [],
        )
        .is_err());
    // Every time column is INTEGER.
    for table in ["writer", "sessions", "app_projects", "findings"] {
        let mut stmt = c
            .prepare(&format!(
                "SELECT name, type FROM pragma_table_info('{table}')"
            ))
            .unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (name, ty) in cols {
            if name.ends_with("_at") || name.ends_with("_since") || name.contains("seen") {
                assert_eq!(ty, "INTEGER", "{table}.{name}");
            }
        }
    }
}

#[test]
fn ddl_checks_reject_unknown_vocabulary() {
    let (_d, path) = db();
    let mut w = owned(&path, "a", T0);
    assert!(w.set_herdr_link("a", "healthy", T0).is_err());
    let mut bad = view("proj", "alpha", "idle", "present");
    bad.state = "starting".to_string();
    assert!(w.publish_sessions(&[bad], T0).is_err());
}

// ── age_text ─────────────────────────────────────────────────────────────────

#[test]
fn age_text_follows_the_documented_steps() {
    assert_eq!(age_text(-5), "0s");
    assert_eq!(age_text(119), "119s");
    assert_eq!(age_text(120), "2m");
    assert_eq!(age_text(7_199), "119m");
    assert_eq!(age_text(7_200), "2h");
}

// ── sqlite_utc_to_ms ─────────────────────────────────────────────────────────

#[test]
fn sqlite_utc_to_ms_parses_both_forms() {
    assert_eq!(sqlite_utc_to_ms("1970-01-01 00:00:00"), Some(0));
    assert_eq!(
        sqlite_utc_to_ms("2026-11-26 12:34:56"),
        Some(1_795_696_496_000)
    );
    assert_eq!(
        sqlite_utc_to_ms("2026-11-26 12:34:56.789"),
        Some(1_795_696_496_789)
    );
    assert_eq!(
        sqlite_utc_to_ms("2026-11-26 12:34:56.5"),
        Some(1_795_696_496_500)
    );
    assert_eq!(
        sqlite_utc_to_ms("2026-11-26 12:34:56.123456"),
        Some(1_795_696_496_123)
    );
    assert_eq!(
        sqlite_utc_to_ms("2024-02-29 00:00:00"),
        Some(1_709_164_800_000)
    );
    for bad in [
        "",
        "now",
        "2026-11-26",
        "2026-13-01 00:00:00",
        "2023-02-29 00:00:00",
        "2026-11-26 24:00:00",
        "2026-11-26 12:34:56.",
        "2026-11-26 12:34:56.x",
        "26-11-26 12:34:56",
    ] {
        assert_eq!(sqlite_utc_to_ms(bad), None, "{bad:?}");
    }
}

#[test]
fn now_ms_is_unix_millis() {
    let t = now_ms();
    assert!(t > 1_700_000_000_000, "{t}");
}

#[test]
fn status_db_path_is_under_keitora_home() {
    let _g = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os(crate::KEITORA_HOME_ENV);
    std::env::set_var(crate::KEITORA_HOME_ENV, "/tmp/keitora-status-test");
    let got = status_db_path();
    match prev {
        Some(v) => std::env::set_var(crate::KEITORA_HOME_ENV, v),
        None => std::env::remove_var(crate::KEITORA_HOME_ENV),
    }
    assert_eq!(
        got,
        PathBuf::from("/tmp/keitora-status-test").join("status.db")
    );
}

// ── overlay_sessions ─────────────────────────────────────────────────────────

fn row(worktree: &str, state: &str) -> WorktreeSession {
    WorktreeSession {
        project: "proj".to_string(),
        worktree: worktree.to_string(),
        state: state.to_string(),
        current_task_id: Some(5),
        pid: Some(1234),
        last_heartbeat: "2026-11-26 00:00:00".to_string(),
        driven_by: None,
        brokered: true,
        escalated_reason: Some("needs a human".to_string()),
        agent: None,
        herdr_status: None,
        stale: false,
    }
}

fn views(vs: &[StatusSession]) -> HashMap<String, StatusSession> {
    vs.iter().map(|v| (v.worktree.clone(), v.clone())).collect()
}

fn find<'a>(out: &'a [WorktreeSession], wt: &str) -> &'a WorktreeSession {
    out.iter().find(|s| s.worktree == wt).expect(wt)
}

#[test]
fn overlay_sessions_table() {
    let none = HashMap::new();

    // A fresh view beats a stale `offline` row (A1).
    let v = views(&[view("proj", "alpha", "idle", "present")]);
    let out = overlay_sessions(vec![row("alpha", "offline")], Some(&v), &[], &none);
    let a = find(&out, "alpha");
    assert_eq!(a.state, "idle");
    assert_eq!(a.agent.as_deref(), Some("present"));
    assert_eq!(a.herdr_status.as_deref(), Some("idle"));
    assert!(!a.stale);
    assert_eq!(a.current_task_id, Some(5), "a live view keeps the task");
    assert!(!a.brokered, "non-waiting clears brokered");
    assert_eq!(a.escalated_reason, None);

    // A waiting view keeps the broker fields.
    let v = views(&[view("proj", "alpha", "waiting", "present")]);
    let out = overlay_sessions(vec![row("alpha", "idle")], Some(&v), &[], &none);
    assert!(find(&out, "alpha").brokered);
    assert!(find(&out, "alpha").escalated_reason.is_some());

    // A bare shell (agent absent) with an `idle` row stays offline, task/pid cleared.
    let v = views(&[view("proj", "alpha", "offline", "absent")]);
    let out = overlay_sessions(vec![row("alpha", "idle")], Some(&v), &[], &none);
    let a = find(&out, "alpha");
    assert_eq!(a.state, "offline");
    assert_eq!(a.agent.as_deref(), Some("absent"));
    assert_eq!((a.current_task_id, a.pid), (None, None));
    assert!(!a.brokered);

    // Row without a view, engine ready: offline, absent, offline rule applied.
    let v = views(&[]);
    let out = overlay_sessions(vec![row("bravo", "busy")], Some(&v), &[], &none);
    let b = find(&out, "bravo");
    assert_eq!(b.state, "offline");
    assert_eq!(b.agent.as_deref(), Some("absent"));
    assert_eq!((b.current_task_id, b.pid), (None, None));
    assert_eq!(b.escalated_reason, None);

    // View without a row: synthesized with driven_by.
    let drivers = HashMap::from([("charlie".to_string(), "alpha".to_string())]);
    let v = views(&[view("proj", "charlie", "busy", "present")]);
    let out = overlay_sessions(vec![], Some(&v), &[], &drivers);
    let c = find(&out, "charlie");
    assert_eq!(c.state, "busy");
    assert_eq!(c.project, "");
    assert_eq!(c.last_heartbeat, "");
    assert_eq!(c.driven_by.as_deref(), Some("alpha"));
    assert_eq!(c.agent.as_deref(), Some("present"));
    assert_eq!((c.current_task_id, c.pid), (None, None));

    // A stale view carries its stale flag.
    let mut sv = view("proj", "alpha", "busy", "present");
    sv.stale = true;
    let out = overlay_sessions(vec![row("alpha", "idle")], Some(&views(&[sv])), &[], &none);
    assert!(find(&out, "alpha").stale);

    // Not ready (`None`): rows as-is, scoped, marked stale.
    let configured = vec!["alpha".to_string(), "bravo".to_string()];
    let out = overlay_sessions(
        vec![
            row("bravo", "idle"),
            row("alpha", "offline"),
            row("wt", "busy"),
        ],
        None,
        &configured,
        &none,
    );
    assert_eq!(
        out.iter().map(|s| s.worktree.as_str()).collect::<Vec<_>>(),
        ["alpha", "bravo"]
    );
    assert!(out.iter().all(|s| s.stale));
    assert_eq!(find(&out, "alpha").state, "offline");
    assert_eq!(find(&out, "bravo").current_task_id, Some(5));
    assert!(find(&out, "bravo").brokered, "not ready leaves rows as-is");

    // Unconfigured names (rows and views) are dropped; output sorted.
    let v = views(&[
        view("proj", "wt", "idle", "present"),
        view("proj", "bravo", "idle", "present"),
    ]);
    let out = overlay_sessions(
        vec![row("wt", "idle"), row("alpha", "idle")],
        Some(&v),
        &configured,
        &none,
    );
    assert_eq!(
        out.iter().map(|s| s.worktree.as_str()).collect::<Vec<_>>(),
        ["alpha", "bravo"]
    );
    assert_eq!(find(&out, "alpha").state, "offline");
    assert_eq!(find(&out, "bravo").state, "idle");
}

#[test]
fn overlay_sessions_marks_a_starting_agent() {
    let none = HashMap::new();
    let v = views(&[
        view("proj", "alpha", "busy", "starting"),
        view("proj", "bravo", "busy", "present"),
    ]);
    let out = overlay_sessions(vec![row("alpha", "offline")], Some(&v), &[], &none);
    assert!(
        find(&out, "alpha").is_starting(),
        "a row overlaid by a starting view"
    );
    assert!(
        !find(&out, "bravo").is_starting(),
        "a present agent is not starting"
    );

    let v = views(&[view("proj", "charlie", "busy", "starting")]);
    let out = overlay_sessions(vec![], Some(&v), &[], &none);
    assert!(
        find(&out, "charlie").is_starting(),
        "a synthesized row keeps it too"
    );

    let mut r = row("alpha", "busy");
    r.agent = Some("starting".to_string());
    let out = overlay_sessions(vec![r], Some(&views(&[])), &[], &none);
    assert!(
        !find(&out, "alpha").is_starting(),
        "no view means no agent, not starting"
    );
}

#[test]
fn is_starting_follows_the_agent_presence() {
    let mut r = row("alpha", "busy");
    assert!(!r.is_starting());
    r.agent = Some("starting".to_string());
    assert!(r.is_starting());
    r.agent = Some("present".to_string());
    assert!(!r.is_starting());
    let json = serde_json::to_value(&r).unwrap();
    assert!(json.get("starting").is_none());
}

// ── Beat, app_projects, findings ─────────────────────────────────────────────

fn app_project(project: &str) -> AppProject {
    AppProject {
        project: project.into(),
        registry_key: project.into(),
        db_path: format!("D:/repos/{project}/tasks.db"),
        open: true,
        open_reported_at: Some(T0),
        watcher_state: "running".into(),
        watcher_started_at: Some(T0),
        watcher_last_event_at: None,
        last_pass_at: None,
        last_pass_trigger: None,
        pending_count: 2,
        oldest_pending_at: Some(T0 - 1_000),
        restarts: 1,
        last_error: "boom".into(),
        last_error_at: Some(T0 - 5),
    }
}

fn finding(project: &str, worktree: &str, check: &str, last_seen: i64) -> FindingRow {
    FindingRow {
        project: project.into(),
        worktree: worktree.into(),
        check_id: check.into(),
        severity: "warn".into(),
        message: format!("{check} fired"),
        detail: r#"{"n":1}"#.into(),
        first_seen: T0,
        last_seen,
        occurrences: 2,
        cleared_at: Some(T0 + 9),
        remedy: "reinstalled hooks".into(),
        remedy_at: Some(T0 + 3),
    }
}

fn key_of(f: &FindingRow) -> FindingKey {
    FindingKey {
        project: f.project.clone(),
        worktree: f.worktree.clone(),
        check_id: f.check_id.clone(),
    }
}

#[test]
fn beat_writes_heartbeat_link_snapshot_and_queued_columns_in_one_go() {
    let (_d, path) = db();
    let mut w = owned(&path, "a", T0);
    assert!(w.app_project_set(&app_project("proj")).unwrap());
    let f = finding("proj", "alpha", "hooks_missing", T0);
    assert!(w
        .findings_apply(&[FindingWrite::Upsert(f.clone())])
        .unwrap());

    let touches = vec![(
        "proj".to_string(),
        AppProjectTouch {
            last_pass_at: Some(T0 + 10),
            last_pass_trigger: Some("sweep".into()),
            pending_count: Some(0),
            oldest_pending_at: Some(None),
            ..Default::default()
        },
    )];
    let seen = vec![(key_of(&f), T0 + 60_000)];
    let beat = Beat {
        link: Some(("up", T0 + 1)),
        last_snapshot_at: Some(T0 + 2),
        touches: &touches,
        last_seen: &seen,
    };
    assert!(w.beat("a", T0 + 5_000, &beat).unwrap());

    let r = reader(&path);
    let wr = r.writer().unwrap().unwrap();
    assert_eq!(wr.heartbeat_at, T0 + 5_000);
    assert_eq!(
        (wr.herdr_link.as_str(), wr.herdr_link_since),
        ("up", T0 + 1)
    );
    assert_eq!(wr.last_snapshot_at, Some(T0 + 2));
    let p = r.app_project("proj").unwrap().unwrap();
    assert_eq!(p.last_pass_at, Some(T0 + 10));
    assert_eq!(p.last_pass_trigger.as_deref(), Some("sweep"));
    assert_eq!(p.pending_count, 0);
    assert_eq!(p.oldest_pending_at, None);
    // Untouched columns keep their values.
    assert_eq!(p.watcher_last_event_at, None);
    assert_eq!(p.restarts, 1);
    assert_eq!(p.last_error, "boom");
    assert_eq!(
        r.findings("proj", Some("alpha")).unwrap()[0].last_seen,
        T0 + 60_000
    );

    // last_seen never moves back.
    let back = vec![(key_of(&f), T0 + 1)];
    let beat = Beat {
        last_seen: &back,
        ..Default::default()
    };
    assert!(w.beat("a", T0 + 10_000, &beat).unwrap());
    assert_eq!(
        r.findings("proj", Some("alpha")).unwrap()[0].last_seen,
        T0 + 60_000
    );
}

#[test]
fn a_lease_loser_beats_and_writes_nothing() {
    let (_d, path) = db();
    let mut a = owned(&path, "a", T0);
    let mut b = StatusWriter::open(&path).unwrap();
    assert_eq!(
        b.try_acquire(&me("b"), T0 + 16_000).unwrap(),
        LeaseOutcome::Owned
    );
    let touches = vec![("proj".to_string(), AppProjectTouch::default())];
    let beat = Beat {
        link: Some(("down", T0 + 17_000)),
        touches: &touches,
        ..Default::default()
    };
    assert!(!a.beat("a", T0 + 17_000, &beat).unwrap());
    assert_eq!(a.owner(), None);
    assert!(!a.app_project_set(&app_project("proj")).unwrap());
    assert!(!a
        .findings_apply(&[FindingWrite::Upsert(finding("proj", "", "x", T0))])
        .unwrap());
    let r = reader(&path);
    let wr = r.writer().unwrap().unwrap();
    assert_eq!(wr.instance_id, "b");
    assert_eq!(wr.herdr_link, "starting");
    assert_eq!(r.app_project("proj").unwrap(), None);
    assert!(r.findings("proj", None).unwrap().is_empty());
    // Never owned: a beat is refused without touching the file.
    let mut c = StatusWriter::open(&path).unwrap();
    assert!(!c.beat("c", T0 + 18_000, &Beat::default()).unwrap());
}

#[test]
fn app_project_set_and_delete_round_trip() {
    let (_d, path) = db();
    let mut w = owned(&path, "a", T0);
    let row = app_project("proj");
    assert!(w.app_project_set(&row).unwrap());
    assert_eq!(reader(&path).app_project("proj").unwrap(), Some(row));
    assert!(w.app_project_delete("proj").unwrap());
    assert_eq!(reader(&path).app_project("proj").unwrap(), None);
}

#[test]
fn findings_apply_round_trips_every_column() {
    let (_d, path) = db();
    let mut w = owned(&path, "a", T0);
    let f1 = finding("proj", "alpha", "hooks_missing", T0 + 1);
    let f2 = finding("", "", "status_writer", T0 + 2);
    assert!(w
        .findings_apply(&[
            FindingWrite::Upsert(f1.clone()),
            FindingWrite::Upsert(f2.clone())
        ])
        .unwrap());
    let r = reader(&path);
    assert_eq!(r.findings("proj", None).unwrap(), vec![f1.clone()]);
    assert_eq!(r.findings("", None).unwrap(), vec![f2.clone()]);

    assert!(w
        .findings_apply(&[
            FindingWrite::LastSeen {
                key: key_of(&f1),
                last_seen: T0 + 99,
            },
            FindingWrite::Delete(key_of(&f2)),
        ])
        .unwrap());
    assert_eq!(r.findings("proj", None).unwrap()[0].last_seen, T0 + 99);
    assert!(r.findings("", None).unwrap().is_empty());

    // A clear empties the table; later writes in the same batch land after it.
    assert!(w
        .findings_apply(&[FindingWrite::Clear, FindingWrite::Upsert(f2.clone())])
        .unwrap());
    assert!(r.findings("proj", None).unwrap().is_empty());
    assert_eq!(r.findings("", None).unwrap(), vec![f2]);
}

#[test]
fn touches_merge_later_fields_over_earlier_ones() {
    let mut t = AppProjectTouch {
        last_pass_at: Some(1),
        pending_count: Some(3),
        oldest_pending_at: Some(Some(5)),
        ..Default::default()
    };
    t.merge(AppProjectTouch {
        last_pass_at: Some(2),
        watcher_last_event_at: Some(7),
        oldest_pending_at: Some(None),
        ..Default::default()
    });
    assert_eq!(
        t,
        AppProjectTouch {
            last_pass_at: Some(2),
            last_pass_trigger: None,
            watcher_last_event_at: Some(7),
            pending_count: Some(3),
            oldest_pending_at: Some(None),
        }
    );
}
