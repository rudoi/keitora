//! `status_loop` — one backend loop for everything that used to be polled.
//!
//! The loop wakes every [`WAKE`] and only does what is due: an in-process
//! `sysinfo` sample of the server (the rail card's CPU/memory, pushed as
//! `daemon://stats`), a `herdr status server` probe (liveness + version, fed to the
//! [`Watchdog`]), a `pane list` that keeps the resume store and the recovery
//! controller's busy/recency view fresh, and the `daemon://status` push (on change
//! plus a heartbeat). On a loaded machine (low headroom) it slows down, so Keitora
//! spawns fewer herdr processes exactly when memory is short.
//!
//! [`StatusLoop::tick`] takes `now` and every dependency is injected, so the whole
//! cadence is testable without sleeping, a real clock or a real herdr.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::EmitSink;
use crate::backend::{DaemonStats, SessionKey, StatusPayload, HINT_BUSY};
use crate::headroom::{Headroom, HeadroomSnapshot, PHYS_MARGIN_BYTES};

use super::cli::{PaneInfo, ServerStatus};
use super::exe_missing::ExeMissing;
use super::gate::ServerGate;
use super::keymap::{HerdrPane, KeyMap};
use super::ops::{HerdrError, HerdrOps};
use super::recovery::machine::Observation;
use super::recovery::RecoveryHandle;
use super::resume::{observation_pairs, trusted_observations, ResumeStore};
use super::supervisor::Supervisor;
use super::watchdog::{classify_sample, ProbeSample, Watchdog};
use super::{agent, build_status_payload, sample_process_stats};

/// How often the loop wakes to check what is due.
pub const WAKE: Duration = Duration::from_secs(1);
/// Unconditional `daemon://status` re-emit (the frontend's safety net for a
/// dropped event).
pub const HEARTBEAT: Duration = Duration::from_secs(30);
/// How often memory headroom is sampled to pick the cadence.
pub const HEADROOM_EVERY: Duration = Duration::from_secs(5);
/// How old a cached running status may be for a pulled snapshot to use it.
pub const SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(15);
/// How old a pane list may be for the session listing to reuse it.
pub const SESSIONS_MAX_AGE: Duration = Duration::from_secs(5);
/// [`SESSIONS_MAX_AGE`] while memory headroom is low.
pub const LOW_SESSIONS_MAX_AGE: Duration = Duration::from_secs(30);
/// The first retry after a failed probe; each further failure doubles it, up to
/// the cadence's probe interval.
pub const PROBE_RETRY_BASE: Duration = Duration::from_secs(2);

/// Event carrying [`DaemonStats`].
pub const STATS_EVENT: &str = "daemon://stats";
/// Event carrying [`StatusPayload`].
pub const STATUS_EVENT: &str = "daemon://status";

/// How often each kind of work runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cadence {
    /// In-process sample (stats) and status payload rebuild.
    pub sample: Duration,
    /// `status server` probe.
    pub probe: Duration,
    /// `pane list` observation.
    pub observe: Duration,
}

/// The normal cadence.
pub const HEALTHY_CADENCE: Cadence = Cadence {
    sample: Duration::from_secs(2),
    probe: Duration::from_secs(10),
    observe: Duration::from_secs(30),
};

/// The cadence while memory headroom is low: observation is slowed, never off (a
/// fresh resume store matters most right before a death).
pub const LOW_CADENCE: Cadence = Cadence {
    sample: Duration::from_secs(10),
    probe: Duration::from_secs(30),
    observe: Duration::from_secs(60),
};

impl Cadence {
    pub fn for_mode(low: bool) -> Cadence {
        if low {
            LOW_CADENCE
        } else {
            HEALTHY_CADENCE
        }
    }
}

/// How long after the last probe the next one is due, given `fails`
/// consecutive failures: the cadence's interval while healthy, otherwise a
/// backoff from [`PROBE_RETRY_BASE`] capped at that interval.
pub fn probe_interval(fails: u32, cadence: Cadence) -> Duration {
    if fails == 0 {
        return cadence.probe;
    }
    let doublings = (fails - 1).min(16);
    PROBE_RETRY_BASE
        .saturating_mul(1u32 << doublings)
        .min(cadence.probe)
}

/// Whether something last done at `last` (never: `None`) is due again at `now`.
pub fn due(now: Instant, last: Option<Instant>, interval: Duration) -> bool {
    last.is_none_or(|l| now.saturating_duration_since(l) >= interval)
}

/// Low-headroom mode with hysteresis: enter as soon as commit or physical memory
/// is below its threshold; leave only after two consecutive samples with both at
/// least 1.25x their threshold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LowHeadroom {
    low: bool,
    good_streak: u8,
}

impl LowHeadroom {
    pub fn is_low(&self) -> bool {
        self.low
    }

    /// Feed one sample; returns the new mode when it changed. An unavailable
    /// sample changes nothing.
    pub fn update(&mut self, snap: Option<HeadroomSnapshot>, base: u64) -> Option<bool> {
        let snap = snap?;
        let short = snap.commit_avail < base || snap.phys_avail < PHYS_MARGIN_BYTES;
        if !self.low {
            if short {
                self.low = true;
                self.good_streak = 0;
                return Some(true);
            }
            return None;
        }
        let ample = snap.commit_avail >= base + base / 4
            && snap.phys_avail >= PHYS_MARGIN_BYTES + PHYS_MARGIN_BYTES / 4;
        if !ample {
            self.good_streak = 0;
            return None;
        }
        self.good_streak += 1;
        if self.good_streak >= 2 {
            self.low = false;
            self.good_streak = 0;
            return Some(false);
        }
        None
    }
}

/// What the loop last learned, read by the pulled commands.
#[derive(Clone, Debug, Default)]
pub struct StatusCache {
    /// The last `status server` answer and when it was taken.
    pub server: Option<(ServerStatus, Instant)>,
    /// The last stats sample (`running: false` default when not running).
    pub stats: DaemonStats,
    /// The last pane list, when it was taken and the keymap version it was
    /// taken at.
    pub panes: Option<(Vec<PaneInfo>, Instant, u64)>,
    /// The loop runs at the low-headroom cadence.
    pub low: bool,
}

pub type SharedStatusCache = Arc<Mutex<StatusCache>>;

/// The panes the session listing reports: the cached list while it is recent
/// (younger than the cadence's [`SESSIONS_MAX_AGE`]) and the keymap has not
/// changed since, else one `pane list` that refreshes the cache.
pub fn session_panes(
    ops: &dyn HerdrOps,
    keymap: &KeyMap,
    cache: &Mutex<StatusCache>,
    now: Instant,
) -> Result<Vec<PaneInfo>, HerdrError> {
    let version = keymap.version();
    {
        let c = cache.lock().unwrap();
        let max_age = if c.low {
            LOW_SESSIONS_MAX_AGE
        } else {
            SESSIONS_MAX_AGE
        };
        if let Some((panes, at, v)) = &c.panes {
            if *v == version && now.saturating_duration_since(*at) < max_age {
                return Ok(panes.clone());
            }
        }
    }
    let panes = ops.pane_list()?;
    cache.lock().unwrap().panes = Some((panes.clone(), now, version));
    Ok(panes)
}

/// The last pane list the cache holds, however old and whatever keymap version
/// it was taken at; `None` before the first list. Never calls herdr, so a
/// frequent UI poll can read it without a process spawn.
pub fn cached_session_panes(cache: &Mutex<StatusCache>) -> Option<Vec<PaneInfo>> {
    cache
        .lock()
        .unwrap()
        .panes
        .as_ref()
        .map(|(panes, _, _)| panes.clone())
}

/// The cached status when it is fresh enough to answer a pulled snapshot without
/// a herdr call: present, running and at most [`SNAPSHOT_MAX_AGE`] old.
pub fn warm_status(cache: &StatusCache, now: Instant) -> Option<ServerStatus> {
    let (s, at) = cache.server.as_ref()?;
    (s.running && now.saturating_duration_since(*at) <= SNAPSHOT_MAX_AGE).then(|| s.clone())
}

/// The status a pulled snapshot reports: the warm cache, else one live probe that
/// refreshes the cache (a failed probe leaves it as it was and reports nothing).
pub fn snapshot_status(
    ops: &dyn HerdrOps,
    cache: &Mutex<StatusCache>,
    now: Instant,
) -> Option<ServerStatus> {
    if let Some(s) = warm_status(&cache.lock().unwrap(), now) {
        return Some(s);
    }
    let s = ops.status_server().ok()?;
    cache.lock().unwrap().server = Some((s.clone(), now));
    Some(s)
}

/// The recovery controller's view of who was mid-turn: one [`Observation`] per
/// keymap entry with a live pane, busy when its agent is working or blocked.
pub fn observations_from(
    entries: &[(SessionKey, HerdrPane)],
    panes: &[PaneInfo],
) -> Vec<Observation> {
    entries
        .iter()
        .filter_map(|(key, entry)| {
            let pane = panes.iter().find(|p| p.pane_id == entry.pane_id)?;
            let status = pane.agent_status.as_deref().unwrap_or("");
            let busy =
                agent::agent_status_to_state_hint(status, false).as_deref() == Some(HINT_BUSY);
            Some(Observation {
                key: key.clone(),
                busy,
            })
        })
        .collect()
}

/// Sleep `d` in short slices, returning early once `stop` is set.
fn sleep_unless(stop: &AtomicBool, d: Duration) {
    const SLICE: Duration = Duration::from_millis(100);
    let mut left = d;
    while !left.is_zero() && !stop.load(Ordering::SeqCst) {
        let step = left.min(SLICE);
        std::thread::sleep(step);
        left -= step;
    }
}

/// Finds the server's OS pid for the stats sample.
pub type PidSample = Box<dyn FnMut(&mut sysinfo::System) -> Option<u32> + Send>;

/// Everything the loop reads or feeds.
pub struct StatusLoopDeps {
    pub ops: Arc<dyn HerdrOps>,
    pub gate: Arc<dyn ServerGate>,
    pub headroom: Arc<dyn Headroom>,
    /// The commit margin the recovery gate uses; below it the loop slows down.
    pub base_margin_bytes: u64,
    pub store: Arc<ResumeStore>,
    pub keymap: Arc<KeyMap>,
    pub recovery: RecoveryHandle,
    pub sink: Arc<dyn EmitSink>,
    pub explicit_stop: Arc<AtomicBool>,
    pub supervisor: Arc<Supervisor>,
    pub live: Arc<Mutex<HashSet<SessionKey>>>,
    pub watchdog: Arc<Watchdog>,
    pub cache: SharedStatusCache,
    /// `None`: ask `ops.server_pid`.
    pub pid_sample: Option<PidSample>,
    /// The backend's "herdr was not found" state, kept current by each probe.
    pub exe_missing: Arc<ExeMissing>,
    /// Whether the launch decision about herdr has been made (the keymap is
    /// reconciled, or recovery owns the server). Nothing is observed before.
    pub boot_ready: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// The merged status loop. See the module docs.
pub struct StatusLoop {
    deps: StatusLoopDeps,
    pid_sample: PidSample,
    sys: sysinfo::System,
    low: LowHeadroom,
    last_headroom: Option<Instant>,
    last_sample: Option<Instant>,
    last_probe: Option<Instant>,
    last_observe: Option<Instant>,
    /// The last emitted status payload and when.
    last_status: Option<(StatusPayload, Instant)>,
    /// Whether the last emitted stats said running (`None`: nothing emitted).
    stats_running: Option<bool>,
    /// The explicit-stop payload went out for the current stop.
    stop_reported: bool,
    /// The gate refused (or the user stopped) on an earlier tick: probe at once
    /// when that clears.
    was_unhealthy: bool,
    /// Consecutive failed probes; re-probes back off (see [`probe_interval`]).
    probe_fails: u32,
    /// The pid the last sample found.
    last_pid: Option<u32>,
}

impl StatusLoop {
    pub fn new(mut deps: StatusLoopDeps) -> StatusLoop {
        let pid_sample = deps.pid_sample.take().unwrap_or_else(|| {
            let ops = Arc::clone(&deps.ops);
            Box::new(move |sys: &mut sysinfo::System| ops.server_pid(sys))
        });
        StatusLoop {
            deps,
            pid_sample,
            sys: sysinfo::System::new(),
            low: LowHeadroom::default(),
            last_headroom: None,
            last_sample: None,
            last_probe: None,
            last_observe: None,
            last_status: None,
            stats_running: None,
            stop_reported: false,
            was_unhealthy: false,
            probe_fails: 0,
            last_pid: None,
        }
    }

    /// Whether the loop is in low-headroom mode.
    #[cfg(test)]
    pub fn is_low(&self) -> bool {
        self.low.is_low()
    }

    /// One wake: do whatever is due at `now`.
    pub fn tick(&mut self, now: Instant) {
        // 1. The user stopped the server: say so once, touch nothing.
        if self.deps.explicit_stop.load(Ordering::SeqCst) {
            if !self.stop_reported {
                self.stop_reported = true;
                self.was_unhealthy = true;
                self.emit_status(now, None, true);
                self.set_stats(DaemonStats::default());
            }
            return;
        }
        self.stop_reported = false;

        if due(now, self.last_headroom, HEADROOM_EVERY) {
            self.last_headroom = Some(now);
            let snap = self.deps.headroom.sample();
            if let Some(low) = self.low.update(snap, self.deps.base_margin_bytes) {
                log::info!(
                    "herdr: status loop cadence low={low} commit_avail={:?}",
                    snap.map(|s| s.commit_avail)
                );
            }
        }
        let cadence = Cadence::for_mode(self.low.is_low());
        self.deps.cache.lock().unwrap().low = self.low.is_low();

        // 2. Recovery owns the server: no herdr calls at all.
        if self.deps.gate.admit(None).is_err() {
            self.was_unhealthy = true;
            if due(now, self.last_sample, cadence.sample) {
                self.last_sample = Some(now);
                self.set_stats(DaemonStats::default());
            }
            self.emit_status(now, None, false);
            return;
        }

        // 3. Probe.
        let interval = probe_interval(self.probe_fails, cadence);
        if due(now, self.last_probe, interval) || self.was_unhealthy {
            self.probe(now);
        }
        self.was_unhealthy = false;

        // 4. Sample.
        if due(now, self.last_sample, cadence.sample) {
            self.last_sample = Some(now);
            self.sample(now);
        }

        // 5. Status push (cheap: built from the cache).
        let status = self.cached_running();
        self.emit_status(now, status, false);

        // 7. Observe (only once the launch decision is made: before it, the
        // keymap may still describe a server that was replaced).
        if due(now, self.last_observe, cadence.observe)
            && self.cached_running().is_some()
            && (self.deps.boot_ready)()
        {
            self.last_observe = Some(now);
            self.observe(now);
        }
    }

    /// Run until `stop` is set, waking every [`WAKE`].
    pub fn run(self, stop: Arc<AtomicBool>) {
        let flag = Arc::clone(&stop);
        self.run_with(&stop, Instant::now, move |d| sleep_unless(&flag, d));
    }

    /// [`Self::run`] over an injected clock and sleep.
    pub fn run_with(
        mut self,
        stop: &AtomicBool,
        mut now: impl FnMut() -> Instant,
        mut sleep: impl FnMut(Duration),
    ) {
        while !stop.load(Ordering::SeqCst) {
            self.tick(now());
            sleep(WAKE);
        }
    }

    /// The cached status if it says running.
    fn cached_running(&self) -> Option<ServerStatus> {
        let cache = self.deps.cache.lock().unwrap();
        cache
            .server
            .as_ref()
            .filter(|(s, _)| s.running)
            .map(|(s, _)| s.clone())
    }

    /// Whether a probe result means anything to the watchdog: not while a stop
    /// is expected, and not for our own child (its waiter is authoritative).
    fn feeds_watchdog(&self) -> bool {
        !self.deps.watchdog.stop_expected() && self.deps.supervisor.spawned_pid().is_none()
    }

    /// Feed one result to the watchdog (when it means anything). A failure
    /// that is not a refusal is checked against the server's process, which a
    /// slow server keeps while a dead one does not.
    fn report(&mut self, r: &Result<ServerStatus, HerdrError>) {
        if !self.feeds_watchdog() {
            return;
        }
        let pid_alive = match classify_sample(r) {
            ProbeSample::Timeout | ProbeSample::ExitFail => {
                (self.pid_sample)(&mut self.sys).is_some()
            }
            _ => false,
        };
        self.deps.watchdog.report_probe(r, pid_alive);
    }

    fn probe(&mut self, now: Instant) {
        self.last_probe = Some(now);
        let r = self.deps.ops.status_server();
        self.report(&r);
        // A herdr executable that went missing (or came back) changes what the
        // UI is told, on both events; it is never a server death (see `report`).
        super::observe_exe(self.deps.sink.as_ref(), &self.deps.exe_missing, &r, &|| {
            self.deps.recovery.snapshot()
        });
        let mut cache = self.deps.cache.lock().unwrap();
        match r {
            Ok(s) => {
                self.probe_fails = if s.running {
                    0
                } else {
                    self.probe_fails.saturating_add(1)
                };
                cache.server = Some((s, now));
            }
            Err(_) => {
                // Keep what we knew (the version), but it is no longer running.
                self.probe_fails = self.probe_fails.saturating_add(1);
                if let Some((s, _)) = cache.server.as_mut() {
                    s.running = false;
                }
            }
        }
    }

    fn sample(&mut self, now: Instant) {
        if self.cached_running().is_none() {
            self.last_pid = None;
            self.set_stats(DaemonStats::default());
            return;
        }
        let mut pid = (self.pid_sample)(&mut self.sys);
        if self.last_pid.is_some() && pid.is_none() {
            // The server we were sampling is gone: find out now, not at the
            // next probe.
            self.probe(now);
            if self.cached_running().is_none() {
                self.last_pid = None;
                self.set_stats(DaemonStats::default());
                return;
            }
            pid = (self.pid_sample)(&mut self.sys);
        }
        self.last_pid = pid;
        let status = self.cached_running();
        let mut stats = sample_process_stats(&mut self.sys, pid);
        stats.running = true;
        stats.version = status.and_then(|s| s.version);
        self.set_stats(stats);
    }

    fn observe(&mut self, now: Instant) {
        let version = self.deps.keymap.version();
        match self.deps.ops.pane_list() {
            Ok(panes) => {
                self.deps.cache.lock().unwrap().panes = Some((panes.clone(), now, version));
                // Only panes of the current server generation describe the
                // sessions; an older entry's id may now name a restored shell.
                let generation = self.deps.keymap.generation();
                let entries: Vec<(SessionKey, HerdrPane)> = self
                    .deps
                    .keymap
                    .entries()
                    .into_iter()
                    .filter(|(_, p)| p.server_generation == generation)
                    .collect();
                let same_server = self.on_reconciled_server();
                self.deps.store.observe(&trusted_observations(
                    observation_pairs(&entries, &panes),
                    same_server,
                ));
                self.deps
                    .recovery
                    .observe(&observations_from(&entries, &panes));
            }
            Err(e) => self.report(&Err(e)),
        }
    }

    /// Whether the running server is the instance the keymap was reconciled
    /// against (compared by pid, from the in-process sample).
    fn on_reconciled_server(&mut self) -> bool {
        let Some(known) = self.deps.keymap.instance().filter(|i| i.pid != 0) else {
            return false;
        };
        (self.pid_sample)(&mut self.sys) == Some(known.pid)
    }

    /// Cache `stats`; emit it while running, and once when it flips to not
    /// running.
    fn set_stats(&mut self, stats: DaemonStats) {
        let running = stats.running;
        self.deps.cache.lock().unwrap().stats = stats.clone();
        if running || self.stats_running != Some(false) {
            if let Ok(v) = serde_json::to_value(&stats) {
                self.deps.sink.emit(STATS_EVENT, v);
            }
        }
        self.stats_running = Some(running);
    }

    /// Emit `daemon://status` when it changed or the heartbeat is due.
    fn emit_status(&mut self, now: Instant, status: Option<ServerStatus>, explicit_stop: bool) {
        let payload = build_status_payload(
            explicit_stop,
            &self.deps.supervisor,
            &*self.deps.ops,
            status,
            &self.deps.live,
            &self.deps.exe_missing,
        );
        let send = match &self.last_status {
            None => true,
            Some((last, at)) => *last != payload || due(now, Some(*at), HEARTBEAT),
        };
        if !send {
            return;
        }
        if let Ok(v) = serde_json::to_value(&payload) {
            self.deps.sink.emit(STATUS_EVENT, v);
        }
        self.last_status = Some((payload, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::cli::{AgentSession, AgentSessionKind};
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::gate::{GateRefusal, RECOVERY_EVENT};
    use crate::backend::herdr::ops::{Generation, Op, ServerInstance};
    use crate::backend::herdr::recovery::machine::Event;
    use crate::backend::herdr::recovery::recording_handle;
    use crate::backend::herdr::watchdog::DeathEvent;
    use crate::headroom::{FakeHeadroom, MIB};
    use serde_json::Value;
    use std::sync::mpsc::Receiver;

    const GIB: u64 = 1024 * MIB;
    const BASE: u64 = 4 * GIB;

    fn snap(commit: u64, phys: u64) -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: commit,
            phys_avail: phys,
            pressure: None,
        }
    }

    fn plenty() -> HeadroomSnapshot {
        snap(64 * GIB, 32 * GIB)
    }

    // ── Cadence ──────────────────────────────────────────────────────────────

    #[test]
    fn cadence_tables() {
        assert_eq!(
            Cadence::for_mode(false),
            Cadence {
                sample: Duration::from_secs(2),
                probe: Duration::from_secs(10),
                observe: Duration::from_secs(30),
            }
        );
        assert_eq!(
            Cadence::for_mode(true),
            Cadence {
                sample: Duration::from_secs(10),
                probe: Duration::from_secs(30),
                observe: Duration::from_secs(60),
            }
        );
        assert_eq!(WAKE, Duration::from_secs(1));
        assert_eq!(HEARTBEAT, Duration::from_secs(30));
        assert_eq!(HEADROOM_EVERY, Duration::from_secs(5));
    }

    #[test]
    fn failed_probes_back_off_up_to_the_probe_interval() {
        let secs = |n| Duration::from_secs(n);
        let healthy: Vec<Duration> = (0..6).map(|f| probe_interval(f, HEALTHY_CADENCE)).collect();
        assert_eq!(
            healthy,
            vec![secs(10), secs(2), secs(4), secs(8), secs(10), secs(10)]
        );
        let low: Vec<Duration> = (0..7).map(|f| probe_interval(f, LOW_CADENCE)).collect();
        assert_eq!(
            low,
            vec![
                secs(30),
                secs(2),
                secs(4),
                secs(8),
                secs(16),
                secs(30),
                secs(30)
            ]
        );
        assert_eq!(probe_interval(u32::MAX, LOW_CADENCE), secs(30));
    }

    #[test]
    fn due_table() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        assert!(due(t0, None, Duration::from_secs(2)));
        assert!(!due(s(1), Some(t0), Duration::from_secs(2)));
        assert!(due(s(2), Some(t0), Duration::from_secs(2)));
        assert!(due(s(3), Some(t0), Duration::from_secs(2)));
        // A clock that appears to run backwards is never due early.
        assert!(!due(t0, Some(s(5)), Duration::from_secs(2)));
        for (low, cadence) in [(false, HEALTHY_CADENCE), (true, LOW_CADENCE)] {
            let c = Cadence::for_mode(low);
            assert_eq!(c, cadence);
            assert!(!due(
                t0 + c.sample - Duration::from_millis(1),
                Some(t0),
                c.sample
            ));
            assert!(due(t0 + c.probe, Some(t0), c.probe));
        }
    }

    #[test]
    fn low_headroom_enters_on_commit_or_phys() {
        let mut m = LowHeadroom::default();
        assert_eq!(m.update(Some(plenty()), BASE), None);
        assert_eq!(m.update(Some(snap(BASE - 1, 32 * GIB)), BASE), Some(true));
        let mut m = LowHeadroom::default();
        assert_eq!(
            m.update(Some(snap(64 * GIB, PHYS_MARGIN_BYTES - 1)), BASE),
            Some(true)
        );
        assert!(m.is_low());
        // Unavailable samples change nothing.
        assert_eq!(m.update(None, BASE), None);
        assert!(m.is_low());
    }

    #[test]
    fn low_headroom_leaves_after_two_ample_samples() {
        let ample = snap(BASE + BASE / 4, PHYS_MARGIN_BYTES + PHYS_MARGIN_BYTES / 4);
        let mut m = LowHeadroom::default();
        m.update(Some(snap(0, 0)), BASE);
        assert_eq!(m.update(Some(ample), BASE), None);
        assert!(m.is_low());
        assert_eq!(m.update(Some(ample), BASE), Some(false));
        assert!(!m.is_low());

        // Back above base but under 1.25x is not enough, and resets the streak.
        let mut m = LowHeadroom::default();
        m.update(Some(snap(0, 0)), BASE);
        m.update(Some(ample), BASE);
        assert_eq!(m.update(Some(snap(BASE + 1, 32 * GIB)), BASE), None);
        assert_eq!(m.update(Some(ample), BASE), None);
        assert!(m.is_low());
        assert_eq!(m.update(Some(ample), BASE), Some(false));

        // Commit ample but physical only just above its margin: still low.
        let mut m = LowHeadroom::default();
        m.update(Some(snap(0, 0)), BASE);
        let phys_short = snap(64 * GIB, PHYS_MARGIN_BYTES + 1);
        assert_eq!(m.update(Some(phys_short), BASE), None);
        assert_eq!(m.update(Some(phys_short), BASE), None);
        assert!(m.is_low());
    }

    // ── Harness ──────────────────────────────────────────────────────────────

    #[derive(Default)]
    struct TestGate(AtomicBool);

    impl TestGate {
        fn refuse(&self, on: bool) {
            self.0.store(on, Ordering::SeqCst);
        }
    }

    impl ServerGate for TestGate {
        fn admit(&self, _key: Option<&SessionKey>) -> Result<(), GateRefusal> {
            if self.0.load(Ordering::SeqCst) {
                Err(GateRefusal::RecoveryPending)
            } else {
                Ok(())
            }
        }
    }

    type Emitted = Arc<Mutex<Vec<(String, Value)>>>;

    struct Sink(Emitted);

    impl EmitSink for Sink {
        fn emit(&self, event: &str, payload: Value) {
            self.0.lock().unwrap().push((event.to_string(), payload));
        }
    }

    struct Rig {
        fake: Arc<FakeHerdr>,
        gate: Arc<TestGate>,
        events: Emitted,
        stop: Arc<AtomicBool>,
        live: Arc<Mutex<HashSet<SessionKey>>>,
        keymap: Arc<KeyMap>,
        cache: SharedStatusCache,
        deaths: Receiver<(Generation, DeathEvent)>,
        watchdog: Arc<Watchdog>,
        recovery_rx: Receiver<Event>,
        pid: Arc<Mutex<Option<u32>>>,
        ready: Arc<AtomicBool>,
        exe_missing: Arc<ExeMissing>,
        t0: Instant,
        status_loop: StatusLoop,
    }

    impl Rig {
        fn new() -> Rig {
            Self::with_headroom(FakeHeadroom::constant(plenty()))
        }

        fn with_headroom(headroom: FakeHeadroom) -> Rig {
            let fake = FakeHerdr::new();
            let gate = Arc::new(TestGate::default());
            let events: Emitted = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));
            let live = Arc::new(Mutex::new(HashSet::new()));
            let keymap = Arc::new(KeyMap::with_path(fake.env().keymap_path()));
            let store = Arc::new(ResumeStore::open(&fake.env().resume_db_path()).unwrap());
            let cache: SharedStatusCache = Arc::default();
            let headroom: Arc<dyn Headroom> = Arc::new(headroom);
            let (tx, deaths) = std::sync::mpsc::channel();
            let watchdog = Arc::new(Watchdog::new(
                tx,
                Arc::clone(&headroom),
                Arc::new(Generation::default),
            ));
            let (recovery, recovery_rx) = recording_handle();
            // Our own test process: a pid sysinfo can sample cheaply.
            let pid = Arc::new(Mutex::new(Some(std::process::id())));
            let pid_in = Arc::clone(&pid);
            let ready = Arc::new(AtomicBool::new(true));
            let ready_in = Arc::clone(&ready);
            let exe_missing = Arc::new(ExeMissing::new(fake.env().discovery.clone()));
            let status_loop = StatusLoop::new(StatusLoopDeps {
                ops: Arc::clone(&fake) as Arc<dyn HerdrOps>,
                gate: Arc::clone(&gate) as Arc<dyn ServerGate>,
                headroom,
                base_margin_bytes: BASE,
                store: Arc::clone(&store),
                keymap: Arc::clone(&keymap),
                recovery,
                sink: Arc::new(Sink(Arc::clone(&events))),
                explicit_stop: Arc::clone(&stop),
                supervisor: Arc::new(Supervisor::new()),
                live: Arc::clone(&live),
                watchdog: Arc::clone(&watchdog),
                cache: Arc::clone(&cache),
                pid_sample: Some(Box::new(move |_sys: &mut sysinfo::System| {
                    *pid_in.lock().unwrap()
                })),
                exe_missing: Arc::clone(&exe_missing),
                boot_ready: Arc::new(move || ready_in.load(Ordering::SeqCst)),
            });
            Rig {
                fake,
                gate,
                events,
                stop,
                live,
                keymap,
                cache,
                deaths,
                watchdog,
                recovery_rx,
                pid,
                ready,
                exe_missing,
                t0: Instant::now(),
                status_loop,
            }
        }

        fn at(&mut self, secs: u64) {
            self.status_loop.tick(self.t0 + Duration::from_secs(secs));
        }

        fn count(&self, op: Op) -> usize {
            self.fake.calls().iter().filter(|c| c.op == op).count()
        }

        fn herdr_calls(&self) -> usize {
            self.count(Op::StatusServer) + self.count(Op::PaneList)
        }

        fn take(&self, event: &str) -> Vec<Value> {
            let mut all = self.events.lock().unwrap();
            let (hit, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|(e, _)| e == event);
            *all = rest;
            hit.into_iter().map(|(_, v)| v).collect()
        }

        fn statuses(&self) -> Vec<Value> {
            self.take(STATUS_EVENT)
        }

        fn stats(&self) -> Vec<Value> {
            self.take(STATS_EVENT)
        }

        fn observed(&self) -> Vec<Vec<Observation>> {
            self.recovery_rx
                .try_iter()
                .filter_map(|e| match e {
                    Event::Observed(o) => Some(o),
                    _ => None,
                })
                .collect()
        }
    }

    fn key(w: &str) -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: w.into(),
        }
    }

    // ── Tick ─────────────────────────────────────────────────────────────────

    #[test]
    fn first_tick_does_everything_once() {
        let mut r = Rig::new();
        r.at(0);
        assert_eq!(r.count(Op::StatusServer), 1);
        assert_eq!(r.count(Op::PaneList), 1);
        let statuses = r.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0]["state"], "Connected");
        let stats = r.stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0]["running"], true);
        assert_eq!(stats[0]["pid"], std::process::id());
        assert!(r.cache.lock().unwrap().stats.running);
    }

    #[test]
    fn a_wake_with_nothing_due_does_nothing() {
        let mut r = Rig::new();
        r.at(0);
        r.statuses();
        r.stats();
        let calls = r.fake.calls().len();
        r.at(1);
        assert_eq!(r.fake.calls().len(), calls);
        assert!(r.events.lock().unwrap().is_empty());
    }

    #[test]
    fn healthy_cadence_over_a_minute() {
        let mut r = Rig::new();
        for s in 0..=60 {
            r.at(s);
        }
        // Probes at 0,10,..,60; observes at 0,30,60; samples every 2 s.
        assert_eq!(r.count(Op::StatusServer), 7);
        assert_eq!(r.count(Op::PaneList), 3);
        assert_eq!(r.stats().len(), 31);
        // Status: first emit plus heartbeats at 30 and 60.
        assert_eq!(r.statuses().len(), 3);
    }

    #[test]
    fn low_headroom_slows_everything() {
        let mut r = Rig::with_headroom(FakeHeadroom::constant(snap(GIB, 32 * GIB)));
        for s in 0..=60 {
            r.at(s);
        }
        // Headroom is sampled first, so even the first tick uses the low cadence.
        assert!(r.status_loop.is_low());
        assert_eq!(r.count(Op::StatusServer), 3); // 0, 30, 60
        assert_eq!(r.count(Op::PaneList), 2); // 0, 60
        assert_eq!(r.stats().len(), 7); // 0, 10, .., 60
    }

    #[test]
    fn identical_samples_wait_for_the_heartbeat_but_changes_emit_at_once() {
        let mut r = Rig::new();
        r.at(0);
        assert_eq!(r.statuses().len(), 1);
        r.at(2);
        r.at(4);
        assert!(r.statuses().is_empty());
        r.live.lock().unwrap().insert(key("a"));
        r.at(5);
        let s = r.statuses();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0]["session_count"], 1);
        r.at(6);
        assert!(r.statuses().is_empty());
        r.at(35);
        assert_eq!(r.statuses().len(), 1, "heartbeat");
    }

    // ── Gating ───────────────────────────────────────────────────────────────

    #[test]
    fn explicit_stop_makes_no_calls_and_reports_once() {
        let mut r = Rig::new();
        r.stop.store(true, Ordering::SeqCst);
        r.at(0);
        r.at(10);
        r.at(40);
        assert_eq!(r.fake.calls().len(), 0, "{:?}", r.fake.calls());
        let statuses = r.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0]["state"], "Disconnected");
        let stats = r.stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0]["running"], false);

        r.stop.store(false, Ordering::SeqCst);
        r.at(41);
        assert_eq!(r.count(Op::StatusServer), 1);
        assert_eq!(r.statuses()[0]["state"], "Connected");
    }

    #[test]
    fn a_refusing_gate_makes_no_calls_and_the_return_probes_at_once() {
        let mut r = Rig::new();
        r.gate.refuse(true);
        for s in 0..=40 {
            r.at(s);
        }
        assert_eq!(r.herdr_calls(), 0, "{:?}", r.fake.calls());
        let statuses = r.statuses();
        assert_eq!(statuses[0]["state"], "Disconnected");
        let stats = r.stats();
        assert_eq!(stats.len(), 1, "not-running stats go out once");
        assert_eq!(stats[0]["running"], false);

        r.gate.refuse(false);
        r.at(41);
        assert_eq!(r.count(Op::StatusServer), 1);
        assert_eq!(r.statuses()[0]["state"], "Connected");
    }

    #[test]
    fn a_refusing_gate_after_running_reports_disconnected() {
        let mut r = Rig::new();
        r.at(0);
        r.statuses();
        r.stats();
        r.gate.refuse(true);
        r.at(1);
        assert_eq!(r.statuses()[0]["state"], "Disconnected");
        r.at(2);
        assert_eq!(r.stats()[0]["running"], false);
        r.at(4);
        assert!(r.stats().is_empty());
    }

    // ── Store feeding ────────────────────────────────────────────────────────

    fn session(id: &str) -> AgentSession {
        AgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: AgentSessionKind::Id,
            value: id.into(),
        }
    }

    fn row(r: &Rig, w: &str) -> Option<keitora_tasks::ResumeRow> {
        keitora_tasks::LocalStore::open(&r.fake.env().resume_db_path().to_string_lossy())
            .unwrap()
            .resume_get("p", w)
            .unwrap()
    }

    fn record(r: &Rig, w: &str, pane: &PaneInfo, cwd: &str) {
        r.keymap.record(
            key(w),
            pane.workspace_id.clone(),
            pane.pane_id.clone(),
            pane.terminal_id.clone(),
            cwd.into(),
        );
    }

    #[test]
    fn observe_feeds_the_store_and_the_recovery_view() {
        let mut r = Rig::new();
        let pane = r
            .fake
            .add_pane("/repos/wt", Some(("claude", session("sess-1"))));
        record(&r, "wt", &pane, "/repos/wt");
        r.at(0);
        let row = row(&r, "wt").expect("observed");
        assert_eq!(row.session_id.as_deref(), Some("sess-1"));
        let observed = r.observed();
        assert_eq!(
            observed,
            vec![vec![Observation {
                key: key("wt"),
                busy: false
            }]]
        );
    }

    #[test]
    fn no_observation_while_the_gate_refuses() {
        let mut r = Rig::new();
        let pane = r
            .fake
            .add_pane("/repos/wt", Some(("claude", session("sess-1"))));
        record(&r, "wt", &pane, "/repos/wt");
        r.gate.refuse(true);
        r.at(0);
        r.at(30);
        assert!(row(&r, "wt").is_none());
        assert!(r.observed().is_empty());
    }

    #[test]
    fn nothing_is_observed_before_the_launch_decision() {
        let mut r = Rig::new();
        let pane = r
            .fake
            .add_pane("/repos/wt", Some(("claude", session("sess-1"))));
        record(&r, "wt", &pane, "/repos/wt");
        r.ready.store(false, Ordering::SeqCst);
        r.at(0);
        r.at(30);
        assert_eq!(r.count(Op::PaneList), 0);
        assert!(row(&r, "wt").is_none());
        assert!(r.observed().is_empty());
        r.ready.store(true, Ordering::SeqCst);
        r.at(31);
        assert!(row(&r, "wt").is_some(), "observed once decided");
    }

    #[test]
    fn a_shell_on_another_server_never_marks_the_agent_quit() {
        let mut r = Rig::new();
        // The keymap's server is another process than the one answering.
        r.keymap.set_instance(Some(ServerInstance {
            pid: std::process::id() + 1,
            start_time: 1,
        }));
        let pane = r
            .fake
            .add_pane("/repos/wt", Some(("claude", session("sess-1"))));
        record(&r, "wt", &pane, "/repos/wt");
        r.at(0);
        assert!(
            row(&r, "wt").unwrap().agent_present,
            "a running agent is recorded"
        );
        // The answering server shows the same id and cwd as a bare shell (a
        // restore that reused the id).
        r.fake.set_agent_present(&pane.pane_id, false);
        r.at(30);
        assert!(row(&r, "wt").unwrap().agent_present, "not marked quit");

        // On the reconciled server the same observation is a real quit.
        r.keymap.set_instance(Some(ServerInstance {
            pid: std::process::id(),
            start_time: 1,
        }));
        r.at(60);
        assert!(!row(&r, "wt").unwrap().agent_present);
    }

    #[test]
    fn an_entry_of_an_older_generation_is_not_observed() {
        let mut r = Rig::new();
        let pane = r
            .fake
            .add_pane("/repos/wt", Some(("claude", session("sess-1"))));
        record(&r, "wt", &pane, "/repos/wt");
        r.keymap.note_death_bump();
        r.at(0);
        assert!(row(&r, "wt").is_none());
        assert!(r.observed().iter().all(|o| o.is_empty()));
    }

    #[test]
    fn the_observed_pane_list_answers_the_session_listing() {
        let mut r = Rig::new();
        r.at(0);
        assert_eq!(r.count(Op::PaneList), 1);
        let t = r.t0 + Duration::from_secs(1);
        session_panes(&*r.fake, &r.keymap, &r.cache, t).unwrap();
        assert_eq!(r.count(Op::PaneList), 1, "reused");
        // Too old, or the keymap changed since: listed again.
        session_panes(&*r.fake, &r.keymap, &r.cache, r.t0 + SESSIONS_MAX_AGE).unwrap();
        assert_eq!(r.count(Op::PaneList), 2);
        r.keymap.begin_recovery(&[key("a")]);
        session_panes(&*r.fake, &r.keymap, &r.cache, r.t0 + SESSIONS_MAX_AGE).unwrap();
        assert_eq!(r.count(Op::PaneList), 3);
    }

    #[test]
    fn the_session_listing_reuses_panes_longer_while_headroom_is_low() {
        let mut r = Rig::with_headroom(FakeHeadroom::constant(snap(GIB, 32 * GIB)));
        r.at(0);
        assert!(r.cache.lock().unwrap().low);
        let t = r.t0 + SESSIONS_MAX_AGE + Duration::from_secs(1);
        session_panes(&*r.fake, &r.keymap, &r.cache, t).unwrap();
        assert_eq!(r.count(Op::PaneList), 1, "still fresh at the low cadence");
    }

    #[test]
    fn a_failed_pane_list_reaches_the_watchdog_not_the_store() {
        for err in [
            HerdrError::Timeout,
            HerdrError::SpawnFailed {
                os_error: Some(1455),
                message: "paging file too small".into(),
            },
        ] {
            let mut r = Rig::new();
            let pane = r
                .fake
                .add_pane("/repos/wt", Some(("claude", session("sess-1"))));
            record(&r, "wt", &pane, "/repos/wt");
            r.fake.fail_next(Op::PaneList, err);
            r.at(0);
            assert_eq!(r.count(Op::PaneList), 1);
            assert!(row(&r, "wt").is_none());
            assert!(r.observed().is_empty());
        }
    }

    #[test]
    fn a_failed_pane_list_counts_as_a_failed_probe() {
        let mut r = Rig::new();
        // No server process to be seen: a timeout counts.
        *r.pid.lock().unwrap() = None;
        r.fake.fail_next(Op::PaneList, HerdrError::Timeout);
        // The probe answers, then the pane list times out: one failure.
        r.at(0);
        r.watchdog.report_probe(&Err(HerdrError::Timeout), false);
        assert!(r.deaths.try_recv().is_err());
        // The third consecutive failure declares the server dead, so the pane
        // list's timeout was counted.
        r.watchdog.report_probe(&Err(HerdrError::Timeout), false);
        assert!(r.deaths.try_recv().is_ok());
    }

    #[test]
    fn observations_from_derives_busy_from_agent_status() {
        let pane = |id: &str, status: Option<&str>| PaneInfo {
            workspace_id: "w".into(),
            pane_id: id.into(),
            terminal_id: format!("t-{id}"),
            cwd: Some("/x".into()),
            agent: Some("claude".into()),
            agent_status: status.map(str::to_string),
            agent_session: None,
            scroll: None,
        };
        let entry = |id: &str| HerdrPane {
            workspace_id: "w".into(),
            pane_id: id.into(),
            terminal_id: format!("t-{id}"),
            gen: 1,
            cwd: "/x".into(),
            server_generation: Generation::default(),
        };
        let entries = vec![
            (key("a"), entry("p1")),
            (key("b"), entry("p2")),
            (key("c"), entry("p3")),
            (key("d"), entry("p4")),
            (key("gone"), entry("p9")),
        ];
        let panes = vec![
            pane("p1", Some("working")),
            pane("p2", Some("blocked")),
            pane("p3", Some("idle")),
            pane("p4", None),
        ];
        let got = observations_from(&entries, &panes);
        let busy: Vec<(String, bool)> = got.into_iter().map(|o| (o.key.worktree, o.busy)).collect();
        assert_eq!(
            busy,
            [
                ("a".to_string(), true),
                ("b".to_string(), true),
                ("c".to_string(), false),
                ("d".to_string(), false)
            ]
        );
    }

    // ── Failure ──────────────────────────────────────────────────────────────

    #[test]
    fn a_timed_out_probe_is_retried_with_backoff_until_it_answers() {
        let mut r = Rig::new();
        *r.pid.lock().unwrap() = None;
        r.at(0);
        r.statuses();
        r.stats();
        for _ in 0..2 {
            r.fake.fail_next(Op::StatusServer, HerdrError::Timeout);
        }
        r.at(10);
        assert_eq!(r.count(Op::StatusServer), 2);
        assert_eq!(r.statuses()[0]["state"], "Disconnected");
        assert!(!r.cache.lock().unwrap().server.as_ref().unwrap().0.running);
        r.at(11);
        assert_eq!(r.count(Op::StatusServer), 2, "not every wake");
        r.at(12);
        assert_eq!(r.count(Op::StatusServer), 3, "first retry after 2 s");
        r.at(15);
        assert_eq!(r.count(Op::StatusServer), 3);
        r.at(16);
        assert_eq!(r.count(Op::StatusServer), 4, "second retry 4 s later");
        assert_eq!(r.statuses()[0]["state"], "Connected");
        assert!(r.deaths.try_recv().is_err(), "two failures only");

        // Answered: back to the normal cadence.
        r.at(25);
        assert_eq!(r.count(Op::StatusServer), 4);
        r.at(26);
        assert_eq!(r.count(Op::StatusServer), 5);
    }

    #[test]
    fn timeouts_with_the_server_process_gone_declare_it_dead() {
        let mut r = Rig::new();
        *r.pid.lock().unwrap() = None;
        r.at(0);
        r.fake.hang(Op::StatusServer, true);
        for s in 1..=15 {
            r.at(s);
        }
        assert_eq!(r.count(Op::StatusServer), 3, "at 0, 10 and 12");
        assert!(r.deaths.try_recv().is_err());
        r.at(16);
        assert_eq!(r.count(Op::StatusServer), 4);
        assert!(r.deaths.try_recv().is_ok(), "three timeouts, no process");
    }

    #[test]
    fn a_slow_server_whose_process_runs_is_not_declared_dead() {
        let mut r = Rig::new();
        r.at(0);
        r.fake.hang(Op::StatusServer, true);
        for s in 1..=120 {
            r.at(s);
        }
        assert!(r.deaths.try_recv().is_err(), "its process still runs");
        let probes = r.count(Op::StatusServer);
        assert!(probes <= 16, "failed probes back off ({probes})");
    }

    #[test]
    fn refusals_count_even_while_a_pid_is_seen() {
        let mut r = Rig::new();
        r.at(0);
        r.fake.kill_server(Some(1));
        for s in 1..=15 {
            r.at(s);
        }
        assert!(r.deaths.try_recv().is_err(), "two refusals so far (10, 12)");
        r.at(16);
        assert!(r.deaths.try_recv().is_ok(), "the third refusal");
    }

    #[test]
    fn a_spawn_failed_probe_is_not_a_death() {
        let mut r = Rig::new();
        r.at(0);
        for _ in 0..5 {
            r.fake.fail_next(
                Op::StatusServer,
                HerdrError::SpawnFailed {
                    os_error: Some(1455),
                    message: "paging file too small".into(),
                },
            );
        }
        for s in 10..=40 {
            r.at(s);
        }
        // Retries at 12, 16, 24 and 34 (capped at the probe interval).
        assert_eq!(r.count(Op::StatusServer), 6);
        assert!(r.deaths.try_recv().is_err());
    }

    #[test]
    fn a_missing_herdr_executable_is_reported_as_not_found_and_never_as_a_death() {
        use crate::backend::herdr::exe_missing::NOT_FOUND_CODE;
        let mut r = Rig::new();
        // An adopted server, so every probe result reaches the watchdog.
        r.watchdog
            .note_instance(r.fake.server_instance().expect("a running server"));
        r.at(0);
        assert_eq!(r.statuses().last().unwrap()["state"], "Connected");
        assert!(r.take(RECOVERY_EVENT).is_empty(), "nothing to say yet");

        // The executable goes away under a running server.
        r.fake.set_exe_missing(true);
        *r.pid.lock().unwrap() = None;
        for s in 10..=120 {
            r.at(s);
        }
        assert!(r.count(Op::StatusServer) > 4, "it kept probing");
        assert!(
            r.deaths.try_recv().is_err(),
            "a herdr that cannot be run says nothing about the server"
        );
        let message = r.exe_missing.message().expect("the state is set");
        let status = r.statuses().last().cloned().unwrap();
        assert_eq!(status["state"], "Disconnected");
        assert_eq!(status["reason"], Value::String(message.clone()));
        assert_eq!(status["restartable"], true);
        let recovery = r.take(RECOVERY_EVENT);
        assert_eq!(recovery.len(), 1, "pushed once, when the state changed");
        assert_eq!(recovery[0]["reason"], Value::String(message));
        assert_eq!(recovery[0]["reason_code"], NOT_FOUND_CODE);
        assert_eq!(recovery[0]["phase"], "Healthy");

        // It comes back: the next probe clears the message on both events.
        r.fake.set_exe_missing(false);
        *r.pid.lock().unwrap() = Some(std::process::id());
        for s in 121..=160 {
            r.at(s);
        }
        assert_eq!(r.exe_missing.message(), None);
        let status = r.statuses().last().cloned().unwrap();
        assert_eq!(status["state"], "Connected");
        assert!(status.get("reason").is_none(), "{status}");
        let recovery = r.take(RECOVERY_EVENT);
        assert_eq!(recovery.len(), 1);
        assert!(recovery[0]["reason"].is_null());
        assert!(recovery[0]["reason_code"].is_null());
        assert!(r.deaths.try_recv().is_err());
    }

    #[test]
    fn a_vanished_pid_probes_at_once() {
        let mut r = Rig::new();
        r.at(0);
        assert_eq!(r.count(Op::StatusServer), 1);
        *r.pid.lock().unwrap() = None;
        r.fake.kill_server(Some(1));
        r.at(2);
        assert_eq!(r.count(Op::StatusServer), 2, "probed on the sample tick");
        let stats = r.stats();
        assert_eq!(stats.last().unwrap()["running"], false);
        assert_eq!(r.statuses().last().unwrap()["state"], "Disconnected");
    }

    #[test]
    fn run_with_ticks_until_stopped() {
        let r = Rig::new();
        let fake = Arc::clone(&r.fake);
        let stop = AtomicBool::new(false);
        let t0 = r.t0;
        let clock = std::cell::Cell::new(0u64);
        let mut wakes = 0u64;
        r.status_loop.run_with(
            &stop,
            || t0 + Duration::from_secs(clock.get()),
            |d| {
                assert_eq!(d, WAKE);
                wakes += 1;
                clock.set(clock.get() + 1);
                if wakes == 11 {
                    stop.store(true, Ordering::SeqCst);
                }
            },
        );
        assert_eq!(wakes, 11);
        let probes = fake
            .calls()
            .iter()
            .filter(|c| c.op == Op::StatusServer)
            .count();
        assert_eq!(probes, 2, "at 0 s and 10 s");
    }

    // ── Pulled snapshot ──────────────────────────────────────────────────────

    fn running() -> ServerStatus {
        ServerStatus {
            running: true,
            version: Some("0.9.3".into()),
            protocol: Some(3),
            socket: None,
        }
    }

    #[test]
    fn snapshot_status_uses_a_warm_cache() {
        let fake = FakeHerdr::new();
        let t0 = Instant::now();
        let cache = Mutex::new(StatusCache {
            server: Some((running(), t0)),
            ..StatusCache::default()
        });
        let got = snapshot_status(&*fake, &cache, t0 + SNAPSHOT_MAX_AGE);
        assert_eq!(got, Some(running()));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn snapshot_status_probes_once_when_cold_stale_or_down() {
        let t0 = Instant::now();
        let later = t0 + SNAPSHOT_MAX_AGE + Duration::from_secs(1);
        let down = ServerStatus {
            running: false,
            ..running()
        };
        for (cached, now) in [
            (None, t0),
            (Some((running(), t0)), later),
            (Some((down, t0)), t0),
        ] {
            let fake = FakeHerdr::new();
            let cache = Mutex::new(StatusCache {
                server: cached,
                ..StatusCache::default()
            });
            let got = snapshot_status(&*fake, &cache, now).expect("live answer");
            assert!(got.running);
            let calls = fake.calls();
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert_eq!(calls[0].op, Op::StatusServer);
            let (s, at) = cache.lock().unwrap().server.clone().unwrap();
            assert_eq!(s, got);
            assert_eq!(at, now);
        }
    }

    #[test]
    fn snapshot_status_with_a_failing_probe_keeps_the_cache() {
        let fake = FakeHerdr::new();
        fake.fail_next(Op::StatusServer, HerdrError::Timeout);
        let t0 = Instant::now();
        let cache = Mutex::new(StatusCache::default());
        assert_eq!(snapshot_status(&*fake, &cache, t0), None);
        assert!(cache.lock().unwrap().server.is_none());
    }
}
