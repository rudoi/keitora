//! Pipeline tests: temp DBs, a recording pass runner and presence sink, a test
//! watcher registry, and a fake clock. Nothing touches herdr or the real registry.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use keitora_tasks::{LocalStore, WorktreeEntry};

use super::*;
use crate::watchers::{RestartWhy, WatcherHealth, WatcherId, WatcherKind};

/// Records requested passes.
#[derive(Default)]
struct Recorder(Mutex<Vec<(String, Instant, Trigger)>>);

impl PassRunner for Recorder {
    fn run_at(&self, project: String, due: Instant, _after: Duration, why: Trigger) {
        self.0.lock().unwrap().push((project, due, why));
    }
}

impl Recorder {
    fn sweeps(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, w)| *w == Trigger::Sweep)
            .map(|(p, _, _)| p.clone())
            .collect()
    }
}

/// Records presence writes.
#[derive(Default)]
struct Sink {
    sets: Mutex<Vec<AppProject>>,
    touches: Mutex<Vec<(String, AppProjectTouch)>>,
    deletes: Mutex<Vec<String>>,
}

impl PresenceSink for Sink {
    fn set(&self, row: &AppProject) -> bool {
        self.sets.lock().unwrap().push(row.clone());
        true
    }
    fn touch(&self, project: &str, touch: AppProjectTouch) {
        self.touches
            .lock()
            .unwrap()
            .push((project.to_string(), touch));
    }
    fn delete(&self, project: &str) {
        self.deletes.lock().unwrap().push(project.to_string());
    }
}

impl Sink {
    fn writes(&self) -> usize {
        self.sets.lock().unwrap().len()
            + self.touches.lock().unwrap().len()
            + self.deletes.lock().unwrap().len()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    db_path: String,
    pipeline: Pipeline,
    runtime: DriveRuntime,
    runner: Arc<Recorder>,
    sink: Arc<Sink>,
    watchers: Arc<WatcherRegistry>,
    restarts: Arc<AtomicUsize>,
    projects: Arc<Mutex<Vec<(String, ProjectConfig)>>>,
    t0: Instant,
    t0_ms: i64,
}

/// Project `proj` (alpha driving bravo) with its DB created, registered as `proj`.
fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let mut pc = ProjectConfig::scaffold(&root);
    pc.effective_worktrees = Some(
        ["alpha", "bravo"]
            .into_iter()
            .map(WorktreeEntry::name_only)
            .collect(),
    );
    let db_path = dir.path().join("tasks.db");
    pc.tasks.db_path = db_path.clone();
    let db_path = db_path.to_string_lossy().into_owned();
    LocalStore::open(&db_path)
        .unwrap()
        .drive_bind("alpha", "bravo", None, false)
        .unwrap();
    let runtime = DriveRuntime::default();
    let runner = Arc::new(Recorder::default());
    runtime.set_runner(runner.clone());
    let sink = Arc::new(Sink::default());
    let watchers: Arc<WatcherRegistry> = Arc::default();
    let restarts = Arc::new(AtomicUsize::new(0));
    let (weak, count) = (Arc::downgrade(&watchers), Arc::clone(&restarts));
    watchers.set_restart(
        WatcherKind::Bg,
        Arc::new(move |project: &str| {
            count.fetch_add(1, Ordering::SeqCst);
            let h = Arc::new(WatcherHealth::default());
            h.force_state(WatchState::Running);
            weak.upgrade()
                .unwrap()
                .register(WatcherId::new(WatcherKind::Bg, project), h);
            true
        }),
    );
    let projects = Arc::new(Mutex::new(vec![("proj".to_string(), pc)]));
    let list = Arc::clone(&projects);
    let pipeline = Pipeline::new(
        Box::new(move || list.lock().unwrap().clone()),
        Arc::new(ReadStores::default()),
        runtime.clone(),
        Arc::clone(&watchers),
        Some(sink.clone() as Arc<dyn PresenceSink>),
    );
    Rig {
        _dir: dir,
        db_path,
        pipeline,
        runtime,
        runner,
        sink,
        watchers,
        restarts,
        projects,
        t0: Instant::now(),
        t0_ms: keitora_tasks::now_ms(),
    }
}

impl Rig {
    fn tick(&mut self, secs: u64) {
        self.pipeline.tick(
            self.t0 + Duration::from_secs(secs),
            self.t0_ms + secs as i64 * 1000,
        );
    }
    fn store(&self) -> LocalStore {
        LocalStore::open(&self.db_path).unwrap()
    }
    fn file_send(&self) -> i64 {
        self.store()
            .drive_request_add(
                "send",
                "bravo",
                "alpha",
                &serde_json::json!({"text": "hi"}),
                false,
            )
            .unwrap()
            .id
    }
    fn last_set(&self) -> AppProject {
        self.sink.sets.lock().unwrap().last().unwrap().clone()
    }
}

#[test]
fn the_sweep_asks_for_a_pass_while_requests_are_pending_even_with_dead_watchers() {
    let mut r = rig();
    // Every watcher is dead: only the sweep can notice the request.
    let dead = Arc::new(WatcherHealth::default());
    dead.mark_exited("event channel disconnected");
    r.watchers
        .register(WatcherId::new(WatcherKind::Bg, "proj"), dead);
    let id = r.file_send();

    r.tick(0);
    assert_eq!(r.runner.sweeps(), ["proj"], "the first tick sweeps");
    assert!(
        r.runtime.has_pending_for("proj", "bravo"),
        "the hint is refreshed"
    );
    // The supervisor restarted the dead watcher meanwhile.
    assert_eq!(r.restarts.load(Ordering::SeqCst), 1);

    // Not again before the sweep interval.
    let token = r.runner.0.lock().unwrap()[0].1;
    assert!(r.runtime.take_due("proj", token));
    r.tick(9);
    assert_eq!(r.runner.sweeps().len(), 1);
    r.tick(10);
    assert_eq!(
        r.runner.sweeps().len(),
        2,
        "every ten seconds while pending"
    );

    // Served: the sweep stops asking.
    let token = r.runner.0.lock().unwrap()[1].1;
    assert!(r.runtime.take_due("proj", token));
    r.store().drive_request_begin(id).unwrap();
    r.tick(20);
    assert_eq!(r.runner.sweeps().len(), 2);
    assert!(!r.runtime.has_pending_for("proj", "bravo"));
}

#[test]
fn the_sweep_reads_without_writing_and_a_missing_db_has_nothing_pending() {
    let mut r = rig();
    r.file_send();
    let reader = r.store();
    let v = reader.data_version().unwrap();
    for s in [0, 10, 20] {
        r.tick(s);
        let tokens: Vec<Instant> = r.runner.0.lock().unwrap().iter().map(|e| e.1).collect();
        for t in tokens {
            r.runtime.take_due("proj", t);
        }
    }
    assert_eq!(reader.data_version().unwrap(), v, "the sweep never writes");

    // A project whose DB doesn't exist: nothing pending, and no file is created.
    let missing = r._dir.path().join("other").join("tasks.db");
    let mut pc = ProjectConfig::scaffold(r._dir.path());
    pc.project.name = "other-proj".into();
    pc.tasks.db_path = missing.clone();
    r.projects.lock().unwrap().push(("other-proj".into(), pc));
    r.tick(30);
    assert!(!r.runner.sweeps().contains(&"other-proj".to_string()));
    assert!(!missing.exists());
}

#[test]
fn a_silent_watcher_is_restarted_once_while_a_request_ages() {
    let mut r = rig();
    let live = Arc::new(WatcherHealth::default());
    live.force_state(WatchState::Running);
    r.watchers
        .register(WatcherId::new(WatcherKind::Bg, "proj"), Arc::clone(&live));
    r.file_send();
    let created = r.store().drive_requests_pending().unwrap()[0]
        .created_at
        .clone();
    let created_ms = keitora_tasks::sqlite_utc_to_ms(&created).unwrap();
    // The watcher is alive but saw nothing since well before the request.
    live.set_times(created_ms + 20_000, created_ms - 60_000);
    r.pipeline.tick(r.t0, created_ms + 20_000);
    assert_eq!(r.restarts.load(Ordering::SeqCst), 1, "silent: restarted");
    // The respawned watcher is still silent about the same request: left alone.
    r.watchers
        .health(&WatcherId::new(WatcherKind::Bg, "proj"))
        .unwrap()
        .set_times(created_ms + 30_000, created_ms - 60_000);
    r.pipeline
        .tick(r.t0 + Duration::from_secs(10), created_ms + 30_000);
    assert_eq!(r.restarts.load(Ordering::SeqCst), 1);
}

#[test]
fn presence_is_written_on_change_only() {
    let mut r = rig();
    let live = Arc::new(WatcherHealth::default());
    live.force_state(WatchState::Running);
    // It sees every request (its last event is never before one was filed).
    live.set_times(0, i64::MAX / 2);
    r.watchers
        .register(WatcherId::new(WatcherKind::Bg, "proj"), Arc::clone(&live));

    r.tick(0);
    let row = r.last_set();
    assert_eq!(row.project, "proj");
    assert_eq!(row.registry_key, "proj");
    assert_eq!(row.db_path, r.db_path);
    assert!(!row.open);
    assert_eq!(row.open_reported_at, None, "the window hasn't reported");
    assert_eq!(row.watcher_state, "running");
    assert_eq!(row.pending_count, 0);
    assert_eq!(r.sink.writes(), 1);

    // Idle ticks: nothing written.
    for s in 1..25 {
        r.tick(s);
    }
    assert_eq!(r.sink.writes(), 1, "zero writes while nothing changes");

    // The window reports the project open: written at once.
    r.runtime.set_open(vec!["proj".into()]);
    r.tick(25);
    let row = r.last_set();
    assert!(row.open);
    assert!(row.open_reported_at.is_some());
    assert_eq!(r.sink.writes(), 2);
    // A re-report of the same set moves only the report time: not written.
    r.runtime.set_open(vec!["proj".into()]);
    r.tick(26);
    assert_eq!(r.sink.writes(), 2);

    // A request turns pending on: written at once with its count and age.
    r.file_send();
    r.tick(30);
    let row = r.last_set();
    assert_eq!(row.pending_count, 1);
    assert!(row.oldest_pending_at.is_some());
    assert_eq!(r.sink.writes(), 3);
    // A second one only moves the count: queued for the heartbeat.
    r.file_send();
    r.tick(40);
    assert_eq!(r.sink.sets.lock().unwrap().len(), 3);
    let touches = r.sink.touches.lock().unwrap().clone();
    assert_eq!(touches.len(), 1);
    assert_eq!(touches[0].1.pending_count, Some(2));
    assert_eq!(
        touches[0].1.oldest_pending_at, None,
        "the oldest didn't move"
    );

    // The watcher dies: written at once.
    live.mark_exited("event channel disconnected");
    r.watchers.set_restart(WatcherKind::Bg, Arc::new(|_| false));
    r.tick(41);
    let row = r.last_set();
    assert_eq!(row.watcher_state, "exited");

    // The project leaves the registry: its row is deleted.
    r.projects.lock().unwrap().clear();
    r.tick(50);
    assert_eq!(*r.sink.deletes.lock().unwrap(), ["proj"]);
}

#[test]
fn presence_columns_split_between_urgent_and_throttled() {
    let base = AppProject {
        project: "proj".into(),
        registry_key: "proj".into(),
        db_path: "D:/repos/proj/tasks.db".into(),
        open: true,
        open_reported_at: Some(1),
        watcher_state: "running".into(),
        watcher_started_at: Some(1),
        watcher_last_event_at: Some(1),
        last_pass_at: Some(1),
        last_pass_trigger: Some("watcher".into()),
        pending_count: 1,
        oldest_pending_at: Some(1),
        restarts: 0,
        last_error: String::new(),
        last_error_at: None,
    };
    let with = |f: &dyn Fn(&mut AppProject)| {
        let mut r = base.clone();
        f(&mut r);
        r
    };
    for urgent in [
        with(&|r| r.open = false),
        with(&|r| r.open_reported_at = None),
        with(&|r| r.watcher_state = "exited".into()),
        with(&|r| r.last_error = "store open failed".into()),
        with(&|r| r.pending_count = 0),
        with(&|r| r.restarts = 1),
    ] {
        assert!(presence_urgent(&base, &urgent), "{urgent:?}");
    }
    for throttled in [
        with(&|r| r.open_reported_at = Some(2)),
        with(&|r| r.pending_count = 5),
        with(&|r| r.last_pass_at = Some(2)),
        with(&|r| r.watcher_last_event_at = Some(2)),
    ] {
        assert!(!presence_urgent(&base, &throttled), "{throttled:?}");
    }
    assert_eq!(presence_touch(&base, &base), None);
    assert_eq!(
        presence_touch(
            &base,
            &with(&|r| {
                r.last_pass_at = Some(5);
                r.last_pass_trigger = Some("sweep".into());
                r.oldest_pending_at = None;
            })
        ),
        Some(AppProjectTouch {
            last_pass_at: Some(5),
            last_pass_trigger: Some("sweep".into()),
            watcher_last_event_at: None,
            pending_count: None,
            oldest_pending_at: Some(None),
        })
    );
}

#[test]
fn a_standby_sink_is_retried_until_it_writes() {
    #[derive(Default)]
    struct Standby(AtomicUsize);
    impl PresenceSink for Standby {
        fn set(&self, _row: &AppProject) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            false
        }
        fn touch(&self, _: &str, _: AppProjectTouch) {}
        fn delete(&self, _: &str) {}
    }
    let mut r = rig();
    let standby = Arc::new(Standby::default());
    r.pipeline.presence = Some(standby.clone());
    r.tick(0);
    r.tick(1);
    assert_eq!(
        standby.0.load(Ordering::SeqCst),
        2,
        "not written: tried again"
    );
}

#[test]
fn pending_in_lists_drive_rows_and_counts_dispatch_rows_in_one_read() {
    let r = rig();
    let stores = ReadStores::default();
    let path = std::path::Path::new(&r.db_path);
    let (drive, dispatch) = pending_in(&stores, path);
    assert!(matches!(drive, DrivePending::None), "{drive:?}");
    assert_eq!(dispatch, 0);

    let local = LocalStore::open(&r.db_path).unwrap();
    let id = local
        .drive_request_add(
            "send",
            "bravo",
            "alpha",
            &serde_json::json!({"text": "hi"}),
            false,
        )
        .unwrap()
        .id;
    local.dispatch_request_add(1, "bravo", "alpha").unwrap();
    local.dispatch_request_add(2, "bravo", "alpha").unwrap();
    let (drive, dispatch) = pending_in(&stores, path);
    match drive {
        DrivePending::Rows(rows) => assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), [id]),
        other => panic!("{other:?}"),
    }
    assert_eq!(dispatch, 2);

    // A missing DB has nothing pending (and isn't created).
    let missing = r._dir.path().join("nope").join("tasks.db");
    let (drive, dispatch) = pending_in(&stores, &missing);
    assert!(matches!(drive, DrivePending::None));
    assert_eq!(dispatch, 0);
    assert!(!missing.exists());
}

#[test]
fn the_cli_allows_for_how_far_the_throttled_presence_columns_trail() {
    // Queued on a tick, written on the next heartbeat, against a stamp truncated to
    // the second: the CLI's allowance must cover all three.
    let trail = TICK + crate::engine::writer::HEARTBEAT_EVERY + Duration::from_secs(1);
    assert!(
        trail.as_millis() as i64 <= keitora_tasks::PRESENCE_TOUCH_LAG_MS,
        "{trail:?}"
    );
}

#[test]
fn supervisor_restarts_become_health_notes_for_the_restart_window() {
    use crate::health::RestartNote;
    use crate::watchers::RESTART_WINDOW_MS;
    const T0: i64 = 1_800_000_000_000;
    let restarted = |kind, why| Restarted {
        id: WatcherId::new(kind, "proj"),
        why,
        n: 1,
        ok: true,
    };
    let note = |watcher, why| RestartNote {
        at_ms: T0,
        watcher,
        why,
    };
    let silent = RestartWhy::Silent {
        request: 3,
        age_s: 16,
    };
    let mut notes = HashMap::new();
    note_restarts(
        &mut notes,
        &[
            restarted(WatcherKind::Bg, RestartWhy::Exited("notify error".into())),
            restarted(WatcherKind::Tasks, RestartWhy::Stalled),
            restarted(WatcherKind::Bg, silent.clone()),
        ],
        T0,
    );
    assert_eq!(
        notes["proj"],
        vec![
            note(WatcherKind::Bg, RestartWhy::Exited("notify error".into())),
            note(WatcherKind::Tasks, RestartWhy::Stalled),
            note(WatcherKind::Bg, silent),
        ]
    );
    window_restarts(&mut notes, T0 + RESTART_WINDOW_MS - 1);
    assert_eq!(notes["proj"].len(), 3, "still in the window");
    window_restarts(&mut notes, T0 + RESTART_WINDOW_MS);
    assert!(notes.is_empty(), "aged out");
}
