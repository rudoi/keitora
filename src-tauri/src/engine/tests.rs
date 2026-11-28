//! Engine-level tests over [`FakeEvidence`] and a fake clock, plus the test helpers
//! other modules' tests build engines with.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::model::{obs, AgentObs, HookEvidence, ProjectEvidence};
use super::writer::{LeaseState, StatusPublisher};
use super::*;

/// Evidence from a map the test fills.
#[derive(Default)]
pub(crate) struct FakeEvidence {
    projects: Mutex<HashMap<String, ProjectEvidence>>,
    reads: Mutex<Vec<String>>,
}

impl FakeEvidence {
    /// Set `project`'s hook row for `worktree` (`None` removes it).
    pub(crate) fn set_row(&self, project: &str, worktree: &str, row: Option<HookEvidence>) {
        let mut projects = self.projects.lock().unwrap();
        let p = projects
            .entry(project.to_string())
            .or_insert_with(|| ProjectEvidence {
                db_path: format!("D:/repos/{project}/tasks.db"),
                ..ProjectEvidence::default()
            });
        match row {
            Some(r) => p.rows.insert(worktree.to_string(), r),
            None => p.rows.remove(worktree),
        };
    }

    /// Which projects were read, in order.
    pub(crate) fn reads(&self) -> Vec<String> {
        self.reads.lock().unwrap().clone()
    }
}

impl Evidence for FakeEvidence {
    fn project(&self, project: &str) -> Option<ProjectEvidence> {
        self.reads.lock().unwrap().push(project.to_string());
        self.projects.lock().unwrap().get(project).cloned()
    }
}

impl SessionEngine {
    /// An engine over `evidence` with no publisher, on the real clock.
    pub(crate) fn new_for_test(evidence: Arc<dyn Evidence>) -> SharedEngine {
        SessionEngine::new(evidence, None)
    }
}

/// Ingest `keys` twice, [`SETTLE`] apart, on the real clock: idles settle and the
/// views stay fresh (the second snapshot is stamped ahead of now).
pub(crate) fn ingest_settled(engine: &SessionEngine, keys: Vec<(SessionKey, Option<AgentObs>)>) {
    let now = Instant::now();
    let ms = keitora_tasks::now_ms();
    for (at, at_ms) in [(now, ms), (now + SETTLE, ms + SETTLE.as_millis() as i64)] {
        engine.ingest(HerdrSnapshot {
            at,
            at_ms,
            generation: Generation(1),
            keys: keys.clone(),
        });
    }
}

/// A clock that only moves when told.
#[derive(Clone)]
pub(crate) struct TestClock(Arc<Mutex<Instant>>);

impl TestClock {
    pub(crate) fn new(at: Instant) -> Self {
        TestClock(Arc::new(Mutex::new(at)))
    }
    pub(crate) fn set(&self, at: Instant) {
        *self.0.lock().unwrap() = at;
    }
    pub(crate) fn clock(&self) -> Clock {
        let c = Arc::clone(&self.0);
        Arc::new(move || *c.lock().unwrap())
    }
}

const T0_MS: i64 = 1_800_000_000_000;

fn key(w: &str) -> SessionKey {
    SessionKey {
        project: "proj".into(),
        worktree: w.into(),
    }
}

/// An engine on a fake clock, its evidence, and the fake time origin.
struct Rig {
    engine: SharedEngine,
    evidence: Arc<FakeEvidence>,
    clock: TestClock,
    t0: Instant,
    generation: Generation,
}

impl Rig {
    fn new() -> Rig {
        Self::with_publisher(None)
    }

    fn with_publisher(publisher: Option<Arc<StatusPublisher>>) -> Rig {
        let t0 = Instant::now();
        let clock = TestClock::new(t0);
        let evidence = Arc::new(FakeEvidence::default());
        let engine = SessionEngine::with_clock(
            Arc::clone(&evidence) as Arc<dyn Evidence>,
            publisher,
            clock.clock(),
        );
        Rig {
            engine,
            evidence,
            clock,
            t0,
            generation: Generation(1),
        }
    }

    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }

    /// Move the clock to `ms` and ingest `keys` there.
    fn ingest(&self, ms: u64, keys: Vec<(SessionKey, Option<AgentObs>)>) {
        self.clock.set(self.at(ms));
        self.engine.ingest(HerdrSnapshot {
            at: self.at(ms),
            at_ms: T0_MS + ms as i64,
            generation: self.generation,
            keys,
        });
    }

    fn state(&self, w: &str) -> String {
        match self.engine.view(&key(w)) {
            KeyView::View(v) => v.state,
            other => panic!("no view for {w}: {other:?}"),
        }
    }

    fn view(&self, w: &str) -> StatusSession {
        match self.engine.view(&key(w)) {
            KeyView::View(v) => v,
            other => panic!("no view for {w}: {other:?}"),
        }
    }

    /// Ingest `o` for `w` twice, 1 s apart, so an idle settles.
    fn settle(&self, ms: u64, w: &str, o: &AgentObs) {
        self.ingest(ms, vec![(key(w), Some(o.clone()))]);
        self.ingest(ms + 1_000, vec![(key(w), Some(o.clone()))]);
    }
}

fn row(state: &str, written_ms: i64) -> HookEvidence {
    HookEvidence {
        state: state.into(),
        written_ms: Some(written_ms),
        brokered: false,
        escalated_reason: None,
        session_id: None,
    }
}

fn agy(status: &str, scs: u64) -> AgentObs {
    AgentObs {
        agent: Some("agy".into()),
        ..obs("w1:p1", status, scs, None)
    }
}

fn recorder(engine: &SessionEngine) -> Arc<Mutex<Vec<Vec<ViewChange>>>> {
    let seen: Arc<Mutex<Vec<Vec<ViewChange>>>> = Arc::default();
    let s = Arc::clone(&seen);
    engine.subscribe(Box::new(move |c| s.lock().unwrap().push(c.to_vec())));
    seen
}

#[test]
fn no_snapshot_means_not_ready() {
    let r = Rig::new();
    assert_eq!(r.engine.view(&key("alpha")), KeyView::NotReady);
    let v = r.engine.views("proj");
    assert!(!v.ready);
    assert!(v.views.is_empty());
    assert!(!r.engine.is_ready());
    assert_eq!(r.engine.link_state().link, HerdrLink::Starting);
    assert_eq!(r.engine.link_state().last_snapshot_ms, None);
}

#[test]
fn gate_refusal_marks_every_view_stale_never_offline() {
    let r = Rig::new();
    r.settle(0, "alpha", &obs("w1:p1", "idle", 3, None));
    assert!(r.engine.is_ready());
    assert!(!r.view("alpha").stale);
    r.engine.mark_link(HerdrLink::Recovering, T0_MS + 2_000);
    let v = r.view("alpha");
    assert!(v.stale);
    assert_eq!(v.state, "idle", "the last state is kept");
    assert_eq!(v.agent, "present");
    assert!(!r.engine.is_ready());
    let ls = r.engine.link_state();
    assert_eq!(ls.link, HerdrLink::Recovering);
    assert_eq!(ls.since_ms, T0_MS + 2_000);
    assert!(!ls.ready);
    // The next snapshot brings it back.
    r.ingest(
        3_000,
        vec![(key("alpha"), Some(obs("w1:p1", "idle", 3, None)))],
    );
    assert!(!r.view("alpha").stale);
    assert_eq!(r.engine.link_state().link, HerdrLink::Up);
}

#[test]
fn snapshot_older_than_three_intervals_is_stale() {
    let r = Rig::new();
    r.engine.set_window_open(true);
    r.settle(0, "alpha", &obs("w1:p1", "idle", 3, None));
    assert_eq!(r.engine.mode(), Mode::Warm);
    // The last snapshot was at 1 s; warm is 5 s, so 15 s later is still fresh.
    r.clock.set(r.at(16_000));
    assert!(!r.view("alpha").stale);
    r.clock.set(r.at(16_001));
    assert!(r.view("alpha").stale);
    assert_eq!(r.view("alpha").state, "idle");
}

#[test]
fn poll_failed_goes_stale_by_age_without_offline() {
    let r = Rig::new();
    let seen = recorder(&r.engine);
    r.settle(0, "alpha", &obs("w1:p1", "working", 3, None));
    seen.lock().unwrap().clear();
    // A busy agent is not in flight, so the engine polls cold: 15 s, stale after 45 s.
    r.clock.set(r.at(10_000));
    r.engine.poll_failed(r.at(10_000));
    assert!(seen.lock().unwrap().is_empty(), "still fresh: no change");
    r.clock.set(r.at(46_001));
    r.engine.poll_failed(r.at(46_001));
    let v = r.view("alpha");
    assert!(v.stale);
    assert_eq!(v.state, "busy");
    let changes = seen.lock().unwrap().clone();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].len(), 1);
    assert!(changes[0][0].next.as_ref().unwrap().stale);
    // poll_failed counts as a poll: not due again until the interval passes.
    assert!(!r.engine.due(r.at(47_000), false));
    assert!(r.engine.due(r.at(61_001), false));
}

#[test]
fn missing_key_after_first_snapshot_is_missing() {
    let r = Rig::new();
    r.ingest(0, vec![(key("alpha"), None)]);
    assert_eq!(r.engine.view(&key("bravo")), KeyView::Missing);
    assert_eq!(r.state("alpha"), "offline");
}

#[test]
fn subscribers_get_changes_once_and_nothing_without_change() {
    let r = Rig::new();
    let seen = recorder(&r.engine);
    let o = obs("w1:p1", "working", 3, None);
    r.ingest(
        0,
        vec![(key("alpha"), Some(o.clone())), (key("bravo"), None)],
    );
    {
        let s = seen.lock().unwrap();
        assert_eq!(s.len(), 1, "one batch per ingest");
        assert_eq!(s[0].len(), 2);
        assert!(s[0].iter().all(|c| c.prev.is_none() && c.next.is_some()));
    }
    r.ingest(
        1_000,
        vec![(key("alpha"), Some(o.clone())), (key("bravo"), None)],
    );
    assert_eq!(seen.lock().unwrap().len(), 1, "no change, no call");
    r.engine.mark_link(HerdrLink::Up, T0_MS);
    assert_eq!(seen.lock().unwrap().len(), 1, "same link, no call");

    let o2 = obs("w1:p1", "blocked", 4, None);
    r.ingest(2_000, vec![(key("alpha"), Some(o2)), (key("bravo"), None)]);
    let s = seen.lock().unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(s[1].len(), 1);
    assert_eq!(s[1][0].key, key("alpha"));
    assert_eq!(s[1][0].prev.as_ref().unwrap().state, "busy");
    assert_eq!(s[1][0].next.as_ref().unwrap().state, "waiting");
}

#[test]
fn memory_is_dropped_for_removed_keys() {
    let r = Rig::new();
    let seen = recorder(&r.engine);
    r.settle(0, "alpha", &obs("w1:p1", "idle", 3, None));
    r.ingest(5_000, vec![]);
    assert_eq!(r.engine.view(&key("alpha")), KeyView::Missing);
    assert!(r.engine.key_health().is_empty());
    let last = seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.len(), 1);
    assert!(last[0].next.is_none());
    // Coming back starts from scratch: unsettled again.
    r.ingest(
        6_000,
        vec![(key("alpha"), Some(obs("w1:p1", "idle", 3, None)))],
    );
    assert!(!r.view("alpha").settled);
}

#[test]
fn mode_table() {
    let r = Rig::new();
    assert_eq!(r.engine.mode(), Mode::Cold);
    r.engine.set_window_open(true);
    assert_eq!(r.engine.mode(), Mode::Warm);
    r.engine.set_window_open(false);

    r.engine.set_pending("proj", true);
    assert_eq!(r.engine.mode(), Mode::Hot);
    r.engine.set_pending("proj", false);
    assert_eq!(r.engine.mode(), Mode::Cold);

    r.engine.note_delivery(&key("alpha"));
    assert_eq!(r.engine.mode(), Mode::Hot);
    r.ingest(0, vec![]);
    assert_eq!(r.engine.mode(), Mode::Cold, "deliveries reload on ingest");

    r.engine.note_launch(&key("alpha"));
    assert_eq!(r.engine.mode(), Mode::Hot);
    r.engine.note_launch_failed(&key("alpha"));
    assert_eq!(r.engine.mode(), Mode::Cold);
    // A launch never marked done stops counting after the grace.
    r.engine.note_launch(&key("alpha"));
    r.clock.set(r.at(61_000));
    assert_eq!(r.engine.mode(), Mode::Cold);

    // An unsettled idle is hot until it settles.
    let idle = obs("w1:p1", "idle", 3, None);
    r.ingest(70_000, vec![(key("alpha"), Some(idle.clone()))]);
    assert_eq!(r.engine.mode(), Mode::Hot);
    r.ingest(71_000, vec![(key("alpha"), Some(idle))]);
    assert_eq!(r.engine.mode(), Mode::Cold);
    // Starting is hot.
    let starting = AgentObs {
        launch_pending: true,
        ..obs("w1:p1", "unknown", 4, None)
    };
    r.ingest(72_000, vec![(key("alpha"), Some(starting))]);
    assert_eq!(r.engine.mode(), Mode::Hot);

    let secs = Duration::from_secs;
    for (mode, low, want) in [
        (Mode::Hot, false, secs(1)),
        (Mode::Hot, true, secs(2)),
        (Mode::Warm, false, secs(5)),
        (Mode::Warm, true, secs(15)),
        (Mode::Cold, false, secs(15)),
        (Mode::Cold, true, secs(30)),
    ] {
        assert_eq!(interval(mode, low), want, "{mode:?} low={low}");
    }
}

#[test]
fn due_follows_the_mode_interval() {
    let r = Rig::new();
    assert!(r.engine.due(r.at(0), false), "never polled");
    r.ingest(0, vec![]);
    assert!(!r.engine.due(r.at(14_999), false));
    assert!(r.engine.due(r.at(15_000), false));
    assert!(!r.engine.due(r.at(29_999), true), "low cold is 30 s");
    r.engine.set_pending("proj", true);
    assert!(r.engine.due(r.at(1_000), false));
    assert!(!r.engine.due(r.at(1_999), true), "low hot is 2 s");
}

#[test]
fn a_noted_launch_reads_starting_until_herdr_lists_the_agent() {
    let r = Rig::new();
    r.clock.set(r.at(0));
    r.engine.note_launch(&key("alpha"));
    r.ingest(500, vec![(key("alpha"), None)]);
    let v = r.view("alpha");
    assert_eq!((v.state.as_str(), v.agent.as_str()), ("busy", "starting"));
    r.engine.note_launch_failed(&key("alpha"));
    r.ingest(1_500, vec![(key("alpha"), None)]);
    assert_eq!(r.view("alpha").agent, "absent");
}

#[test]
fn a_launch_stays_noted_until_herdr_lists_its_agent() {
    let r = Rig::new();
    r.clock.set(r.at(0));
    r.engine.note_launch(&key("alpha"));
    // The launch call returned, but herdr has no entry for the pane yet.
    r.ingest(500, vec![(key("alpha"), None)]);
    assert_eq!(r.view("alpha").agent, "starting");
    assert_eq!(r.engine.mode(), Mode::Hot);
    // herdr lists the agent: the note is done.
    r.ingest(
        1_500,
        vec![(key("alpha"), Some(obs("w1:p1", "working", 3, None)))],
    );
    assert_eq!(r.view("alpha").agent, "present");
    // The agent exits: absent at once, no leftover launch note.
    r.ingest(2_500, vec![(key("alpha"), None)]);
    assert_eq!(r.view("alpha").agent, "absent");
    assert_eq!(r.engine.mode(), Mode::Cold);
}

#[test]
fn a_launch_that_never_shows_up_lapses_after_the_grace() {
    let r = Rig::new();
    r.clock.set(r.at(0));
    r.engine.note_launch(&key("alpha"));
    r.ingest(30_000, vec![(key("alpha"), None)]);
    assert_eq!(r.view("alpha").agent, "starting");
    r.ingest(
        LAUNCH_GRACE.as_millis() as u64 + 1_000,
        vec![(key("alpha"), None)],
    );
    assert_eq!(r.view("alpha").agent, "absent");
    assert_eq!(r.engine.mode(), Mode::Cold);
}

#[test]
fn a_switch_to_a_faster_mode_does_not_age_a_cold_snapshot() {
    let r = Rig::new();
    r.settle(0, "alpha", &obs("w1:p1", "working", 3, None));
    assert_eq!(r.engine.mode(), Mode::Cold);
    // 10 s into a cold interval, a request goes pending: hot is 1 s, but the
    // snapshot was taken under the cold 15 s cadence.
    r.clock.set(r.at(11_000));
    r.engine.set_pending("proj", true);
    assert_eq!(r.engine.mode(), Mode::Hot);
    assert!(!r.view("alpha").stale, "judged against the cold interval");
    // Past three cold intervals it is stale all the same.
    r.clock.set(r.at(46_001));
    assert!(r.view("alpha").stale);
    // A hot snapshot is judged against hot (or the current mode, if longer).
    r.ingest(
        47_000,
        vec![(key("alpha"), Some(obs("w1:p1", "working", 3, None)))],
    );
    r.clock.set(r.at(50_001));
    assert!(r.view("alpha").stale);
}

#[test]
fn evidence_is_read_once_per_project_per_ingest() {
    let r = Rig::new();
    let other = SessionKey {
        project: "other-proj".into(),
        worktree: "alpha".into(),
    };
    r.ingest(
        0,
        vec![(key("alpha"), None), (key("bravo"), None), (other, None)],
    );
    assert_eq!(r.evidence.reads(), vec!["other-proj", "proj"]);
}

#[test]
fn a_newer_hook_busy_row_vetoes_the_idle() {
    let r = Rig::new();
    let idle = obs("w1:p1", "idle", 3, None);
    r.settle(0, "alpha", &idle);
    assert_eq!(r.state("alpha"), "idle");
    r.evidence
        .set_row("proj", "alpha", Some(row("busy", T0_MS + 5_000)));
    r.ingest(5_000, vec![(key("alpha"), Some(idle))]);
    assert_eq!(r.state("alpha"), "busy");
}

#[test]
fn a1_missing_row_agy_present_idle_reads_idle() {
    let r = Rig::new();
    r.settle(0, "alpha", &agy("idle", 2));
    let v = r.engine.views("proj");
    assert!(v.ready);
    let a = &v.views["alpha"];
    assert_eq!(a.state, "idle");
    assert_eq!(a.agent, "present");
    assert_eq!(a.agent_kind.as_deref(), Some("agy"));
    assert!(a.settled && !a.stale);
}

#[test]
fn a1_stale_offline_row_with_agent_present_reads_idle() {
    let r = Rig::new();
    r.evidence
        .set_row("proj", "alpha", Some(row("offline", T0_MS + 60_000)));
    r.settle(0, "alpha", &obs("w1:p1", "idle", 2, None));
    assert_eq!(r.state("alpha"), "idle");
    assert_eq!(r.view("alpha").agent, "present");
}

#[test]
fn a1_bare_shell_with_idle_row_reads_offline() {
    let r = Rig::new();
    r.evidence
        .set_row("proj", "alpha", Some(row("idle", T0_MS)));
    let shell = AgentObs {
        agent: None,
        ..obs("w1:p1", "unknown", 0, None)
    };
    r.settle(0, "alpha", &shell);
    let v = r.view("alpha");
    assert_eq!((v.state.as_str(), v.agent.as_str()), ("offline", "absent"));
}

#[test]
fn key_health_reports_presence_and_pane() {
    let r = Rig::new();
    r.ingest(
        0,
        vec![(key("alpha"), Some(obs("w1:p1", "working", 1, None)))],
    );
    r.ingest(
        1_000,
        vec![(key("alpha"), Some(obs("w1:p1", "mystery", 2, None)))],
    );
    let h = r.engine.key_health();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].1.present_since_ms, Some(T0_MS));
    assert_eq!(h[0].1.unknown_since_ms, Some(T0_MS + 1_000));
    assert_eq!(h[0].1.pane_id.as_deref(), Some("w1:p1"));
    assert_eq!(r.engine.all_views().len(), 1);
}

#[test]
fn ingest_and_link_changes_reach_status_db() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.db");
    let me = keitora_tasks::WriterIdentity {
        instance_id: "a".into(),
        pid: 7,
        version: "0.0.0-test".into(),
        started_at: T0_MS,
    };
    let publisher = StatusPublisher::open(path.clone(), me, T0_MS);
    assert_eq!(publisher.lease(), LeaseState::Owned);
    let r = Rig::with_publisher(Some(Arc::clone(&publisher)));
    r.settle(0, "alpha", &obs("w1:p1", "idle", 3, None));
    let reader = keitora_tasks::StatusReader::open(&path).unwrap().unwrap();
    let rows = reader.sessions("proj").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, "idle");
    assert!(rows[0].settled && !rows[0].stale);
    let w = reader.writer().unwrap().unwrap();
    assert_eq!(w.herdr_link, "up");
    assert_eq!(w.herdr_link_since, T0_MS);
    publisher.beat(T0_MS + 5_000);
    assert_eq!(
        reader.writer().unwrap().unwrap().last_snapshot_at,
        Some(T0_MS + 1_000)
    );

    r.engine.mark_link(HerdrLink::Down, T0_MS + 6_000);
    assert!(reader.sessions("proj").unwrap()[0].stale);
    assert_eq!(reader.writer().unwrap().unwrap().herdr_link, "down");
}

#[test]
fn running_agent_keys_count_present_and_starting_and_panes_until_ready() {
    let r = Rig::new();
    let shell = AgentObs {
        agent: None,
        ..obs("w1:p2", "unknown", 0, None)
    };
    let starting = AgentObs {
        launch_pending: true,
        ..obs("w1:p3", "unknown", 1, None)
    };
    let panes = || {
        vec![
            key("bravo"),
            key("delta"),
            SessionKey::agent_update(),
            key("bravo"),
        ]
    };
    // Not ready: every pane (reserved excluded), once each.
    assert_eq!(
        running_agent_keys(&r.engine.all_views(), r.engine.is_ready(), panes()),
        vec![key("bravo"), key("delta")]
    );
    r.ingest(
        0,
        vec![
            (key("alpha"), Some(obs("w1:p1", "idle", 1, None))),
            (key("bravo"), Some(shell)),
            (key("charlie"), Some(starting)),
            (key("delta"), None),
        ],
    );
    // Ready: agents present or starting only; the panes no longer count.
    assert_eq!(
        running_agent_keys(&r.engine.all_views(), r.engine.is_ready(), panes()),
        vec![key("alpha"), key("charlie")]
    );
    // A stale view keeps its last presence (never under-counted).
    r.engine.mark_link(HerdrLink::Down, T0_MS + 1_000);
    assert_eq!(
        running_agent_keys(&r.engine.all_views(), true, Vec::new()),
        vec![key("alpha"), key("charlie")]
    );
}

#[test]
fn running_count_and_keys_fall_back_on_live_panes_until_fresh() {
    use crate::backend::fake::FakeBackend;
    use crate::backend::{PtyBackend, SessionSummary};

    let r = Rig::new();
    let fake = Arc::new(FakeBackend::new());
    let backend: Arc<dyn PtyBackend> = fake.clone();
    // No backend attached: nothing to fall back on.
    assert_eq!(r.engine.running_count(), 0);
    assert_eq!(r.engine.running_keys_or_all_live(), Ok(Vec::new()));

    r.engine.attach_backend(&backend);
    for k in [key("alpha"), key("bravo"), SessionKey::agent_update()] {
        fake.insert_live(k);
    }
    let summary = |k: SessionKey| SessionSummary {
        key: k,
        gen: 1,
        state_hint: None,
    };
    fake.push_list_sessions(Ok(vec![summary(key("alpha")), summary(key("bravo"))]));
    // Not ready: every live pane counts (the reserved one never does).
    assert_eq!(r.engine.running_count(), 2);
    assert_eq!(
        r.engine.running_keys_or_all_live(),
        Ok(vec![key("alpha"), key("bravo")])
    );
    fake.push_list_sessions(Err(crate::backend::PtyBackendError::NotConnected));
    assert!(r.engine.running_keys_or_all_live().is_err());

    let shell = AgentObs {
        agent: None,
        ..obs("w1:p2", "unknown", 0, None)
    };
    r.ingest(
        0,
        vec![
            (key("alpha"), Some(obs("w1:p1", "idle", 1, None))),
            (key("bravo"), Some(shell)),
        ],
    );
    // Fresh: only the running agents; a bare shell pane no longer counts.
    let calls = fake.calls().len();
    assert_eq!(r.engine.running_count(), 1);
    assert_eq!(r.engine.running_keys_or_all_live(), Ok(vec![key("alpha")]));
    assert_eq!(
        fake.calls().len(),
        calls,
        "a fresh engine never asks the backend"
    );

    // Stale again (herdr down): the live panes count once more.
    r.engine.mark_link(HerdrLink::Down, T0_MS + 1_000);
    assert_eq!(r.engine.running_count(), 2);
}

// ── Reproductions ───────────────────────────────────────────────────────────
//
// The smallest step lists for the two "live pane reads offline" reports, over a
// temp `tasks.db`, a fake backend, the real drive runner and a fake clock.

use keitora_tasks::{CliKind, ProjectConfig};

/// The drive host the repros need: a fixed provider per worktree, nothing else.
struct ReproHost(HashMap<String, CliKind>);

impl crate::drive::DriveHost for ReproHost {
    fn emit_restart(&self, _ev: &crate::drive::DriveRestartEvent) {}
    fn emit_swap(&self, _ev: &crate::drive::DriveSwapEvent) {}
    fn worktrees_changed(&self) {}
    fn current_cli(&self, _project: &str, _config: &ProjectConfig, worktree: &str) -> CliKind {
        self.0.get(worktree).copied().unwrap_or(CliKind::Claude)
    }
    fn persist_cli(&self, _: &ProjectConfig, _: &str, _: CliKind) -> Result<(), String> {
        Ok(())
    }
    fn refresh_integration(&self) {}
    fn schedule_pass(&self, _project: &str, _after: Duration) {}
}

/// Project `proj` (worktrees alpha, bravo, charlie) under a temp dir: its config
/// (pointing at the temp `tasks.db`), a fake backend, an engine over that DB's hook
/// rows on a fake clock, and the drive runner's env over the same backend.
struct ReproRig {
    _dir: tempfile::TempDir,
    config: ProjectConfig,
    fake: Arc<crate::backend::fake::FakeBackend>,
    engine: SharedEngine,
    clock: TestClock,
    t0: Instant,
    /// Wall ms at `t0` (real, so hook rows and deliveries line up with the ticks).
    t0_ms: i64,
    env: Arc<crate::drive::DriveEnv>,
}

fn repro_rig(host: ReproHost) -> ReproRig {
    use keitora_tasks::WorktreeEntry;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let mut config = ProjectConfig::scaffold(&root);
    config.effective_worktrees = Some(
        ["alpha", "bravo", "charlie"]
            .into_iter()
            .map(WorktreeEntry::name_only)
            .collect(),
    );
    let db_path = dir.path().join("tasks.db").to_string_lossy().into_owned();
    config.tasks.db_path = db_path.clone().into();
    keitora_tasks::LocalStore::open(&db_path).unwrap();
    let fake = Arc::new(crate::backend::fake::FakeBackend::new());
    let lookup = (db_path.clone(), config.worktrees());
    let evidence = super::evidence::RegistryEvidence::with_lookup(Box::new(move |p| {
        (p == "proj").then(|| lookup.clone())
    }));
    let t0 = Instant::now();
    let clock = TestClock::new(t0);
    let engine = SessionEngine::with_clock(Arc::new(evidence), None, clock.clock());
    let runtime = crate::drive::DriveRuntime::default();
    runtime.set_open(vec!["proj".to_string()]);
    let env = Arc::new(crate::drive::DriveEnv {
        project: config.project.name.clone(),
        config: config.clone(),
        db_path,
        backend: fake.clone(),
        registry: crate::transfer_registry::TransferRegistry::new(),
        runtime,
        host: Arc::new(host),
        timeouts: crate::drive::Timeouts {
            max_age_secs: 600,
            reset: Duration::from_millis(300),
            swap_confirm: Duration::from_millis(300),
            swap_run: Duration::from_millis(300),
            send_settle: crate::drive::SEND_SETTLE_TIMEOUT,
        },
        engine: engine.clone(),
    });
    assert_eq!(env.project, "proj");
    ReproRig {
        _dir: dir,
        config,
        fake,
        engine,
        clock,
        t0,
        t0_ms: keitora_tasks::now_ms(),
        env,
    }
}

impl ReproRig {
    fn store(&self) -> keitora_tasks::LocalStore {
        keitora_tasks::LocalStore::open(&self.env.db_path).unwrap()
    }

    /// One status-loop tick at `ms`: every live key with its agent entry, as herdr
    /// lists them.
    fn tick(&self, ms: u64) {
        use crate::backend::PtyBackend as _;
        let at = self.t0 + Duration::from_millis(ms);
        self.clock.set(at);
        let mut keys: Vec<SessionKey> = self.fake.live_set_snapshot().into_iter().collect();
        keys.sort_by(|a, b| a.worktree.cmp(&b.worktree));
        let keys = keys
            .into_iter()
            .map(|k| {
                let o = self.fake.agent_snapshot(&k).unwrap();
                (k, o)
            })
            .collect();
        self.engine.ingest(HerdrSnapshot {
            at,
            at_ms: self.t0_ms + ms as i64,
            generation: Generation(1),
            keys,
        });
    }

    /// Run one drive pass and finish its jobs.
    fn pass(&self) {
        let gate = crate::dispatch::DispatchGate::default();
        let jobs = tauri::async_runtime::block_on(crate::drive::run_pass(
            &self.env,
            &gate,
            crate::drive::Trigger::Watcher,
        ));
        for job in jobs {
            tauri::async_runtime::block_on(job);
        }
    }

    /// `worktree`'s state as `sessions_list` reports it (`offline` without a row).
    fn listed_state(&self, worktree: &str) -> String {
        crate::tasks::reconciled_sessions(&self.engine.views("proj"), "proj", &self.config)
            .unwrap()
            .into_iter()
            .find(|s| s.worktree == worktree)
            .map_or_else(|| "offline".to_string(), |s| s.state)
    }
}

fn repro_key(w: &str) -> SessionKey {
    SessionKey {
        project: "proj".into(),
        worktree: w.into(),
    }
}

#[test]
fn repro_a1_agy_live_without_a_row_reads_idle_and_send_not_refused() {
    let r = repro_rig(ReproHost(HashMap::new()));
    r.store().drive_bind("bravo", "alpha", None, false).unwrap();
    // 1. agy runs idle in alpha's pane.
    r.fake.insert_live(repro_key("alpha"));
    r.fake.set_agent(
        repro_key("alpha"),
        Some(AgentObs {
            agent: Some("agy".into()),
            agent_session_source: Some("herdr:antigravity_cli".into()),
            ..obs("w1:p1", "idle", 2, None)
        }),
    );
    // 2. no worktree_sessions row.
    assert!(r.store().sessions_list().unwrap().is_empty());
    // 3. two ticks a second apart: settled.
    r.tick(0);
    r.tick(1_000);
    // 4. sessions_list, then send --no-wake.
    assert_eq!(r.listed_state("alpha"), "idle", "sessions_list");
    let id = r
        .store()
        .drive_request_add(
            "send",
            "alpha",
            "bravo",
            &serde_json::json!({"text": "x"}),
            false,
        )
        .unwrap()
        .id;
    r.pass();
    let req = r.store().drive_request_get(id).unwrap().unwrap();
    assert_eq!(req.status, "applied", "send refused: {}", req.reason);
}

#[test]
fn repro_a2_reset_to_codex_then_late_session_end_keeps_send_and_wake_in_step() {
    let r = repro_rig(ReproHost(HashMap::from([(
        "bravo".to_string(),
        CliKind::Codex,
    )])));
    r.store().drive_bind("alpha", "bravo", None, false).unwrap();
    // 1. bravo's row reads offline.
    r.store()
        .session_upsert("bravo", "offline", None, None)
        .unwrap();
    // 2. codex runs idle in bravo's pane, as after `reset --to codex`.
    r.fake.insert_live(repro_key("bravo"));
    r.fake.set_agent(
        repro_key("bravo"),
        Some(AgentObs {
            agent: Some("codex".into()),
            agent_session_source: Some("herdr:codex".into()),
            ..obs("w1:p2", "idle", 4, None)
        }),
    );
    // 3. two ticks a second apart.
    r.tick(0);
    r.tick(1_000);
    // 4. the old session's SessionEnd lands late.
    r.store()
        .session_upsert("bravo", "offline", None, None)
        .unwrap();
    // 5. wake, then send --no-wake: both must agree the agent is up.
    let wake = r
        .store()
        .drive_request_add("wake", "bravo", "alpha", &serde_json::json!({}), false)
        .unwrap()
        .id;
    r.pass();
    let w = r.store().drive_request_get(wake).unwrap().unwrap();
    assert_eq!(w.status, "applied", "wake refused: {}", w.reason);
    let woke: serde_json::Value = serde_json::from_str(&w.result).unwrap();
    assert_eq!(woke["already_awake"], serde_json::json!(true), "wake");
    let send = r
        .store()
        .drive_request_add(
            "send",
            "bravo",
            "alpha",
            &serde_json::json!({"text": "x"}),
            false,
        )
        .unwrap()
        .id;
    r.pass();
    let s = r.store().drive_request_get(send).unwrap().unwrap();
    assert_eq!(s.status, "applied", "send refused: {}", s.reason);
}

#[test]
fn stale_writer_row_from_a_previous_session_is_ignored_end_to_end() {
    let r = repro_rig(ReproHost(HashMap::new()));
    // A new agent session runs idle in alpha's pane.
    r.fake.insert_live(repro_key("alpha"));
    r.fake.set_agent(
        repro_key("alpha"),
        Some(AgentObs {
            agent_session: Some("s-new".into()),
            ..obs("w1:p1", "idle", 2, None)
        }),
    );
    r.tick(0);
    r.tick(1_000);
    assert_eq!(r.listed_state("alpha"), "idle");
    // The killed session's hook lands late, after herdr's last change.
    std::thread::sleep(Duration::from_millis(1_100));
    r.store()
        .session_upsert_with_id("alpha", "busy", None, None, Some("s-old"))
        .unwrap();
    let late_ms = keitora_tasks::now_ms() - r.t0_ms + 1_000;
    r.tick(late_ms as u64);
    assert_eq!(
        r.listed_state("alpha"),
        "idle",
        "a row from a previous session is not evidence"
    );
    // The same write from the live session does veto the idle.
    r.store()
        .session_upsert_with_id("alpha", "busy", None, None, Some("s-new"))
        .unwrap();
    r.tick(late_ms as u64 + 1_000);
    assert_eq!(
        r.listed_state("alpha"),
        "busy",
        "the live session's row counts"
    );
}

// ── Turn tracking ───────────────────────────────────────────────────────────
//
// The engine over a temp `tasks.db` (read through the real evidence, written as the
// turn store), the real drive runner delivering the prompt, a fake backend and a
// fake clock. `bravo` drives `alpha`; alpha's pane is `w1:p1`.

use super::turns::{TranscriptSource, TurnKind, TurnReply};
use keitora_tasks::DriveTurn;

/// A transcript source that answers every lookup with `reply` (or misses).
struct FixedTranscript(Result<&'static str, &'static str>);

impl TranscriptSource for FixedTranscript {
    fn reply(&self, _q: &super::turns::ReplyQuery) -> Result<TurnReply, &'static str> {
        self.0.map(|t| TurnReply { text: t.into() })
    }
}

fn agent(label: &str, status: &str, scs: u64, cs: Option<u64>) -> AgentObs {
    AgentObs {
        agent: Some(label.into()),
        agent_session_source: Some(format!("herdr:{label}")),
        ..obs("w1:p1", status, scs, cs)
    }
}

impl ReproRig {
    /// Set alpha's herdr entry.
    fn herdr(&self, o: AgentObs) {
        self.fake.set_agent(repro_key("alpha"), Some(o));
    }

    /// File a send from bravo to alpha and run a pass; returns the delivery id.
    fn deliver(&self, text: &str) -> i64 {
        let id = self
            .store()
            .drive_request_add(
                "send",
                "alpha",
                "bravo",
                &serde_json::json!({ "text": text }),
                false,
            )
            .unwrap()
            .id;
        self.pass();
        let req = self.store().drive_request_get(id).unwrap().unwrap();
        assert_eq!(req.status, "applied", "send refused: {}", req.reason);
        let result: serde_json::Value = serde_json::from_str(&req.result).unwrap();
        result["delivery_id"].as_i64().unwrap()
    }

    fn turns(&self) -> Vec<DriveTurn> {
        self.store().drive_turns_unacked("alpha").unwrap()
    }
}

/// A rig where bravo drives alpha, alpha runs `label` idle at `scs`/`cs` (settled at
/// 1 s), and lookups run inline.
fn turn_rig(label: &str, scs: u64, cs: Option<u64>) -> ReproRig {
    let r = repro_rig(ReproHost(HashMap::new()));
    r.engine.set_spawner(Arc::new(|job| job()));
    r.store().drive_bind("bravo", "alpha", None, false).unwrap();
    r.fake.insert_live(repro_key("alpha"));
    r.herdr(agent(label, "idle", scs, cs));
    r.tick(0);
    r.tick(1_000);
    r
}

/// alpha works on a delivered prompt (from 2 s), then idles at `scs`/`cs` from 3 s;
/// the idle settles at 4 s.
fn work_then_idle(r: &ReproRig, label: &str, scs: u64, cs: Option<u64>) {
    r.herdr(agent(label, "working", scs - 1, None));
    r.tick(2_000);
    r.herdr(agent(label, "idle", scs, cs));
    r.tick(3_000);
    r.tick(4_000);
}

#[test]
fn codex_delivery_without_stop_finishes_on_completion_seq() {
    let r = turn_rig("codex", 11, Some(4));
    let delivery = r.deliver("fix it");
    let b = r
        .store()
        .drive_delivery_get("alpha")
        .unwrap()
        .unwrap()
        .baseline
        .unwrap();
    assert_eq!((b.state_change_seq, b.completion_seq), (11, Some(4)));
    // Idle with a new stamp, settled at 4 s; nothing yet (waiting for a Stop reply).
    work_then_idle(&r, "codex", 13, Some(5));
    assert!(r.turns().is_empty(), "waits up to 3 s for the hook");
    r.tick(5_000);
    r.tick(6_000);
    assert!(r.turns().is_empty());
    r.tick(7_000);
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let t = &rows[0];
    assert_eq!(
        (t.kind.as_str(), t.delivery_id, t.outcome.as_str()),
        ("driver", Some(delivery), "finished")
    );
    assert_eq!(
        (t.signal.as_str(), t.clause.as_str()),
        ("herdr", "completion_seq")
    );
    assert_eq!((t.reply.as_str(), t.reply_source.as_str()), ("", "none"));
    assert_eq!(t.caveat, None);
    // No second row on later ticks.
    r.tick(8_000);
    r.tick(9_000);
    assert_eq!(r.turns().len(), 1);
}

#[test]
fn herdr_completion_waits_3s_for_the_stop_reply() {
    let r = turn_rig("codex", 11, Some(4));
    r.deliver("fix it");
    work_then_idle(&r, "codex", 13, Some(5));
    r.tick(5_000);
    r.store()
        .session_reply_record("alpha", "all fixed")
        .unwrap();
    r.tick(6_000);
    let rows = r.turns();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].reply.as_str(), rows[0].reply_source.as_str()),
        ("all fixed", "hook")
    );
    assert_eq!(rows[0].signal, "herdr");
    assert_eq!(rows[0].clause, "completion_seq");
}

#[test]
fn stop_hook_first_wins_with_signal_hook() {
    let r = turn_rig("claude", 11, Some(4));
    let delivery = r.deliver("fix it");
    r.herdr(agent("claude", "working", 12, None));
    r.tick(2_000);
    // The Stop hook lands while herdr still reads working.
    r.store()
        .session_reply_record("alpha", "done early")
        .unwrap();
    r.tick(3_000);
    let rows = r.turns();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].signal.as_str(), rows[0].clause.as_str()),
        ("hook", "stop_hook")
    );
    assert_eq!(rows[0].delivery_id, Some(delivery));
    assert_eq!(
        (rows[0].reply.as_str(), rows[0].reply_source.as_str()),
        ("done early", "hook")
    );
    // herdr's completion comes later: neither a second driver row nor a human one.
    r.herdr(agent("claude", "idle", 13, Some(5)));
    for ms in [4_000, 5_000, 6_000, 7_000, 8_000, 9_000] {
        r.tick(ms);
    }
    assert_eq!(r.turns().len(), 1, "{:?}", r.turns());
}

#[test]
fn persisted_baseline_resolves_after_an_app_restart() {
    let r = turn_rig("codex", 11, Some(4));
    let delivery = r.deliver("fix it");
    // The app restarts: a fresh engine over the same DB, nothing in memory.
    let db = r.env.db_path.clone();
    let lookup = (db, r.config.worktrees());
    let evidence = super::evidence::RegistryEvidence::with_lookup(Box::new(move |p| {
        (p == "proj").then(|| lookup.clone())
    }));
    let fresh = SessionEngine::with_clock(Arc::new(evidence), None, r.clock.clock());
    // The turn finished while the app was down.
    r.herdr(agent("codex", "idle", 13, Some(5)));
    for ms in [10_000u64, 11_000, 12_000, 13_000, 14_000] {
        let at = r.t0 + Duration::from_millis(ms);
        r.clock.set(at);
        fresh.ingest(HerdrSnapshot {
            at,
            at_ms: r.t0_ms + ms as i64,
            generation: Generation(1),
            keys: vec![(
                repro_key("alpha"),
                Some(agent("codex", "idle", 13, Some(5))),
            )],
        });
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].delivery_id, Some(delivery));
    assert_eq!(rows[0].clause, "completion_seq");
}

#[test]
fn agy_herdr_only_completion_carries_the_caveat() {
    let r = turn_rig("agy", 11, None);
    r.deliver("fix it");
    // agy goes done with no stamp (back-to-back prompts): the seq clause decides.
    work_then_idle(&r, "agy", 13, None);
    r.herdr(agent("agy", "done", 13, None));
    for ms in [5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].clause, "state_change_seq");
    assert_eq!(rows[0].caveat.as_deref(), Some(turns::AGY_CAVEAT));
}

#[test]
fn agy_unmapped_session_yields_none_with_read_hint() {
    // The real reader over empty temp dirs: herdr reported no agy session id.
    let home = tempfile::tempdir().unwrap();
    let r = turn_rig("agy", 11, None);
    r.engine
        .set_transcript_source(Arc::new(crate::transcript::AgentTranscripts::new(
            crate::transcript::TranscriptDirs {
                claude_projects: None,
                codex_home: None,
                agy_conversations: Some(home.path().join("conversations")),
            },
            Box::new(|_| None),
        )));
    r.deliver("fix it");
    work_then_idle(&r, "agy", 13, None);
    r.herdr(agent("agy", "done", 13, None));
    for ms in [5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    // `none` is what makes `wait` print the `keitora read` hint.
    assert_eq!(
        (rows[0].reply.as_str(), rows[0].reply_source.as_str()),
        ("", "none")
    );
    assert_eq!(rows[0].caveat.as_deref(), Some(turns::AGY_CAVEAT));
    let h = r.engine.turn_health();
    assert_eq!(h[0].1.recent[0].transcript_miss, Some("not_found"));
}

#[test]
fn human_turn_when_nothing_is_outstanding() {
    let r = turn_rig("codex", 11, Some(4));
    work_then_idle(&r, "codex", 13, Some(5));
    for ms in [5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].kind.as_str(), rows[0].delivery_id),
        ("human", None)
    );
    assert_eq!(rows[0].clause, "completion_seq");
    assert_eq!(rows[0].reply_source, "none");
}

#[test]
fn second_completion_after_the_driver_turn_is_human() {
    let r = turn_rig("codex", 11, Some(4));
    r.deliver("fix it");
    work_then_idle(&r, "codex", 13, Some(5));
    for ms in [5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    assert_eq!(r.turns().len(), 1);
    // A message the human queued mid-turn runs next.
    r.herdr(agent("codex", "working", 14, None));
    r.tick(8_000);
    r.herdr(agent("codex", "idle", 15, Some(6)));
    for ms in [9_000, 10_000, 11_000, 12_000, 13_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    let kinds: Vec<&str> = rows.iter().map(|t| t.kind.as_str()).collect();
    assert_eq!(kinds, vec!["driver", "human"]);
}

#[test]
fn completion_while_the_send_is_queued_undelivered_is_human() {
    let r = turn_rig("codex", 11, Some(4));
    r.herdr(agent("codex", "working", 12, None));
    r.tick(2_000);
    // The driver queues a send; the agent is busy, so it waits undelivered.
    let id = r
        .store()
        .drive_request_add(
            "send",
            "alpha",
            "bravo",
            &serde_json::json!({ "text": "next" }),
            true,
        )
        .unwrap()
        .id;
    r.pass();
    assert_eq!(
        r.store().drive_request_get(id).unwrap().unwrap().status,
        "pending"
    );
    r.herdr(agent("codex", "idle", 13, Some(5)));
    for ms in [3_000, 4_000, 5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, "human");
}

#[test]
fn no_events_for_completions_before_the_first_snapshot() {
    // alpha finished turns before the engine's first look: idle with a stamp.
    let r = turn_rig("codex", 21, Some(20));
    for ms in [2_000, 3_000, 4_000, 5_000, 6_000] {
        r.tick(ms);
    }
    assert!(r.turns().is_empty());
}

#[test]
fn undriven_worktree_records_nothing() {
    let r = turn_rig("codex", 11, Some(4));
    r.store().drive_release("bravo", "alpha").unwrap();
    work_then_idle(&r, "codex", 13, Some(5));
    for ms in [5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    assert!(r.store().drive_turn_latest("alpha").unwrap().is_none());
    assert!(r.engine.turn_health().is_empty());
}

#[test]
fn standby_instance_writes_no_turns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("status.db");
    let id = |n: &str| keitora_tasks::WriterIdentity {
        instance_id: n.into(),
        pid: 7,
        version: "0.0.0-test".into(),
        started_at: keitora_tasks::now_ms(),
    };
    let _holder = StatusPublisher::open(path.clone(), id("a"), keitora_tasks::now_ms());
    let standby = StatusPublisher::open(path, id("b"), keitora_tasks::now_ms());
    assert!(matches!(standby.lease(), LeaseState::Standby { .. }));

    let r = repro_rig(ReproHost(HashMap::new()));
    let db = r.env.db_path.clone();
    let lookup = (db, r.config.worktrees());
    let evidence = super::evidence::RegistryEvidence::with_lookup(Box::new(move |p| {
        (p == "proj").then(|| lookup.clone())
    }));
    let engine = SessionEngine::with_clock(Arc::new(evidence), Some(standby), r.clock.clock());
    r.store().drive_bind("bravo", "alpha", None, false).unwrap();
    let ingest = |ms: u64, o: AgentObs| {
        let at = r.t0 + Duration::from_millis(ms);
        r.clock.set(at);
        engine.ingest(HerdrSnapshot {
            at,
            at_ms: r.t0_ms + ms as i64,
            generation: Generation(1),
            keys: vec![(repro_key("alpha"), Some(o))],
        });
    };
    ingest(0, agent("codex", "idle", 11, Some(4)));
    ingest(1_000, agent("codex", "idle", 11, Some(4)));
    ingest(2_000, agent("codex", "working", 12, None));
    for ms in [3_000, 4_000, 5_000, 6_000, 7_000, 8_000] {
        ingest(ms, agent("codex", "idle", 13, Some(5)));
    }
    assert!(r.store().drive_turn_latest("alpha").unwrap().is_none());
    assert!(engine.turn_health().is_empty(), "a standby tracks no turns");
}

#[test]
fn occupant_change_writes_a_restarted_row() {
    let r = turn_rig("codex", 11, Some(4));
    let delivery = r.deliver("fix it");
    // /clear or a resume: a new agent session in the same pane.
    r.herdr(AgentObs {
        agent_session: Some("s-2".into()),
        ..agent("codex", "idle", 12, None)
    });
    r.tick(2_000);
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].outcome.as_str(), rows[0].clause.as_str()),
        ("restarted", "occupant_changed")
    );
    assert_eq!(rows[0].delivery_id, Some(delivery));
    assert_eq!(rows[0].reply_source, "none");
    // The new occupant's first settled idle is no human turn.
    for ms in [3_000, 4_000, 5_000] {
        r.tick(ms);
    }
    assert_eq!(r.turns().len(), 1);
}

#[test]
fn a_new_agent_session_starts_a_new_ledger() {
    let r = turn_rig("codex", 11, Some(4));
    // /clear or a resume with nothing outstanding: the new session's first settled
    // idle is no human turn, however its seqs moved.
    let s2 = |status: &str, scs: u64, cs: Option<u64>| AgentObs {
        agent_session: Some("s-2".into()),
        ..agent("codex", status, scs, cs)
    };
    r.herdr(s2("idle", 13, Some(5)));
    for ms in [2_000, 3_000, 4_000, 5_000, 6_000] {
        r.tick(ms);
    }
    assert!(r.turns().is_empty(), "{:?}", r.turns());
    // Its own next completion counts.
    r.herdr(s2("working", 14, None));
    r.tick(7_000);
    r.herdr(s2("idle", 15, Some(6)));
    for ms in [8_000, 9_000, 10_000, 11_000, 12_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].kind, "human");
}

#[test]
fn no_change_no_turn_writes() {
    let r = turn_rig("codex", 11, Some(4));
    let ro = keitora_tasks::LocalStore::open_read_only(&r.env.db_path).unwrap();
    let before = ro.data_version().unwrap();
    for n in 0..10 {
        r.tick(2_000 + n * 1_000);
    }
    assert_eq!(ro.data_version().unwrap(), before, "nothing written");
    assert!(r.turns().is_empty());
}

#[test]
fn transcript_reply_used_after_1s_when_no_hook() {
    let r = turn_rig("codex", 11, Some(4));
    r.engine
        .set_transcript_source(Arc::new(FixedTranscript(Ok("from the rollout"))));
    r.deliver("fix it");
    work_then_idle(&r, "codex", 13, Some(5));
    r.tick(5_000); // 1 s after detection: the lookup runs (inline here).
    r.tick(6_000); // its result is used.
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].reply.as_str(), rows[0].reply_source.as_str()),
        ("from the rollout", "transcript")
    );
    assert!(
        r.store().session_reply_get("alpha").unwrap().is_none(),
        "the engine never writes session_replies"
    );
    let h = r.engine.turn_health();
    assert_eq!(h[0].1.recent[0].reply_source, "transcript");

    // A hook reply on the same ingest as the transcript result wins.
    let r = turn_rig("codex", 11, Some(4));
    r.engine
        .set_transcript_source(Arc::new(FixedTranscript(Ok("from the rollout"))));
    r.deliver("fix it");
    work_then_idle(&r, "codex", 13, Some(5));
    r.tick(5_000);
    r.store()
        .session_reply_record("alpha", "from the hook")
        .unwrap();
    r.tick(6_000);
    let rows = r.turns();
    assert_eq!(
        (rows[0].reply.as_str(), rows[0].reply_source.as_str()),
        ("from the hook", "hook")
    );
}

#[test]
fn codex_delivery_without_stop_takes_its_reply_from_the_rollout() {
    let codex_home = tempfile::tempdir().unwrap();
    let r = turn_rig("codex", 11, Some(4));
    let source = crate::transcript::AgentTranscripts::new(
        crate::transcript::TranscriptDirs {
            claude_projects: None,
            codex_home: Some(codex_home.path().to_path_buf()),
            agy_conversations: None,
        },
        Box::new(|_| Some(std::path::PathBuf::from("/Users/me/proj/alpha"))),
    );
    r.engine.set_transcript_source(Arc::new(source));
    let session = |o: AgentObs| AgentObs {
        agent_session: Some("t-e2e".into()),
        agent_session_kind: Some("id".into()),
        ..o
    };
    r.herdr(session(agent("codex", "idle", 11, Some(4))));
    r.tick(1_500);
    r.deliver("fix it");
    let delivered_at = r
        .store()
        .drive_delivery_get("alpha")
        .unwrap()
        .unwrap()
        .baseline
        .unwrap()
        .delivered_at_ms;
    // The rollout Codex wrote for the delivery's turn, in its dated dir.
    let at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(delivered_at + 500).unwrap();
    let ts = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let dir = codex_home
        .path()
        .join("sessions")
        .join(at.format("%Y/%m/%d").to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let rollout = [
        serde_json::json!({"timestamp": ts, "type": "session_meta", "payload": {"id": "t-e2e"}}),
        serde_json::json!({"timestamp": ts, "type": "event_msg", "payload": {"type": "task_started"}}),
        serde_json::json!({"timestamp": ts, "type": "event_msg",
            "payload": {"type": "task_complete", "last_agent_message": "fixed"}}),
    ]
    .map(|v| v.to_string())
    .join("\n");
    std::fs::write(dir.join("rollout-x-t-e2e.jsonl"), rollout).unwrap();

    r.herdr(session(agent("codex", "working", 12, None)));
    r.tick(2_000);
    r.herdr(session(agent("codex", "idle", 13, Some(5))));
    r.tick(3_000);
    r.tick(4_000);
    r.tick(5_000);
    r.tick(6_000);
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].reply.as_str(), rows[0].reply_source.as_str()),
        ("fixed", "transcript")
    );
    // `read --last` reads the latest turn row.
    let last = r.store().drive_turn_latest("alpha").unwrap().unwrap();
    assert_eq!(
        (last.reply.as_str(), last.reply_source.as_str()),
        ("fixed", "transcript")
    );
    assert!(r.store().session_reply_get("alpha").unwrap().is_none());
}

#[test]
fn a_transcript_miss_is_recorded_for_the_health_monitor() {
    let r = turn_rig("codex", 11, Some(4));
    r.deliver("fix it");
    work_then_idle(&r, "codex", 13, Some(5));
    for ms in [5_000, 6_000, 7_000] {
        r.tick(ms);
    }
    let h = r.engine.turn_health();
    let note = &h[0].1.recent[0];
    assert_eq!(note.kind, TurnKind::Driver);
    assert_eq!(
        (note.reply_source, note.transcript_miss),
        ("none", Some("no_provider_support"))
    );
}

#[test]
fn turn_health_records_completions_and_no_activity() {
    let r = turn_rig("codex", 11, Some(4));
    let delivery = r.deliver("fix it");
    // Nothing happens for 30 s after the delivery.
    for s in 2..=29u64 {
        r.tick(s * 1_000);
    }
    assert_eq!(r.engine.turn_health()[0].1.no_activity, None);
    r.tick(33_000);
    let (id, _) = r.engine.turn_health()[0].1.no_activity.expect("recorded");
    assert_eq!(id, delivery);
    // Working clears it.
    r.herdr(agent("codex", "working", 12, None));
    r.tick(34_000);
    assert_eq!(r.engine.turn_health()[0].1.no_activity, None);

    // The ring keeps the last ten completions.
    let mut scs = 12;
    let mut ms = 35_000;
    for n in 0..12u64 {
        scs += 2;
        r.herdr(agent("codex", "working", scs - 1, None));
        r.tick(ms);
        r.herdr(agent("codex", "idle", scs, Some(100 + n)));
        for _ in 0..5 {
            ms += 1_000;
            r.tick(ms);
        }
        ms += 1_000;
    }
    let h = r.engine.turn_health();
    assert_eq!(h[0].1.recent.len(), turns::RECENT_CAP);
    assert!(r.store().drive_turns_unacked("alpha").unwrap().len() >= 12);
}

#[test]
fn awaited_keys_poll_hot_until_the_turn_is_recorded() {
    let r = turn_rig("codex", 11, Some(4));
    assert_eq!(r.engine.mode(), Mode::Cold);
    r.deliver("fix it");
    r.tick(2_000);
    assert_eq!(r.engine.mode(), Mode::Hot, "a delivery is outstanding");
    work_then_idle(&r, "codex", 13, Some(5));
    r.tick(5_000);
    assert_eq!(r.engine.mode(), Mode::Hot, "a reply is pending");
    r.tick(7_000);
    r.tick(8_000);
    assert_eq!(r.engine.mode(), Mode::Cold);
}

#[test]
fn a_hook_resolved_turn_herdr_barely_saw_does_not_claim_the_human_turn() {
    let r = turn_rig("claude", 11, Some(4));
    let delivery = r.deliver("fix it");
    // herdr moves one step and drops its stamp; working is never sampled.
    r.herdr(agent("claude", "idle", 12, None));
    r.tick(2_000);
    r.store().session_reply_record("alpha", "done").unwrap();
    for ms in [3_000, 4_000, 5_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].delivery_id, rows[0].clause.as_str()),
        (Some(delivery), "stop_hook")
    );
    // The human's next turn, with a new stamp.
    r.herdr(agent("claude", "working", 13, None));
    r.tick(6_000);
    r.herdr(agent("claude", "idle", 14, Some(5)));
    for ms in [7_000, 8_000, 9_000, 10_000, 11_000, 12_000] {
        r.tick(ms);
    }
    let kinds: Vec<String> = r.turns().iter().map(|t| t.kind.clone()).collect();
    assert_eq!(kinds, vec!["driver", "human"]);
}

#[test]
fn a_hook_resolved_turn_herdr_never_saw_does_not_claim_the_human_turn() {
    let r = turn_rig("claude", 11, Some(4));
    r.deliver("fix it");
    // The Stop hook reports the turn; herdr's entry never moves.
    r.tick(2_000);
    r.store().session_reply_record("alpha", "done").unwrap();
    for ms in [3_000, 4_000, 5_000] {
        r.tick(ms);
    }
    assert_eq!(r.turns().len(), 1);
    r.herdr(agent("claude", "working", 12, None));
    r.tick(6_000);
    r.herdr(agent("claude", "idle", 13, Some(5)));
    for ms in [7_000, 8_000, 9_000, 10_000, 11_000, 12_000] {
        r.tick(ms);
    }
    let kinds: Vec<String> = r.turns().iter().map(|t| t.kind.clone()).collect();
    assert_eq!(kinds, vec!["driver", "human"]);
}

/// Make every `drive_turns` insert into `r`'s DB fail (`true`) or work again.
fn fail_turn_writes(r: &ReproRig, fail: bool) {
    let conn = rusqlite::Connection::open(&r.env.db_path).unwrap();
    let sql = if fail {
        "CREATE TRIGGER fail_turns BEFORE INSERT ON drive_turns \
         BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END;"
    } else {
        "DROP TRIGGER fail_turns;"
    };
    conn.execute_batch(sql).unwrap();
}

#[test]
fn a_failed_driver_turn_write_is_retried() {
    let r = turn_rig("codex", 11, None);
    let delivery = r.deliver("fix it");
    // Only the observed activity finishes this turn (one seq step, no stamp).
    r.herdr(agent("codex", "working", 12, None));
    r.tick(2_000);
    r.herdr(agent("codex", "idle", 12, None));
    for ms in [3_000, 4_000, 5_000, 6_000] {
        r.tick(ms);
    }
    fail_turn_writes(&r, true);
    r.tick(7_000);
    fail_turn_writes(&r, false);
    assert!(r.turns().is_empty());
    for ms in [8_000, 9_000, 10_000, 11_000, 12_000] {
        r.tick(ms);
    }
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (
            rows[0].kind.as_str(),
            rows[0].delivery_id,
            rows[0].clause.as_str()
        ),
        ("driver", Some(delivery), "observed_active")
    );
}

#[test]
fn a_failed_human_turn_write_is_retried() {
    let r = turn_rig("codex", 11, Some(4));
    work_then_idle(&r, "codex", 13, Some(5));
    r.tick(5_000);
    r.tick(6_000);
    fail_turn_writes(&r, true);
    r.tick(7_000);
    r.tick(8_000);
    fail_turn_writes(&r, false);
    assert!(r.turns().is_empty());
    r.tick(9_000);
    let rows = r.turns();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].kind.as_str(), rows[0].delivery_id),
        ("human", None)
    );
    r.tick(10_000);
    assert_eq!(r.turns().len(), 1, "written once");
}
