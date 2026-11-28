//! The monitor on a fake clock: cadence, the dirty-key debounce, input gathering
//! from a test engine (no herdr), cached facts, the lease rewrite, the PATH probe and
//! remedies. Facts, the probe and remedies are injected; nothing reads a real home.

use std::sync::atomic::AtomicUsize;

use keitora_tasks::{ProjectConfig, WorktreeEntry};

use super::*;
use crate::engine::model::HookEvidence;
use crate::engine::tests::FakeEvidence;
use crate::engine::SessionEngine;
use crate::health::{
    PendingFacts, AGENT_START_REFUSED, HOOKS_MISSING, PIPELINE_STALLED, UNCONFIGURED_SESSIONS,
};
use crate::hooks::ClaudeFacts;

const S: Duration = Duration::from_secs(1);

/// Records finding writes; its lease can be switched.
struct FakeSink {
    lease: Mutex<LeaseState>,
    writes: Mutex<Vec<FindingWrite>>,
}

impl FakeSink {
    fn new(lease: LeaseState) -> Arc<FakeSink> {
        Arc::new(FakeSink {
            lease: Mutex::new(lease),
            writes: Mutex::default(),
        })
    }
    fn take(&self) -> Vec<FindingWrite> {
        std::mem::take(&mut *self.writes.lock().unwrap())
    }
}

impl FindingSink for FakeSink {
    fn lease(&self) -> LeaseState {
        self.lease.lock().unwrap().clone()
    }
    fn apply(&self, writes: &[FindingWrite]) {
        if *self.lease.lock().unwrap() == LeaseState::Owned {
            self.writes.lock().unwrap().extend_from_slice(writes);
        }
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    projects: Vec<(String, ProjectConfig)>,
    pipeline: HashMap<String, ProjectPipeline>,
    monitor: HealthMonitor,
    sink: Arc<FakeSink>,
    facts_reads: Arc<AtomicUsize>,
    /// Whether bravo's Claude hooks are installed (what the fake facts report).
    hooks_installed: Arc<AtomicBool>,
    remedies: Arc<Mutex<Vec<Remedy>>>,
    notified: Arc<Mutex<Vec<usize>>>,
    probes: Arc<AtomicUsize>,
    t0: Instant,
    t0_ms: i64,
}

fn project_config(dir: &std::path::Path) -> ProjectConfig {
    let root = dir.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let mut pc = ProjectConfig::scaffold(&root);
    pc.effective_worktrees = Some(
        ["alpha", "bravo"]
            .into_iter()
            .map(WorktreeEntry::name_only)
            .collect(),
    );
    pc
}

fn rig_with(engine: Option<SharedEngine>, path_cli: Option<PathCli>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let pc = project_config(dir.path());
    let facts_reads = Arc::new(AtomicUsize::new(0));
    let hooks_installed = Arc::new(AtomicBool::new(true));
    let (reads, installed) = (Arc::clone(&facts_reads), Arc::clone(&hooks_installed));
    let facts: FactsFn = Arc::new(move |_pc: &ProjectConfig, wt: &str| {
        reads.fetch_add(1, Ordering::SeqCst);
        WorktreeFacts {
            provider: CliKind::Claude,
            folder: Some(wt.to_string()),
            facts: Some(HookFacts {
                bin: Some("C:/Users/x/bin/keitora.exe".into()),
                codex: None,
                claude: Some(ClaudeFacts {
                    hooks_installed: wt != "bravo" || installed.load(Ordering::SeqCst),
                }),
                agy: None,
                last_launch: None,
                last_hook_error: None,
            }),
            codex_config: None,
        }
    });
    let probes = Arc::new(AtomicUsize::new(0));
    let p = Arc::clone(&probes);
    let version: VersionFn = Arc::new(move || {
        p.fetch_add(1, Ordering::SeqCst);
        path_cli.clone()
    });
    let remedies: Arc<Mutex<Vec<Remedy>>> = Arc::default();
    let r = Arc::clone(&remedies);
    let notified: Arc<Mutex<Vec<usize>>> = Arc::default();
    let n = Arc::clone(&notified);
    let sink = FakeSink::new(LeaseState::Owned);
    let monitor = HealthMonitor::new(HealthDeps {
        engine,
        sink: Some(sink.clone() as Arc<dyn FindingSink>),
        facts,
        version,
        remedy: Arc::new(move |x| r.lock().unwrap().push(x)),
        spawn: Arc::new(|job| job()),
        notify: Arc::new(move |rows| n.lock().unwrap().push(rows.len())),
        remedies_allowed: true,
    });
    Rig {
        _dir: dir,
        projects: vec![("proj".into(), pc)],
        pipeline: HashMap::new(),
        monitor,
        sink,
        facts_reads,
        hooks_installed,
        remedies,
        notified,
        probes,
        t0: Instant::now(),
        t0_ms: keitora_tasks::now_ms(),
    }
}

fn rig() -> Rig {
    rig_with(None, None)
}

impl Rig {
    fn tick(&mut self, after: Duration) {
        let known = &self.pipeline;
        let facts = PipelineFacts {
            projects: &self.projects,
            pipeline: &|name: &str| known.get(name).cloned().unwrap_or_default(),
        };
        self.monitor.tick(
            self.t0 + after,
            self.t0_ms + after.as_millis() as i64,
            &facts,
        );
    }
    fn checks(&self) -> Vec<(String, String, Option<i64>)> {
        self.monitor
            .list()
            .lock()
            .unwrap()
            .iter()
            .map(|f| (f.worktree.clone(), f.check_id.clone(), f.cleared_at))
            .collect()
    }
    fn mark(&self, worktree: &str, at: Duration) {
        let change = ViewChange {
            key: SessionKey {
                project: "proj".into(),
                worktree: worktree.into(),
            },
            prev: None,
            next: None,
        };
        mark_dirty(&self.monitor.dirty(), &[change], self.t0 + at);
    }
}

#[test]
fn the_whole_app_is_evaluated_once_per_period() {
    let mut r = rig();
    for s in 0..30 {
        r.tick(S * s);
    }
    assert_eq!(
        r.monitor.evaluations, 1,
        "the first tick, then nothing for 30 s"
    );
    r.tick(S * 30);
    assert_eq!(r.monitor.evaluations, 2);
}

#[test]
fn a_dirty_worktree_is_reevaluated_two_seconds_after_its_first_mark() {
    let mut r = rig();
    r.tick(Duration::ZERO);
    r.mark("bravo", S * 5);
    r.tick(S * 6);
    r.mark("bravo", S * 6); // a later mark keeps the first one
    assert_eq!(r.monitor.evaluations, 1, "not before 2 s");
    r.tick(S * 7);
    assert_eq!(r.monitor.evaluations, 2, "2 s after the first mark");
    r.tick(S * 8);
    r.tick(S * 9);
    assert_eq!(r.monitor.evaluations, 2, "the mark was taken");
}

#[test]
fn a_scoped_reevaluation_reconciles_only_the_dirty_worktree() {
    let mut r = rig();
    r.hooks_installed.store(false, Ordering::SeqCst);
    r.tick(Duration::ZERO);
    assert!(r
        .checks()
        .contains(&("bravo".into(), HOOKS_MISSING.into(), None)));
    // The fix lands in the files, but only alpha is re-evaluated: bravo's finding
    // stays (and its facts aren't due anyway).
    r.hooks_installed.store(true, Ordering::SeqCst);
    r.mark("alpha", S * 3);
    r.tick(S * 5);
    assert_eq!(r.monitor.evaluations, 2);
    assert!(r
        .checks()
        .contains(&("bravo".into(), HOOKS_MISSING.into(), None)));
}

#[test]
fn the_list_follows_every_evaluation_and_the_window_hears_last_seen_per_flush() {
    let mut r = rig();
    r.hooks_installed.store(false, Ordering::SeqCst);
    r.tick(Duration::ZERO);
    let last_seen = |r: &Rig| {
        r.monitor
            .list()
            .lock()
            .unwrap()
            .iter()
            .find(|f| f.check_id == HOOKS_MISSING)
            .map(|f| f.last_seen)
    };
    assert_eq!(last_seen(&r), Some(r.t0_ms));
    assert_eq!(r.notified.lock().unwrap().len(), 1);
    r.tick(EVALUATE_EVERY);
    assert_eq!(
        last_seen(&r),
        Some(r.t0_ms + EVALUATE_EVERY.as_millis() as i64),
        "the list the IPC reads moves with each evaluation"
    );
    assert_eq!(
        r.notified.lock().unwrap().len(),
        1,
        "the window isn't told before a flush"
    );
    r.tick(EVALUATE_EVERY * 2);
    assert_eq!(
        r.notified.lock().unwrap().len(),
        2,
        "told when last_seen is flushed"
    );
}

#[test]
fn a_scoped_reevaluation_gathers_only_the_dirty_worktree() {
    let mut r = rig();
    // A unique project name: launch records are process-wide.
    let mut pc = r.projects[0].1.clone();
    pc.project.name = "health-scope-probe".into();
    r.projects = vec![("health-scope-probe".into(), pc)];
    r.tick(Duration::ZERO);
    assert_eq!(r.facts_reads.load(Ordering::SeqCst), 2, "alpha and bravo");
    // bravo relaunched (its facts would be re-read), but only alpha is dirty.
    let bravo = SessionKey {
        project: "health-scope-probe".into(),
        worktree: "bravo".into(),
    };
    crate::hooks::note_launch(
        &bravo,
        crate::hooks::LaunchRecord {
            at_ms: r.t0_ms,
            cli: CliKind::Claude,
            trust_bypass: true,
            env_names: vec![crate::hooks::PANE_MARKER_ENV.into()],
            claude_settings: crate::hooks::ClaudeSettings::NotApplicable,
            hook_version: crate::hooks::HOOK_VERSION,
            start_refused: None,
        },
    );
    let change = ViewChange {
        key: SessionKey {
            project: "health-scope-probe".into(),
            worktree: "alpha".into(),
        },
        prev: None,
        next: None,
    };
    mark_dirty(&r.monitor.dirty(), &[change], r.t0 + S * 3);
    r.tick(S * 5);
    assert_eq!(r.monitor.evaluations, 2);
    assert_eq!(
        r.facts_reads.load(Ordering::SeqCst),
        2,
        "bravo is out of scope: not gathered"
    );
    r.tick(EVALUATE_EVERY);
    assert_eq!(
        r.facts_reads.load(Ordering::SeqCst),
        3,
        "the full pass reads it"
    );
}

#[test]
fn inputs_come_from_the_engine_and_its_evidence_without_herdr() {
    let evidence = Arc::new(FakeEvidence::default());
    let now = keitora_tasks::now_ms();
    // A recent session row of a worktree the project doesn't configure.
    evidence.set_row(
        "proj",
        "zulu",
        Some(HookEvidence {
            state: "idle".into(),
            written_ms: Some(now),
            brokered: false,
            escalated_reason: None,
            session_id: None,
        }),
    );
    let engine = SessionEngine::new_for_test(evidence.clone());
    let mut r = rig_with(Some(engine), None);
    r.tick(Duration::ZERO);
    assert_eq!(
        r.checks(),
        vec![("".into(), UNCONFIGURED_SESSIONS.into(), None)],
        "unconfigured session names reach the monitor"
    );
    let list = r.monitor.list();
    let rows = list.lock().unwrap();
    assert_eq!(
        rows[0].message,
        "proj: session rows exist for unconfigured worktrees: zulu"
    );
    assert_eq!(
        *r.notified.lock().unwrap(),
        vec![1],
        "the window heard once"
    );
}

#[test]
fn a_launch_record_reaches_the_checks_without_a_file_read() {
    let evidence = Arc::new(FakeEvidence::default());
    let engine = SessionEngine::new_for_test(evidence);
    let mut r = rig_with(Some(engine), None);
    // A unique project name: launch records are process-wide.
    let mut pc = r.projects[0].1.clone();
    pc.project.name = "health-launch-probe".into();
    r.projects = vec![("health-launch-probe".into(), pc)];
    r.tick(Duration::ZERO);
    let reads = r.facts_reads.load(Ordering::SeqCst);
    let key = SessionKey {
        project: "health-launch-probe".into(),
        worktree: "bravo".into(),
    };
    crate::hooks::note_launch(
        &key,
        crate::hooks::LaunchRecord {
            at_ms: r.t0_ms,
            cli: CliKind::Claude,
            trust_bypass: false,
            env_names: vec![crate::hooks::PANE_MARKER_ENV.into()],
            claude_settings: crate::hooks::ClaudeSettings::NotApplicable,
            hook_version: crate::hooks::HOOK_VERSION,
            start_refused: Some("agent_not_ready".into()),
        },
    );
    r.tick(EVALUATE_EVERY);
    assert!(r
        .checks()
        .iter()
        .any(|(wt, id, _)| wt == "bravo" && id == AGENT_START_REFUSED));
    assert_eq!(
        r.facts_reads.load(Ordering::SeqCst),
        reads + 1,
        "a new launch re-reads that worktree's facts only"
    );
}

#[test]
fn facts_are_cached_and_reread_after_a_remedy() {
    let mut r = rig();
    r.hooks_installed.store(false, Ordering::SeqCst);
    r.tick(Duration::ZERO);
    assert_eq!(r.facts_reads.load(Ordering::SeqCst), 2, "alpha and bravo");
    assert_eq!(
        *r.remedies.lock().unwrap(),
        vec![Remedy::ReinstallHooks {
            project: "proj".into(),
            worktree: "bravo".into()
        }]
    );
    let row = r.monitor.list().lock().unwrap()[0].clone();
    assert_eq!(row.remedy, "reinstalled hooks");
    // The remedy fixed the files: bravo's facts are re-read 5 s later, not 5 min.
    r.hooks_installed.store(true, Ordering::SeqCst);
    r.tick(EVALUATE_EVERY);
    assert_eq!(r.facts_reads.load(Ordering::SeqCst), 3, "only bravo again");
    r.tick(EVALUATE_EVERY * 2);
    assert_eq!(r.facts_reads.load(Ordering::SeqCst), 3, "cached");
    let cleared = r
        .checks()
        .into_iter()
        .find(|(_, id, _)| id == HOOKS_MISSING)
        .unwrap();
    assert!(
        cleared.2.is_some(),
        "clean on re-read: cleared after two evaluations"
    );
    assert_eq!(r.remedies.lock().unwrap().len(), 1);
}

#[test]
fn a_fresh_lease_holder_rewrites_the_table_and_a_standby_writes_nothing() {
    let mut r = rig();
    *r.sink.lease.lock().unwrap() = LeaseState::Standby {
        pid: 7,
        version: "0.5.0".into(),
        heartbeat_at: 0,
    };
    r.hooks_installed.store(false, Ordering::SeqCst);
    r.tick(Duration::ZERO);
    assert!(r.sink.take().is_empty(), "standby: nothing written");
    assert_eq!(r.checks().len(), 2, "kept in memory (and status_writer)");
    *r.sink.lease.lock().unwrap() = LeaseState::Owned;
    r.tick(S);
    let w = r.sink.take();
    assert_eq!(w[0], FindingWrite::Clear);
    assert_eq!(w.len(), 3, "clear, then every kept row");
    r.tick(S * 2);
    assert!(r.sink.take().is_empty(), "then only changes");
}

#[test]
fn the_path_probe_runs_off_thread_every_ten_minutes() {
    let cli = PathCli {
        path: "C:/Users/x/AppData/Local/keitora/keitora.exe".into(),
        version: "0.1.0".into(),
        dev_build: false,
        bundled_version: Some("0.2.0".into()),
        managed_stale: false,
        refreshable: false,
    };
    let mut r = rig_with(None, Some(cli));
    r.tick(Duration::ZERO);
    assert_eq!(r.probes.load(Ordering::SeqCst), 1);
    assert!(r
        .checks()
        .iter()
        .any(|(_, id, _)| id == crate::health::VERSION_SKEW));
    r.tick(VERSION_EVERY - S);
    assert_eq!(r.probes.load(Ordering::SeqCst), 1);
    r.tick(VERSION_EVERY);
    assert_eq!(r.probes.load(Ordering::SeqCst), 2);
}

#[test]
fn health_list_holds_active_and_recently_cleared_findings() {
    let mut r = rig();
    r.pipeline.insert(
        "proj".into(),
        ProjectPipeline {
            oldest_pending: Some(PendingFacts {
                id: 4,
                created_at_ms: r.t0_ms - 60_000,
                seen: false,
            }),
            last_pass_ms: None,
            restarts: Vec::new(),
        },
    );
    r.tick(Duration::ZERO);
    assert_eq!(r.checks(), vec![("".into(), PIPELINE_STALLED.into(), None)]);
    r.pipeline.clear();
    r.tick(EVALUATE_EVERY);
    let at = r.t0_ms + EVALUATE_EVERY.as_millis() as i64;
    assert_eq!(
        r.checks(),
        vec![("".into(), PIPELINE_STALLED.into(), Some(at))],
        "instant clear, still listed"
    );
    let ipc = r.monitor.list().lock().unwrap().clone();
    assert_eq!(ipc[0].cleared_at, Some(at));
    assert_eq!(ipc[0].check_id, PIPELINE_STALLED);
    assert_eq!(ipc[0].detail["request"], serde_json::json!(4));
    let v = serde_json::to_value(&ipc[0]).unwrap();
    for k in [
        "project",
        "worktree",
        "checkId",
        "severity",
        "message",
        "detail",
        "firstSeen",
        "lastSeen",
        "occurrences",
        "clearedAt",
        "remedy",
        "remedyAt",
    ] {
        assert!(v.get(k).is_some(), "{k} in {v}");
    }
}

#[test]
fn pipeline_facts_are_asked_only_when_the_monitor_evaluates() {
    let mut r = rig();
    let asked = AtomicUsize::new(0);
    let pipeline = |_: &str| {
        asked.fetch_add(1, Ordering::SeqCst);
        ProjectPipeline::default()
    };
    for s in 0..30 {
        r.monitor.tick(
            r.t0 + S * s,
            r.t0_ms + 1000 * s as i64,
            &PipelineFacts {
                projects: &r.projects,
                pipeline: &pipeline,
            },
        );
    }
    assert_eq!(r.monitor.evaluations, 1);
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "one project, one evaluation"
    );
}

#[test]
fn the_bundled_version_is_kept_once_found_and_asked_again_while_missing() {
    let cell = std::sync::OnceLock::new();
    let asked = AtomicUsize::new(0);
    let probe = |answer: Option<&str>| {
        asked.fetch_add(1, Ordering::SeqCst);
        answer.map(str::to_string)
    };
    assert_eq!(kept_once_found(&cell, || probe(None)), None);
    assert_eq!(
        kept_once_found(&cell, || probe(Some("0.6.0"))),
        Some("0.6.0".into()),
        "a failed probe isn't cached"
    );
    assert_eq!(
        kept_once_found(&cell, || probe(Some("0.7.0"))),
        Some("0.6.0".into())
    );
    assert_eq!(asked.load(Ordering::SeqCst), 2, "not asked once found");
}
