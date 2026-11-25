//! `supervisor` — starts / stops Keitora's isolated herdr server.
//!
//! `require_running` only checks `herdr status server --json` (ordinary callers
//! never start the server); `start` spawns a detached `herdr server` with the
//! isolated env and polls until the socket answers, and is reserved for the
//! explicit recovery entry points; `adopt` watches a server someone else started.
//! `stop` issues `herdr server stop`.
//!
//! The `pid` the supervisor reports is the restart signal the frontend keys off
//! (it MUST change across server restarts). We capture the
//! spawned child's OS pid; when the running server was started externally (we did
//! not spawn it) we fall back to a monotonic instance counter so the reported id
//! still changes on every (re)start.
//!
//! Each server start (spawned here, or an already-running server adopted) is
//! written as a lifecycle record carrying a memory headroom sample.
//!
//! With a [`Watchdog`] attached, a spawned server is handed to a waiter thread
//! once it answers, and its unexpected exit is reported as a death; every
//! started or adopted server is noted with the watchdog, and every deliberate
//! stop calls [`Supervisor::expect_stop`] first so it never reads as a death.
//!
//! TODO: take an explicit singleton lock (flock)
//! rather than relying on herdr's own single-socket guarantee + the status probe.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::PtyBackendError;
use crate::headroom::{Headroom, SystemHeadroom};
use crate::lifecycle::{self, LifecycleEvent, ServerStartHow};

use super::cli::IntegrationStatus;
use super::ops::{ExitInfo, HerdrOps, ServerInstance, ServerProcess};
use super::watchdog::{format_exit, Watchdog, UNKNOWN_INSTANCE};

/// Opt-out env var (any value): never run `herdr integration install` (it writes the
/// user's REAL ~/.claude / ~/.codex). Set by `yarn dev:herdr` and the gated tests.
pub const NO_INTEGRATION_ENV: &str = "KEITORA_HERDR_NO_INTEGRATION";

/// Pure opt-out check: integration install is enabled only when the opt-out env
/// var is absent.
pub fn integration_enabled(env_val: Option<String>) -> bool {
    env_val.is_none()
}

/// Max time to wait for a freshly spawned server to answer its socket.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval while waiting for the server to come up.
const POLL_INTERVAL: Duration = Duration::from_millis(150);
/// How long a requested stop may take before the server is killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a killed server may take to go away.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// Reads of a just-started server's identity before giving up on its start time.
const IDENTITY_ATTEMPTS: u32 = 5;

/// Force-kills a server process by pid (never by image name).
pub type Killer = Arc<dyn Fn(u32) + Send + Sync>;

/// The server child this supervisor started and still owns.
struct SpawnedServer {
    pid: u32,
    /// Set before a deliberate stop; the child's waiter then reports no death.
    expected_stop: Arc<AtomicBool>,
    /// Set by the child's waiter once the process has exited.
    exited: Arc<AtomicBool>,
}

pub struct Supervisor {
    /// The server child we last spawned, if any.
    spawned: Mutex<Option<SpawnedServer>>,
    /// Monotonic (re)start counter — the surrogate restart signal.
    instance: AtomicU32,
    /// `ensure_ready` runs at most once per process (until `refresh_integration`
    /// resets it).
    ready_done: AtomicBool,
    /// How many times `ensure_ready` got past its once-latch.
    ready_runs: AtomicU32,
    /// Memory probe sampled into each server-start record.
    headroom: Arc<dyn Headroom>,
    /// The adopted-server start record is written at most once per process.
    adopted_logged: AtomicBool,
    /// Max time to wait for a freshly spawned server to answer its socket.
    start_timeout: Duration,
    /// Poll interval while waiting for the server to come up.
    poll_interval: Duration,
    /// How long a requested stop may take before the server is killed.
    stop_timeout: Duration,
    /// How long a killed server may take to go away.
    kill_wait: Duration,
    /// Kills a server that refused to stop.
    killer: Killer,
    /// Receives death reports; `None` drops them (nothing watches the server).
    watchdog: Option<Arc<Watchdog>>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Supervisor::with_headroom(Arc::new(SystemHeadroom))
    }
}

impl Supervisor {
    /// An unwatched supervisor (deaths are dropped); the backend attaches a watchdog.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new() -> Self {
        Supervisor::default()
    }

    /// A supervisor sampling headroom from `headroom` (tests inject a fake).
    pub fn with_headroom(headroom: Arc<dyn Headroom>) -> Self {
        Supervisor {
            spawned: Mutex::new(None),
            instance: AtomicU32::new(0),
            ready_done: AtomicBool::new(false),
            ready_runs: AtomicU32::new(0),
            headroom,
            adopted_logged: AtomicBool::new(false),
            start_timeout: START_TIMEOUT,
            poll_interval: POLL_INTERVAL,
            stop_timeout: STOP_TIMEOUT,
            kill_wait: KILL_WAIT,
            killer: Arc::new(crate::backend::platform::kill_group_kill),
            watchdog: None,
        }
    }

    /// Report server deaths to `watchdog`.
    pub fn with_watchdog(mut self, watchdog: Arc<Watchdog>) -> Self {
        self.watchdog = Some(watchdog);
        self
    }

    /// Override the start timeout and poll interval (tests use short ones so a
    /// server that never comes up does not cost the full timeout).
    #[cfg(test)]
    pub fn with_timing(mut self, start_timeout: Duration, poll_interval: Duration) -> Self {
        self.start_timeout = start_timeout;
        self.poll_interval = poll_interval;
        self
    }

    /// Override the stop and kill waits and the killer (tests record the pid
    /// instead of killing anything).
    #[cfg(test)]
    pub fn with_stop(
        mut self,
        stop_timeout: Duration,
        kill_wait: Duration,
        killer: Killer,
    ) -> Self {
        self.stop_timeout = stop_timeout;
        self.kill_wait = kill_wait;
        self.killer = killer;
        self
    }

    /// Check that the server answers; never starts it. A server that reports
    /// itself not running is a death (reported at once), unless a stop was
    /// requested: then the caller is told the server is simply not connected.
    pub fn require_running(&self, ops: &dyn HerdrOps) -> Result<(), PtyBackendError> {
        match ops.status_server() {
            Ok(s) if s.running => Ok(()),
            Ok(_) => {
                if let Some(wd) = &self.watchdog {
                    if wd.stop_expected() {
                        return Err(PtyBackendError::NotConnected);
                    }
                    wd.report_down_now();
                }
                Err(PtyBackendError::RecoveryPending)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Watch a running server this supervisor did not start (a no-op for our
    /// own child, whose waiter is authoritative).
    pub(super) fn adopt(&self, ops: &dyn HerdrOps) {
        if self.spawned_pid().is_some() {
            return;
        }
        self.note_adopted();
        self.watch_adopted(ops);
    }

    /// Record the start of a server this supervisor just spawned.
    fn note_spawned(&self, pid: u32, how: ServerStartHow) {
        lifecycle::record(LifecycleEvent::ServerStart {
            pid: Some(pid),
            how,
            headroom: self.headroom.sample(),
        });
    }

    /// Record adopting a server someone else started. Only the first call per
    /// process records.
    fn note_adopted(&self) {
        if self.adopted_logged.swap(true, Ordering::SeqCst) {
            return;
        }
        lifecycle::record(LifecycleEvent::ServerStart {
            pid: None,
            how: ServerStartHow::Adopted,
            headroom: self.headroom.sample(),
        });
    }

    /// Watch an adopted server (once per instance: re-noting the same server
    /// would reset the probe counter).
    fn watch_adopted(&self, ops: &dyn HerdrOps) {
        let Some(wd) = &self.watchdog else {
            return;
        };
        let instance = ops.server_instance().unwrap_or(UNKNOWN_INSTANCE);
        if wd.noted_instance() != Some(instance) {
            wd.note_instance(instance);
        }
    }

    /// Install herdr's claude/codex integrations (state hooks) once per process.
    /// SKIPPED entirely when `KEITORA_HERDR_NO_INTEGRATION` is set (process env or
    /// the herdr env pairs) and always under `cfg(test)`, since the install writes
    /// the real global `~/.claude` / `~/.codex`. Every failure is non-fatal.
    pub fn ensure_ready(&self, ops: &dyn HerdrOps) {
        if self.ready_done.swap(true, Ordering::SeqCst) {
            return;
        }
        self.ready_runs.fetch_add(1, Ordering::SeqCst);
        let from_cli = ops
            .env()
            .env
            .iter()
            .find(|(k, _)| k == NO_INTEGRATION_ENV)
            .map(|(_, v)| v.clone());
        let opt_out = std::env::var(NO_INTEGRATION_ENV).ok().or(from_cli);
        if cfg!(test) || !integration_enabled(opt_out) {
            log::info!("herdr: integration install skipped ({NO_INTEGRATION_ENV} / test)");
            return;
        }
        for target in ["claude", "codex"] {
            match ops.integration_status(target) {
                Ok(IntegrationStatus::NotInstalled | IntegrationStatus::Outdated) => {
                    match ops.integration_install(target) {
                        Ok(()) => log::info!("herdr: installed {target} integration"),
                        Err(e) => log::warn!(
                            "herdr: {target} integration install failed: {}; {target} \
                             session ids will not be recorded, so a resume falls back \
                             to the newest conversation",
                            PtyBackendError::from(e)
                        ),
                    }
                }
                Ok(_) => {}
                Err(e) => log::warn!(
                    "herdr: {target} integration status failed: {}",
                    PtyBackendError::from(e)
                ),
            }
        }
    }

    /// Run the integration install again even though it already ran this
    /// process: a new herdr binary may ship changed hooks. Skipped under the same
    /// conditions as [`Self::ensure_ready`] (test builds, the opt-out env var).
    pub fn refresh_integration(&self, ops: &dyn HerdrOps) {
        self.ready_done.store(false, Ordering::SeqCst);
        self.ensure_ready(ops);
    }

    /// How many times [`Self::ensure_ready`] got past its once-latch.
    #[cfg(test)]
    pub fn ready_runs(&self) -> u32 {
        self.ready_runs.load(Ordering::SeqCst)
    }

    /// Spawn `herdr server` detached and wait for it to answer. An exit before it
    /// answers fails the start at once; a server still silent at the start timeout
    /// is killed. Once it answers, a waiter thread owns the process and reports
    /// its unexpected exit to the watchdog. Only the explicit recovery entry
    /// points call this; ordinary callers use [`Self::require_running`].
    ///
    /// Never a second server next to one that answers: herdr refuses a taken
    /// socket, so the new child would exit (and read as a death) while the
    /// readiness poll is answered by the old server. A server that already
    /// answers is used as it is.
    pub(super) fn start(
        &self,
        ops: &dyn HerdrOps,
        how: ServerStartHow,
    ) -> Result<ServerInstance, PtyBackendError> {
        if ops.status_server().is_ok_and(|s| s.running) {
            log::warn!("herdr: a server already answers; using it instead of starting another");
            return Ok(self.take_over_live(ops));
        }
        let mut child = ops
            .spawn_server()
            .map_err(|e| PtyBackendError::Other(format!("herdr server spawn failed: {e}")))?;
        let spawned_at = Instant::now();
        let pid = child.pid();
        self.instance.fetch_add(1, Ordering::SeqCst);
        self.note_spawned(pid, how);

        let deadline = spawned_at + self.start_timeout;
        loop {
            if ops.status_server().map(|s| s.running).unwrap_or(false) {
                break;
            }
            match child.try_wait() {
                Ok(Some(exit)) => {
                    return Err(PtyBackendError::Other(format!(
                        "herdr server exited before answering ({})",
                        format_exit(exit.code, exit.signal)
                    )));
                }
                Ok(None) => {}
                Err(e) => log::warn!("herdr: server try_wait failed: {e}"),
            }
            if Instant::now() >= deadline {
                if let Err(e) = child.kill() {
                    log::warn!("herdr: killing the unresponsive server failed: {e}");
                }
                // Collect the killed child so no zombie is left on unix.
                if let Err(e) = child.wait() {
                    log::warn!("herdr: waiting on the killed server failed: {e}");
                }
                return Err(PtyBackendError::Other(
                    "herdr server did not come up within the start timeout".into(),
                ));
            }
            std::thread::sleep(self.poll_interval);
        }

        // Another server may have won the socket while our child exited: the
        // answer is then that server's, and it is adopted rather than taken
        // for ours (so its identity is not resolved through our child's pid).
        if let Ok(Some(exit)) = child.try_wait() {
            log::warn!(
                "herdr: our server {pid} exited ({}) while another one answers; using that one",
                format_exit(exit.code, exit.signal)
            );
            return Ok(self.take_over_live(ops));
        }

        let instance = self.resolve_identity(ops).unwrap_or_else(|| {
            // Still the right pid (our child's); the start time stays unknown
            // (0), which identity comparisons treat as a pid-only identity.
            log::warn!("herdr: could not read server {pid}'s start time; identifying it by pid");
            ServerInstance { pid, start_time: 0 }
        });
        let expected_stop = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        *self.spawned.lock().unwrap() = Some(SpawnedServer {
            pid,
            expected_stop: Arc::clone(&expected_stop),
            exited: Arc::clone(&exited),
        });
        if let Some(wd) = &self.watchdog {
            wd.note_instance(instance);
        }
        self.spawn_waiter(child, instance, spawned_at, expected_stop, exited);
        Ok(instance)
    }

    /// The running server's identity, retried a few times (a process scan can
    /// miss a server that has only just come up).
    fn resolve_identity(&self, ops: &dyn HerdrOps) -> Option<ServerInstance> {
        for attempt in 0..IDENTITY_ATTEMPTS {
            if let Some(i) = ops.server_instance() {
                return Some(i);
            }
            if attempt + 1 < IDENTITY_ATTEMPTS {
                std::thread::sleep(self.poll_interval);
            }
        }
        None
    }

    /// Use the server that answers now. Our own child, while it lives, stays
    /// ours (its waiter is authoritative); anything else is adopted.
    fn take_over_live(&self, ops: &dyn HerdrOps) -> ServerInstance {
        {
            let mut spawned = self.spawned.lock().unwrap();
            if spawned
                .as_ref()
                .is_some_and(|s| s.exited.load(Ordering::SeqCst))
            {
                *spawned = None;
            }
        }
        self.adopt(ops);
        self.resolve_identity(ops).unwrap_or(UNKNOWN_INSTANCE)
    }

    /// Block in `wait()` on a thread (which also reaps the child, so no zombie is
    /// left on unix) and report an unexpected exit. The waiter owns the process,
    /// so a live server is stopped by CLI or pid, never through it.
    fn spawn_waiter(
        &self,
        mut child: Box<dyn ServerProcess>,
        instance: ServerInstance,
        spawned_at: Instant,
        expected_stop: Arc<AtomicBool>,
        exited: Arc<AtomicBool>,
    ) {
        let watchdog = self.watchdog.clone();
        std::thread::spawn(move || {
            let exit = child.wait().unwrap_or_else(|e| {
                log::warn!("herdr: waiting on the server failed: {e}; exit code unknown");
                ExitInfo {
                    code: None,
                    signal: None,
                }
            });
            exited.store(true, Ordering::SeqCst);
            let how = format_exit(exit.code, exit.signal);
            if expected_stop.load(Ordering::SeqCst) {
                log::info!(
                    "herdr: server {} exited after a requested stop ({how})",
                    instance.pid
                );
                return;
            }
            match watchdog {
                Some(wd) => wd.child_exited(wd.generation(), instance, spawned_at, exit),
                None => log::warn!("herdr: server {} exited unexpectedly ({how})", instance.pid),
            }
        });
    }

    /// A deliberate stop is about to happen: neither the child waiter nor the
    /// adopted-server probe may report it as a death. Lifted when the next server
    /// is noted.
    pub fn expect_stop(&self) {
        if let Some(s) = self.spawned.lock().unwrap().as_ref() {
            s.expected_stop.store(true, Ordering::SeqCst);
        }
        if let Some(wd) = &self.watchdog {
            wd.expect_stop();
        }
    }

    /// Whether a stop is expected of our child and by the watchdog.
    fn stop_expectation(&self) -> (bool, bool) {
        let child = self
            .spawned
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.expected_stop.load(Ordering::SeqCst));
        let watchdog = self.watchdog.as_ref().is_some_and(|wd| wd.stop_expected());
        (child, watchdog)
    }

    /// Whether a deliberate stop (the user's stop or restart) is under way:
    /// the server being down, or a call to it failing, is then expected.
    pub fn stop_expected(&self) -> bool {
        let (child, watchdog) = self.stop_expectation();
        child || watchdog
    }

    /// Undo an [`Self::expect_stop`] whose stop was never issued: back to what
    /// [`Self::stop_expectation`] read before it.
    fn restore_stop_expectation(&self, (child, watchdog): (bool, bool)) {
        if let Some(s) = self.spawned.lock().unwrap().as_ref() {
            s.expected_stop.store(child, Ordering::SeqCst);
        }
        if let (Some(wd), false) = (&self.watchdog, watchdog) {
            wd.stop_not_issued();
        }
    }

    /// Stop the server (`herdr server stop`) and clear the captured pid.
    pub fn stop(&self, ops: &dyn HerdrOps) -> Result<(), PtyBackendError> {
        // Lets a log reader tell a requested stop from a death.
        log::info!("herdr: server stop requested");
        self.expect_stop();
        let r = ops.server_stop().map_err(PtyBackendError::from);
        *self.spawned.lock().unwrap() = None;
        self.instance.fetch_add(1, Ordering::SeqCst);
        r
    }

    /// Stop the server deliberately and wait until it is gone: ask it to stop,
    /// wait for it to stop answering, and if it is still up kill it by pid
    /// (our child's, else the running instance's) and wait once more. A server
    /// that survives that is an error.
    ///
    /// When the stop cannot even be asked for because the herdr executable is
    /// missing, this fails at once: every wait below polls through the same
    /// executable, and killing the server would only lose its sessions to a
    /// restart that cannot start anything.
    pub(super) fn stop_and_wait(&self, ops: &dyn HerdrOps) -> Result<(), PtyBackendError> {
        log::info!("herdr: server stop requested for a restart");
        let was_expected = self.stop_expectation();
        self.expect_stop();
        // Read before stopping: a stopped server has no instance to report. A
        // child of ours that already exited is not the one to kill.
        let pid = self
            .live_child_pid()
            .or_else(|| ops.server_instance().map(|i| i.pid))
            .filter(|&p| p != 0);
        if let Err(e) = ops.server_stop() {
            log::warn!("herdr: server stop failed: {e}");
            if e.is_exe_missing() {
                // Nothing was asked to stop: watch the server as before.
                self.restore_stop_expectation(was_expected);
                return Err(e.into());
            }
        }
        if !self.wait_until_down(ops, self.stop_timeout) {
            match pid {
                Some(pid) => {
                    log::warn!("herdr: server {pid} did not stop; killing it");
                    (self.killer)(pid);
                }
                None => log::warn!("herdr: server did not stop and its pid is unknown"),
            }
            if !self.wait_until_down(ops, self.kill_wait) {
                return Err(PtyBackendError::Other("could not stop herdr".into()));
            }
        }
        *self.spawned.lock().unwrap() = None;
        self.instance.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Poll until the server reports itself not running, for at most `limit`.
    fn wait_until_down(&self, ops: &dyn HerdrOps, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if matches!(ops.status_server(), Ok(s) if !s.running) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    /// The OS pid of the server child we spawned, if any. `None` when the
    /// running server was started externally (we attached to an existing socket) or after
    /// a stop. Unlike [`Self::restart_signal`] this is never a surrogate counter, so it is
    /// the only pid safe to hand to `sysinfo` for CPU/memory sampling.
    pub fn spawned_pid(&self) -> Option<u32> {
        self.spawned.lock().unwrap().as_ref().map(|s| s.pid)
    }

    /// [`Self::spawned_pid`], unless that child has already exited.
    fn live_child_pid(&self) -> Option<u32> {
        self.spawned
            .lock()
            .unwrap()
            .as_ref()
            .filter(|s| !s.exited.load(Ordering::SeqCst))
            .map(|s| s.pid)
    }

    /// The restart-signal id: the real child pid when we spawned the server, else a
    /// monotonic surrogate. Guaranteed to differ across (re)starts.
    pub fn restart_signal(&self) -> u32 {
        if let Some(pid) = self.spawned_pid() {
            return pid;
        }
        // No spawned child (externally started / after stop): surrogate counter,
        // offset so it never collides with a plausible low pid of 0.
        self.instance.load(Ordering::SeqCst).wrapping_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::fake::FakeHerdr;
    use crate::backend::herdr::ops::{HerdrError, Op};
    use crate::headroom::{FakeHeadroom, HeadroomSnapshot, MIB};

    fn snap() -> HeadroomSnapshot {
        HeadroomSnapshot {
            commit_avail: 5120 * MIB,
            phys_avail: 3200 * MIB,
            pressure: None,
        }
    }

    #[test]
    fn note_spawned_records_spawned_start_with_headroom() {
        let sup = Supervisor::with_headroom(Arc::new(FakeHeadroom::constant(snap())));
        let ((), got) = lifecycle::with_capture(|| sup.note_spawned(4242, ServerStartHow::Spawned));
        assert_eq!(
            got,
            vec![LifecycleEvent::ServerStart {
                pid: Some(4242),
                how: ServerStartHow::Spawned,
                headroom: Some(snap()),
            }]
        );
    }

    #[test]
    fn note_adopted_records_once_per_process() {
        let sup = Supervisor::with_headroom(Arc::new(FakeHeadroom::constant(snap())));
        let ((), got) = lifecycle::with_capture(|| {
            sup.note_adopted();
            sup.note_adopted();
        });
        assert_eq!(
            got,
            vec![LifecycleEvent::ServerStart {
                pid: None,
                how: ServerStartHow::Adopted,
                headroom: Some(snap()),
            }]
        );
    }

    #[test]
    fn start_record_without_headroom_sample() {
        let sup = Supervisor::with_headroom(Arc::new(FakeHeadroom::unavailable()));
        let ((), got) = lifecycle::with_capture(|| sup.note_spawned(7, ServerStartHow::Spawned));
        assert_eq!(
            got,
            vec![LifecycleEvent::ServerStart {
                pid: Some(7),
                how: ServerStartHow::Spawned,
                headroom: None,
            }]
        );
    }

    #[test]
    fn restart_signal_changes_across_restarts() {
        let sup = Supervisor::new();
        let a = sup.restart_signal();
        // Simulate a restart cycle that could not capture a child pid (surrogate).
        sup.instance.fetch_add(1, Ordering::SeqCst);
        let b = sup.restart_signal();
        assert_ne!(a, b, "the restart signal must change across a (re)start");
    }

    fn quiet_supervisor() -> Supervisor {
        Supervisor::with_headroom(Arc::new(FakeHeadroom::unavailable()))
    }

    #[test]
    fn start_spawns_records_how_and_returns_the_instance() {
        let fake = FakeHerdr::new();
        fake.kill_server(Some(1));
        let sup = quiet_supervisor();
        let (r, got) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Restart));
        assert_eq!(Some(r.unwrap()), fake.server_instance());
        assert_eq!(sup.spawned_pid(), Some(1001), "the fake's new instance pid");
        assert_eq!(sup.restart_signal(), 1001);
        assert_eq!(
            got,
            vec![LifecycleEvent::ServerStart {
                pid: Some(1001),
                how: ServerStartHow::Restart,
                headroom: None,
            }]
        );
    }

    #[test]
    fn require_running_never_starts_the_server() {
        let fake = FakeHerdr::new();
        let (sup, _wd, rx) = watched_supervisor();
        assert!(sup.require_running(&*fake).is_ok());

        fake.kill_server(Some(1));
        assert!(matches!(
            sup.require_running(&*fake),
            Err(PtyBackendError::RecoveryPending)
        ));
        let (_, ev) = rx.try_recv().expect("a refused status is reported at once");
        assert_eq!(ev.source, DeathSource::Adopted);
        assert!(rx.try_recv().is_err(), "exactly one report");
        assert!(!fake.calls().iter().any(|c| c.op == Op::SpawnServer));
    }

    #[test]
    fn require_running_after_a_requested_stop_is_not_connected_and_reports_nothing() {
        let fake = FakeHerdr::new();
        let (sup, _wd, rx) = watched_supervisor();
        sup.stop(&*fake).unwrap();
        assert!(matches!(
            sup.require_running(&*fake),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(rx.try_recv().is_err());
        assert!(!fake.calls().iter().any(|c| c.op == Op::SpawnServer));
    }

    #[test]
    fn require_running_passes_call_errors_through() {
        let fake = FakeHerdr::new();
        let (sup, _wd, rx) = watched_supervisor();
        fake.fail_next(Op::StatusServer, HerdrError::Timeout);
        assert!(matches!(
            sup.require_running(&*fake),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(rx.try_recv().is_err(), "a timeout alone is not a death");
    }

    #[test]
    fn start_spawn_failure_errors_without_waiting() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.fail_spawn_server(Some(std::io::ErrorKind::PermissionDenied));
        let sup = quiet_supervisor();
        let t0 = Instant::now();
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned));
        let err = r.unwrap_err().to_string();
        assert!(err.starts_with("herdr server spawn failed"), "{err}");
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "no start-timeout wait"
        );
        assert_eq!(sup.spawned_pid(), None);
    }

    #[test]
    fn start_times_out_when_the_server_never_answers() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.hang(Op::SpawnServer, true);
        let sup =
            quiet_supervisor().with_timing(Duration::from_millis(50), Duration::from_millis(5));
        let t0 = Instant::now();
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned));
        let err = r.unwrap_err().to_string();
        assert!(err.contains("did not come up"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(5));
        let polls = fake
            .calls()
            .iter()
            .filter(|c| c.op == Op::StatusServer)
            .count();
        assert!(polls >= 2, "polled the status until the deadline ({polls})");
    }

    #[test]
    fn ensure_ready_is_skipped_under_test() {
        let fake = FakeHerdr::new();
        let sup = quiet_supervisor();
        sup.ensure_ready(&*fake);
        sup.ensure_ready(&*fake);
        assert!(
            fake.calls().is_empty(),
            "no integration status/install under cfg(test): {:?}",
            fake.calls()
        );
    }

    #[test]
    fn refresh_integration_resets_the_once_latch() {
        let fake = FakeHerdr::new();
        let sup = quiet_supervisor();
        sup.ensure_ready(&*fake);
        sup.ensure_ready(&*fake);
        assert_eq!(sup.ready_runs(), 1, "once per process");
        sup.refresh_integration(&*fake);
        assert_eq!(sup.ready_runs(), 2, "the refresh runs it again");
        sup.ensure_ready(&*fake);
        assert_eq!(sup.ready_runs(), 2, "and latches again");
        assert!(
            fake.calls().is_empty(),
            "still no integration status/install under cfg(test): {:?}",
            fake.calls()
        );
    }

    #[test]
    fn stop_issues_server_stop_and_clears_the_pid() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let sup = quiet_supervisor();
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned));
        assert!(sup.spawned_pid().is_some());
        sup.stop(&*fake).unwrap();
        assert_eq!(sup.spawned_pid(), None);
        assert!(!fake.status_server().unwrap().running);
    }

    use super::super::ops::Generation;
    use super::super::watchdog::{DeathEvent, DeathSource};
    use std::sync::mpsc::{self, Receiver};

    type DeathRx = Receiver<(Generation, DeathEvent)>;

    fn watched_supervisor() -> (Supervisor, Arc<Watchdog>, DeathRx) {
        let (tx, rx) = mpsc::channel();
        let wd = Arc::new(Watchdog::new(
            tx,
            Arc::new(FakeHeadroom::constant(snap())),
            Arc::new(|| Generation(4)),
        ));
        let sup = quiet_supervisor()
            .with_timing(Duration::from_secs(5), Duration::from_millis(5))
            .with_watchdog(Arc::clone(&wd));
        (sup, wd, rx)
    }

    const NTSTATUS_ABORT: i32 = 0xC000_0409u32 as i32;

    #[test]
    fn spawned_child_death_is_reported_once() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let (sup, wd, rx) = watched_supervisor();
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());
        let instance = wd.noted_instance().expect("the started server is watched");
        assert_eq!(instance.pid, 1001);
        assert_eq!(Some(instance), fake.server_instance());

        fake.kill_server(Some(NTSTATUS_ABORT));
        let (gen, ev) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the waiter reports the exit");
        assert_eq!(gen, Generation(4));
        assert_eq!(ev.source, DeathSource::Child);
        assert_eq!(ev.exit_code, Some(NTSTATUS_ABORT));
        assert_eq!(ev.instance, instance);
        assert_eq!(ev.headroom_at_death, Some(snap()));
        assert!(ev.uptime < Duration::from_secs(60));

        // The adopted path cannot report the same server again.
        wd.report_down_now();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn requested_stop_reports_no_death_until_the_next_server() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let (sup, wd, rx) = watched_supervisor();
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());

        sup.stop(&*fake).unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the child's exit after a stop is not a death"
        );
        for _ in 0..5 {
            wd.report_probe(&fake.status_server(), false);
        }
        wd.report_down_now();
        assert!(rx.try_recv().is_err(), "adopted reports are suppressed too");

        // The next server lifts the suppression.
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());
        assert_eq!(wd.noted_instance().map(|i| i.pid), Some(1002));
        fake.kill_server(Some(1));
        let (_, ev) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("death of the next server");
        assert_eq!(ev.exit_code, Some(1));
        assert_eq!(ev.instance.pid, 1002);
    }

    #[test]
    fn expect_stop_flags_the_child_before_an_external_stop() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let (sup, _wd, rx) = watched_supervisor();
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());
        sup.expect_stop();
        fake.kill_server(Some(0));
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn server_exiting_before_it_answers_fails_the_start_at_once() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.hang(Op::SpawnServer, true);
        let (sup, wd, rx) = watched_supervisor();
        let killer = {
            let fake = Arc::clone(&fake);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !fake.calls().iter().any(|c| c.op == Op::SpawnServer) {
                    assert!(Instant::now() < deadline, "server never spawned");
                    std::thread::sleep(Duration::from_millis(1));
                }
                fake.kill_server(Some(3));
            })
        };
        let t0 = Instant::now();
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned));
        killer.join().unwrap();
        let err = r.unwrap_err().to_string();
        assert!(err.contains("exited before answering (exit 3)"), "{err}");
        assert!(
            t0.elapsed() < Duration::from_secs(4),
            "did not wait out the timeout"
        );
        assert_eq!(sup.spawned_pid(), None);
        assert_eq!(wd.noted_instance(), None, "a failed start is not watched");
        assert!(rx.try_recv().is_err(), "a failed start is not a death");
    }

    #[test]
    fn adopted_server_is_noted_once_per_instance() {
        let fake = FakeHerdr::new();
        let (sup, wd, rx) = watched_supervisor();
        let _ = lifecycle::with_capture(|| sup.adopt(&*fake));
        assert_eq!(wd.noted_instance(), fake.server_instance());

        // Two failed probes, then another adopt for the same server must
        // not reset the counter: the third failure declares it dead.
        wd.report_probe(&Err(HerdrError::Timeout), false);
        wd.report_probe(&Err(HerdrError::Timeout), false);
        let _ = lifecycle::with_capture(|| sup.adopt(&*fake));
        wd.report_probe(&Err(HerdrError::Timeout), false);
        let (_, ev) = rx.try_recv().expect("declared dead");
        assert_eq!(ev.source, DeathSource::Adopted);
        assert_eq!(Some(ev.instance), fake.server_instance());
    }

    /// A killer that records each pid it is asked to kill (and runs `then`).
    fn recording_killer(then: impl Fn() + Send + Sync + 'static) -> (Killer, Arc<Mutex<Vec<u32>>>) {
        let killed = Arc::new(Mutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        let killer: Killer = Arc::new(move |pid| {
            k.lock().unwrap().push(pid);
            then();
        });
        (killer, killed)
    }

    fn fast_stop(sup: Supervisor, killer: Killer) -> Supervisor {
        sup.with_stop(Duration::from_millis(30), Duration::from_millis(30), killer)
    }

    #[test]
    fn stop_and_wait_stops_our_child_without_a_death_or_a_kill() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let (sup, _wd, rx) = watched_supervisor();
        let (killer, killed) = recording_killer(|| {});
        let sup = fast_stop(sup, killer);
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());
        sup.stop_and_wait(&*fake).unwrap();
        assert_eq!(sup.spawned_pid(), None);
        assert!(killed.lock().unwrap().is_empty());
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.op == Op::ServerStop)
                .count(),
            1
        );
    }

    #[test]
    fn stop_and_wait_kills_a_server_that_refuses_by_pid_then_gives_up() {
        let fake = FakeHerdr::new();
        fake.fail_next(Op::ServerStop, HerdrError::Timeout);
        let (sup, _wd, rx) = watched_supervisor();
        let (killer, killed) = recording_killer(|| {});
        let sup = fast_stop(sup, killer);
        let err = sup.stop_and_wait(&*fake).unwrap_err().to_string();
        assert_eq!(err, "could not stop herdr");
        assert_eq!(
            *killed.lock().unwrap(),
            vec![1000],
            "the adopted server's pid"
        );
        assert!(rx.try_recv().is_err(), "a requested stop is not a death");
    }

    #[test]
    fn stop_and_wait_fails_at_once_when_herdr_is_missing_and_keeps_watching() {
        use crate::backend::herdr::fake::exe_missing_error;
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let (sup, wd, rx) = watched_supervisor();
        let (killer, killed) = recording_killer(|| {});
        // Waits long enough that sitting through either one fails the test.
        let sup = sup.with_stop(Duration::from_secs(30), Duration::from_secs(30), killer);
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());
        let before = fake.calls().len();

        fake.set_exe_missing(true);
        let t0 = Instant::now();
        let err = sup.stop_and_wait(&*fake).unwrap_err();
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "no stop or kill wait"
        );
        assert_eq!(
            err.to_string(),
            PtyBackendError::from(exe_missing_error()).to_string()
        );
        assert!(killed.lock().unwrap().is_empty(), "nothing is killed");
        let ops: Vec<Op> = fake.calls()[before..].iter().map(|c| c.op).collect();
        assert_eq!(ops, vec![Op::ServerStop], "no status polling: {ops:?}");
        // The server was not stopped: it is still ours and still watched.
        assert_eq!(sup.spawned_pid(), Some(1001));
        assert!(!wd.stop_expected());
        fake.set_exe_missing(false);
        fake.kill_server(Some(NTSTATUS_ABORT));
        let (_, ev) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("its death is still reported");
        assert_eq!(ev.instance.pid, 1001);
    }

    #[test]
    fn stop_and_wait_with_herdr_missing_keeps_an_earlier_expected_stop() {
        let fake = FakeHerdr::new();
        let (sup, wd, rx) = watched_supervisor();
        sup.adopt(&*fake);
        // The user stopped the server; a later restart finds herdr gone.
        sup.expect_stop();
        fake.set_exe_missing(true);
        assert!(sup.stop_and_wait(&*fake).is_err());
        assert!(wd.stop_expected(), "the user's stop still stands");
        wd.report_down_now();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn require_running_with_herdr_missing_is_an_error_not_a_death() {
        let fake = FakeHerdr::new();
        let (sup, wd, rx) = watched_supervisor();
        sup.adopt(&*fake);
        fake.set_exe_missing(true);
        for _ in 0..5 {
            assert!(matches!(
                sup.require_running(&*fake),
                Err(PtyBackendError::Other(m)) if m.starts_with("herdr spawn failed")
            ));
        }
        assert!(
            rx.try_recv().is_err(),
            "a missing executable is not a death"
        );
        assert!(!wd.death_reported());
    }

    #[test]
    fn stop_and_wait_succeeds_once_the_kill_lands() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        let (sup, _wd, _rx) = watched_supervisor();
        let target = Arc::clone(&fake);
        let (killer, killed) = recording_killer(move || target.kill_server(Some(9)));
        let sup = fast_stop(sup, killer);
        let _ = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned).unwrap());
        fake.fail_next(Op::ServerStop, HerdrError::Timeout);
        sup.stop_and_wait(&*fake).unwrap();
        assert_eq!(*killed.lock().unwrap(), vec![1001], "our child's pid");
    }

    #[test]
    fn start_with_a_live_server_uses_it_instead_of_spawning_another() {
        let fake = FakeHerdr::new();
        let live = fake.server_instance().unwrap();
        let (sup, wd, rx) = watched_supervisor();
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Restart));
        assert_eq!(r.unwrap(), live, "the server that answers");
        assert!(
            !fake.calls().iter().any(|c| c.op == Op::SpawnServer),
            "no second server: {:?}",
            fake.calls()
        );
        assert_eq!(sup.spawned_pid(), None, "not our child");
        assert_eq!(wd.noted_instance(), Some(live), "watched as adopted");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_child_that_exits_while_another_server_answers_is_not_taken_for_it() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.hang(Op::SpawnServer, true);
        let foreign = ServerInstance {
            pid: 4242,
            start_time: 9_000_000,
        };
        let (sup, wd, rx) = watched_supervisor();
        let swap = {
            let fake = Arc::clone(&fake);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !fake.calls().iter().any(|c| c.op == Op::SpawnServer) {
                    assert!(Instant::now() < deadline, "server never spawned");
                    std::thread::sleep(Duration::from_millis(1));
                }
                // Our child loses the socket race and exits; the winner answers.
                fake.replace_with_foreign(1001, foreign);
            })
        };
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Restart));
        swap.join().unwrap();
        assert_eq!(r.unwrap(), foreign);
        assert_eq!(
            sup.spawned_pid(),
            None,
            "the exited child is not the server"
        );
        assert_eq!(wd.noted_instance(), Some(foreign));
        assert!(fake.reaped(1001), "the exited child is collected");
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "the child's exit is not the answering server's death"
        );
    }

    #[test]
    fn a_server_killed_at_the_start_timeout_is_reaped() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.hang(Op::SpawnServer, true);
        let sup =
            quiet_supervisor().with_timing(Duration::from_millis(30), Duration::from_millis(5));
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Spawned));
        assert!(r.unwrap_err().to_string().contains("did not come up"));
        assert!(fake.reaped(1001), "the killed child's exit is collected");
    }

    #[test]
    fn start_retries_an_identity_that_cannot_be_read_at_once() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.fail_next(Op::ServerInstance, HerdrError::Timeout);
        fake.fail_next(Op::ServerInstance, HerdrError::Timeout);
        let (sup, wd, _rx) = watched_supervisor();
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Restart));
        let got = r.unwrap();
        assert_eq!(Some(got), fake.server_instance(), "the full identity");
        assert_ne!(got.start_time, 0);
        assert_eq!(wd.noted_instance(), Some(got));
    }

    #[test]
    fn start_with_an_unreadable_identity_reports_only_the_pid() {
        let fake = FakeHerdr::new();
        fake.kill_server(None);
        fake.hang(Op::ServerInstance, true);
        let (sup, _wd, _rx) = watched_supervisor();
        let (r, _) = lifecycle::with_capture(|| sup.start(&*fake, ServerStartHow::Restart));
        assert_eq!(
            r.unwrap(),
            ServerInstance {
                pid: 1001,
                start_time: 0
            },
            "the child's pid, start time unknown"
        );
    }

    #[test]
    fn integration_enabled_respects_opt_out() {
        assert!(integration_enabled(None));
        assert!(!integration_enabled(Some("1".into())));
        assert!(
            !integration_enabled(Some(String::new())),
            "any value opts out"
        );
    }
}
