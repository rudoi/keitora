//! Drive requests: a driver worktree's `keitora drive` verbs that act on another
//! worktree's pane (`send`, `read`, `reset`, `swap`, `wake`) arrive as `drive_requests` rows in
//! the project's `tasks.db`. The tasks-DB watchers start [`run_drive_requests`] when a
//! pending row appears; it runs each row once (compare-and-set begin) and records the
//! outcome on the row, which the waiting CLI reads back.
//!
//! - `send` and `read` run inline, in request order. `send` goes only to an idle
//!   session (a `--queue` row stays pending until it is idle) and holds the
//!   [`DispatchGate`] so it never races a board dispatch.
//! - `reset` and `swap` run as their own tasks: the backend asks the window to restart
//!   the pane (`drive://restart`) or to run the transfer flow (`drive://swap`), and the
//!   window reports back through [`drive_request_resolve`]. Only the runner writes the
//!   row, so a request is resolved in exactly one place.
//! - A `wake` (`keitora wake`) starts an agent that isn't running: the same
//!   `drive://restart` event in wake mode, which the pane turns into a launch
//!   (terminated) or a spawn (ended). An awake target is a no-op. A row of an unknown
//!   kind is refused, so an app that predates `wake` never runs one as a reset. (A
//!   `reset` with `"mode": "wake"` from an older CLI is still taken as a wake.)
//! - A `send` with `"wake": true` to a worktree with no running agent wakes it first
//!   (resuming), then stays pending like a `--queue` send until the agent is idle.
//!
//! A request is served only for a project the window reports open
//! ([`drive_set_open_projects`]); until the first report, rows stay pending.
//!
//! Passes are asked for through [`DriveRuntime::request_pass`] by everything that can
//! make a request actionable: a tasks-DB write, an engine view or link change, a
//! window report, a scheduled recheck and the pipeline's safety sweep. Each pass
//! records on every row it leaves pending why it waits (`wait_reason`) and logs one
//! line per request, so a request that never moves explains itself.
//!
//! The DB-only drive commands (`drives_list`, `drive_release`, `drive_take_over`,
//! `drive_pause`) work for any registered project, with or without a window.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::sync::mpsc;

use keitora_tasks::{
    Baseline, CliKind, Drive, DriveRequest, LocalStore, ProjectConfig, StatusSession,
    DRIVE_READ_LINES_DEFAULT, DRIVE_READ_LINES_MAX, DRIVE_READ_RESULT_MAX,
    DRIVE_REQUEST_MAX_AGE_SECS, DRIVE_RETENTION_SECS, DRIVE_SEND_TEXT_MAX,
};

use crate::activity::lock;
use crate::backend::{map_write_line_result, SessionKey, SharedBackend, WriteLineOutcome};
use crate::dispatch::{
    collapse, injection_detail, transfer_refusal, unknown_worktree_refusal, DispatchGate,
    SUBMIT_DELAY,
};
use crate::engine::gate::{self, Hold, Refusal, Verdict};
use crate::engine::model::{AgentObs, HookEvidence, ViewQuery};
use crate::engine::{HerdrLink, KeyView, ProjectViews, SharedEngine, ViewChange, SETTLE};
use crate::transfer_registry::TransferRegistry;

/// How long the window has to restart a pane for a `reset`.
pub(crate) const DRIVE_RESET_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a `swap` confirm may stay unanswered in the window.
pub(crate) const SWAP_CONFIRM_TIMEOUT: Duration = Duration::from_secs(300);
/// How long an accepted `swap` may take to finish (the transfer's own restart
/// backstop plus the import).
pub(crate) const SWAP_RUN_TIMEOUT: Duration = Duration::from_secs(420);
/// A `running`/`pending_confirm` row this process isn't tracking and older than this
/// was left behind by an earlier app run.
const STUCK_AFTER_SECS: i64 = 600;
/// How long a delivered `send` holds its worktree busy when the driven session is never
/// seen to start the turn (its hook may be missing or slow).
pub(crate) const SEND_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after a woken send's settle window its follow-up pass runs, so the pass
/// lands just past the deadline it checks.
const WAKE_RECHECK_SLACK: Duration = Duration::from_millis(500);
/// Pass requests for one project closer together than this collapse into one pass.
pub(crate) const PASS_COALESCE: Duration = Duration::from_millis(150);
/// How often a project's pass also refuses stuck rows and purges old ones (expiry
/// runs on every pass).
const HOUSEKEEPING_EVERY: Duration = Duration::from_secs(60);
/// Prefix of a `read` result whose head was cut to fit the cap.
const TRUNCATED_MARKER: &str = "[... earlier output truncated ...]\n";

/// Window event: restart a driven pane fresh (a `reset`).
pub(crate) const RESTART_EVENT: &str = "drive://restart";
/// Window event: run the transfer flow for a driven pane (a `swap`).
pub(crate) const SWAP_EVENT: &str = "drive://swap";

// ---------------------------------------------------------------------------
// Managed state
// ---------------------------------------------------------------------------

/// What the window reports for a `reset`/`swap` it was asked to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriveReport {
    /// The human confirmed a swap; the transfer is starting.
    Accepted,
    /// The restart or transfer finished.
    Applied(String),
    /// The window refused, or the restart or transfer failed.
    Refused(String),
}

/// Which window flow a waiter belongs to (only a swap is confirmed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitKind {
    Reset,
    Swap,
}

type WaiterKey = (String, i64);

#[derive(Default)]
struct RuntimeInner {
    /// Resolved project names the window reports as open (warm). `None` until the
    /// first report.
    open: Mutex<Option<HashSet<String>>>,
    /// reset/swap requests awaiting the window: (project, request id) -> report channel.
    waiters: Mutex<HashMap<WaiterKey, (WaitKind, mpsc::UnboundedSender<DriveReport>)>>,
    /// (project, worktree) with a reset/swap in flight; a second one is refused.
    in_flight: Mutex<HashSet<(String, String)>>,
    /// (project, request id) this process has begun and not yet resolved.
    tracked: Mutex<HashSet<WaiterKey>>,
    /// (project, worktree) a `send` was just typed into, until its turn is seen
    /// starting: herdr's `working` lags the keystrokes.
    sent: Mutex<HashMap<(String, String), RecentSend>>,
    /// (project, send request id) whose target was woken for it, or that waited on a
    /// reset/swap/wake holding its target's slot: the send then waits for idle like a
    /// queued one.
    woken: Mutex<HashMap<WaiterKey, WakeMark>>,
    /// (project, send request id) with a follow-up pass scheduled, and when it fires.
    rechecks: Mutex<HashMap<WaiterKey, Instant>>,
    /// Project -> the due times of its requested passes that haven't started.
    queued: Mutex<HashMap<String, BTreeSet<Instant>>>,
    /// Runs requested passes; none until the app (or a test) installs one.
    runner: Mutex<Option<Arc<dyn PassRunner>>>,
    /// Project -> worktree -> pending requests, as of the last pass or sweep. A hint
    /// for the engine trigger only.
    pending_by_worktree: Mutex<HashMap<String, HashMap<String, u32>>>,
    /// When the window last reported its open projects (unix ms).
    open_reported_at: Mutex<Option<i64>>,
    /// (project, request id) -> the last outcome logged for it (a waiting request is
    /// logged again only when its reason changes).
    last_logged: Mutex<HashMap<WaiterKey, String>>,
    /// Project -> when its last pass started (unix ms) and what asked for it.
    last_pass: Mutex<HashMap<String, (i64, Trigger)>>,
    /// DB path -> the writable store the last pass used, kept for the next one, with
    /// the DB's `data_version` as that pass began reading (other connections' commits
    /// move it; the pass's own writes don't).
    stores: Mutex<HashMap<String, (LocalStore, i64)>>,
    /// Project -> when its pass last ran the full housekeeping.
    housekept: Mutex<HashMap<String, Instant>>,
    /// Every pass-log line, kept for tests to read.
    #[cfg(test)]
    pass_lines: Mutex<Vec<String>>,
}

/// What asked for a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Trigger {
    /// A tasks-DB watcher saw a write.
    Watcher,
    /// The pipeline's safety sweep found pending rows.
    Sweep,
    /// A pass (or a hold that lifts by time) asked for a follow-up.
    Recheck,
    /// An engine view or link change could unblock a request.
    Engine,
    /// The window reported its open projects.
    OpenSet,
}

impl Trigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Trigger::Watcher => "watcher",
            Trigger::Sweep => "sweep",
            Trigger::Recheck => "recheck",
            Trigger::Engine => "engine",
            Trigger::OpenSet => "open_set",
        }
    }
}

/// Runs a requested pass once it is due: sleep until `due`, then (only if
/// [`DriveRuntime::take_due`] still holds `due` for the project) run it.
pub(crate) trait PassRunner: Send + Sync {
    fn run_at(&self, project: String, due: Instant, after: Duration, why: Trigger);
}

/// How a window report compares with the previous one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenChange {
    /// The first report (or the first after the window closed).
    First,
    Changed,
    Same,
}

/// Process-wide pass ids, for the pass log.
static PASS_IDS: AtomicU64 = AtomicU64::new(0);

/// Why a waking send is waiting, and since when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WakeMark {
    at: Instant,
    /// This send started the wake. Otherwise it only waited while another reset, swap
    /// or wake held the slot, so it can't blame a wake that never came up.
    own: bool,
}

/// A delivered `send` whose turn hasn't been seen starting yet.
#[derive(Debug, Clone, Copy)]
struct RecentSend {
    at: Instant,
    /// The delivery the hold belongs to (0 for a dispatch, which records none).
    delivery_id: i64,
    /// herdr's `state_change_seq` for the target when the send went in; any change
    /// means the turn moved.
    baseline_scs: u64,
}

/// Managed state shared by the runner and the drive commands. Cheap to clone; every
/// clone sees the same state.
#[derive(Default, Clone)]
pub struct DriveRuntime(Arc<RuntimeInner>);

/// Releases a worktree's reset/swap slot on drop, and asks for a pass over its
/// project: a request waiting on the slot can go now, and a freed slot is no DB write.
pub(crate) struct InFlightGuard {
    runtime: DriveRuntime,
    key: (String, String),
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        lock(&self.runtime.0.in_flight).remove(&self.key);
        self.runtime
            .request_pass(&self.key.0, Duration::ZERO, Trigger::Recheck);
    }
}

/// Stops tracking a request on drop.
pub(crate) struct TrackGuard {
    runtime: DriveRuntime,
    key: WaiterKey,
}

impl Drop for TrackGuard {
    fn drop(&mut self) {
        lock(&self.runtime.0.tracked).remove(&self.key);
    }
}

/// Removes a request's waiter on drop.
struct WaiterGuard {
    runtime: DriveRuntime,
    key: WaiterKey,
}

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        lock(&self.runtime.0.waiters).remove(&self.key);
    }
}

impl DriveRuntime {
    /// The pass-log lines written so far.
    #[cfg(test)]
    pub(crate) fn pass_lines(&self) -> Vec<String> {
        lock(&self.0.pass_lines).clone()
    }

    /// The open set, or `None` before the window's first report.
    pub(crate) fn open_set(&self) -> Option<HashSet<String>> {
        lock(&self.0.open).clone()
    }

    /// Store the window's open projects and stamp the report's time.
    pub(crate) fn set_open(&self, projects: Vec<String>) -> OpenChange {
        let next: HashSet<String> = projects.into_iter().collect();
        *lock(&self.0.open_reported_at) = Some(keitora_tasks::now_ms());
        let mut open = lock(&self.0.open);
        let change = match open.as_ref() {
            None => OpenChange::First,
            Some(o) if *o == next => OpenChange::Same,
            Some(_) => OpenChange::Changed,
        };
        *open = Some(next);
        change
    }

    /// The window went away: forget its open set, so its next report counts as the
    /// first (and rows wait for it).
    pub(crate) fn clear_open(&self) {
        *lock(&self.0.open) = None;
        *lock(&self.0.open_reported_at) = None;
    }

    /// When `project`'s last pass started (unix ms) and what asked for it.
    pub(crate) fn last_pass(&self, project: &str) -> Option<(i64, Trigger)> {
        lock(&self.0.last_pass).get(project).copied()
    }

    /// When the window last reported its open projects (unix ms), if it has.
    pub(crate) fn open_reported_at(&self) -> Option<i64> {
        *lock(&self.0.open_reported_at)
    }

    /// Whether `project` is in the window's open set.
    pub(crate) fn is_open(&self, project: &str) -> bool {
        lock(&self.0.open)
            .as_ref()
            .is_some_and(|o| o.contains(project))
    }

    /// The writable store the last pass over `db_path` left (`None` when none is
    /// idle: a first pass, one running now, or one that failed).
    fn take_store(&self, db_path: &str) -> Option<LocalStore> {
        lock(&self.0.stores).remove(db_path).map(|(s, _)| s)
    }

    /// Keep `store` for the next pass over `db_path`; `version` is its `data_version`
    /// as this pass began reading.
    fn put_store(&self, db_path: &str, store: LocalStore, version: i64) {
        lock(&self.0.stores).insert(db_path.to_string(), (store, version));
    }

    /// Drop the kept stores of DBs `keep` rejects (their project left the registry).
    pub(crate) fn retain_stores(&self, keep: impl Fn(&str) -> bool) {
        lock(&self.0.stores).retain(|path, _| keep(path));
    }

    /// Whether nothing but a pass's own writes changed `db_path` since the last pass
    /// over it began reading (so a watcher event for it has nothing new to show).
    /// `false` while unknown: no idle store, or a pass running.
    pub(crate) fn unchanged_since_pass(&self, db_path: &str) -> bool {
        lock(&self.0.stores)
            .get(db_path)
            .is_some_and(|(s, v)| s.data_version().ok() == Some(*v))
    }

    /// Claim `project`'s full housekeeping at `now`: `true` at most once per
    /// [`HOUSEKEEPING_EVERY`].
    fn housekeeping_due(&self, project: &str, now: Instant) -> bool {
        let mut at = lock(&self.0.housekept);
        if at
            .get(project)
            .is_some_and(|t| now.saturating_duration_since(*t) < HOUSEKEEPING_EVERY)
        {
            return false;
        }
        at.insert(project.to_string(), now);
        true
    }

    /// Install what runs requested passes.
    pub(crate) fn set_runner(&self, runner: Arc<dyn PassRunner>) {
        *lock(&self.0.runner) = Some(runner);
    }

    /// Ask for a pass over `project` no sooner than `after`. `false` when deduped: an
    /// already requested pass that hasn't started serves this one (or nothing runs
    /// passes yet).
    pub(crate) fn request_pass(&self, project: &str, after: Duration, why: Trigger) -> bool {
        self.request_pass_at(Instant::now(), project, after, why)
    }

    /// [`Self::request_pass`] at `now`. The due time is at least [`PASS_COALESCE`]
    /// away. A requested pass due at most [`PASS_COALESCE`] before this one serves it
    /// (so a burst of requests collapses into one pass); any other due time is kept
    /// as its own pass, so a sooner pass never swallows a later recheck (whose
    /// deadline the sooner one would run before).
    pub(crate) fn request_pass_at(
        &self,
        now: Instant,
        project: &str,
        after: Duration,
        why: Trigger,
    ) -> bool {
        let Some(runner) = lock(&self.0.runner).clone() else {
            return false;
        };
        let after = after.max(PASS_COALESCE);
        let due = now + after;
        {
            let mut queued = lock(&self.0.queued);
            let dues = queued.entry(project.to_string()).or_default();
            if dues.range(due - PASS_COALESCE..=due).next().is_some() {
                return false;
            }
            dues.insert(due);
        }
        runner.run_at(project.to_string(), due, after, why);
        true
    }

    /// A requested pass for `project` due at `due` is about to run: `true` (and that
    /// request is cleared) while it is still queued. Other due times stay queued.
    pub(crate) fn take_due(&self, project: &str, due: Instant) -> bool {
        let mut queued = lock(&self.0.queued);
        let Some(dues) = queued.get_mut(project) else {
            return false;
        };
        let taken = dues.remove(&due);
        if dues.is_empty() {
            queued.remove(project);
        }
        taken
    }

    /// Record `project`'s pending requests (the engine trigger's hint).
    pub(crate) fn set_pending_rows(&self, project: &str, rows: &[DriveRequest]) {
        let mut by_wt: HashMap<String, u32> = HashMap::new();
        for r in rows {
            *by_wt.entry(r.worktree.clone()).or_default() += 1;
        }
        let mut all = lock(&self.0.pending_by_worktree);
        if by_wt.is_empty() {
            all.remove(project);
        } else {
            all.insert(project.to_string(), by_wt);
        }
    }

    /// Whether `project` had a pending request for `worktree`, as last recorded.
    pub(crate) fn has_pending_for(&self, project: &str, worktree: &str) -> bool {
        lock(&self.0.pending_by_worktree)
            .get(project)
            .is_some_and(|m| m.contains_key(worktree))
    }

    /// The projects with pending requests, as last recorded, sorted.
    pub(crate) fn pending_projects(&self) -> Vec<String> {
        let mut p: Vec<String> = lock(&self.0.pending_by_worktree).keys().cloned().collect();
        p.sort();
        p
    }

    /// Record `outcome` as the last logged for `(project, id)`; `true` when it differs
    /// from the one before.
    fn note_logged(&self, project: &str, id: i64, outcome: &str) -> bool {
        let mut logged = lock(&self.0.last_logged);
        let key = (project.to_string(), id);
        if logged.get(&key).map(String::as_str) == Some(outcome) {
            return false;
        }
        logged.insert(key, outcome.to_string());
        true
    }

    /// Keep a pass-log line for tests.
    #[cfg_attr(not(test), allow(unused_variables))]
    fn record_line(&self, line: &str) {
        #[cfg(test)]
        lock(&self.0.pass_lines).push(line.to_string());
    }

    /// Whether a reset, swap or wake holds `worktree`'s slot.
    fn slot_held(&self, project: &str, worktree: &str) -> bool {
        lock(&self.0.in_flight).contains(&(project.to_string(), worktree.to_string()))
    }

    /// Take `worktree`'s reset/swap slot; `None` when one is already in flight.
    pub(crate) fn claim_worktree(&self, project: &str, worktree: &str) -> Option<InFlightGuard> {
        let key = (project.to_string(), worktree.to_string());
        if !lock(&self.0.in_flight).insert(key.clone()) {
            return None;
        }
        Some(InFlightGuard {
            runtime: self.clone(),
            key,
        })
    }

    /// Track a begun request until the guard drops.
    pub(crate) fn track(&self, project: &str, id: i64) -> TrackGuard {
        let key = (project.to_string(), id);
        lock(&self.0.tracked).insert(key.clone());
        TrackGuard {
            runtime: self.clone(),
            key,
        }
    }

    /// Hold `worktree`: a `send` was just typed into it while herdr's
    /// `state_change_seq` read `baseline_scs`.
    pub(crate) fn mark_sent(
        &self,
        project: &str,
        worktree: &str,
        delivery_id: i64,
        baseline_scs: u64,
    ) {
        lock(&self.0.sent).insert(
            (project.to_string(), worktree.to_string()),
            RecentSend {
                at: Instant::now(),
                delivery_id,
                baseline_scs,
            },
        );
    }

    /// When the hold of a send just typed into `worktree` lapses by time, if it has one.
    fn send_settle_deadline(
        &self,
        project: &str,
        worktree: &str,
        settle: Duration,
    ) -> Option<Instant> {
        lock(&self.0.sent)
            .get(&(project.to_string(), worktree.to_string()))
            .map(|r| r.at + settle)
    }

    fn clear_sent(&self, project: &str, worktree: &str) {
        lock(&self.0.sent).remove(&(project.to_string(), worktree.to_string()));
    }

    /// Whether a recent `send` still holds `worktree`. The hold ends (and is
    /// forgotten) once herdr reports the agent `working` or `blocked`, its
    /// `state_change_seq` moved off the baseline, or `settle` has elapsed.
    pub(crate) fn send_settling(
        &self,
        project: &str,
        worktree: &str,
        view: &StatusSession,
        settle: Duration,
    ) -> bool {
        self.send_settling_or_turned(project, worktree, view, settle, |_| false)
    }

    /// [`Self::send_settling`], also released once `turned(delivery_id)` says a turn
    /// row exists for the held delivery (asked only for a recorded delivery).
    pub(crate) fn send_settling_or_turned(
        &self,
        project: &str,
        worktree: &str,
        view: &StatusSession,
        settle: Duration,
        turned: impl FnOnce(i64) -> bool,
    ) -> bool {
        let key = (project.to_string(), worktree.to_string());
        let mut sent = lock(&self.0.sent);
        let Some(recent) = sent.get(&key) else {
            return false;
        };
        let started = matches!(view.herdr_status.as_deref(), Some("working" | "blocked"));
        let moved = view.state_change_seq != i64::try_from(recent.baseline_scs).ok();
        if started
            || moved
            || recent.at.elapsed() >= settle
            || (recent.delivery_id > 0 && turned(recent.delivery_id))
        {
            sent.remove(&key);
            return false;
        }
        true
    }

    /// Record that send `id`'s target is being (or was just) woken for it.
    fn mark_woken(&self, project: &str, id: i64) {
        let mark = WakeMark {
            at: Instant::now(),
            own: true,
        };
        lock(&self.0.woken).insert((project.to_string(), id), mark);
    }

    /// Record that send `id` is waiting on someone else's reset, swap or wake. A mark
    /// of its own wake is kept as it is (its own wake holds the slot meanwhile).
    fn mark_slot_wait(&self, project: &str, id: i64) {
        let mut woken = lock(&self.0.woken);
        let mark = woken.entry((project.to_string(), id)).or_insert(WakeMark {
            at: Instant::now(),
            own: false,
        });
        if !mark.own {
            mark.at = Instant::now();
        }
    }

    /// Send `id`'s wake mark, if it has one.
    fn wake_mark(&self, project: &str, id: i64) -> Option<WakeMark> {
        lock(&self.0.woken).get(&(project.to_string(), id)).copied()
    }

    fn forget_woken(&self, project: &str, id: i64) {
        let key = (project.to_string(), id);
        lock(&self.0.woken).remove(&key);
        lock(&self.0.rechecks).remove(&key);
    }

    /// Forget the wake marks and rechecks of `project`'s requests that are no longer
    /// pending (expired, or resolved without passing through a pass that forgets them).
    fn retain_woken(&self, project: &str, pending: &HashSet<i64>) {
        lock(&self.0.woken).retain(|(p, id), _| p != project || pending.contains(id));
        lock(&self.0.rechecks).retain(|(p, id), _| p != project || pending.contains(id));
        lock(&self.0.last_logged).retain(|(p, id), _| p != project || pending.contains(id));
    }

    /// Claim the follow-up pass for send `id` at `due`: `false` when one already
    /// scheduled fires at or after `due` (so one recheck serves every pass that asks).
    fn claim_recheck(&self, project: &str, id: i64, due: Instant) -> bool {
        let mut rechecks = lock(&self.0.rechecks);
        let key = (project.to_string(), id);
        if rechecks.get(&key).is_some_and(|at| *at >= due) {
            return false;
        }
        rechecks.insert(key, due);
        true
    }

    fn is_tracked(&self, project: &str, id: i64) -> bool {
        lock(&self.0.tracked).contains(&(project.to_string(), id))
    }

    fn register_waiter(
        &self,
        project: &str,
        id: i64,
        kind: WaitKind,
    ) -> (mpsc::UnboundedReceiver<DriveReport>, WaiterGuard) {
        let (tx, rx) = mpsc::unbounded_channel();
        let key = (project.to_string(), id);
        lock(&self.0.waiters).insert(key.clone(), (kind, tx));
        (
            rx,
            WaiterGuard {
                runtime: self.clone(),
                key,
            },
        )
    }

    /// Forward a window report to the runner waiting on `(project, id)`. Never writes
    /// the row: the runner does.
    pub(crate) fn report(
        &self,
        project: &str,
        id: i64,
        outcome: &str,
        reason: String,
    ) -> Result<(), String> {
        let report = match outcome {
            "accepted" => DriveReport::Accepted,
            "applied" => DriveReport::Applied(reason),
            "refused" => DriveReport::Refused(reason),
            _ => return Err("outcome must be accepted, applied or refused".to_string()),
        };
        let waiters = lock(&self.0.waiters);
        let Some((kind, tx)) = waiters.get(&(project.to_string(), id)) else {
            return Err("request is no longer waiting for Keitora".to_string());
        };
        if report == DriveReport::Accepted && *kind != WaitKind::Swap {
            return Err("only a swap is confirmed".to_string());
        }
        tx.send(report)
            .map_err(|_| "request is no longer waiting for Keitora".to_string())
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What a `drive://restart` asks of the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartMode {
    /// Kill the running session and start again (a `reset`).
    Reset,
    /// Start an agent that isn't running: launch a terminated session, spawn an ended
    /// one; a running one is left alone (a `wake`).
    Wake,
}

impl RestartMode {
    /// What the window failed to do, in a timeout message.
    fn verb(self) -> &'static str {
        match self {
            RestartMode::Reset => "restart",
            RestartMode::Wake => "wake",
        }
    }

    /// The request's name in a failure message.
    fn label(self) -> &'static str {
        match self {
            RestartMode::Reset => "reset",
            RestartMode::Wake => "wake",
        }
    }
}

/// `drive://restart` payload: restart or wake the driven pane. The provider is already
/// persisted. `fresh` starts a new conversation instead of resuming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveRestartEvent {
    pub request_id: i64,
    pub project: String,
    pub worktree: String,
    pub driver: String,
    pub mode: RestartMode,
    pub fresh: bool,
    /// The pane must start a new session: a reset, or a wake that is `fresh` or moves to
    /// another provider. Such a wake can't be satisfied by resuming or keeping a session
    /// the pane still holds (that one runs the old conversation or provider).
    pub replace: bool,
}

/// `drive://swap` payload: run the transfer flow for the driven pane to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveSwapEvent {
    pub request_id: i64,
    pub project: String,
    pub worktree: String,
    pub driver: String,
    pub to: CliKind,
}

/// The app side effects a reset/swap needs, behind a trait so the runner is testable
/// without a window or the real user config.
pub(crate) trait DriveHost: Send + Sync + 'static {
    fn emit_restart(&self, ev: &DriveRestartEvent);
    fn emit_swap(&self, ev: &DriveSwapEvent);
    /// Tell the window the worktree registry changed (a provider was persisted).
    fn worktrees_changed(&self);
    /// The provider `worktree` runs now.
    fn current_cli(&self, project: &str, config: &ProjectConfig, worktree: &str) -> CliKind;
    /// Persist `cli` as `worktree`'s provider, keeping its stored session ids.
    fn persist_cli(
        &self,
        config: &ProjectConfig,
        worktree: &str,
        cli: CliKind,
    ) -> Result<(), String>;
    /// Run herdr's integration install again, before a relaunch onto a provider
    /// whose integration waits for it to be found
    /// ([`keitora_tasks::ProviderSpec::integration_on_switch`]). The provider's hooks
    /// are written by the spawn itself, before its agent starts.
    fn refresh_integration(&self);
    /// Run another pass over `project`'s pending requests after `after`: a waiting
    /// send's deadline can pass with no DB write to trigger one.
    fn schedule_pass(&self, project: &str, after: Duration);
}

/// The real host: Tauri events and the user registry.
struct TauriHost<R: tauri::Runtime>(tauri::AppHandle<R>);

impl<R: tauri::Runtime> DriveHost for TauriHost<R> {
    fn emit_restart(&self, ev: &DriveRestartEvent) {
        use tauri::Emitter as _;
        let _ = self.0.emit(RESTART_EVENT, ev);
    }

    fn emit_swap(&self, ev: &DriveSwapEvent) {
        use tauri::Emitter as _;
        let _ = self.0.emit(SWAP_EVENT, ev);
    }

    fn worktrees_changed(&self) {
        use tauri::Emitter as _;
        let _ = self.0.emit("worktrees://changed", ());
    }

    fn current_cli(&self, project: &str, config: &ProjectConfig, worktree: &str) -> CliKind {
        // Re-resolve so a provider persisted since this pass started is seen.
        let user = keitora_tasks::UserConfig::load().unwrap_or_default();
        let fresh = user.project_config_by_name(project);
        let entries = fresh.as_ref().unwrap_or(config).worktree_entries();
        let entry = entries.into_iter().find(|e| e.name == worktree);
        keitora_tasks::resolve_cli(entry.as_ref(), &user)
    }

    fn persist_cli(
        &self,
        config: &ProjectConfig,
        worktree: &str,
        cli: CliKind,
    ) -> Result<(), String> {
        crate::transfer::persist_worktree_cli(
            config,
            worktree,
            cli,
            keitora_tasks::ProviderIds::keep(),
        )
    }

    fn refresh_integration(&self) {
        use tauri::Manager as _;
        if let Some(backend) = self.0.try_state::<SharedBackend>() {
            backend.inner().refresh_integration();
        }
    }

    fn schedule_pass(&self, project: &str, after: Duration) {
        use tauri::Manager as _;
        if let Some(rt) = self.0.try_state::<DriveRuntime>() {
            rt.request_pass(project, after, Trigger::Recheck);
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// How a request ends: `applied` with a result, or `refused` with a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub status: &'static str,
    pub reason: String,
    pub result: String,
}

impl Outcome {
    fn applied(result: impl Into<String>) -> Self {
        Outcome {
            status: "applied",
            reason: String::new(),
            result: result.into(),
        }
    }

    fn refused(reason: impl Into<String>) -> Self {
        Outcome {
            status: "refused",
            reason: reason.into(),
            result: String::new(),
        }
    }

    /// A queued send put back to `pending` at the last moment; `reason` is why it waits.
    /// Never recorded on the row as an outcome.
    fn deferred(reason: impl Into<String>) -> Self {
        Outcome {
            status: "deferred",
            reason: reason.into(),
            result: String::new(),
        }
    }
}

/// A request's parsed payload.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Parsed {
    /// `wake`: start the agent first when it isn't running.
    Send {
        text: String,
        wake: bool,
    },
    Read {
        lines: u32,
    },
    /// A `reset` row: a fresh restart, or in wake mode a start of an agent that isn't
    /// running (resuming unless `fresh`).
    Restart {
        to: Option<String>,
        mode: RestartMode,
        fresh: bool,
    },
    Swap {
        to: String,
    },
}

#[derive(Deserialize)]
struct SendPayload {
    text: String,
    /// Absent on rows from a CLI that predates waking: never wake for those.
    #[serde(default)]
    wake: bool,
}

#[derive(Deserialize)]
struct ReadPayload {
    #[serde(default)]
    lines: Option<i64>,
}

#[derive(Deserialize)]
struct ResetPayload {
    #[serde(default)]
    to: Option<String>,
    /// `reset` (the default), or `wake` on a row from an older CLI that filed a wake
    /// as a reset.
    #[serde(default)]
    mode: Option<String>,
    /// Wake mode only: start a new conversation instead of resuming.
    #[serde(default)]
    fresh: bool,
}

#[derive(Deserialize)]
struct WakePayload {
    #[serde(default)]
    to: Option<String>,
    /// Start a new conversation instead of resuming.
    #[serde(default)]
    fresh: bool,
}

#[derive(Deserialize)]
struct SwapPayload {
    to: String,
}

fn bad_payload(e: serde_json::Error) -> String {
    format!("bad request payload: {e}")
}

/// Parse a row's payload for its kind.
fn parse_request(kind: &str, payload: &str) -> Result<Parsed, String> {
    match kind {
        "send" => serde_json::from_str::<SendPayload>(payload)
            .map(|p| Parsed::Send {
                text: p.text,
                wake: p.wake,
            })
            .map_err(bad_payload),
        "read" => serde_json::from_str::<ReadPayload>(payload)
            .map(|p| Parsed::Read {
                lines: clamp_lines(p.lines),
            })
            .map_err(bad_payload),
        "reset" => {
            let p = serde_json::from_str::<ResetPayload>(payload).map_err(bad_payload)?;
            let (mode, fresh) = match p.mode.as_deref() {
                None | Some("reset") => (RestartMode::Reset, true),
                Some("wake") => (RestartMode::Wake, p.fresh),
                Some(other) => return Err(format!("unknown reset mode `{other}`")),
            };
            Ok(Parsed::Restart {
                to: p.to,
                mode,
                fresh,
            })
        }
        "wake" => serde_json::from_str::<WakePayload>(payload)
            .map(|p| Parsed::Restart {
                to: p.to,
                mode: RestartMode::Wake,
                fresh: p.fresh,
            })
            .map_err(bad_payload),
        "swap" => serde_json::from_str::<SwapPayload>(payload)
            .map(|p| Parsed::Swap { to: p.to })
            .map_err(bad_payload),
        // Refused, never guessed at: a kind from a newer CLI.
        other => Err(format!("unknown request kind `{other}`")),
    }
}

/// History lines to read: the default when absent, clamped to the allowed range.
fn clamp_lines(lines: Option<i64>) -> u32 {
    lines
        .unwrap_or(DRIVE_READ_LINES_DEFAULT)
        .clamp(1, DRIVE_READ_LINES_MAX) as u32
}

/// Parse a free-text provider name: its config name or its executable name (so both
/// `antigravity` and `agy` work). The error lists every provider.
fn parse_provider(name: &str) -> Result<CliKind, String> {
    CliKind::from_name(name).ok_or_else(|| {
        let names: Vec<String> = CliKind::ALL
            .into_iter()
            .map(|c| {
                let spec = c.spec();
                if spec.binary == spec.config_key {
                    spec.config_key.to_string()
                } else {
                    format!("{} ({})", spec.config_key, spec.binary)
                }
            })
            .collect();
        format!(
            "unknown provider `{name}`; expected one of: {}",
            names.join(", ")
        )
    })
}

/// Facts the common gates check, gathered after begin.
struct GateFacts<'a> {
    open: &'a HashSet<String>,
    configured: &'a [String],
    drive: Option<&'a Drive>,
    /// The transfer refusal text when the worktree is mid-transfer.
    transfer: Option<String>,
    terminated: bool,
}

/// Pure: the first common gate that refuses a request, in order. `read` is allowed on
/// a paused drive and during a transfer.
fn common_refusal(
    kind: &str,
    project: &str,
    worktree: &str,
    requested_by: &str,
    facts: &GateFacts<'_>,
) -> Option<String> {
    if !facts.open.contains(project) {
        return Some("project not open in Keitora".to_string());
    }
    if let Some(r) = unknown_worktree_refusal(worktree, facts.configured) {
        return Some(r);
    }
    let Some(drive) = facts.drive.filter(|d| d.driver == requested_by) else {
        return Some(format!("@{worktree} is not driven by @{requested_by}"));
    };
    let changes_pane = kind != "read";
    if changes_pane && drive.paused {
        return Some(format!(
            "drive of @{worktree} is paused; resume it before sending"
        ));
    }
    if changes_pane {
        if let Some(r) = &facts.transfer {
            return Some(r.clone());
        }
    }
    if facts.terminated {
        return Some(format!(
            "@{worktree} is terminated; launch it from the rail first"
        ));
    }
    None
}

/// A sanitised prompt: one line (submitted with `write_line`), or a block that keeps
/// its newlines (pasted, then submitted with a lone Enter).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Prompt {
    Line(String),
    Block(String),
}

/// Pure: sanitise a `send` text. Line endings become `\n`; a single line is collapsed
/// (no control char can submit early or smuggle an escape); a multi-line text keeps
/// `\n` only (tabs become spaces, other controls are dropped).
fn sanitize_prompt(text: &str) -> Result<Prompt, String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.trim().is_empty() {
        return Err("empty prompt".to_string());
    }
    if text.len() > DRIVE_SEND_TEXT_MAX {
        return Err("prompt too long (max 32 KiB)".to_string());
    }
    let text = text.trim_end();
    if !text.contains('\n') {
        let line = collapse(text);
        if line.is_empty() {
            return Err("empty prompt".to_string());
        }
        return Ok(Prompt::Line(line));
    }
    let body: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let body = body.trim_end().to_string();
    if body.trim().is_empty() {
        return Err("empty prompt".to_string());
    }
    Ok(Prompt::Block(body))
}

/// Pure: why a `send` can't go to a session in `state`; `None` when idle. The
/// offline and busy texts end with `detail` (see [`refusal_detail`]), so the refusal
/// stored in the request says what each source saw.
fn state_refusal(state: &str, worktree: &str, detail: &str) -> Option<String> {
    match state {
        "idle" => None,
        "busy" => Some(format!(
            "@{worktree} is busy; send only goes to an idle session (use --queue to wait for \
             it) {detail}"
        )),
        "waiting" => Some(format!(
            "@{worktree} is waiting on a prompt; answer it first (or use --queue)"
        )),
        "offline" => Some(format!(
            "@{worktree} is offline; start its session first {detail}"
        )),
        other => Some(format!("@{worktree} is {other}")),
    }
}

/// Pure: `(herdr: {no agent | status}; {last hook write {age} ago | no hook rows})`
/// from the engine's `view` and the worktree's `hook` row at `now_ms`.
fn refusal_detail(
    view: Option<&StatusSession>,
    hook: Option<&HookEvidence>,
    now_ms: i64,
) -> String {
    let herdr = match view {
        Some(v) if v.is_running() => v.herdr_status.as_deref().unwrap_or("unknown"),
        _ => "no agent",
    };
    let hook = match hook.map(|h| h.written_ms) {
        Some(Some(at)) => format!(
            "last hook write {} ago",
            keitora_tasks::age_text((now_ms - at) / 1000)
        ),
        Some(None) => "hook row with no write time".to_string(),
        None => "no hook rows".to_string(),
    };
    format!("(herdr: {herdr}; {hook})")
}

/// Pure: keep at most the last `max` bytes of `text`, cut at a char boundary. A cut
/// text starts with [`TRUNCATED_MARKER`], which counts inside the cap.
fn tail_cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let budget = max.saturating_sub(TRUNCATED_MARKER.len());
    let mut start = text.len() - budget;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{TRUNCATED_MARKER}{}", &text[start..])
}

/// Pure: why a swap from `current` to `to` is refused. Swap is the transfer engine,
/// which carries context only between its pairs (claude <-> codex); any other provider
/// change goes through `reset --to`.
fn swap_refusal(worktree: &str, current: CliKind, to: CliKind) -> Option<String> {
    if to == current {
        return Some(format!(
            "@{worktree} is already on {}",
            to.spec().config_key
        ));
    }
    if !crate::transfer::is_transfer_pair(current, to) {
        return Some(format!(
            "swap can't carry context from {} to {}; use reset --to {} instead",
            current.spec().config_key,
            to.spec().config_key,
            to.spec().config_key
        ));
    }
    None
}

/// Pure: whether a SQLite `datetime('now')` stamp is more than `secs` before `now`.
/// An unparseable stamp is never old (it is left alone).
fn is_older_than(created_at: &str, now: chrono::DateTime<chrono::Utc>, secs: i64) -> bool {
    chrono::NaiveDateTime::parse_from_str(created_at, "%Y-%m-%d %H:%M:%S")
        .map(|t| (now - t.and_utc()).num_seconds() > secs)
        .unwrap_or(false)
}

/// "5 minutes" for whole minutes, else "N s".
fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 && s.is_multiple_of(60) {
        let m = s / 60;
        format!("{m} minute{}", if m == 1 { "" } else { "s" })
    } else {
        format!("{s} s")
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Window timeouts and the pending-request age limit (shortened in tests).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    /// A pending request older than this is refused as expired.
    pub max_age_secs: i64,
    pub reset: Duration,
    pub swap_confirm: Duration,
    pub swap_run: Duration,
    /// How long a delivered `send` holds its worktree busy at most.
    pub send_settle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            max_age_secs: DRIVE_REQUEST_MAX_AGE_SECS,
            reset: DRIVE_RESET_TIMEOUT,
            swap_confirm: SWAP_CONFIRM_TIMEOUT,
            swap_run: SWAP_RUN_TIMEOUT,
            send_settle: SEND_SETTLE_TIMEOUT,
        }
    }
}

/// Everything one project's pass needs.
pub(crate) struct DriveEnv {
    /// Resolved `[project].name` (the session namespace).
    pub project: String,
    pub config: ProjectConfig,
    pub db_path: String,
    pub backend: SharedBackend,
    pub registry: TransferRegistry,
    pub runtime: DriveRuntime,
    pub host: Arc<dyn DriveHost>,
    pub timeouts: Timeouts,
    /// The session engine: the one liveness and state answer for send and wake.
    pub engine: SharedEngine,
}

impl DriveEnv {
    fn store(&self) -> Result<LocalStore, String> {
        LocalStore::open(&self.db_path).map_err(|e| e.to_string())
    }

    /// The writable store for a pass: the one the last pass left, else a fresh one.
    fn pass_store(&self) -> Result<LocalStore, String> {
        match self.runtime.take_store(&self.db_path) {
            Some(s) => Ok(s),
            None => self.store(),
        }
    }

    fn key(&self, worktree: &str) -> SessionKey {
        SessionKey {
            project: self.project.clone(),
            worktree: worktree.to_string(),
        }
    }

    /// Make sure a pass runs just after send `id`'s settle window (marked at `at`)
    /// closes, so a wake that never came up is refused even with no DB write.
    fn recheck_after_settle(&self, id: i64, at: Instant) {
        let due = at + self.timeouts.send_settle + WAKE_RECHECK_SLACK;
        if self.runtime.claim_recheck(&self.project, id, due) {
            self.host
                .schedule_pass(&self.project, due.saturating_duration_since(Instant::now()));
        }
    }

    /// A send `id` left waiting on a hold that lifts by time (a send just typed into
    /// its target, or an idle not settled yet) gets a pass once the hold can lift:
    /// neither ends with a DB write, and a send hold's lapse is no view change either.
    fn recheck_time_hold(&self, id: i64, worktree: &str, sets: &SessionSets) {
        let due = match self.runtime.send_settle_deadline(
            &self.project,
            worktree,
            self.timeouts.send_settle,
        ) {
            Some(at) => at,
            None if sets
                .views
                .views
                .get(worktree)
                .is_some_and(|v| !v.stale && v.is_idle() && !v.is_idle_settled()) =>
            {
                Instant::now() + SETTLE
            }
            None => return,
        };
        let due = due + WAKE_RECHECK_SLACK;
        if self.runtime.claim_recheck(&self.project, id, due) {
            self.host
                .schedule_pass(&self.project, due.saturating_duration_since(Instant::now()));
        }
    }

    /// A send put back to pending at the last moment (the fresh read disagreed
    /// with the engine's view) gets a pass once the view has had a tick to catch up:
    /// the requeue is this pass's own write, which the watcher skips, and a read that
    /// only says "not ready for input" changes no view. (Requests a few passes apart
    /// fold into one on the pass queue.)
    fn recheck_deferred(&self) {
        self.host
            .schedule_pass(&self.project, SETTLE + WAKE_RECHECK_SLACK);
    }

    fn terminated(&self, worktree: &str) -> bool {
        self.backend
            .terminated_keys(&self.project)
            .contains(worktree)
    }

    /// Whether `worktree` has a running agent, from fresh session sets.
    fn awake(&self, worktree: &str) -> Awake {
        self.sets(&mut None).awake(worktree)
    }

    /// The engine's views and the backend's terminated set, from `snapshot`, taken on
    /// first use and reused until the caller clears it.
    fn sets<'a>(&self, snapshot: &'a mut Option<SessionSets>) -> &'a SessionSets {
        snapshot.get_or_insert_with(|| SessionSets {
            views: self.engine.views(&self.project),
            terminated: self.backend.terminated_keys(&self.project),
            hooks: self.hook_rows(),
            at_ms: keitora_tasks::now_ms(),
        })
    }

    /// The project's hook rows, through the engine's cached evidence (for refusal
    /// detail).
    fn hook_rows(&self) -> HashMap<String, HookEvidence> {
        self.engine
            .project_evidence(&self.project)
            .map(|ev| ev.rows)
            .unwrap_or_default()
    }

    /// Whether a send to `worktree` may go now, waits, or is refused, from the
    /// session sets in `snapshot` and the hold of a send just typed into it.
    fn send_state(
        &self,
        local: &LocalStore,
        snapshot: &mut Option<SessionSets>,
        worktree: &str,
        queued: bool,
    ) -> SendGate {
        self.sets(snapshot).send_gate(worktree, queued, |view| {
            self.runtime.send_settling_or_turned(
                &self.project,
                worktree,
                view,
                self.timeouts.send_settle,
                |id| matches!(local.drive_turn_for_delivery(id), Ok(Some(_))),
            )
        })
    }
}

/// Whether a worktree has a running agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Awake {
    Yes,
    No,
    /// The engine has no fresh answer (no snapshot, herdr not up, or stale views):
    /// nothing is started on it.
    Unknown,
}

/// What a send may do now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendGate {
    Ready,
    /// A transient condition: the request stays pending with this reason.
    Wait(String),
    Refuse(String),
}

/// The engine's views of one project and the backend's terminated worktrees,
/// snapshotted once per pass.
pub(crate) struct SessionSets {
    pub views: ProjectViews,
    /// Worktrees whose session is terminated.
    pub terminated: HashSet<String>,
    /// The project's hook rows by worktree, for refusal detail.
    pub hooks: HashMap<String, HookEvidence>,
    /// Wall ms the sets were taken (hook ages in refusal detail).
    pub at_ms: i64,
}

impl SessionSets {
    /// Whether `worktree` has a running agent: terminated -> no; no fresh view ->
    /// unknown; an agent present or starting -> yes.
    pub(crate) fn awake(&self, worktree: &str) -> Awake {
        if self.terminated.contains(worktree) {
            return Awake::No;
        }
        if !self.views.ready {
            return Awake::Unknown;
        }
        match self.views.views.get(worktree) {
            Some(v) if v.stale => Awake::Unknown,
            Some(v) if v.is_running() => Awake::Yes,
            _ => Awake::No,
        }
    }

    /// Pure: the engine's send gate ([`gate::send_gate`]) for `worktree`, worded for
    /// drive. A fresh view that is not idle is refused, or waits when the send is
    /// `queued`; no session at all is refused either way.
    pub(crate) fn send_gate(
        &self,
        worktree: &str,
        queued: bool,
        settling: impl FnOnce(&StatusSession) -> bool,
    ) -> SendGate {
        match gate::send_gate(&self.views, worktree, settling) {
            Verdict::Go => SendGate::Ready,
            Verdict::Wait(hold) => SendGate::Wait(wait_reason_for(&WaitFact::Hold(hold, worktree))),
            Verdict::Refuse(Refusal::NotIdle(view)) if queued => {
                let status = if view.is_running() {
                    view.herdr_status.as_deref().unwrap_or("unknown")
                } else {
                    "no agent"
                };
                SendGate::Wait(wait_reason_for(&WaitFact::NotIdle { worktree, status }))
            }
            Verdict::Refuse(r) => {
                let detail = refusal_detail(
                    self.views.views.get(worktree),
                    self.hooks.get(worktree),
                    self.at_ms,
                );
                SendGate::Refuse(state_refusal(r.state(), worktree, &detail).unwrap_or_default())
            }
        }
    }
}

/// Why a pending request waits, as facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitFact<'a> {
    /// The window hasn't reported its open projects yet.
    Unreported,
    /// An engine send-gate hold on `worktree`.
    Hold(Hold, &'a str),
    /// A queued send whose target isn't idle; `status` is herdr's (`working`,
    /// `blocked`, `unknown`, `starting`, or `no agent` for an absent agent).
    NotIdle { worktree: &'a str, status: &'a str },
    /// The send woke its target, which doesn't show up yet.
    Woken(&'a str),
    /// Another reset, swap or wake holds the worktree's slot.
    SlotHeld,
    /// A dispatch holds the send gate.
    DispatchGate,
}

/// Pure: the one vocabulary of `wait_reason`s (also the send gate's wait texts).
pub(crate) fn wait_reason_for(fact: &WaitFact<'_>) -> String {
    match *fact {
        WaitFact::Unreported => "window hasn't reported open projects".to_string(),
        WaitFact::Hold(Hold::Link(link), _) => format!("waiting for herdr ({})", link.as_str()),
        WaitFact::Hold(Hold::FirstSnapshot, _) => "waiting for first herdr snapshot".to_string(),
        WaitFact::Hold(Hold::Stale, _) => "waiting for herdr (stale snapshot)".to_string(),
        WaitFact::Hold(Hold::Starting, worktree) => wait_reason_for(&WaitFact::NotIdle {
            worktree,
            status: "starting",
        }),
        WaitFact::Hold(Hold::Unsettled, worktree) => format!("waiting for @{worktree} to settle"),
        WaitFact::Hold(Hold::SendSettling, worktree) => {
            format!("waiting for @{worktree} to start its turn (send settling)")
        }
        WaitFact::NotIdle { worktree, status } => {
            format!("waiting for @{worktree} to be idle (herdr: {status})")
        }
        WaitFact::Woken(worktree) => format!("waiting for @{worktree}'s agent to come up (woken)"),
        WaitFact::SlotHeld => "waiting on the wake slot".to_string(),
        WaitFact::DispatchGate => "waiting for the dispatch gate".to_string(),
    }
}

/// Pure: why the engine has no fresh answer for `views`: herdr's link, no snapshot
/// yet, or stale views.
fn freshness_hold(views: &ProjectViews) -> Hold {
    match views.link {
        HerdrLink::Recovering | HerdrLink::Down | HerdrLink::Unavailable => Hold::Link(views.link),
        _ if !views.ready => Hold::FirstSnapshot,
        _ => Hold::Stale,
    }
}

/// A reset/swap body, spawned by the caller.
pub(crate) type Job = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Expiry on every pass; the rest at most once per [`HOUSEKEEPING_EVERY`]. Errors
/// are logged and ignored.
fn housekeeping(local: &LocalStore, env: &DriveEnv, now: chrono::DateTime<chrono::Utc>) {
    match local.drive_requests_expire(env.timeouts.max_age_secs) {
        Ok(0) => {}
        Ok(n) => log::info!("drive requests: {n} expired in {}", env.project),
        Err(e) => log::warn!("drive requests: expiry failed: {e}"),
    }
    if !env.runtime.housekeeping_due(&env.project, Instant::now()) {
        return;
    }
    match local.drive_requests_in_flight() {
        Ok(rows) => {
            for r in rows {
                if !env.runtime.is_tracked(&env.project, r.id)
                    && is_older_than(&r.created_at, now, STUCK_AFTER_SECS)
                {
                    let _ = local.drive_request_resolve(
                        r.id,
                        "refused",
                        "Keitora restarted before this finished",
                        "",
                    );
                }
            }
        }
        Err(e) => log::warn!("drive requests: cannot list in-flight: {e}"),
    }
    if let Err(e) = local.drive_purge(DRIVE_RETENTION_SECS) {
        log::warn!("drive requests: purge failed: {e}");
    }
    match local.drive_turns_prune() {
        Ok(0) => {}
        Ok(n) => log::debug!("drive turns: {n} pruned in {}", env.project),
        Err(e) => log::warn!("drive turns: prune failed: {e}"),
    }
}

fn resolve(local: &LocalStore, req: &DriveRequest, project: &str, outcome: &Outcome) {
    log::info!(
        "drive request {} ({project}: {} @{} by @{}): {} {}",
        req.id,
        req.kind,
        req.worktree,
        req.requested_by,
        outcome.status,
        outcome.reason
    );
    if let Err(e) =
        local.drive_request_resolve(req.id, outcome.status, &outcome.reason, &outcome.result)
    {
        log::warn!("drive request {}: cannot record outcome: {e}", req.id);
    }
}

/// Pure: the line that opens a pass. `open` prints sorted, `none` when empty and
/// `unreported` before the window's first report.
pub(crate) fn pass_start_line(
    pass_id: u64,
    project: &str,
    trigger: Trigger,
    open: Option<&HashSet<String>>,
    db: &str,
    pending: &[i64],
) -> String {
    let open = match open {
        None => "unreported".to_string(),
        Some(o) if o.is_empty() => "none".to_string(),
        Some(o) => {
            let mut v: Vec<&str> = o.iter().map(String::as_str).collect();
            v.sort_unstable();
            v.join(",")
        }
    };
    let pending: Vec<String> = pending.iter().map(i64::to_string).collect();
    format!(
        "drive pass {pass_id} start project={project} trigger={} open={open} db={db} pending=[{}]",
        trigger.as_str(),
        pending.join(",")
    )
}

/// Pure: one request's line in a pass.
pub(crate) fn pass_request_line(pass_id: u64, req: &DriveRequest, outcome: &str) -> String {
    format!(
        "drive pass {pass_id} request {} ({} @{} by @{}): {outcome}",
        req.id, req.kind, req.worktree, req.requested_by
    )
}

/// One pass's log: every line goes to the app log (and to tests), a waiting request
/// only when its reason changed since it was last logged.
struct PassLog<'a> {
    env: &'a DriveEnv,
    pass_id: u64,
    /// The pass started with pending requests (otherwise it logs at debug only).
    busy: bool,
    begun: usize,
}

impl PassLog<'_> {
    fn line(&self, line: &str) {
        if self.busy {
            log::info!("{line}");
        } else {
            log::debug!("{line}");
        }
        self.env.runtime.record_line(line);
    }

    /// Log `req`'s outcome in this pass.
    fn outcome(&self, req: &DriveRequest, outcome: &str) {
        self.env
            .runtime
            .note_logged(&self.env.project, req.id, outcome);
        self.line(&pass_request_line(self.pass_id, req, outcome));
    }

    /// Record on the row that `req` waits for `reason`, and log it if the reason is
    /// new for it.
    fn waiting(&self, local: &LocalStore, req: &DriveRequest, reason: &str) {
        note_wait(local, req.id, reason);
        let outcome = format!("waiting: {reason}");
        if self
            .env
            .runtime
            .note_logged(&self.env.project, req.id, &outcome)
        {
            self.line(&pass_request_line(self.pass_id, req, &outcome));
        }
    }

    /// Resolve `req` with `outcome` and log it.
    fn resolve(&self, local: &LocalStore, req: &DriveRequest, outcome: &Outcome) {
        resolve(local, req, &self.env.project, outcome);
        let text = match outcome.status {
            "refused" => format!("refused: {}", outcome.reason),
            s => s.to_string(),
        };
        self.outcome(req, &text);
    }
}

/// Record why pending request `id` waits (a write only when the reason changed).
fn note_wait(local: &LocalStore, id: i64, reason: &str) {
    if let Err(e) = local.drive_request_note_wait(id, reason) {
        log::warn!("drive request {id}: cannot record why it waits: {e}");
    }
}

/// One pass over `env`'s pending requests, in id order (a read goes ahead only of a
/// send held on a busy dispatch gate). `send`/`read` run inline and are resolved
/// here; begun `reset`/`swap` bodies are returned for the caller to spawn. Every
/// request left pending records why.
pub(crate) async fn run_pass(
    env: &Arc<DriveEnv>,
    gate: &DispatchGate,
    trigger: Trigger,
) -> Vec<Job> {
    let started = Instant::now();
    let pass_id = PASS_IDS.fetch_add(1, Ordering::Relaxed) + 1;
    let mut jobs: Vec<Job> = Vec::new();
    let local = match env.pass_store() {
        Ok(l) => l,
        Err(e) => {
            log::warn!("drive requests: cannot open {}: {e}", env.db_path);
            return jobs;
        }
    };
    // Read before anything else: a commit from another connection after this (a
    // request filed mid-pass) moves it, so its watcher event isn't mistaken for ours.
    let version = local.data_version().ok();
    housekeeping(&local, env, chrono::Utc::now());
    let pending = match local.drive_requests_pending() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("drive requests: cannot list pending: {e}");
            return jobs;
        }
    };
    lock(&env.runtime.0.last_pass).insert(env.project.clone(), (keitora_tasks::now_ms(), trigger));
    let open = env.runtime.open_set();
    let ids: Vec<i64> = pending.iter().map(|r| r.id).collect();
    let mut log = PassLog {
        env,
        pass_id,
        busy: !pending.is_empty(),
        begun: 0,
    };
    log.line(&pass_start_line(
        pass_id,
        &env.project,
        trigger,
        open.as_ref(),
        &env.db_path,
        &ids,
    ));
    env.runtime
        .retain_woken(&env.project, &ids.iter().copied().collect());
    // The engine trigger's hint, before this pass reads any view: a change after that
    // read (one this pass may have decided on too early) asks for another pass.
    env.runtime.set_pending_rows(&env.project, &pending);
    // Poll hot while requests wait on the engine; re-read once the pass is done.
    let dispatch_pending = local.dispatch_requests_has_pending().unwrap_or(false);
    env.engine
        .set_pending(&env.project, !ids.is_empty() || dispatch_pending);
    let local = match open {
        // The window hasn't reported yet: every row waits for it.
        None => {
            let reason = wait_reason_for(&WaitFact::Unreported);
            for req in &pending {
                log.waiting(&local, req, &reason);
            }
            local
        }
        Some(open) => serve(env, gate, local, &open, pending, &mut log, &mut jobs).await,
    };
    // What's left (rows filed meanwhile, deferred sends, sends waiting on a wake) as
    // the DB has it. A request waiting on a condition that lifts without a DB write
    // is passed again by its own trigger: the engine, a time-hold recheck, a freed
    // slot, or the window's report; the sweep backs them all.
    let left = local.drive_requests_pending();
    let left_n = left.as_ref().map_or(0, Vec::len);
    let has_left = left.as_ref().map_or(true, |l| !l.is_empty());
    env.runtime
        .set_pending_rows(&env.project, left.as_deref().unwrap_or_default());
    env.engine.set_pending(
        &env.project,
        has_left || local.dispatch_requests_has_pending().unwrap_or(false),
    );
    // A store that read fine is kept for the next pass; one that failed is dropped.
    if let (Ok(_), Some(version)) = (&left, version) {
        env.runtime.put_store(&env.db_path, local, version);
    }
    log.line(&format!(
        "drive pass {pass_id} done project={} took={}ms begun={} left={}",
        env.project,
        started.elapsed().as_millis(),
        log.begun,
        left_n
    ));
    jobs
}

/// Serve `requests` (the open set is known) in order. A send that finds the dispatch
/// gate busy is held, with every later request but a read, until the rest are served;
/// then the held ones run in order, waiting for the gate. So a read filed after a
/// send sees its prompt typed, yet never waits behind a dispatch. Takes the store and
/// hands it back: a borrowed one would keep the future from being `Send`.
async fn serve(
    env: &Arc<DriveEnv>,
    gate: &DispatchGate,
    store: LocalStore,
    open: &HashSet<String>,
    requests: Vec<DriveRequest>,
    log: &mut PassLog<'_>,
    jobs: &mut Vec<Job>,
) -> LocalStore {
    let configured = env.config.worktrees();
    // The engine's views, shared by every request in this pass; dropped after a
    // delivery so the next check sees the session the send just moved.
    let mut snapshot: Option<SessionSets> = None;
    // Drive rows by driven worktree, loaded for the first waking send.
    let mut drives: Option<HashMap<String, Drive>> = None;
    let project_open = open.contains(&env.project);
    let mut queue: VecDeque<DriveRequest> = requests.into();
    let mut held: VecDeque<DriveRequest> = VecDeque::new();
    // Serving the held requests: these wait for the gate.
    let mut on_gate = false;
    loop {
        let req = match queue.pop_front() {
            // Only a read goes ahead of a held request.
            Some(r) if !held.is_empty() && r.kind != "read" => {
                held.push_back(r);
                continue;
            }
            Some(r) => r,
            None => match held.pop_front() {
                Some(r) => {
                    on_gate = true;
                    r
                }
                None => break,
            },
        };
        let parsed = parse_request(&req.kind, &req.payload);
        // A send to a worktree with no running agent wakes it first (unless the send
        // opted out); it then waits, un-begun, for idle like a queued send.
        let wake = if matches!(parsed, Ok(Parsed::Send { wake: true, .. })) && project_open {
            let drives = drives.get_or_insert_with(|| {
                store
                    .drives_list()
                    .map(|ds| ds.into_iter().map(|d| (d.driven.clone(), d)).collect())
                    .unwrap_or_default()
            });
            let facts = GateFacts {
                open,
                configured: &configured,
                drive: drives.get(&req.worktree),
                transfer: transfer_refusal(&env.registry, &env.project, &req.worktree),
                terminated: false,
            };
            wake_gate(env, &req, &facts, env.sets(&mut snapshot))
        } else {
            WakeGate::Proceed { woken: false }
        };
        let mut woke_but_down = None;
        let queued = match wake {
            WakeGate::Proceed { woken } => req.queue || woken,
            WakeGate::Wait(reason) => {
                log.waiting(&store, &req, &reason);
                continue;
            }
            WakeGate::Start(claim) => {
                note_wait(
                    &store,
                    req.id,
                    &wait_reason_for(&WaitFact::Woken(&req.worktree)),
                );
                log.outcome(&req, "job: spawned");
                jobs.push(Box::pin(run_auto_wake(env.clone(), req, claim)));
                continue;
            }
            WakeGate::Refuse(r) => {
                woke_but_down = Some(r);
                false
            }
        };
        // A send that can't go yet waits, un-begun (see [`send_wait`]).
        if woke_but_down.is_none() && matches!(parsed, Ok(Parsed::Send { .. })) && project_open {
            // Checked under the gate when it is free. A busy gate holds only a send it
            // alone keeps from going: one that waits anyway says why, gate or not.
            let free = gate.0.try_lock().ok();
            let mut wait = send_wait(env, &store, &mut snapshot, open, &configured, &req, queued);
            let busy = free.is_none();
            drop(free);
            if busy && wait.is_none() {
                log.waiting(&store, &req, &wait_reason_for(&WaitFact::DispatchGate));
                if !on_gate {
                    held.push_back(req);
                    continue;
                }
                let _g = gate.0.lock().await;
                // A dispatch ran meanwhile: look again.
                snapshot = None;
                wait = send_wait(env, &store, &mut snapshot, open, &configured, &req, queued);
            }
            if let Some(reason) = wait {
                env.recheck_time_hold(req.id, &req.worktree, env.sets(&mut snapshot));
                log.waiting(&store, &req, &reason);
                continue;
            }
        }
        // A wake never starts on stale information: it waits for a fresh view.
        if matches!(
            parsed,
            Ok(Parsed::Restart {
                mode: RestartMode::Wake,
                ..
            })
        ) && project_open
            && env.sets(&mut snapshot).awake(&req.worktree) == Awake::Unknown
        {
            let hold = freshness_hold(&env.sets(&mut snapshot).views);
            log.waiting(
                &store,
                &req,
                &wait_reason_for(&WaitFact::Hold(hold, &req.worktree)),
            );
            continue;
        }
        match store.drive_request_begin(req.id) {
            Ok(true) => log.begun += 1,
            Ok(false) => {
                log.outcome(&req, "skipped: taken by another pass");
                continue;
            }
            Err(e) => {
                log::warn!("drive request {}: cannot begin: {e}", req.id);
                continue;
            }
        }
        let track = env.runtime.track(&env.project, req.id);
        env.runtime.forget_woken(&env.project, req.id);
        if let Some(r) = woke_but_down {
            log.resolve(&store, &req, &Outcome::refused(r));
            continue;
        }
        let parsed = match parsed {
            Ok(p) => p,
            Err(r) => {
                log.resolve(&store, &req, &Outcome::refused(r));
                continue;
            }
        };
        // A wake brings a terminated session back, so that gate doesn't apply to it.
        let is_wake = matches!(
            parsed,
            Parsed::Restart {
                mode: RestartMode::Wake,
                ..
            }
        );
        let drive = store.drive_get(&req.worktree).ok().flatten();
        let refusal = common_refusal(
            &req.kind,
            &env.project,
            &req.worktree,
            &req.requested_by,
            &GateFacts {
                open,
                configured: &configured,
                drive: drive.as_ref(),
                transfer: transfer_refusal(&env.registry, &env.project, &req.worktree),
                terminated: !is_wake && env.terminated(&req.worktree),
            },
        );
        if let Some(r) = refusal {
            log.resolve(&store, &req, &Outcome::refused(r));
            continue;
        }
        let outcome = match parsed {
            Parsed::Send { text, .. } => match sanitize_prompt(&text) {
                Err(r) => Outcome::refused(r),
                Ok(prompt) => {
                    let _g = gate.0.lock().await;
                    match env.send_state(&store, &mut snapshot, &req.worktree, queued) {
                        // Only a race since the check above gets here: refuse.
                        SendGate::Wait(r) | SendGate::Refuse(r) => Outcome::refused(r),
                        SendGate::Ready => {
                            snapshot = None;
                            deliver_send(env, &store, &req, &prompt, queued)
                        }
                    }
                }
            },
            Parsed::Read { lines } => deliver_read(env, &req.worktree, lines),
            p @ (Parsed::Restart { .. } | Parsed::Swap { .. }) => {
                match env.runtime.claim_worktree(&env.project, &req.worktree) {
                    None => Outcome::refused(format!(
                        "another reset, swap or wake is in progress for @{}",
                        req.worktree
                    )),
                    Some(claim) => {
                        log.outcome(&req, "job: spawned");
                        jobs.push(Box::pin(run_job(env.clone(), req, p, claim, track)));
                        continue;
                    }
                }
            }
        };
        if outcome.status == "deferred" {
            // Back to pending at the last moment: the row says why.
            note_wait(&store, req.id, &outcome.reason);
            log.outcome(&req, &format!("deferred: {}", outcome.reason));
            env.recheck_deferred();
            // A send woken for (not `--queue`) keeps waiting like a queued one; a
            // direct send deferred on a transient hold stays direct.
            if queued && !req.queue {
                env.runtime.mark_slot_wait(&env.project, req.id);
            }
            drop(track);
            continue;
        }
        log.resolve(&store, &req, &outcome);
        drop(track);
    }
    store
}

/// Why a send to `req.worktree` stays pending in this pass, `None` when it goes on
/// (to be delivered, or refused at begin): a queued send waits until its target is
/// ready for it; any send waits on a transient condition (herdr not up, a stale view,
/// an agent still starting or settling) unless a common gate refuses it anyway.
fn send_wait(
    env: &DriveEnv,
    store: &LocalStore,
    snapshot: &mut Option<SessionSets>,
    open: &HashSet<String>,
    configured: &[String],
    req: &DriveRequest,
    queued: bool,
) -> Option<String> {
    match env.send_state(store, snapshot, &req.worktree, queued) {
        SendGate::Ready => None,
        SendGate::Wait(r) if queued => Some(r),
        // No session at all: a queued send waits for one.
        SendGate::Refuse(_) if queued => Some(wait_reason_for(&WaitFact::NotIdle {
            worktree: &req.worktree,
            status: "no agent",
        })),
        SendGate::Wait(r) => {
            let drive = store.drive_get(&req.worktree).ok().flatten();
            common_refusal(
                &req.kind,
                &env.project,
                &req.worktree,
                &req.requested_by,
                &GateFacts {
                    open,
                    configured,
                    drive: drive.as_ref(),
                    transfer: transfer_refusal(&env.registry, &env.project, &req.worktree),
                    terminated: env.sets(snapshot).terminated.contains(&req.worktree),
                },
            )
            .is_none()
            .then_some(r)
        }
        SendGate::Refuse(_) => None,
    }
}

/// What a send does about waking its target in this pass.
enum WakeGate {
    /// Go on to the send path; `woken` when the target was woken for this send, so it
    /// waits for idle like a queued send.
    Proceed { woken: bool },
    /// Leave the send pending (a wake is in flight, or just finished), for this reason.
    Wait(String),
    /// The target was woken for this send and is down again: refuse it.
    Refuse(String),
    /// Wake the target now, holding its reset/swap slot.
    Start(InFlightGuard),
}

/// Decide a waking send's step, keeping its wake mark. A send a common gate would
/// refuse (closed project, paused or lost drive, transfer) proceeds so that gate
/// refuses it; the terminated gate is the one a wake lifts, so `facts` leaves it out.
fn wake_gate(
    env: &DriveEnv,
    req: &DriveRequest,
    facts: &GateFacts<'_>,
    sets: &SessionSets,
) -> WakeGate {
    let (rt, project, wt) = (&env.runtime, env.project.as_str(), req.worktree.as_str());
    let mark = rt.wake_mark(project, req.id);
    let gated = common_refusal(&req.kind, project, wt, &req.requested_by, facts);
    let awake = sets.awake(wt);
    if gated.is_some() || awake == Awake::Yes {
        return WakeGate::Proceed {
            woken: mark.is_some(),
        };
    }
    // No fresh view: never wake on stale information.
    if awake == Awake::Unknown {
        return WakeGate::Wait(wait_reason_for(&WaitFact::Hold(
            freshness_hold(&sets.views),
            wt,
        )));
    }
    // A reset, swap or wake holds the slot (this send's own wake, still running, or
    // another request's): wait for it. Waiting isn't waking, so it doesn't start this
    // send's grace period; the holder's end is a DB write or a scheduled pass. The
    // slot is only claimed to start a wake: a claim given up again asks for a pass.
    let slot_wait = || {
        rt.mark_slot_wait(project, req.id);
        WakeGate::Wait(wait_reason_for(&WaitFact::SlotHeld))
    };
    if rt.slot_held(project, wt) {
        return slot_wait();
    }
    match mark {
        // Woken a moment ago: give the live set a moment to show the new session, and
        // make sure a pass comes back once that moment is over.
        Some(WakeMark { own: true, at }) if at.elapsed() < env.timeouts.send_settle => {
            env.recheck_after_settle(req.id, at);
            WakeGate::Wait(wait_reason_for(&WaitFact::Woken(wt)))
        }
        Some(WakeMark { own: true, .. }) => WakeGate::Refuse(format!(
            "@{wt} was woken but its agent did not come up; check its pane"
        )),
        // Not woken for this send yet (at most it waited on another request): wake it.
        _ => match rt.claim_worktree(project, wt) {
            Some(claim) => {
                rt.mark_woken(project, req.id);
                WakeGate::Start(claim)
            }
            None => slot_wait(),
        },
    }
}

/// Wake a send's target, then leave the send pending for the next pass (the woken
/// agent's idle hook write is a DB change that triggers one). A failed wake refuses
/// the send.
async fn run_auto_wake(env: Arc<DriveEnv>, req: DriveRequest, _claim: InFlightGuard) {
    let outcome = run_restart(&env, &req, None, RestartMode::Wake, false).await;
    if outcome.status == "applied" {
        // Restart the grace period from the moment the agent is up, and come back when
        // it ends: an agent that never comes up writes nothing to trigger a pass.
        env.runtime.mark_woken(&env.project, req.id);
        if let Some(mark) = env.runtime.wake_mark(&env.project, req.id) {
            env.recheck_after_settle(req.id, mark.at);
        }
        return;
    }
    env.runtime.forget_woken(&env.project, req.id);
    let Ok(local) = env.store() else { return };
    // Begun here so a concurrent pass can't deliver it meanwhile; a send already
    // taken elsewhere is left to that pass.
    if matches!(local.drive_request_begin(req.id), Ok(true)) {
        let r = format!("could not wake @{}: {}", req.worktree, outcome.reason);
        resolve(&local, &req, &env.project, &Outcome::refused(r));
    }
}

/// Why a fresh `agent list` read forbids typing a prompt now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FreshHold {
    /// The pending reason a deferred queued send carries.
    pub wait: String,
    /// herdr's status as the refusal names it (`starting` for an agent not ready yet).
    pub status: String,
    /// A passing condition (an agent still starting, or a view not yet settled at
    /// this read's seq): any send waits it out, as the pre-delivery gate's waits do.
    /// Otherwise the agent is busy and only a queued send waits.
    pub transient: bool,
}

/// Pure: whether the target may take a prompt now, from a fresh read `obs` and the
/// engine's `view`. It must be `idle`/`done`, launched and input-ready, and the view
/// must be a fresh settled idle at the very same `state_change_seq` (nothing moved
/// between the settled view and this read).
pub(crate) fn fresh_read_hold(obs: &AgentObs, view: &KeyView, worktree: &str) -> Option<FreshHold> {
    let status = if obs.launch_pending || obs.interactive_ready == Some(false) {
        "starting"
    } else {
        obs.agent_status.as_str()
    };
    if !matches!(status, "idle" | "done") {
        return Some(FreshHold {
            wait: wait_reason_for(&WaitFact::NotIdle { worktree, status }),
            status: status.to_string(),
            transient: status == "starting",
        });
    }
    let settled_here = match view {
        KeyView::View(v) => {
            !v.stale
                && v.is_idle_settled()
                && v.state_change_seq == i64::try_from(obs.state_change_seq).ok()
        }
        KeyView::NotReady | KeyView::Missing => false,
    };
    (!settled_here).then(|| FreshHold {
        wait: wait_reason_for(&WaitFact::Hold(Hold::Unsettled, worktree)),
        status: status.to_string(),
        transient: true,
    })
}

/// Pure: the baseline a delivery is judged against, from the fresh read `obs`.
pub(crate) fn baseline_of(obs: &AgentObs, delivered_at_ms: i64) -> Baseline {
    Baseline {
        agent: obs.agent.clone(),
        pane_id: obs.pane_id.clone(),
        terminal_id: obs.terminal_id.clone(),
        agent_session: obs.agent_session.clone(),
        state_change_seq: obs.state_change_seq,
        completion_seq: obs.completion_seq,
        status: obs.agent_status.clone(),
        delivered_at_ms,
    }
}

/// Write a sanitised prompt into an idle session (the caller holds the dispatch gate).
///
/// A fresh `agent list` read right before typing must still show the target settled
/// idle and input-ready; otherwise the send goes back to `pending`
/// ([`Outcome::deferred`]) when it is `queued` or the hold is transient, and is
/// refused when the agent is busy. The read becomes the
/// delivery's [`Baseline`], stored with a new `delivery_id` and `awaiting_turn` in one
/// write before the prompt is typed (so a fast Stop can't be missed) and put back if
/// the prompt wasn't submitted. A delivered send holds the worktree busy until its
/// turn is seen starting.
fn deliver_send(
    env: &DriveEnv,
    local: &LocalStore,
    req: &DriveRequest,
    prompt: &Prompt,
    queued: bool,
) -> Outcome {
    let worktree = req.worktree.as_str();
    let key = env.key(worktree);
    let obs = match env.backend.agent_snapshot(&key) {
        Ok(Some(o)) if o.agent.is_some() => o,
        Ok(_) => {
            let hooks = env.hook_rows();
            let detail = refusal_detail(None, hooks.get(worktree), keitora_tasks::now_ms());
            return Outcome::refused(
                state_refusal("offline", worktree, &detail).unwrap_or_default(),
            );
        }
        Err(e) => {
            let detail = injection_detail(&Err(e), env.terminated(worktree));
            return Outcome::refused(format!("could not send to @{worktree} ({detail})"));
        }
    };
    if let Some(hold) = fresh_read_hold(&obs, &env.engine.view(&key), worktree) {
        if !queued && !hold.transient {
            return Outcome::refused(format!(
                "@{worktree} changed state just before delivery (herdr: {}); use --queue to wait",
                hold.status
            ));
        }
        return match local.drive_request_requeue(req.id) {
            Ok(true) => Outcome::deferred(hold.wait),
            Ok(false) => Outcome::refused(format!("request {} is no longer running", req.id)),
            Err(e) => Outcome::refused(format!("could not requeue the send: {e}")),
        };
    }
    // Put back if the prompt isn't submitted; unreadable means it couldn't be.
    let previous = match local.drive_delivery_get(worktree) {
        Ok(p) => p,
        Err(e) => return Outcome::refused(format!("could not read the delivery: {e}")),
    };
    let baseline = baseline_of(&obs, keitora_tasks::now_ms());
    let (turn, delivery_id) = match local.drive_begin_delivery(worktree, &baseline) {
        Ok(Some(v)) => v,
        Ok(None) => return Outcome::refused(format!("@{worktree} is not driven")),
        Err(e) => return Outcome::refused(format!("could not record the turn: {e}")),
    };
    let restore = || {
        if let Some(prev) = &previous {
            let _ = local.drive_restore_delivery(prev);
        }
    };
    let delay = SUBMIT_DELAY.as_millis() as u32;
    let restore_and_refuse = |result: &Result<(), crate::backend::PtyBackendError>| {
        restore();
        let detail = injection_detail(result, env.terminated(worktree));
        Outcome::refused(format!("could not send to @{worktree} ({detail})"))
    };
    let applied = || {
        env.runtime.mark_sent(
            &env.project,
            worktree,
            delivery_id,
            baseline.state_change_seq,
        );
        env.engine.note_delivery(&key);
        log::info!(
            "drive: delivered to @{worktree} delivery {delivery_id} (herdr {} scs {} cs {})",
            baseline.status,
            baseline.state_change_seq,
            baseline
                .completion_seq
                .map_or_else(|| "-".to_string(), |c| c.to_string()),
        );
        Outcome::applied(
            serde_json::json!({ "awaiting_turn": turn, "delivery_id": delivery_id }).to_string(),
        )
    };
    match prompt {
        Prompt::Line(line) => {
            let result = env.backend.write_line(&key, line, delay);
            match map_write_line_result(&result) {
                WriteLineOutcome::Dispatched => applied(),
                _ => restore_and_refuse(&result),
            }
        }
        Prompt::Block(body) => {
            let pasted = env.backend.paste(&key, body);
            if map_write_line_result(&pasted) != WriteLineOutcome::Dispatched {
                return restore_and_refuse(&pasted);
            }
            std::thread::sleep(SUBMIT_DELAY);
            let enter = env.backend.write_line(&key, "", delay);
            match map_write_line_result(&enter) {
                WriteLineOutcome::Dispatched => applied(),
                _ => {
                    restore();
                    let detail = injection_detail(&enter, env.terminated(worktree));
                    Outcome::refused(format!(
                        "pasted but could not submit to @{worktree} ({detail}); \
                         the text may be left in its input box"
                    ))
                }
            }
        }
    }
}

/// Read the last `lines` lines of the pane's history, capped to the result size.
fn deliver_read(env: &DriveEnv, worktree: &str, lines: u32) -> Outcome {
    match env.backend.read_history(&env.key(worktree), lines) {
        Ok(text) => Outcome::applied(tail_cap(&text, DRIVE_READ_RESULT_MAX)),
        Err(e) => Outcome::refused(format!("could not read @{worktree}: {e}")),
    }
}

/// A spawned reset/swap: run it and record its outcome.
async fn run_job(
    env: Arc<DriveEnv>,
    req: DriveRequest,
    parsed: Parsed,
    _claim: InFlightGuard,
    _track: TrackGuard,
) {
    let outcome = match parsed {
        Parsed::Restart { to, mode, fresh } => run_restart(&env, &req, to, mode, fresh).await,
        Parsed::Swap { to } => run_swap(&env, &req, &to).await,
        _ => Outcome::refused("not a reset, swap or wake"),
    };
    match env.store() {
        Ok(local) => resolve(&local, &req, &env.project, &outcome),
        Err(e) => log::warn!("drive request {}: cannot record outcome: {e}", req.id),
    }
}

/// Clear `awaiting_turn` after a reset/swap: the old turn no longer exists.
fn clear_awaiting_turn(env: &DriveEnv, worktree: &str) {
    if let Ok(local) = env.store() {
        let _ = local.drive_restore_awaiting_turn(worktree, None);
    }
    env.runtime.clear_sent(&env.project, worktree);
}

/// The next report that ends a phase (an `Accepted` outside the confirm phase is
/// ignored), or `None` on timeout or a closed channel (`Some(Err(reason))`).
async fn next_report(
    rx: &mut mpsc::UnboundedReceiver<DriveReport>,
    deadline: tokio::time::Instant,
    accept_ends_phase: bool,
) -> Result<DriveReport, Option<String>> {
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return Err(None),
            Ok(None) => return Err(Some("request is no longer waiting for Keitora".to_string())),
            Ok(Some(DriveReport::Accepted)) if !accept_ends_phase => continue,
            Ok(Some(r)) => return Ok(r),
        }
    }
}

/// Reset or wake, optionally on another provider. A reset restarts the pane fresh. A
/// wake starts the agent when it isn't running (resuming unless `fresh`); a running
/// agent is left alone: applied with `already_awake`, or refused when `to` asks for a
/// provider it isn't on (that is a reset).
async fn run_restart(
    env: &DriveEnv,
    req: &DriveRequest,
    to: Option<String>,
    mode: RestartMode,
    fresh: bool,
) -> Outcome {
    let wt = req.worktree.as_str();
    let to = match to.as_deref().map(parse_provider).transpose() {
        Ok(t) => t,
        Err(r) => return Outcome::refused(r),
    };
    if mode == RestartMode::Wake && env.awake(wt) == Awake::Yes {
        let current = env.host.current_cli(&env.project, &env.config, wt);
        return match to {
            Some(c) if c != current => Outcome::refused(format!(
                "@{wt} is already awake on {}; use reset --to {} to switch",
                current.spec().config_key,
                c.spec().config_key
            )),
            _ => Outcome::applied(
                serde_json::json!({ "provider": current.spec().config_key, "already_awake": true })
                    .to_string(),
            ),
        };
    }
    relaunch(env, req, to, mode, fresh).await
}

/// The window half of a reset or wake: switch the provider when `to` differs, ask the
/// window to restart or wake the pane, and roll the provider back if that doesn't
/// happen. The awaited turn is cleared on success: the old session's turn is gone.
async fn relaunch(
    env: &DriveEnv,
    req: &DriveRequest,
    to: Option<CliKind>,
    mode: RestartMode,
    fresh: bool,
) -> Outcome {
    let wt = req.worktree.as_str();
    let previous = env.host.current_cli(&env.project, &env.config, wt);
    let changed = match to {
        Some(c) if c != previous => {
            if let Err(e) = env.host.persist_cli(&env.config, wt, c) {
                return Outcome::refused(format!(
                    "could not switch @{wt} to {}: {e}",
                    c.spec().config_key
                ));
            }
            env.host.worktrees_changed();
            true
        }
        _ => false,
    };
    let now_on = to.unwrap_or(previous);
    // The restart spawns the persisted provider, and the spawn writes its hooks
    // before the agent starts. herdr's integration for a provider found late is
    // installed here, before the restart, only on a switch onto it (as a window
    // switch does): a plain reset or wake of a worktree already on it skips this.
    if changed && now_on.spec().integration_on_switch {
        let host = env.host.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || host.refresh_integration()).await {
            log::warn!("drive reset: refreshing herdr's integration for @{wt} panicked: {e}");
        }
    }

    let (mut rx, _waiter) = env
        .runtime
        .register_waiter(&env.project, req.id, WaitKind::Reset);
    env.host.emit_restart(&DriveRestartEvent {
        request_id: req.id,
        project: env.project.clone(),
        worktree: wt.to_string(),
        driver: req.requested_by.clone(),
        mode,
        fresh,
        replace: mode == RestartMode::Reset || fresh || changed,
    });
    let deadline = tokio::time::Instant::now() + env.timeouts.reset;
    let failure = match next_report(&mut rx, deadline, false).await {
        Ok(DriveReport::Applied(_)) => None,
        Ok(DriveReport::Refused(r)) => Some(r),
        Ok(DriveReport::Accepted) => Some("only a swap is confirmed".to_string()),
        Err(Some(r)) => Some(r),
        Err(None) => Some(format!(
            "the Keitora window did not {} @{wt} within {} s",
            mode.verb(),
            env.timeouts.reset.as_secs()
        )),
    };
    match failure {
        None => {
            clear_awaiting_turn(env, wt);
            let mut result = serde_json::json!({ "provider": now_on.spec().config_key });
            if mode == RestartMode::Wake {
                result["already_awake"] = serde_json::json!(false);
            }
            Outcome::applied(result.to_string())
        }
        Some(r) => {
            let mut reason = format!("{} failed: {r}", mode.label());
            if changed {
                match env.host.persist_cli(&env.config, wt, previous) {
                    Ok(()) => {
                        env.host.worktrees_changed();
                        reason.push_str(&format!(
                            " (provider restored to {})",
                            previous.spec().config_key
                        ));
                    }
                    Err(e) => reason.push_str(&format!(
                        " (could not restore provider {}: {e})",
                        previous.spec().config_key
                    )),
                }
            }
            Outcome::refused(reason)
        }
    }
}

/// Swap: ask the window to confirm and run the claude <-> codex transfer. The
/// transfer engine owns its persistence, so there is no rollback here.
async fn run_swap(env: &DriveEnv, req: &DriveRequest, to: &str) -> Outcome {
    let wt = req.worktree.as_str();
    let to = match parse_provider(to) {
        Ok(t) => t,
        Err(r) => return Outcome::refused(r),
    };
    let current = env.host.current_cli(&env.project, &env.config, wt);
    if let Some(r) = swap_refusal(wt, current, to) {
        return Outcome::refused(r);
    }

    let (mut rx, _waiter) = env
        .runtime
        .register_waiter(&env.project, req.id, WaitKind::Swap);
    let confirming = env.store().and_then(|l| {
        l.drive_request_await_confirm(req.id)
            .map_err(|e| e.to_string())
    });
    if confirming != Ok(true) {
        return Outcome::refused("the request is no longer running");
    }
    env.host.emit_swap(&DriveSwapEvent {
        request_id: req.id,
        project: env.project.clone(),
        worktree: wt.to_string(),
        driver: req.requested_by.clone(),
        to,
    });
    let applied = || {
        clear_awaiting_turn(env, wt);
        Outcome::applied(serde_json::json!({ "provider": to.spec().config_key }).to_string())
    };

    let confirm_deadline = tokio::time::Instant::now() + env.timeouts.swap_confirm;
    match next_report(&mut rx, confirm_deadline, true).await {
        Ok(DriveReport::Accepted) => {}
        Ok(DriveReport::Applied(_)) => return applied(),
        Ok(DriveReport::Refused(r)) => return Outcome::refused(r),
        Err(Some(r)) => return Outcome::refused(r),
        Err(None) => {
            return Outcome::refused(format!(
                "swap was not confirmed in Keitora within {}",
                human_duration(env.timeouts.swap_confirm)
            ))
        }
    }
    match env
        .store()
        .and_then(|l| l.drive_request_resume(req.id).map_err(|e| e.to_string()))
    {
        Ok(true) => {}
        Ok(false) => log::warn!("drive request {}: was no longer awaiting confirm", req.id),
        Err(e) => log::warn!("drive request {}: cannot resume: {e}", req.id),
    }

    let run_deadline = tokio::time::Instant::now() + env.timeouts.swap_run;
    match next_report(&mut rx, run_deadline, false).await {
        Ok(DriveReport::Applied(_)) => applied(),
        Ok(DriveReport::Refused(r)) => Outcome::refused(r),
        Ok(DriveReport::Accepted) => Outcome::refused("the swap was confirmed twice"),
        Err(Some(r)) => Outcome::refused(r),
        Err(None) => Outcome::refused(format!(
            "the transfer did not finish within {}; check @{wt}'s pane",
            human_duration(env.timeouts.swap_run)
        )),
    }
}

/// Execute `project`'s pending drive requests. `project` is a resolved
/// `[project].name` from the registry; a requested pass (see
/// [`DriveRuntime::request_pass`]) runs this when it is due. Safe to run concurrently
/// with itself: each row is taken with a compare-and-set.
pub(crate) async fn run_drive_requests<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project: String,
    trigger: Trigger,
) {
    use tauri::Manager;

    let (Some(client), Some(gate), Some(registry), Some(runtime), Some(engine)) = (
        app.try_state::<SharedBackend>(),
        app.try_state::<DispatchGate>(),
        app.try_state::<TransferRegistry>(),
        app.try_state::<DriveRuntime>(),
        app.try_state::<SharedEngine>(),
    ) else {
        return; // still starting up; the next DB event retries
    };
    let Some(config) = crate::project_stores::registry().by_name(&project).cloned() else {
        log::warn!("drive requests: project {project} is not registered");
        return;
    };
    let env = Arc::new(DriveEnv {
        project,
        db_path: config.tasks.db_path.to_string_lossy().into_owned(),
        config,
        backend: client.inner().clone(),
        registry: registry.inner().clone(),
        runtime: runtime.inner().clone(),
        host: Arc::new(TauriHost(app.clone())),
        timeouts: Timeouts::default(),
        engine: engine.inner().clone(),
    });
    for job in run_pass(&env, &gate, trigger).await {
        tauri::async_runtime::spawn(job);
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A pending brokered prompt, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingPromptView {
    pub id: i64,
    /// `permission` or `question`.
    pub kind: String,
    pub tool_name: String,
    pub summary: String,
    pub created_at: String,
}

/// One drive binding with its pending prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriveView {
    pub driven: String,
    pub driver: String,
    pub since: String,
    pub forced_from: Option<String>,
    pub paused: bool,
    pub escalate_after_s: i64,
    pub awaiting_turn: Option<i64>,
    pub pending: Vec<PendingPromptView>,
}

/// The project's drives, each with its live pending prompts.
fn drive_views(local: &LocalStore) -> Result<Vec<DriveView>, String> {
    let drives = local.drives_list().map_err(|e| e.to_string())?;
    let mut by_driven: HashMap<String, Vec<PendingPromptView>> = HashMap::new();
    for p in local
        .drive_prompts_pending(None, None)
        .map_err(|e| e.to_string())?
    {
        by_driven
            .entry(p.driven.clone())
            .or_default()
            .push(PendingPromptView {
                id: p.id,
                kind: p.kind,
                tool_name: p.tool_name,
                summary: p.summary,
                created_at: p.created_at,
            });
    }
    Ok(drives
        .into_iter()
        .map(|d| DriveView {
            pending: by_driven.remove(&d.driven).unwrap_or_default(),
            driven: d.driven,
            driver: d.driver,
            since: d.since,
            forced_from: d.forced_from,
            paused: d.paused,
            escalate_after_s: d.escalate_after_s,
            awaiting_turn: d.awaiting_turn,
        })
        .collect())
}

/// A registered project's config by resolved name.
fn project_config(project: &str) -> Result<ProjectConfig, String> {
    keitora_tasks::UserConfig::load()
        .map_err(|e| e.to_string())?
        .project_config_by_name(project)
        .ok_or_else(|| format!("project {project} is not registered"))
}

/// Open a registered project's task store by resolved name.
fn project_store(project: &str) -> Result<LocalStore, String> {
    let config = project_config(project)?;
    LocalStore::open(&config.tasks.db_path.to_string_lossy()).map_err(|e| e.to_string())
}

/// The human ends `driven`'s drive (its pending prompts go to the native dialog).
fn release_in(local: &LocalStore, driven: &str) -> Result<(), String> {
    local
        .drive_end(driven)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The human takes over `driven`'s pending prompts; returns how many.
fn take_over_in(local: &LocalStore, driven: &str) -> Result<u32, String> {
    local
        .drive_take_over(driven)
        .map(|n| n as u32)
        .map_err(|e| e.to_string())
}

/// The human pauses or resumes `driven`'s drive.
fn pause_in(local: &LocalStore, driven: &str, paused: bool) -> Result<(), String> {
    local
        .drive_set_paused(driven, paused)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// A project's drives and their pending prompts. Reads through a read-only handle
/// (no schema init); a DB that doesn't exist yet falls back to the creating open.
#[tauri::command(async)]
pub fn drives_list(project: String) -> Result<Vec<DriveView>, String> {
    let config = project_config(&project)?;
    let db_path = config.tasks.db_path.to_string_lossy().into_owned();
    let local = LocalStore::open_read_only(&db_path)
        .or_else(|_| LocalStore::open(&db_path))
        .map_err(|e| e.to_string())?;
    drive_views(&local)
}

/// End `driven`'s drive. Errors when it isn't driven.
#[tauri::command(async)]
pub fn drive_release(project: String, driven: String) -> Result<(), String> {
    release_in(&project_store(&project)?, &driven)
}

/// Take over `driven`'s pending prompts (the binding stays); returns the count.
#[tauri::command(async)]
pub fn drive_take_over(project: String, driven: String) -> Result<u32, String> {
    take_over_in(&project_store(&project)?, &driven)
}

/// Pause or resume `driven`'s drive.
#[tauri::command(async)]
pub fn drive_pause(project: String, driven: String, paused: bool) -> Result<(), String> {
    pause_in(&project_store(&project)?, &driven, paused)
}

/// Forward the window's report for a reset/swap request (`outcome`: `accepted`,
/// `applied` or `refused`) to the runner waiting on it.
#[tauri::command(async)]
pub fn drive_request_resolve(
    project: String,
    request_id: i64,
    outcome: String,
    reason: String,
    runtime: State<'_, DriveRuntime>,
) -> Result<(), String> {
    runtime.report(&project, request_id, &outcome, reason)
}

/// The window's open (warm) projects, by resolved name. The engine polls warm while
/// any is open. A first report (also the first after the window closed) or a changed
/// one gives every registered project with pending requests a pass.
#[tauri::command(async)]
pub fn drive_set_open_projects(
    projects: Vec<String>,
    runtime: State<'_, DriveRuntime>,
    engine: State<'_, SharedEngine>,
) -> Result<(), String> {
    let registry = crate::project_stores::registry();
    report_open_projects(
        &runtime,
        &engine,
        projects,
        registry.projects.iter().map(|(_, pc)| pc),
        &crate::project_stores::shared(),
    );
    Ok(())
}

/// Take the window's report: store the set, then, unless it is unchanged, ask for a
/// pass over every one of `registered` with pending requests.
pub(crate) fn report_open_projects<'a>(
    runtime: &DriveRuntime,
    engine: &SharedEngine,
    projects: Vec<String>,
    registered: impl Iterator<Item = &'a ProjectConfig>,
    stores: &crate::project_stores::ReadStores,
) -> OpenChange {
    let change = set_open_projects(runtime, engine, projects);
    log::debug!("drive: window reported its open projects ({change:?})");
    if change != OpenChange::Same {
        request_pending_passes_in(runtime, registered, stores, |_| true, Trigger::OpenSet);
    }
    change
}

/// Store the window's open projects and set the engine's warm cadence from them.
fn set_open_projects(
    runtime: &DriveRuntime,
    engine: &SharedEngine,
    projects: Vec<String>,
) -> OpenChange {
    engine.set_window_open(!projects.is_empty());
    runtime.set_open(projects)
}

/// Pure: whether a view change can let a waiting request go: the view appeared or
/// went away, settled, its agent came up, it left `busy`/`waiting`, or it turned
/// fresh.
pub(crate) fn could_unblock(c: &ViewChange) -> bool {
    let (Some(prev), Some(next)) = (&c.prev, &c.next) else {
        return true;
    };
    (next.settled && !prev.settled)
        || (prev.agent != "present" && next.agent == "present")
        || (matches!(prev.state.as_str(), "busy" | "waiting")
            && !matches!(next.state.as_str(), "busy" | "waiting"))
        || (prev.stale && !next.stale)
}

/// Ask for a pass over each project whose pending requests an engine change could
/// unblock: a view change of a worktree with a pending request (see
/// [`could_unblock`]), or, for a link or readiness change (an empty batch, which
/// covers a project with no session at all), every project with pending requests.
/// Cheap: no store access on the status loop's thread.
pub(crate) fn watch_engine(engine: &SharedEngine, runtime: &DriveRuntime) {
    let rt = runtime.clone();
    engine.subscribe(Box::new(move |changes| {
        let mut projects: Vec<String> = if changes.is_empty() {
            rt.pending_projects()
        } else {
            changes
                .iter()
                .filter(|c| rt.has_pending_for(&c.key.project, &c.key.worktree) && could_unblock(c))
                .map(|c| c.key.project.clone())
                .collect()
        };
        projects.sort();
        projects.dedup();
        for p in projects {
            rt.request_pass(&p, Duration::ZERO, Trigger::Engine);
        }
    }));
}

/// Ask for a pass over every one of `projects` that `which` accepts and has pending
/// drive or dispatch requests (read through `stores`' shared read-only handles,
/// status only, so a DB of any age works and a missing one has nothing pending).
pub(crate) fn request_pending_passes_in<'a>(
    runtime: &DriveRuntime,
    projects: impl Iterator<Item = &'a ProjectConfig>,
    stores: &crate::project_stores::ReadStores,
    which: impl Fn(&str) -> bool,
    why: Trigger,
) {
    for pc in projects {
        let project = &pc.project.name;
        if !which(project) {
            continue;
        }
        let pending = stores
            .with(&pc.tasks.db_path, |rs| {
                Ok(rs.store.drive_requests_has_pending()?
                    || rs.store.dispatch_requests_has_pending()?)
            })
            .unwrap_or(false);
        if pending {
            runtime.request_pass(project, Duration::ZERO, why);
        }
    }
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
