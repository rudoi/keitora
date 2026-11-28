//! The session engine: one answer to "is an agent running in this worktree, and is it
//! idle, busy or waiting?" for every consumer.
//!
//! Split into a pure [`derive::derive`] (one herdr `agent list` entry plus hook-row
//! evidence -> one [`model::View`] per worktree; no I/O, no clock reads) and a thin
//! driver in the herdr status loop that feeds it snapshots ([`SessionEngine::ingest`]).
//! Every change is published to `status.db` (through [`writer::StatusPublisher`]) and
//! handed to the in-process subscribers. For driven worktrees the same snapshots
//! decide when a delivered turn finished and record it ([`turns`]).

pub mod derive;
pub mod evidence;
pub mod gate;
pub mod model;
pub mod turns;
pub mod writer;

#[cfg(test)]
#[path = "tests.rs"]
pub(crate) mod tests;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use keitora_tasks::StatusSession;

use crate::backend::herdr::ops::Generation;
use crate::backend::{PtyBackend, SessionKey};

use derive::{derive, DeriveInput};
use model::{AgentObs, KeyMemory, ViewQuery};
use turns::{
    NoTranscript, StepOut, TranscriptSource, TurnHealth, TurnInput, TurnInsert, TurnMemory,
    TurnReply,
};
use writer::{LeaseState, StatusPublisher};

pub use model::ProjectEvidence;

/// How long an `idle`/`done` status must hold, at an unchanged `state_change_seq`,
/// across two snapshots before the view counts as settled.
pub const SETTLE: Duration = Duration::from_secs(1);

/// How long a launch Keitora noted reads as `Starting` while herdr has no agent
/// entry for the pane yet.
pub const LAUNCH_GRACE: Duration = Duration::from_secs(60);

/// `agent list` cadence while something is in flight.
pub const HOT: Duration = Duration::from_secs(1);
/// [`HOT`] under low memory headroom.
pub const HOT_LOW: Duration = Duration::from_secs(2);
/// Cadence while the window shows projects and nothing is in flight.
pub const WARM: Duration = Duration::from_secs(5);
/// [`WARM`] under low memory headroom.
pub const WARM_LOW: Duration = Duration::from_secs(15);
/// Cadence otherwise.
pub const COLD: Duration = Duration::from_secs(15);
/// [`COLD`] under low memory headroom.
pub const COLD_LOW: Duration = Duration::from_secs(30);
/// A snapshot older than this many intervals makes every view stale.
pub const STALE_FACTOR: u32 = 3;
/// How long a herdr `unknown` status lasts before the health monitor reports it.
pub const UNKNOWN_FINDING_AFTER: Duration = Duration::from_secs(30);

/// Read-side evidence the engine combines with herdr's snapshot: each project's hook
/// rows and configured worktrees.
pub trait Evidence: Send + Sync {
    /// The project's hook rows and configured worktrees, read through a cached
    /// read-only handle. `None` when the project is unknown or its DB can't be read.
    fn project(&self, project: &str) -> Option<ProjectEvidence>;
}

/// One `agent list` call matched to the keymap.
#[derive(Clone, Debug)]
pub struct HerdrSnapshot {
    pub at: Instant,
    pub at_ms: i64,
    pub generation: Generation,
    /// One entry per current-generation keymap key (reserved keys excluded), with its
    /// matched agent-list entry (`None`: no entry for that pane id).
    pub keys: Vec<(SessionKey, Option<AgentObs>)>,
}

/// How the engine's link to herdr stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HerdrLink {
    /// The server is not up yet, or the launch decision is still pending.
    Starting,
    /// Snapshots arrive.
    Up,
    /// Recovery owns the server.
    Recovering,
    /// The server is stopped or not running.
    Down,
    /// No herdr backend at all.
    Unavailable,
}

impl HerdrLink {
    /// The `status.db` spelling (one of `keitora_tasks::HERDR_LINKS`).
    pub fn as_str(self) -> &'static str {
        match self {
            HerdrLink::Starting => "starting",
            HerdrLink::Up => "up",
            HerdrLink::Recovering => "recovering",
            HerdrLink::Down => "down",
            HerdrLink::Unavailable => "unavailable",
        }
    }
}

/// How often the engine polls `agent list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Hot,
    Warm,
    Cold,
}

/// The `agent list` interval of `mode`, slower under low headroom.
pub fn interval(mode: Mode, low: bool) -> Duration {
    match (mode, low) {
        (Mode::Hot, false) => HOT,
        (Mode::Hot, true) => HOT_LOW,
        (Mode::Warm, false) => WARM,
        (Mode::Warm, true) => WARM_LOW,
        (Mode::Cold, false) => COLD,
        (Mode::Cold, true) => COLD_LOW,
    }
}

/// Keys whose agent runs (present or starting, per `views`), plus every key of
/// `panes` while the engine is not `ready`: before the first snapshot a pane may hold
/// an agent, and a quit prompt or an update must never under-count. Sorted, without
/// duplicates or reserved keys.
pub fn running_agent_keys(
    views: &[(SessionKey, StatusSession)],
    ready: bool,
    panes: impl IntoIterator<Item = SessionKey>,
) -> Vec<SessionKey> {
    let mut keys: Vec<SessionKey> = views
        .iter()
        .filter(|(_, v)| v.is_running())
        .map(|(k, _)| k.clone())
        .collect();
    if !ready {
        keys.extend(panes);
    }
    keys.retain(|k| !k.is_reserved());
    keys.sort_by(|a, b| (&a.project, &a.worktree).cmp(&(&b.project, &b.worktree)));
    keys.dedup();
    keys
}

/// One key's view, as a consumer asks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // short-lived answers, never stored in bulk
pub enum KeyView {
    /// No snapshot has been taken yet.
    NotReady,
    /// No keymap key: never launched in this server.
    Missing,
    View(StatusSession),
}

/// Every view of one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectViews {
    /// A snapshot exists and the link is up.
    pub ready: bool,
    pub link: HerdrLink,
    /// Keyed by worktree.
    pub views: HashMap<String, StatusSession>,
}

/// One view that changed (`None`: no view on that side).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewChange {
    pub key: SessionKey,
    pub prev: Option<StatusSession>,
    pub next: Option<StatusSession>,
}

/// Receives every batch of view changes, on the thread that produced them.
pub type Subscriber = Box<dyn Fn(&[ViewChange]) + Send + Sync>;

type SharedSubscriber = Arc<dyn Fn(&[ViewChange]) + Send + Sync>;

/// The link as the drive wait reasons and the health monitor read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkState {
    pub link: HerdrLink,
    pub since_ms: i64,
    pub last_snapshot_ms: Option<i64>,
    pub ready: bool,
}

/// Per-key facts the health monitor reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyHealth {
    pub present_since_ms: Option<i64>,
    pub unknown_since_ms: Option<i64>,
    pub pane_id: Option<String>,
}

/// The engine shared by the status loop, the backend and every consumer.
pub type SharedEngine = Arc<SessionEngine>;

/// A monotonic clock, injectable for tests.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Runs a blocking job off the status loop (transcript lookups).
pub type Spawner = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// Finished transcript lookups, by the token of the pending reply that asked.
type LookupResults = Arc<Mutex<HashMap<u64, Result<TurnReply, &'static str>>>>;

struct EngineState {
    link: HerdrLink,
    link_since_ms: i64,
    /// `(mono, wall ms)` of the last successful `agent list`.
    last_snapshot: Option<(Instant, i64)>,
    /// The `agent list` interval in effect when the last snapshot was taken: its
    /// staleness is judged against at least this, so a switch to a faster mode
    /// doesn't age every view at once.
    snapshot_interval: Duration,
    memory: HashMap<SessionKey, KeyMemory>,
    /// Launches in flight: cleared once a snapshot shows the pane's agent (or its
    /// launch pending), by `note_launch_failed`, or after [`LAUNCH_GRACE`].
    launches: HashMap<SessionKey, Instant>,
    /// Projects with pending drive/dispatch requests.
    pending: HashSet<String>,
    /// Outstanding deliveries.
    awaited: HashSet<SessionKey>,
    window_open: bool,
    /// Low headroom, from the status loop.
    low: bool,
    last_poll: Option<Instant>,
    /// The rows as last handed out (with their `stale` flags): the base for change
    /// detection.
    published: HashMap<SessionKey, StatusSession>,
    /// Turn state of each driven key in the last snapshot.
    turns: HashMap<SessionKey, TurnMemory>,
    /// The last token given to a pending reply.
    next_token: u64,
    /// The link and readiness subscribers last heard of.
    announced: Option<(HerdrLink, bool)>,
}

impl EngineState {
    /// Whether the link or readiness moved since subscribers last heard (and record
    /// that they now have).
    fn take_link_edge(&mut self) -> bool {
        let now = (self.link, self.ready());
        let edge = self.announced != Some(now);
        self.announced = Some(now);
        edge
    }

    fn mode(&self) -> Mode {
        let in_flight = self.memory.values().any(|m| {
            m.view.is_starting() || (m.view.is_running() && m.view.is_idle() && !m.view.settled)
        });
        if !self.pending.is_empty()
            || !self.awaited.is_empty()
            || !self.launches.is_empty()
            || in_flight
        {
            Mode::Hot
        } else if self.window_open {
            Mode::Warm
        } else {
            Mode::Cold
        }
    }

    fn ready(&self) -> bool {
        self.last_snapshot.is_some() && self.link == HerdrLink::Up
    }

    /// Whether every view is stale at `now`: the link is not up, or the last
    /// snapshot is older than [`STALE_FACTOR`] intervals (the longer of the one in
    /// effect when it was taken and the current one).
    fn stale(&self, now: Instant) -> bool {
        if self.link != HerdrLink::Up {
            return true;
        }
        match self.last_snapshot {
            None => true,
            Some((at, _)) => {
                let every = interval(self.mode(), self.low).max(self.snapshot_interval);
                now.saturating_duration_since(at) > every * STALE_FACTOR
            }
        }
    }

    /// The current rows of the keys `keep` accepts, each with the `stale` flag at `now`.
    fn rows_where<'a>(
        &'a self,
        now: Instant,
        keep: impl Fn(&SessionKey) -> bool + 'a,
    ) -> impl Iterator<Item = (&'a SessionKey, StatusSession)> + 'a {
        let stale = self.stale(now);
        self.memory
            .iter()
            .filter(move |(k, _)| keep(k))
            .map(move |(k, m)| {
                let mut row = m.view.to_status(k);
                row.stale = stale;
                (k, row)
            })
    }

    fn rows(&self, now: Instant) -> HashMap<SessionKey, StatusSession> {
        self.rows_where(now, |_| true)
            .map(|(k, row)| (k.clone(), row))
            .collect()
    }

    fn prune_launches(&mut self, now: Instant) {
        self.launches
            .retain(|_, t| now.saturating_duration_since(*t) <= LAUNCH_GRACE);
    }

    /// Diff the current rows against the published ones, adopt them, and return the
    /// changes plus, when there are any, the full row set (for the publisher). Only
    /// changed rows are cloned for the changes; the full set is built only when
    /// something changed.
    fn take_changes(&mut self, now: Instant) -> (Vec<ViewChange>, Option<Vec<StatusSession>>) {
        let rows = self.rows(now);
        let mut old = std::mem::take(&mut self.published);
        let mut changes = Vec::new();
        for (key, next) in &rows {
            let prev = old.remove(key);
            if prev.as_ref() != Some(next) {
                changes.push(ViewChange {
                    key: key.clone(),
                    prev,
                    next: Some(next.clone()),
                });
            }
        }
        changes.extend(old.into_iter().map(|(key, prev)| ViewChange {
            key,
            prev: Some(prev),
            next: None,
        }));
        let all = (!changes.is_empty()).then(|| {
            changes.sort_by(|a, b| {
                (&a.key.project, &a.key.worktree).cmp(&(&b.key.project, &b.key.worktree))
            });
            let mut all: Vec<StatusSession> = rows.values().cloned().collect();
            all.sort_by(|a, b| (&a.project, &a.worktree).cmp(&(&b.project, &b.worktree)));
            all
        });
        self.published = rows;
        (changes, all)
    }
}

/// See the module docs.
pub struct SessionEngine {
    evidence: Arc<dyn Evidence>,
    publisher: Option<Arc<StatusPublisher>>,
    clock: Clock,
    state: Mutex<EngineState>,
    subscribers: Mutex<Vec<SharedSubscriber>>,
    /// The backend whose live panes stand in while the engine has no fresh answer.
    /// Weak: the backend holds the engine.
    panes: OnceLock<Weak<dyn PtyBackend>>,
    /// Where finished turns' replies are read from when the Stop hook gives none.
    transcripts: Mutex<Arc<dyn TranscriptSource>>,
    /// Runs transcript lookups.
    spawner: Mutex<Spawner>,
    lookups: LookupResults,
    /// Write handles of the `tasks.db`s turns were recorded in, by path.
    turn_stores: Mutex<HashMap<String, keitora_tasks::LocalStore>>,
}

impl SessionEngine {
    /// An engine over `evidence`, publishing to `publisher` when there is one.
    pub fn new(
        evidence: Arc<dyn Evidence>,
        publisher: Option<Arc<StatusPublisher>>,
    ) -> SharedEngine {
        Self::with_clock(evidence, publisher, Arc::new(Instant::now))
    }

    /// [`Self::new`] over an injected clock.
    pub fn with_clock(
        evidence: Arc<dyn Evidence>,
        publisher: Option<Arc<StatusPublisher>>,
        clock: Clock,
    ) -> SharedEngine {
        Arc::new(SessionEngine {
            evidence,
            publisher,
            clock,
            state: Mutex::new(EngineState {
                link: HerdrLink::Starting,
                link_since_ms: keitora_tasks::now_ms(),
                last_snapshot: None,
                snapshot_interval: Duration::ZERO,
                memory: HashMap::new(),
                launches: HashMap::new(),
                pending: HashSet::new(),
                awaited: HashSet::new(),
                window_open: false,
                low: false,
                last_poll: None,
                published: HashMap::new(),
                turns: HashMap::new(),
                next_token: 0,
                announced: None,
            }),
            subscribers: Mutex::new(Vec::new()),
            panes: OnceLock::new(),
            transcripts: Mutex::new(Arc::new(NoTranscript)),
            spawner: Mutex::new(Arc::new(|job| {
                tauri::async_runtime::spawn_blocking(job);
            })),
            lookups: Arc::default(),
            turn_stores: Mutex::new(HashMap::new()),
        })
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    // ── Driver side (the status loop) ───────────────────────────────────────

    /// Whether an `agent list` is due at `now`. Stores `low` for the cadence.
    pub fn due(&self, now: Instant, low: bool) -> bool {
        let mut st = self.state.lock().unwrap();
        st.low = low;
        st.prune_launches(now);
        let every = interval(st.mode(), low);
        st.last_poll
            .is_none_or(|l| now.saturating_duration_since(l) >= every)
    }

    /// Take one snapshot: derive every key's view, drop memory of keys that are gone,
    /// track driven keys' turns, publish the changes and hand them to the
    /// subscribers, then record finished turns and start reply lookups.
    pub fn ingest(&self, snap: HerdrSnapshot) {
        // Evidence is read before the lock: it touches each project's DB once.
        let mut projects: Vec<&str> = snap.keys.iter().map(|(k, _)| k.project.as_str()).collect();
        projects.sort_unstable();
        projects.dedup();
        let evidence: HashMap<&str, ProjectEvidence> = projects
            .into_iter()
            .filter_map(|p| self.evidence.project(p).map(|e| (p, e)))
            .collect();

        // Only the `status.db` lease holder (or an engine without a store) tracks and
        // records turns.
        let tracks_turns = self
            .publisher
            .as_ref()
            .is_none_or(|p| p.lease() == LeaseState::Owned);

        let (changes, rows, link, turn_io) = {
            let mut st = self.state.lock().unwrap();
            st.prune_launches(snap.at);
            st.snapshot_interval = interval(st.mode(), st.low);
            st.last_poll = Some(snap.at);
            st.last_snapshot = Some((snap.at, snap.at_ms));
            if st.link != HerdrLink::Up {
                st.link = HerdrLink::Up;
                st.link_since_ms = snap.at_ms;
            }
            // Deliveries are reloaded from evidence on every snapshot.
            st.awaited.clear();
            let mut memory = HashMap::with_capacity(snap.keys.len());
            for (key, obs) in &snap.keys {
                let hook = evidence
                    .get(key.project.as_str())
                    .and_then(|e| e.rows.get(&key.worktree));
                let derived = derive(&DeriveInput {
                    obs: obs.as_ref(),
                    hook,
                    prev: st.memory.get(key),
                    launch_noted: st.launches.get(key).copied(),
                    generation: snap.generation,
                    now: snap.at,
                    now_ms: snap.at_ms,
                });
                memory.insert(key.clone(), derived.memory);
                // herdr lists the launched agent (or its launch): the note is done.
                if obs
                    .as_ref()
                    .is_some_and(|o| o.agent.is_some() || o.launch_pending)
                {
                    st.launches.remove(key);
                }
            }
            st.memory = memory;
            let turn_io = if tracks_turns {
                self.step_turns(&mut st, &snap, &evidence)
            } else {
                // A standby forgets: on taking the lease it starts from its next snapshot.
                st.turns.clear();
                self.lookups.lock().unwrap().clear();
                StepOut::default()
            };
            let (changes, rows) = st.take_changes(snap.at);
            let edge = st.take_link_edge();
            (changes, rows, (st.link, st.link_since_ms, edge), turn_io)
        };
        self.publish(&changes, rows, link, Some(snap.at_ms), snap.at_ms);
        self.run_turn_io(turn_io);
    }

    /// Step every driven key's turn state on `snap` (the views are already derived),
    /// keep only driven keys, and mark the keys with a delivery or reply pending as
    /// awaited (polled hot).
    fn step_turns(
        &self,
        st: &mut EngineState,
        snap: &HerdrSnapshot,
        evidence: &HashMap<&str, ProjectEvidence>,
    ) -> StepOut {
        let mut out = StepOut::default();
        let mut results = self.lookups.lock().unwrap();
        let mut old = std::mem::take(&mut st.turns);
        for (key, obs) in &snap.keys {
            let Some(ev) = evidence.get(key.project.as_str()) else {
                // No evidence this round (an unreadable DB): keep what we had.
                if let Some(t) = old.remove(key) {
                    st.turns.insert(key.clone(), t);
                }
                continue;
            };
            if !ev.drive.driven.contains(&key.worktree) {
                continue;
            }
            let Some(mem) = st.memory.get(key) else {
                continue;
            };
            let mut tm = old.remove(key).unwrap_or_default();
            tm.step(
                &TurnInput {
                    key,
                    obs: obs.as_ref().filter(|o| o.agent.is_some()),
                    view: &mem.view,
                    evidence: ev,
                    at: snap.at,
                    at_ms: snap.at_ms,
                },
                &mut results,
                &mut st.next_token,
                &mut out,
            );
            st.turns.insert(key.clone(), tm);
        }
        // Results nobody waits for any more are dropped.
        let pending: HashSet<u64> = st
            .turns
            .values()
            .filter_map(|t| t.pending_reply.as_ref().map(|p| p.token))
            .collect();
        results.retain(|token, _| pending.contains(token));
        st.awaited = st
            .turns
            .iter()
            .filter(|(_, t)| t.awaited())
            .map(|(k, _)| k.clone())
            .collect();
        out
    }

    /// Record `out`'s finished turns and start its transcript lookups.
    fn run_turn_io(&self, out: StepOut) {
        for ins in &out.inserts {
            self.write_turn(ins);
        }
        if out.lookups.is_empty() {
            return;
        }
        let source = self.transcripts.lock().unwrap().clone();
        let spawn = self.spawner.lock().unwrap().clone();
        for l in out.lookups {
            let source = Arc::clone(&source);
            let results = Arc::clone(&self.lookups);
            spawn(Box::new(move || {
                let r = source.reply(&l.query);
                results.lock().unwrap().insert(l.token, r);
            }));
        }
    }

    /// Write one turn row to its project's `tasks.db` (never created here), through a
    /// handle kept open per DB. The write completes before the next snapshot's evidence
    /// read, which then no longer lists the delivery.
    fn write_turn(&self, ins: &TurnInsert) {
        let wt = &ins.key.worktree;
        if !Path::new(&ins.db_path).is_file() {
            log::warn!("drive: no tasks.db at {} for @{wt}'s turn", ins.db_path);
            self.with_turns(&ins.key, |t| t.written(ins));
            return;
        }
        let written = {
            let mut stores = self.turn_stores.lock().unwrap();
            let store = match stores.entry(ins.db_path.clone()) {
                Entry::Occupied(e) => Ok(e.into_mut()),
                Entry::Vacant(e) => {
                    keitora_tasks::LocalStore::open(&ins.db_path).map(|store| e.insert(store))
                }
            };
            let written = store.and_then(|store| store.drive_turn_insert(&ins.row()));
            if written.is_err() {
                // A failed handle is reopened on the next write.
                stores.remove(&ins.db_path);
            }
            written
        };
        match written {
            Ok(Some(_)) => {
                let delivery = ins
                    .delivery_id
                    .map_or_else(|| "-".to_string(), |id| id.to_string());
                if ins.outcome == "restarted" {
                    log::info!("drive: @{wt} delivery {delivery} ended: agent restarted mid-turn");
                } else {
                    log::info!(
                        "drive: @{wt} {} turn finished (delivery {delivery}, clause {}, reply {})",
                        ins.kind.as_str(),
                        ins.clause.as_str(),
                        ins.reply_source
                    );
                }
                self.with_turns(&ins.key, |t| {
                    t.written(ins);
                    t.push_note(ins.note());
                });
            }
            Ok(None) => {
                log::debug!(
                    "drive: @{wt} turn for delivery {:?} already recorded or no longer outstanding",
                    ins.delivery_id
                );
                self.with_turns(&ins.key, |t| t.written(ins));
            }
            Err(e) => {
                // Kept and offered again on the next snapshot.
                if self.with_turns(&ins.key, |t| t.write_failed(ins)) == Some(true) {
                    log::warn!("drive: cannot record @{wt}'s turn (will retry): {e}");
                } else {
                    log::debug!("drive: still cannot record @{wt}'s turn: {e}");
                }
            }
        }
    }

    /// Run `f` on `key`'s turn state, if it is tracked.
    fn with_turns<T>(&self, key: &SessionKey, f: impl FnOnce(&mut TurnMemory) -> T) -> Option<T> {
        self.state.lock().unwrap().turns.get_mut(key).map(f)
    }

    /// An `agent list` failed: the poll counts (no retry storm) and the views go
    /// stale by age; nothing turns `offline`.
    pub fn poll_failed(&self, now: Instant) {
        let (changes, rows, link) = {
            let mut st = self.state.lock().unwrap();
            st.last_poll = Some(now);
            let (changes, rows) = st.take_changes(now);
            let edge = st.take_link_edge();
            (changes, rows, (st.link, st.link_since_ms, edge))
        };
        self.publish(&changes, rows, link, None, keitora_tasks::now_ms());
    }

    /// Set the link (and its since-time on a change). Any link but `Up` makes every
    /// view stale at once.
    pub fn mark_link(&self, link: HerdrLink, now_ms: i64) {
        let now = self.now();
        let (changes, rows, link) = {
            let mut st = self.state.lock().unwrap();
            if st.link != link {
                log::info!(
                    "engine: herdr link {} -> {}",
                    st.link.as_str(),
                    link.as_str()
                );
                st.link = link;
                st.link_since_ms = now_ms;
                // Poll at once when herdr comes back, not a whole interval later.
                if link != HerdrLink::Up {
                    st.last_poll = None;
                }
            }
            let (changes, rows) = st.take_changes(now);
            let edge = st.take_link_edge();
            (changes, rows, (st.link, st.link_since_ms, edge))
        };
        self.publish(&changes, rows, link, None, now_ms);
    }

    /// Hand `changes` (and `rows`, the full set, present when anything changed) to
    /// the store and the subscribers. `link` carries whether the link or readiness
    /// moved: subscribers then hear an empty batch even when no view changed (a
    /// project with no session waits on the link too).
    fn publish(
        &self,
        changes: &[ViewChange],
        rows: Option<Vec<StatusSession>>,
        link: (HerdrLink, i64, bool),
        snapshot_ms: Option<i64>,
        now_ms: i64,
    ) {
        if let Some(p) = &self.publisher {
            p.set_link(link.0, link.1);
            if let Some(at) = snapshot_ms {
                p.set_last_snapshot(at);
            }
            if let Some(rows) = rows {
                p.publish(rows, now_ms);
            }
        }
        if changes.is_empty() && !link.2 {
            return;
        }
        for c in changes {
            log::debug!(
                "engine: {} {}->{} agent={} herdr={} scs={:?} cs={:?}",
                c.key,
                c.prev.as_ref().map_or("none", |s| s.state.as_str()),
                c.next.as_ref().map_or("none", |s| s.state.as_str()),
                c.next.as_ref().map_or("none", |s| s.agent.as_str()),
                c.next
                    .as_ref()
                    .and_then(|s| s.herdr_status.as_deref())
                    .unwrap_or("none"),
                c.next.as_ref().and_then(|s| s.state_change_seq),
                c.next.as_ref().and_then(|s| s.completion_seq),
            );
        }
        let subscribers = self.subscribers.lock().unwrap().clone();
        for s in subscribers {
            s(changes);
        }
    }

    // ── Consumer side ───────────────────────────────────────────────────────

    /// Register `f` for every batch of view changes (an empty batch: only the link or
    /// readiness moved).
    pub fn subscribe(&self, f: Subscriber) {
        self.subscribers.lock().unwrap().push(Arc::from(f));
    }

    /// A launch of `key`'s agent started.
    pub fn note_launch(&self, key: &SessionKey) {
        let now = self.now();
        self.state.lock().unwrap().launches.insert(key.clone(), now);
    }

    /// The launch of `key`'s agent failed: it no longer reads `Starting`. A launch
    /// that went through stays noted until a snapshot lists its agent (or the grace
    /// runs out), so the view doesn't drop to `offline` while herdr catches up.
    pub fn note_launch_failed(&self, key: &SessionKey) {
        self.state.lock().unwrap().launches.remove(key);
    }
}

/// Consumer-side reads.
impl SessionEngine {
    /// The current polling mode.
    #[cfg_attr(not(test), allow(dead_code))] // read by the drive wait reasons
    pub fn mode(&self) -> Mode {
        let now = self.now();
        let mut st = self.state.lock().unwrap();
        st.prune_launches(now);
        st.mode()
    }

    /// Current views of `project`'s keys, each with its `stale` flag.
    pub fn views(&self, project: &str) -> ProjectViews {
        let now = self.now();
        let st = self.state.lock().unwrap();
        let views = st
            .rows_where(now, |k| k.project == project)
            .map(|(k, v)| (k.worktree.clone(), v))
            .collect();
        ProjectViews {
            ready: st.ready(),
            link: st.link,
            views,
        }
    }

    /// One key's view.
    pub fn view(&self, key: &SessionKey) -> KeyView {
        let now = self.now();
        let st = self.state.lock().unwrap();
        if st.last_snapshot.is_none() {
            return KeyView::NotReady;
        }
        match st.memory.get(key) {
            Some(m) => {
                let mut row = m.view.to_status(key);
                row.stale = st.stale(now);
                KeyView::View(row)
            }
            None => KeyView::Missing,
        }
    }

    /// Every view.
    pub fn all_views(&self) -> Vec<(SessionKey, StatusSession)> {
        let now = self.now();
        let st = self.state.lock().unwrap();
        let mut all: Vec<_> = st
            .rows_where(now, |_| true)
            .map(|(k, v)| (k.clone(), v))
            .collect();
        drop(st);
        all.sort_by(|a, b| (&a.0.project, &a.0.worktree).cmp(&(&b.0.project, &b.0.worktree)));
        all
    }

    /// A snapshot exists and the link is up.
    #[cfg_attr(not(test), allow(dead_code))] // consumers ask `is_fresh`
    pub fn is_ready(&self) -> bool {
        self.state.lock().unwrap().ready()
    }

    /// Whether the views answer for the panes now: ready and not stale.
    pub fn is_fresh(&self) -> bool {
        let now = self.now();
        let st = self.state.lock().unwrap();
        st.ready() && !st.stale(now)
    }

    /// Fall back on `backend`'s live panes while the engine has no fresh answer
    /// (see [`Self::running_count`] and [`Self::running_keys_or_all_live`]). Set once.
    pub fn attach_backend(&self, backend: &Arc<dyn PtyBackend>) {
        let _ = self.panes.set(Arc::downgrade(backend));
    }

    fn backend(&self) -> Option<Arc<dyn PtyBackend>> {
        self.panes.get().and_then(Weak::upgrade)
    }

    /// How many sessions run an agent (present or starting), for the quit and
    /// upgrade confirmations. While the engine has no fresh answer, every cached live
    /// pane counts too, so the count never comes up short. No herdr call.
    pub fn running_count(&self) -> usize {
        let fresh = self.is_fresh();
        let panes = match (fresh, self.backend()) {
            (false, Some(b)) => b.live_set_snapshot().into_iter().collect(),
            _ => Vec::new(),
        };
        running_agent_keys(&self.all_views(), fresh, panes).len()
    }

    /// The keys with a running agent; while the engine has no fresh answer, every
    /// live pane too (a fresh pane list from the backend), so nothing is missed.
    pub fn running_keys_or_all_live(&self) -> Result<Vec<SessionKey>, String> {
        let fresh = self.is_fresh();
        let panes = match (fresh, self.backend()) {
            (false, Some(b)) => b
                .list_sessions()
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|s| s.key)
                .collect(),
            _ => Vec::new(),
        };
        Ok(running_agent_keys(&self.all_views(), fresh, panes))
    }

    /// Whether `project` has pending drive or dispatch requests (polls hot while so).
    pub fn set_pending(&self, project: &str, pending: bool) {
        let mut st = self.state.lock().unwrap();
        if pending {
            st.pending.insert(project.to_string());
        } else {
            st.pending.remove(project);
        }
    }

    /// Whether `project` has pending drive or dispatch requests, as last set.
    #[cfg_attr(not(test), allow(dead_code))] // the hot cadence reads the set itself
    pub fn is_pending(&self, project: &str) -> bool {
        self.state.lock().unwrap().pending.contains(project)
    }

    /// Whether the window shows a non-empty set of projects (polls warm while so).
    pub fn set_window_open(&self, open: bool) {
        self.state.lock().unwrap().window_open = open;
    }

    /// A send was delivered to `key`: poll hot until the next snapshot.
    pub fn note_delivery(&self, key: &SessionKey) {
        self.state.lock().unwrap().awaited.insert(key.clone());
    }

    /// Each driven key's turn health (recent completions, a delivery with no activity).
    pub fn turn_health(&self) -> Vec<(SessionKey, TurnHealth)> {
        let st = self.state.lock().unwrap();
        let mut all: Vec<_> = st
            .turns
            .iter()
            .map(|(k, t)| (k.clone(), t.health()))
            .collect();
        all.sort_by(|a, b| (&a.0.project, &a.0.worktree).cmp(&(&b.0.project, &b.0.worktree)));
        all
    }

    /// `project`'s hook rows and drive state, through the engine's cached read-only
    /// evidence (re-read only when the DB changed).
    pub fn project_evidence(&self, project: &str) -> Option<ProjectEvidence> {
        self.evidence.project(project)
    }

    /// Read finished turns' replies from `source` when the Stop hook gives none.
    pub fn set_transcript_source(&self, source: Arc<dyn TranscriptSource>) {
        *self.transcripts.lock().unwrap() = source;
    }

    /// Run transcript lookups through `spawner` instead of the blocking pool.
    #[cfg(test)]
    pub(crate) fn set_spawner(&self, spawner: Spawner) {
        *self.spawner.lock().unwrap() = spawner;
    }

    /// The link and snapshot state.
    pub fn link_state(&self) -> LinkState {
        let st = self.state.lock().unwrap();
        LinkState {
            link: st.link,
            since_ms: st.link_since_ms,
            last_snapshot_ms: st.last_snapshot.map(|(_, ms)| ms),
            ready: st.ready(),
        }
    }

    /// Per-key health facts.
    pub fn key_health(&self) -> Vec<(SessionKey, KeyHealth)> {
        let st = self.state.lock().unwrap();
        let mut all: Vec<_> = st
            .memory
            .iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    KeyHealth {
                        present_since_ms: m.present_since_ms,
                        unknown_since_ms: m.unknown_since_ms,
                        pane_id: m.view.pane_id.clone(),
                    },
                )
            })
            .collect();
        all.sort_by(|a, b| (&a.0.project, &a.0.worktree).cmp(&(&b.0.project, &b.0.worktree)));
        all
    }
}
