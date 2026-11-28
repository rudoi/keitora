//! Shared filesystem-watcher scaffolding for a single directory + filename family.
//!
//! Every file watcher runs on [`watch_dir`]: `host::start_tasks_watcher` and
//! `bg_notifier::spawn_project_watcher` watch a `tasks.db` / `-wal` / `-shm` family,
//! `host::start_config_watcher` the active project's `keitora.toml`, and
//! `host::start_registry_watcher` the user registry. All need the same plumbing: a
//! `notify` watcher over a directory, filtered to a filename prefix, draining bursts
//! behind a debounce, and a poll loop that exits promptly when its stop channel is
//! dropped. Only the per-event work, the filename prefix and the debounce differ.
//!
//! A watcher never dies silently: it reports how it ended ([`WatchExit`]) and keeps a
//! [`WatcherHealth`] that the supervisor (`crate::watchers`) reads to restart a
//! watcher that failed to start, lost its event channel or stopped looping.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicI64, AtomicU8, Ordering},
    sync::{mpsc, Mutex},
    time::{Duration, Instant},
};

use crate::locks::lock;

/// How often the loop wakes while idle (stop check and liveness stamp).
pub(crate) const WATCH_POLL: Duration = Duration::from_millis(500);

/// A watcher thread's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchState {
    Starting = 0,
    Running = 1,
    /// Ended on its own (failed to start, lost its channel, or found stalled).
    Exited = 2,
    /// Superseded or stopped on purpose; never restarted.
    Stopped = 3,
}

impl WatchState {
    fn from_u8(v: u8) -> WatchState {
        match v {
            0 => WatchState::Starting,
            1 => WatchState::Running,
            2 => WatchState::Exited,
            _ => WatchState::Stopped,
        }
    }

    /// The `app_projects.watcher_state` spelling (`stopped` reads as `absent`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WatchState::Starting => "starting",
            WatchState::Running => "running",
            WatchState::Exited => "exited",
            WatchState::Stopped => "absent",
        }
    }
}

/// How [`watch_dir`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchExit {
    /// Its stop channel fired or was dropped.
    Stopped,
    /// `notify` couldn't create the watcher or watch the directory.
    StartFailed(String),
    /// The event channel disconnected (the `notify` watcher went away).
    Disconnected,
}

/// One watcher thread's health, shared with whoever supervises it. Times are unix ms
/// (0 = never).
#[derive(Debug)]
pub(crate) struct WatcherHealth {
    state: AtomicU8,
    started_at_ms: AtomicI64,
    /// Stamped every loop iteration (at most [`WATCH_POLL`] apart while running).
    alive_at_ms: AtomicI64,
    /// A `notify` event passed the prefix filter (or an error event arrived).
    last_event_at_ms: AtomicI64,
    last_error: Mutex<Option<(String, i64)>>,
    exit_reason: Mutex<Option<String>>,
    /// Feeds raw `notify` results into a running loop, for tests.
    #[cfg(test)]
    inject: Mutex<Option<mpsc::Sender<notify::Result<notify::Event>>>>,
}

impl Default for WatcherHealth {
    fn default() -> Self {
        WatcherHealth {
            state: AtomicU8::new(WatchState::Starting as u8),
            started_at_ms: AtomicI64::new(0),
            alive_at_ms: AtomicI64::new(0),
            last_event_at_ms: AtomicI64::new(0),
            last_error: Mutex::new(None),
            exit_reason: Mutex::new(None),
            #[cfg(test)]
            inject: Mutex::new(None),
        }
    }
}

fn opt_ms(v: i64) -> Option<i64> {
    (v > 0).then_some(v)
}

impl WatcherHealth {
    /// Health for a watcher no supervisor tracks (the config and registry watchers):
    /// the same bookkeeping, read by nobody.
    pub(crate) fn detached() -> std::sync::Arc<WatcherHealth> {
        std::sync::Arc::default()
    }

    pub(crate) fn state(&self) -> WatchState {
        WatchState::from_u8(self.state.load(Ordering::SeqCst))
    }

    fn set_state(&self, s: WatchState) {
        self.state.store(s as u8, Ordering::SeqCst);
    }

    /// Mark the watcher ended on its own with `reason` (a `Stopped` one stays so).
    pub(crate) fn mark_exited(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let _ = self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
                (s != WatchState::Stopped as u8).then_some(WatchState::Exited as u8)
            });
        *lock(&self.exit_reason) = Some(reason);
    }

    /// Mark the watcher superseded: it is never restarted.
    pub(crate) fn mark_stopped(&self) {
        self.set_state(WatchState::Stopped);
    }

    pub(crate) fn started_at_ms(&self) -> Option<i64> {
        opt_ms(self.started_at_ms.load(Ordering::SeqCst))
    }

    pub(crate) fn alive_at_ms(&self) -> Option<i64> {
        opt_ms(self.alive_at_ms.load(Ordering::SeqCst))
    }

    pub(crate) fn last_event_at_ms(&self) -> Option<i64> {
        opt_ms(self.last_event_at_ms.load(Ordering::SeqCst))
    }

    /// The last error and when it happened.
    pub(crate) fn last_error(&self) -> Option<(String, i64)> {
        lock(&self.last_error).clone()
    }

    /// Record an error the watcher keeps running through.
    pub(crate) fn record_error(&self, msg: impl Into<String>) {
        *lock(&self.last_error) = Some((msg.into(), keitora_tasks::now_ms()));
    }

    pub(crate) fn exit_reason(&self) -> Option<String> {
        lock(&self.exit_reason).clone()
    }

    fn stamp(cell: &AtomicI64) {
        cell.store(keitora_tasks::now_ms(), Ordering::SeqCst);
    }

    /// Test seam: set the stamps directly.
    #[cfg(test)]
    pub(crate) fn set_times(&self, alive_at_ms: i64, last_event_at_ms: i64) {
        self.alive_at_ms.store(alive_at_ms, Ordering::SeqCst);
        self.last_event_at_ms
            .store(last_event_at_ms, Ordering::SeqCst);
    }

    /// Test seam: set when the loop started.
    #[cfg(test)]
    pub(crate) fn set_started(&self, started_at_ms: i64) {
        self.started_at_ms.store(started_at_ms, Ordering::SeqCst);
    }

    /// Test seam: force a state.
    #[cfg(test)]
    pub(crate) fn force_state(&self, s: WatchState) {
        self.set_state(s);
    }

    /// Test seam: a sender into the running loop's raw `notify` channel.
    #[cfg(test)]
    pub(crate) fn injector(&self) -> Option<mpsc::Sender<notify::Result<notify::Event>>> {
        lock(&self.inject).clone()
    }
}

/// Whether a raw `notify` result should wake the loop. A matching event does; an
/// error event does too (a `Rescan` or overflow means events were lost, so the
/// caller should look), after recording it.
fn wakes(
    res: &notify::Result<notify::Event>,
    prefix: &str,
    dir: &str,
    health: &WatcherHealth,
) -> bool {
    match res {
        Ok(ev) => ev.paths.iter().any(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        }),
        Err(e) => {
            log::warn!("watcher {dir}: notify error: {e}");
            health.record_error(format!("notify error: {e}"));
            true
        }
    }
}

/// Watch `dir` for writes to the `file_prefix` family (any filename starting with
/// `file_prefix`, e.g. `tasks.db` / `tasks.db-wal` / `tasks.db-shm`, or `keitora.toml`)
/// and invoke `on_event` once per `debounce`d burst. `health` is kept current
/// throughout (state, liveness, last event, errors).
///
/// Runs **synchronously**: it *is* the watcher-loop body, so the caller owns the
/// thread and whatever state `on_event` captures. Returns how it ended: when the
/// watcher fails to start, when `stop_rx` is signalled (`()` sent) or its sender is
/// dropped, or when the internal event channel disconnects.
///
/// The loop polls with `recv_timeout` rather than a blocking `recv` so the stop
/// check still runs when the watched file is idle (an idle file fires no `notify`
/// events): a blocking recv would never observe the stop signal and the thread
/// would leak.
pub(crate) fn watch_dir(
    dir: PathBuf,
    file_prefix: String,
    debounce: Duration,
    stop_rx: mpsc::Receiver<()>,
    health: std::sync::Arc<WatcherHealth>,
    mut on_event: impl FnMut(),
) -> WatchExit {
    use notify::{RecursiveMode, Watcher};

    let shown = dir.display().to_string();
    // FSEvents reports real paths, so a directory reached through a symlink (macOS
    // `/var` → `/private/var`, or a symlinked home) must be watched by its real path.
    #[cfg(target_os = "macos")]
    let dir = dir.canonicalize().unwrap_or(dir);
    let fail = |e: String| {
        log::warn!("watcher {shown}: failed to start: {e}");
        health.mark_exited(format!("failed to start: {e}"));
        WatchExit::StartFailed(e)
    };
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    #[cfg(test)]
    {
        *lock(&health.inject) = Some(tx.clone());
    }
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => return fail(e.to_string()),
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        return fail(e.to_string());
    }
    health.set_state(WatchState::Running);
    WatcherHealth::stamp(&health.started_at_ms);

    loop {
        WatcherHealth::stamp(&health.alive_at_ms);
        // Stop check: exit if this watcher was superseded (its stop sender was
        // dropped, or `()` was sent). Checked first each iteration so a stop
        // signal is honoured even right after a DB event.
        match stop_rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                health.mark_stopped();
                return WatchExit::Stopped;
            }
            Err(mpsc::TryRecvError::Empty) => {} // still the live watcher
        }
        // Poll with a timeout so the stop check above runs even when the DB is
        // idle. A timeout just loops back to re-check the stop flag.
        match rx.recv_timeout(WATCH_POLL) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(res) if wakes(&res, &file_prefix, &shown, &health) => {}
            Ok(_) => continue, // another file in the directory
        }
        WatcherHealth::stamp(&health.last_event_at_ms);
        // Debounce: drain further events for `debounce` before processing once.
        let deadline = Instant::now() + debounce;
        while let Ok(res) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if res.is_err() {
                wakes(&res, &file_prefix, &shown, &health);
            }
        }
        on_event();
    }
    log::warn!("watcher {shown}: event channel disconnected");
    health.mark_exited("event channel disconnected");
    drop(watcher);
    WatchExit::Disconnected
}

/// Run `watch_dir` and log an exit that wasn't asked for (the config and registry
/// watchers: no supervisor restarts them, so the log is their only trace).
pub(crate) fn watch_dir_logged(
    what: &str,
    dir: PathBuf,
    file_prefix: String,
    debounce: Duration,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl FnMut(),
) {
    match watch_dir(
        dir,
        file_prefix,
        debounce,
        stop_rx,
        WatcherHealth::detached(),
        on_event,
    ) {
        WatchExit::Stopped => {}
        exit => log::warn!("{what} watcher ended: {exit:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    /// Wait up to `budget` for `cond`.
    fn wait_for(budget: Duration, cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + budget;
        while Instant::now() < end {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    /// Start `watch_dir` on `dir` in a thread; `on_event` bumps the returned counter.
    fn spawn_watch(
        dir: PathBuf,
        prefix: &str,
    ) -> (
        mpsc::SyncSender<()>,
        Arc<WatcherHealth>,
        Arc<std::sync::atomic::AtomicUsize>,
        thread::JoinHandle<WatchExit>,
    ) {
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        let health = Arc::new(WatcherHealth::default());
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (h, f, prefix) = (Arc::clone(&health), Arc::clone(&fired), prefix.to_string());
        let handle = thread::spawn(move || {
            watch_dir(
                dir,
                prefix,
                Duration::from_millis(50),
                stop_rx,
                h,
                move || {
                    f.fetch_add(1, Ordering::SeqCst);
                },
            )
        });
        (stop_tx, health, fired, handle)
    }

    /// Dropping the stop sender must wind the loop down promptly even when the
    /// watched directory never changes: the `recv_timeout(500ms)` poll is what
    /// guarantees the disconnect is observed (a blocking recv would leak the
    /// thread).
    #[test]
    fn watch_dir_exits_when_stop_sender_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (stop_tx, health, _fired, handle) = spawn_watch(dir.path().to_path_buf(), "tasks.db");
        assert!(wait_for(Duration::from_secs(3), || health.state()
            == WatchState::Running));
        drop(stop_tx);
        assert!(
            wait_for(Duration::from_secs(3), || handle.is_finished()),
            "watch_dir must exit promptly after its stop sender is dropped",
        );
        assert_eq!(handle.join().unwrap(), WatchExit::Stopped);
        assert_eq!(health.state(), WatchState::Stopped);
    }

    #[test]
    fn a_missing_directory_fails_to_start_and_records_why() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let (_stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        let health = Arc::new(WatcherHealth::default());
        let exit = watch_dir(
            missing,
            "tasks.db".into(),
            Duration::from_millis(50),
            stop_rx,
            Arc::clone(&health),
            || {},
        );
        assert!(matches!(exit, WatchExit::StartFailed(_)), "{exit:?}");
        assert_eq!(health.state(), WatchState::Exited);
        let reason = health.exit_reason().expect("exit reason recorded");
        assert!(reason.starts_with("failed to start: "), "{reason}");
        assert_eq!(health.started_at_ms(), None);
    }

    #[test]
    fn alive_advances_while_idle() {
        let dir = tempfile::tempdir().unwrap();
        let (stop_tx, health, fired, handle) = spawn_watch(dir.path().to_path_buf(), "tasks.db");
        assert!(wait_for(Duration::from_secs(3), || health
            .alive_at_ms()
            .is_some()));
        let first = health.alive_at_ms().unwrap();
        assert!(health.started_at_ms().is_some());
        assert!(
            wait_for(Duration::from_secs(3), || health.alive_at_ms().unwrap()
                > first),
            "the idle loop keeps stamping its liveness"
        );
        assert_eq!(fired.load(Ordering::SeqCst), 0, "no event, no pass");
        assert_eq!(health.last_event_at_ms(), None);
        drop(stop_tx);
        handle.join().unwrap();
    }

    #[test]
    fn a_notify_error_is_recorded_and_still_runs_on_event() {
        let dir = tempfile::tempdir().unwrap();
        let (stop_tx, health, fired, handle) = spawn_watch(dir.path().to_path_buf(), "tasks.db");
        assert!(wait_for(Duration::from_secs(3), || health.state()
            == WatchState::Running));
        let inject = health.injector().expect("a running loop has an injector");
        inject
            .send(Err(notify::Error::generic("events overflowed")))
            .unwrap();
        assert!(wait_for(Duration::from_secs(3), || fired
            .load(Ordering::SeqCst)
            == 1));
        let (msg, at) = health.last_error().expect("error recorded");
        assert!(msg.contains("events overflowed"), "{msg}");
        assert!(at > 0);
        assert_eq!(health.state(), WatchState::Running, "it keeps running");
        drop(stop_tx);
        handle.join().unwrap();
    }

    #[test]
    fn mark_exited_never_revives_a_stopped_watcher() {
        let h = WatcherHealth::default();
        h.mark_stopped();
        h.mark_exited("loop stalled");
        assert_eq!(h.state(), WatchState::Stopped);
        let h = WatcherHealth::default();
        h.mark_exited("loop stalled");
        assert_eq!(h.state(), WatchState::Exited);
        assert_eq!(h.exit_reason().as_deref(), Some("loop stalled"));
    }

    /// The WAL family must wake the watcher: a write from another connection lands in
    /// `-wal` (and `-shm`), and after a truncating checkpoint the next write must still
    /// be seen.
    ///
    /// Not run on macOS: there FSEvents has not been seen to report a second
    /// connection's `-wal` write within the wait on CI runners. The request sweep and
    /// the silent-watcher check are the backstop for a watcher that misses writes.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn wal_writes_from_other_connections_fire_before_and_after_a_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let owner = rusqlite::Connection::open(&path).unwrap();
        owner
            .execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t (x INTEGER);")
            .unwrap();
        let (stop_tx, health, fired, handle) = spawn_watch(dir.path().to_path_buf(), "tasks.db");
        assert!(wait_for(Duration::from_secs(3), || health.state()
            == WatchState::Running));
        // Let the setup writes' events drain before counting.
        thread::sleep(Duration::from_millis(300));
        let base = fired.load(Ordering::SeqCst);

        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute("INSERT INTO t VALUES (1)", []).unwrap();
        assert!(
            wait_for(Duration::from_secs(5), || fired.load(Ordering::SeqCst)
                > base),
            "a write from a second connection fires"
        );

        let checkpointer = rusqlite::Connection::open(&path).unwrap();
        checkpointer
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        let after_checkpoint = fired.load(Ordering::SeqCst);
        writer.execute("INSERT INTO t VALUES (2)", []).unwrap();
        assert!(
            wait_for(Duration::from_secs(5), || fired.load(Ordering::SeqCst)
                > after_checkpoint),
            "a write after a truncating checkpoint fires again"
        );
        drop(stop_tx);
        handle.join().unwrap();
    }
}
