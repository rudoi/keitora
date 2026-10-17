//! `writer` — one ordered input queue per session.
//!
//! `pty_write` runs on the main thread, so it must never block on a pipe, a lock
//! held by a bridge restart, or a herdr CLI process. Each [`SessionKey`] gets one
//! writer thread fed by an `mpsc` channel: the caller only enqueues a
//! [`WriteJob`], and the thread does the gate check, the bridge stdin write, the
//! CLI fallback and the error-driven bridge teardown, one job at a time. One
//! thread per key (not per bridge) keeps keys in order across bridge restarts and
//! the fallback, and keeps injected lines (`write_line`) ordered with typed keys.
//!
//! A writer is bound to one session generation (the keymap `gen`): a job for a
//! newer generation of the key replaces the writer, and the old one (still
//! draining what it already took) only ever touches its own generation's bridge
//! and pane, never the new session's. A job for an older generation than the
//! writer's is refused (`NoSuchSession`): its session is already gone.
//!
//! While the writer is behind, queued input is coalesced: adjacent text jobs are
//! concatenated (likewise bytes) into one `terminal.input` line, flushing at each
//! kind boundary. With no bridge (one is restarting) the writer holds the run for
//! up to [`BRIDGE_WAIT`], collecting what arrives meanwhile, and only then sends the
//! whole run through the CLI fallback as one call.
//!
//! Resizes and repaints go through the same queue, so a resize lands after the
//! keys typed before it and before those typed after it. Each is one
//! `terminal.resize` on the bridge stdin (a repaint is a same-size resize); a
//! failed write restarts the bridge at the wanted size instead. Back-to-back
//! queued resizes collapse to the last one.
//!
//! Scrolls (wheel ticks and page keys) are queued the same way, and back-to-back
//! queued scrolls of one direction and source merge into one; the bridge then
//! writes a wheel scroll as one-line commands (see [`bridge::scroll_payload`]).
//! A scroll is best-effort: one that cannot be written is dropped (never retried,
//! never a bridge restart), and its dead bridge is dropped as failed input drops it.
//!
//! A bridge stdin write can block for good on the full pipe of a control child
//! that is alive but no longer reading. One watchdog thread per [`Writers`]
//! registry scans every writer's in-flight write: one still blocked after
//! [`STALL_LIMIT`] has its bridge restarted, which kills the stuck child, breaks
//! the pipe and frees the writer. A write costs the writer two atomic stores.
//!
//! The thread ends once its sender is dropped ([`Writers::remove`] on kill, a
//! newer generation replacing it, or the backend going away) and its queue is
//! done.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{PtyBackendError, ScrollDirection, ScrollSource, SessionKey};

use super::bridge::{self, BridgeInput, BridgeStdin};

/// How long input waits for a bridge before it goes through the CLI fallback.
pub const BRIDGE_WAIT: Duration = Duration::from_millis(500);

/// How long one bridge stdin write may block before its bridge is restarted.
pub const STALL_LIMIT: Duration = Duration::from_secs(5);

/// How long `write_line` waits for its line to be sent. The line can sit behind
/// queued input (a bridge wait, a stalled write) and is then one herdr CLI call
/// with its own deadline, so this only trips when the writer is wedged.
pub const LINE_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a writer waiting for a bridge looks for one.
const BRIDGE_POLL: Duration = Duration::from_millis(2);

/// How often the stall watchdog scans the writers (at most; a shorter stall
/// limit scans more often).
const WATCH_SCAN: Duration = Duration::from_secs(1);

/// One unit of work for a session's writer, executed in enqueue order.
#[derive(Debug)]
pub enum WriteJob {
    /// Keystrokes / pasted text (sent as `terminal.input` `text`).
    Text(String),
    /// Input that is not UTF-8 (sent as base64 `bytes`).
    Bytes(Vec<u8>),
    /// An injected line (dispatch / handoff); the result goes back on `reply`.
    Line {
        text: String,
        reply: SyncSender<Result<(), PtyBackendError>>,
        /// Claimed by whoever takes the line first: the writer, to send it, or
        /// the caller giving up on it. A line the caller gave up on is not sent.
        claim: LineClaim,
    },
    /// Resize the bridge in place (a no-op when the size is unchanged).
    Resize { cols: u16, rows: u16 },
    /// Ask for a fresh full frame: a resize to the bridge's current size.
    Repaint,
    /// Scroll `lines` toward `direction` from `source`, with the pointer at cell
    /// `column`,`row`.
    Scroll {
        direction: ScrollDirection,
        source: ScrollSource,
        lines: u16,
        column: u16,
        row: u16,
    },
    /// Answers once every earlier job is done.
    #[cfg(test)]
    Flush(SyncSender<()>),
}

impl WriteJob {
    /// The job for raw input: text when it is UTF-8, bytes otherwise.
    pub fn input(data: &[u8]) -> WriteJob {
        match std::str::from_utf8(data) {
            Ok(text) => WriteJob::Text(text.to_string()),
            Err(_) => WriteJob::Bytes(data.to_vec()),
        }
    }

    /// A line job and the receiving end of its result.
    pub fn line(text: &str) -> (WriteJob, LineReply) {
        let (reply, result) = mpsc::sync_channel(1);
        let claim = LineClaim::default();
        let job = WriteJob::Line {
            text: text.to_string(),
            reply,
            claim: claim.clone(),
        };
        (job, LineReply { result, claim })
    }
}

/// Who took an injected line: set once, by the writer or by the caller.
#[derive(Clone, Debug, Default)]
pub struct LineClaim(Arc<AtomicBool>);

impl LineClaim {
    /// Take the line; `false` when the other side already took it.
    fn take(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }
}

/// The caller's end of a [`WriteJob::Line`].
pub struct LineReply {
    result: mpsc::Receiver<Result<(), PtyBackendError>>,
    claim: LineClaim,
}

impl LineReply {
    /// The line's result, waiting at most `timeout` for the writer to take it.
    /// A line still queued then is withdrawn (it will not be sent) and reported
    /// as an error; one the writer already took is waited for once more, as its
    /// send is a CLI call with its own deadline. A writer that went away without
    /// answering means the session was killed or replaced: `NoSuchSession`.
    ///
    /// A line the writer took but never answered for may still land, so it is
    /// not reported as a failure (a caller would send it again): it counts as
    /// sent, with a warning in the log.
    pub fn wait(self, timeout: Duration) -> Result<(), PtyBackendError> {
        match self.result.recv_timeout(timeout) {
            Ok(r) => r,
            Err(RecvTimeoutError::Disconnected) => Err(PtyBackendError::NoSuchSession),
            Err(RecvTimeoutError::Timeout) => {
                if self.claim.take() {
                    return Err(PtyBackendError::Other(format!(
                        "herdr: line not sent within {}s; withdrawn",
                        timeout.as_secs()
                    )));
                }
                match self.result.recv_timeout(timeout) {
                    Ok(r) => r,
                    Err(RecvTimeoutError::Disconnected) => Err(PtyBackendError::NoSuchSession),
                    Err(RecvTimeoutError::Timeout) => {
                        log::warn!(
                            "herdr: a line was taken for sending but not answered within {}ms; \
                             treating it as sent (it may still land) so it is not sent twice",
                            2 * timeout.as_millis()
                        );
                        Ok(())
                    }
                }
            }
        }
    }
}

/// A coalesced run of input jobs of one kind.
#[derive(Debug, PartialEq, Eq)]
enum Run {
    Text(String),
    Bytes(Vec<u8>),
}

impl Run {
    /// Append `job` when it is input of the same kind; otherwise hand it back.
    fn absorb(&mut self, job: WriteJob) -> Result<(), WriteJob> {
        match (self, job) {
            (Run::Text(run), WriteJob::Text(t)) => {
                run.push_str(&t);
                Ok(())
            }
            (Run::Bytes(run), WriteJob::Bytes(b)) => {
                run.extend_from_slice(&b);
                Ok(())
            }
            (_, job) => Err(job),
        }
    }

    fn as_input(&self) -> BridgeInput<'_> {
        match self {
            Run::Text(t) => BridgeInput::Text(t),
            Run::Bytes(b) => BridgeInput::Bytes(b),
        }
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            Run::Text(t) => t.as_bytes(),
            Run::Bytes(b) => b,
        }
    }
}

/// What a writer does to its session, implemented over the backend's parts.
/// Every method acts on the writer's own session generation only. `Sync`
/// because the stall watchdog acts on it from its own thread.
pub trait SessionIo: Send + Sync + 'static {
    /// Whether the server may be used right now.
    fn admit(&self) -> Result<(), PtyBackendError>;
    /// The session's current bridge stdin, if a bridge is up.
    fn stdin(&self) -> Option<BridgeStdin>;
    /// A write to `stdin` failed: drop that bridge (only if it is still the
    /// current one; a restart may already have replaced it).
    fn drop_bridge(&self, stdin: &BridgeStdin);
    /// Send input without a bridge (the herdr CLI).
    fn fallback(&self, data: &[u8]) -> Result<(), PtyBackendError>;
    /// Input just reached the bridge stdin.
    fn stdin_written(&self);
    /// Inject one submitted line.
    fn write_line(&self, text: &str) -> Result<(), PtyBackendError>;
    /// The session's current bridge stdin and its viewport size, if a bridge is up.
    fn bridge(&self) -> Option<(BridgeStdin, (u16, u16))>;
    /// A resize to `cols`×`rows` was written to `stdin`: record the size (only
    /// if `stdin` is still the current bridge's).
    fn resized(&self, stdin: &BridgeStdin, cols: u16, rows: u16);
    /// An in-place resize on `stdin` failed: replace that bridge with one
    /// started at `cols`×`rows` (only if it is still the current one; a bridge
    /// already replaced or detached is left alone).
    fn restart_bridge(
        &self,
        stdin: &BridgeStdin,
        cols: u16,
        rows: u16,
    ) -> Result<(), PtyBackendError>;
}

/// No bridge write in flight.
const IDLE: u64 = 0;
/// The watchdog already acted on the write in flight.
const ACTED: u64 = u64::MAX;

/// Milliseconds since `epoch`, plus one (so never [`IDLE`]).
fn now_ms(epoch: Instant) -> u64 {
    epoch.elapsed().as_millis() as u64 + 1
}

/// A stdin's identity for [`Watch::stdin`] (only compared, never followed).
fn stdin_addr(stdin: &BridgeStdin) -> usize {
    Arc::as_ptr(stdin) as *const () as usize
}

/// One writer's bridge write in flight, as the stall watchdog sees it. The
/// writer only stores to atomics; the shared watchdog thread reads them.
struct Watch<T> {
    io: Arc<T>,
    name: String,
    /// The registry's clock origin.
    epoch: Instant,
    /// When the write in flight started ([`now_ms`]), [`IDLE`], or [`ACTED`].
    since: AtomicU64,
    /// The stdin being written ([`stdin_addr`]).
    stdin: AtomicUsize,
}

impl<T> Watch<T> {
    /// Run `write` on `stdin` under watch.
    fn watched<R>(&self, stdin: &BridgeStdin, write: impl FnOnce() -> R) -> R {
        self.stdin.store(stdin_addr(stdin), Ordering::Relaxed);
        self.since.store(now_ms(self.epoch), Ordering::Release);
        let r = write();
        self.since.store(IDLE, Ordering::Release);
        r
    }
}

/// A watched writer, as the shared watchdog holds it.
trait Stall: Send + Sync {
    /// Act once on a write blocked for at least `limit_ms` at `now`.
    fn check(&self, now: u64, limit_ms: u64);
}

impl<T: SessionIo> Stall for Watch<T> {
    /// A bridge write blocked for the limit gets its bridge restarted at its
    /// current size. Killing the old control child breaks the pipe, so the
    /// blocked write fails and the writer goes on (the failed input then waits
    /// for the new bridge like any other).
    fn check(&self, now: u64, limit_ms: u64) {
        let since = self.since.load(Ordering::Acquire);
        if since == IDLE || since == ACTED || now.saturating_sub(since) < limit_ms {
            return;
        }
        // Read before claiming the stall: a write that ends meanwhile changes
        // `since`, so the claim fails instead of acting on its successor.
        let stuck = self.stdin.load(Ordering::Relaxed);
        if self
            .since
            .compare_exchange(since, ACTED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        log::warn!(
            "herdr writer [{}]: a bridge write is blocked after {limit_ms}ms; restarting the bridge",
            self.name
        );
        // A bridge already replaced or stopped by someone else was killed by
        // that teardown, which freed the write: nothing to do then.
        if let Some((current, (cols, rows))) = self.io.bridge() {
            if stdin_addr(&current) == stuck {
                if let Err(e) = self.io.restart_bridge(&current, cols, rows) {
                    log::warn!("herdr writer [{}]: bridge restart failed: {e}", self.name);
                }
            }
        }
    }
}

/// The one stall watchdog thread of a [`Writers`] registry, started with its
/// first writer. It ends once the registry is gone.
struct Watchdog {
    epoch: Instant,
    /// Every writer started so far; those that ended are pruned on each scan.
    watched: Mutex<Vec<Weak<dyn Stall>>>,
    started: AtomicBool,
    /// How many scan threads were started.
    #[cfg(test)]
    threads: AtomicUsize,
}

impl Watchdog {
    fn new() -> Self {
        Watchdog {
            epoch: Instant::now(),
            watched: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            #[cfg(test)]
            threads: AtomicUsize::new(0),
        }
    }

    /// Watch `w`, starting the scan thread on first use.
    fn watch(self: &Arc<Self>, w: Weak<dyn Stall>, limit: Duration) {
        self.watched.lock().unwrap().push(w);
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("herdr-writer-watch".into())
            .spawn(move || scan_stalls(me, limit));
        match spawned {
            Ok(_) => {
                #[cfg(test)]
                self.threads.fetch_add(1, Ordering::SeqCst);
            }
            Err(e) => {
                log::warn!("herdr writers: no stall watchdog: {e}");
                self.started.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// The watchdog loop: every scan interval, check each live writer for a write
/// blocked past `limit`.
fn scan_stalls(dog: Weak<Watchdog>, limit: Duration) {
    let every = WATCH_SCAN.min(limit / 2).max(Duration::from_millis(1));
    let limit_ms = (limit.as_millis() as u64).max(1);
    loop {
        std::thread::sleep(every);
        let Some(dog) = dog.upgrade() else {
            return;
        };
        let live: Vec<Arc<dyn Stall>> = {
            let mut watched = dog.watched.lock().unwrap();
            watched.retain(|w| w.strong_count() > 0);
            watched.iter().filter_map(Weak::upgrade).collect()
        };
        let now = now_ms(dog.epoch);
        drop(dog);
        for w in live {
            w.check(now, limit_ms);
        }
    }
}

/// The writer loop for one session.
struct Writer<T> {
    io: Arc<T>,
    rx: Receiver<WriteJob>,
    /// Jobs received but not yet run, in order.
    queue: VecDeque<WriteJob>,
    wait: Duration,
    watch: Arc<Watch<T>>,
    name: String,
}

impl<T: SessionIo> Writer<T> {
    fn run(mut self) {
        loop {
            if self.queue.is_empty() {
                match self.rx.recv() {
                    Ok(job) => self.queue.push_back(job),
                    // The sender is gone and the queue is done.
                    Err(_) => return,
                }
            }
            self.drain();
            let Some(job) = self.queue.pop_front() else {
                continue;
            };
            match job {
                WriteJob::Text(t) => {
                    if !self.write_input(Run::Text(t)) {
                        return;
                    }
                }
                WriteJob::Bytes(b) => {
                    if !self.write_input(Run::Bytes(b)) {
                        return;
                    }
                }
                WriteJob::Line { text, reply, claim } => {
                    // The caller gave up on it: it must not land late.
                    if claim.take() {
                        let _ = reply.send(self.io.write_line(&text));
                    }
                }
                WriteJob::Resize { mut cols, mut rows } => {
                    // Only the last of back-to-back resizes (a window drag) matters.
                    while let Some(&WriteJob::Resize { cols: c, rows: r }) = self.queue.front() {
                        (cols, rows) = (c, r);
                        self.queue.pop_front();
                    }
                    self.resize(Some((cols, rows)));
                }
                WriteJob::Repaint => self.resize(None),
                WriteJob::Scroll {
                    direction,
                    source,
                    mut lines,
                    mut column,
                    mut row,
                } => {
                    // A burst of ticks one way from one source is one scroll by
                    // their sum, at the last pointer position. A turn of direction
                    // or a change of source is kept: herdr routes each source its
                    // own way (a wheel report vs a page key) to an app.
                    while let Some(&WriteJob::Scroll {
                        direction: d,
                        source: s,
                        lines: l,
                        column: c,
                        row: r,
                    }) = self.queue.front()
                    {
                        if d != direction || s != source {
                            break;
                        }
                        lines = lines.saturating_add(l);
                        (column, row) = (c, r);
                        self.queue.pop_front();
                    }
                    self.scroll(direction, source, lines, column, row);
                }
                #[cfg(test)]
                WriteJob::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    /// Resize the bridge in place to `size`, or, with `None`, to its current
    /// size (a repaint: herdr answers every resize with a full frame). Not
    /// attached: nothing to do. A failed write restarts the bridge instead.
    fn resize(&self, size: Option<(u16, u16)>) {
        if let Err(e) = self.io.admit() {
            log::debug!("herdr writer [{}]: resize refused: {e}", self.name);
            return;
        }
        let Some((stdin, current)) = self.io.bridge() else {
            return;
        };
        let (cols, rows) = match size {
            Some(size) if size == current => return,
            Some(size) => size,
            None => current,
        };
        let sent = self
            .watch
            .watched(&stdin, || bridge::send_resize_to(&stdin, cols, rows));
        if let Err(e) = sent {
            log::warn!("herdr writer [{}]: {e}; restarting the bridge", self.name);
            if let Err(e) = self.io.restart_bridge(&stdin, cols, rows) {
                log::warn!("herdr writer [{}]: bridge restart failed: {e}", self.name);
            }
            return;
        }
        self.io.resized(&stdin, cols, rows);
    }

    /// Send one scroll down the bridge. Not attached: nothing to do. A failed
    /// write drops the scroll (a lost wheel tick is harmless, so no restart) and
    /// drops the bridge as failed input does: its stdin is dead.
    fn scroll(
        &self,
        direction: ScrollDirection,
        source: ScrollSource,
        lines: u16,
        column: u16,
        row: u16,
    ) {
        if let Err(e) = self.io.admit() {
            log::debug!("herdr writer [{}]: scroll refused: {e}", self.name);
            return;
        }
        let Some(stdin) = self.io.stdin() else {
            return;
        };
        let sent = self.watch.watched(&stdin, || {
            bridge::send_scroll_to(&stdin, direction, source, lines, column, row)
        });
        if let Err(e) = sent {
            log::debug!("herdr writer [{}]: {e}; scroll dropped", self.name);
            self.io.drop_bridge(&stdin);
        }
    }

    /// Move everything already sent into the queue.
    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(job) => self.queue.push_back(job),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Append the queued input that directly follows `run` and is of its kind.
    fn absorb_into(&mut self, run: &mut Run) {
        while let Some(job) = self.queue.pop_front() {
            if let Err(job) = run.absorb(job) {
                self.queue.push_front(job);
                return;
            }
        }
    }

    /// Send one run of input: down the bridge, or, when none is up within the
    /// wait, through the fallback as one call. `false` when the sender went away
    /// while waiting (the session is gone; the run is dropped).
    fn write_input(&mut self, mut run: Run) -> bool {
        self.absorb_into(&mut run);
        if let Err(e) = self.io.admit() {
            log::debug!("herdr writer [{}]: input refused: {e}", self.name);
            return true;
        }
        let mut deadline: Option<Instant> = None;
        loop {
            if let Some(stdin) = self.io.stdin() {
                let sent = self
                    .watch
                    .watched(&stdin, || bridge::send_input_to(&stdin, run.as_input()));
                match sent {
                    Ok(()) => {
                        self.io.stdin_written();
                        return true;
                    }
                    // A dead child (broken pipe), a bridge stopped mid-restart, or
                    // one the stall watchdog killed: drop it so a future attach
                    // starts cleanly, then wait for a replacement before falling back.
                    Err(e) => {
                        log::warn!("herdr writer [{}]: {e}", self.name);
                        self.io.drop_bridge(&stdin);
                    }
                }
            }
            let now = Instant::now();
            let deadline = *deadline.get_or_insert(now + self.wait);
            if now >= deadline {
                if let Err(e) = self.io.fallback(run.as_bytes()) {
                    log::warn!("herdr writer [{}]: fallback input failed: {e}", self.name);
                }
                return true;
            }
            // Wait for a bridge, collecting the input that arrives meanwhile.
            match self.rx.recv_timeout(BRIDGE_POLL.min(deadline - now)) {
                Ok(job) => self.queue.push_back(job),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return false,
            }
            self.drain();
            self.absorb_into(&mut run);
        }
    }
}

/// A running writer: the session generation it serves, its queue's sending
/// end and its thread.
struct SessionWriter {
    gen: u64,
    tx: Sender<WriteJob>,
    thread: JoinHandle<()>,
}

/// The writer threads, one per session key, created on first use.
pub struct Writers {
    map: Mutex<HashMap<SessionKey, SessionWriter>>,
    /// Writers replaced by a newer generation, still finishing their queue.
    #[cfg(test)]
    retired: Mutex<Vec<JoinHandle<()>>>,
    /// How long input waits for a bridge (shorter in tests).
    wait: Duration,
    /// How long one bridge write may block (shorter in tests).
    stall: Duration,
    /// How long `write_line` waits for its line (shorter in tests).
    line_timeout: Duration,
    /// The stall watchdog shared by every writer of this registry.
    watchdog: Arc<Watchdog>,
}

impl Default for Writers {
    fn default() -> Self {
        Writers::with_wait(BRIDGE_WAIT)
    }
}

impl Writers {
    pub fn with_wait(wait: Duration) -> Self {
        Writers {
            map: Mutex::new(HashMap::new()),
            #[cfg(test)]
            retired: Mutex::new(Vec::new()),
            wait,
            stall: STALL_LIMIT,
            line_timeout: LINE_REPLY_TIMEOUT,
            watchdog: Arc::new(Watchdog::new()),
        }
    }

    /// Set how long one bridge write may block before its bridge is restarted.
    #[cfg(test)]
    pub fn with_stall_limit(mut self, stall: Duration) -> Self {
        self.stall = stall;
        self
    }

    /// Set how long `write_line` waits for its line.
    #[cfg(test)]
    pub fn with_line_timeout(mut self, timeout: Duration) -> Self {
        self.line_timeout = timeout;
        self
    }

    /// How long `write_line` waits for its line.
    pub fn line_timeout(&self) -> Duration {
        self.line_timeout
    }

    /// Enqueue `job` for generation `gen` of `key`, starting a writer over
    /// `make_io()` if it has none for that generation. A writer of an older
    /// generation is replaced: its queue closes and it finishes what it already
    /// took against its own generation only. A job for a generation older than
    /// the writer's is refused with `NoSuchSession` (a line's reply reads
    /// `NoSuchSession` too): a stale caller never displaces a newer session's
    /// writer. Never blocks on the session's I/O.
    pub fn send<T: SessionIo>(
        &self,
        key: &SessionKey,
        gen: u64,
        job: WriteJob,
        make_io: impl FnOnce() -> T,
    ) -> Result<(), PtyBackendError> {
        let mut map = self.map.lock().unwrap();
        let job = match map.get(key) {
            Some(w) if w.gen == gen => match w.tx.send(job) {
                Ok(()) => return Ok(()),
                // The thread is gone (it panicked): start a new one below.
                Err(mpsc::SendError(job)) => job,
            },
            Some(w) if gen < w.gen => return Err(PtyBackendError::NoSuchSession),
            Some(_) => {
                if let Some(old) = map.remove(key) {
                    self.retire(old);
                }
                job
            }
            None => job,
        };
        let (tx, rx) = mpsc::channel();
        let name = format!("{}/{} #{gen}", key.project, key.worktree);
        let io = Arc::new(make_io());
        let watch = Arc::new(Watch {
            io: Arc::clone(&io),
            name: name.clone(),
            epoch: self.watchdog.epoch,
            since: AtomicU64::new(IDLE),
            stdin: AtomicUsize::new(0),
        });
        let watched: Arc<dyn Stall> = watch.clone();
        let writer = Writer {
            io,
            rx,
            queue: VecDeque::new(),
            wait: self.wait,
            watch,
            name: name.clone(),
        };
        let thread = std::thread::Builder::new()
            .name(format!("herdr-writer {name}"))
            .spawn(move || writer.run())
            .map_err(|e| PtyBackendError::Other(format!("herdr writer thread: {e}")))?;
        self.watchdog.watch(Arc::downgrade(&watched), self.stall);
        // The receiver lives in the new thread, so this cannot fail.
        let _ = tx.send(job);
        map.insert(key.clone(), SessionWriter { gen, tx, thread });
        Ok(())
    }

    /// Close a replaced writer's queue; its thread ends on its own.
    fn retire(&self, old: SessionWriter) {
        drop(old.tx);
        #[cfg(test)]
        self.retired.lock().unwrap().push(old.thread);
    }

    /// Stop `key`'s writer: its queue closes, so the thread ends once the jobs
    /// already queued are done (input still waiting for a bridge is dropped).
    /// Returns the thread for a caller that wants to wait for it.
    pub fn remove(&self, key: &SessionKey) -> Option<JoinHandle<()>> {
        let w = self.map.lock().unwrap().remove(key)?;
        drop(w.tx);
        Some(w.thread)
    }

    /// Whether `key` has a writer.
    #[cfg(test)]
    pub fn contains(&self, key: &SessionKey) -> bool {
        self.map.lock().unwrap().contains_key(key)
    }

    /// Wait for every writer a newer generation replaced to end.
    #[cfg(test)]
    pub fn join_retired(&self) {
        let retired = std::mem::take(&mut *self.retired.lock().unwrap());
        for t in retired {
            t.join().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex as StdMutex};

    /// What the scripted session saw, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Seen {
        Stdin(String),
        Fallback(String),
        Line(String),
        DroppedBridge,
        Resized(u16, u16),
        Restarted(u16, u16),
    }

    /// A bridge stdin recording each line it receives.
    struct LineStdin(Arc<StdMutex<Vec<Seen>>>);
    impl Write for LineStdin {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let line = String::from_utf8(buf.to_vec()).unwrap();
            self.0.lock().unwrap().push(Seen::Stdin(line));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A scripted session: a swappable bridge stdin and a log.
    #[derive(Clone, Default)]
    struct Script {
        seen: Arc<StdMutex<Vec<Seen>>>,
        stdin: Arc<StdMutex<Option<BridgeStdin>>>,
        size: Arc<StdMutex<(u16, u16)>>,
        refuse: Arc<StdMutex<bool>>,
    }

    impl Script {
        fn attach(&self) -> BridgeStdin {
            let s: BridgeStdin = Arc::new(StdMutex::new(Some(Box::new(LineStdin(Arc::clone(
                &self.seen,
            ))))));
            *self.stdin.lock().unwrap() = Some(Arc::clone(&s));
            s
        }
        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl SessionIo for Script {
        fn admit(&self) -> Result<(), PtyBackendError> {
            if *self.refuse.lock().unwrap() {
                Err(PtyBackendError::RecoveryPending)
            } else {
                Ok(())
            }
        }
        fn stdin(&self) -> Option<BridgeStdin> {
            self.stdin.lock().unwrap().clone()
        }
        fn drop_bridge(&self, stdin: &BridgeStdin) {
            let mut cur = self.stdin.lock().unwrap();
            if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, stdin)) {
                *cur = None;
            }
            self.seen.lock().unwrap().push(Seen::DroppedBridge);
        }
        fn fallback(&self, data: &[u8]) -> Result<(), PtyBackendError> {
            let text = String::from_utf8_lossy(data).into_owned();
            self.seen.lock().unwrap().push(Seen::Fallback(text));
            Ok(())
        }
        fn stdin_written(&self) {}
        fn write_line(&self, text: &str) -> Result<(), PtyBackendError> {
            self.seen.lock().unwrap().push(Seen::Line(text.to_string()));
            if text == "fail" {
                return Err(PtyBackendError::NoSuchSession);
            }
            Ok(())
        }
        fn bridge(&self) -> Option<(BridgeStdin, (u16, u16))> {
            let stdin = self.stdin()?;
            Some((stdin, *self.size.lock().unwrap()))
        }
        fn resized(&self, _stdin: &BridgeStdin, cols: u16, rows: u16) {
            *self.size.lock().unwrap() = (cols, rows);
            self.seen.lock().unwrap().push(Seen::Resized(cols, rows));
        }
        fn restart_bridge(
            &self,
            _stdin: &BridgeStdin,
            cols: u16,
            rows: u16,
        ) -> Result<(), PtyBackendError> {
            self.seen.lock().unwrap().push(Seen::Restarted(cols, rows));
            *self.size.lock().unwrap() = (cols, rows);
            self.attach();
            Ok(())
        }
    }

    fn key() -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: "w".into(),
        }
    }

    fn flush(writers: &Writers, script: &Script) {
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 1, WriteJob::Flush(tx), || script.clone())
            .unwrap();
        rx.recv().unwrap();
    }

    fn parse(line: &str) -> serde_json::Value {
        serde_json::from_str(line.trim_end()).unwrap()
    }

    /// Everything typed down the bridge, decoded and concatenated.
    fn stdin_text(seen: &[Seen]) -> String {
        seen.iter()
            .filter_map(|s| match s {
                Seen::Stdin(l) => parse(l)["text"].as_str().map(str::to_string),
                _ => None,
            })
            .collect()
    }

    /// The bridge stdin lines, parsed (a write of several lines gives each).
    fn stdin_lines(seen: &[Seen]) -> Vec<serde_json::Value> {
        seen.iter()
            .filter_map(|s| match s {
                Seen::Stdin(l) => Some(l.lines().map(parse).collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// How many bridge stdin writes were made.
    fn stdin_writes(seen: &[Seen]) -> usize {
        seen.iter().filter(|s| matches!(s, Seen::Stdin(_))).count()
    }

    fn resize_json(cols: u16, rows: u16) -> serde_json::Value {
        serde_json::json!({"type": "terminal.resize", "cols": cols, "rows": rows})
    }

    #[test]
    fn input_is_text_when_utf8_and_bytes_otherwise() {
        assert!(matches!(WriteJob::input(b"ls\r"), WriteJob::Text(t) if t == "ls\r"));
        assert!(matches!(WriteJob::input(&[0xff, 0x41]), WriteJob::Bytes(b) if b == [0xff, 0x41]));
    }

    #[test]
    fn a_run_absorbs_only_its_own_kind() {
        let mut run = Run::Text("a".into());
        assert!(run.absorb(WriteJob::Text("b".into())).is_ok());
        assert!(matches!(
            run.absorb(WriteJob::Bytes(vec![1])),
            Err(WriteJob::Bytes(_))
        ));
        assert_eq!(run, Run::Text("ab".into()));
        let mut run = Run::Bytes(vec![1]);
        assert!(run.absorb(WriteJob::Bytes(vec![2])).is_ok());
        assert!(run.absorb(WriteJob::Text("x".into())).is_err());
        assert_eq!(run, Run::Bytes(vec![1, 2]));
    }

    #[test]
    fn jobs_run_in_fifo_order() {
        let script = Script::default();
        script.attach();
        let writers = Writers::default();
        let mut expected = String::new();
        for i in 0..500 {
            let s = format!("{i},");
            expected.push_str(&s);
            writers
                .send(&key(), 1, WriteJob::Text(s), || script.clone())
                .unwrap();
        }
        flush(&writers, &script);
        assert_eq!(stdin_text(&script.seen()), expected);
    }

    /// A session whose `write_line` waits for a go-ahead, so jobs sent after a
    /// held line queue up behind it.
    struct Hold(Script, StdMutex<mpsc::Receiver<()>>);
    impl SessionIo for Hold {
        fn admit(&self) -> Result<(), PtyBackendError> {
            self.0.admit()
        }
        fn stdin(&self) -> Option<BridgeStdin> {
            self.0.stdin()
        }
        fn drop_bridge(&self, s: &BridgeStdin) {
            self.0.drop_bridge(s)
        }
        fn fallback(&self, d: &[u8]) -> Result<(), PtyBackendError> {
            self.0.fallback(d)
        }
        fn stdin_written(&self) {}
        fn write_line(&self, text: &str) -> Result<(), PtyBackendError> {
            self.1.lock().unwrap().recv().unwrap();
            self.0.write_line(text)
        }
        fn bridge(&self) -> Option<(BridgeStdin, (u16, u16))> {
            self.0.bridge()
        }
        fn resized(&self, s: &BridgeStdin, cols: u16, rows: u16) {
            self.0.resized(s, cols, rows)
        }
        fn restart_bridge(
            &self,
            s: &BridgeStdin,
            cols: u16,
            rows: u16,
        ) -> Result<(), PtyBackendError> {
            self.0.restart_bridge(s, cols, rows)
        }
    }

    /// Starts `key()`'s writer held on a line; everything sent next queues up
    /// until the returned sender releases it.
    fn hold_writer(writers: &Writers, script: &Script) -> mpsc::SyncSender<()> {
        let (gate_tx, gate_rx) = mpsc::sync_channel::<()>(0);
        let hold = Hold(script.clone(), StdMutex::new(gate_rx));
        let (job, _reply) = WriteJob::line("held");
        writers.send(&key(), 1, job, move || hold).unwrap();
        gate_tx
    }

    fn unused() -> Hold {
        unreachable!()
    }

    #[test]
    fn queued_input_coalesces_and_flushes_at_kind_boundaries() {
        let script = Script::default();
        script.attach();
        let writers = Writers::default();
        // A line the writer is held on, so everything after it queues up behind.
        let gate_tx = hold_writer(&writers, &script);
        for c in ["a", "b", "c"] {
            writers
                .send(&key(), 1, WriteJob::Text(c.into()), unused)
                .unwrap();
        }
        writers
            .send(&key(), 1, WriteJob::Bytes(vec![0xff]), unused)
            .unwrap();
        writers
            .send(&key(), 1, WriteJob::Text("d".into()), unused)
            .unwrap();
        writers
            .send(&key(), 1, WriteJob::Text("e".into()), unused)
            .unwrap();
        gate_tx.send(()).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 1, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();

        let seen = script.seen();
        let lines: Vec<serde_json::Value> = seen
            .iter()
            .filter_map(|s| match s {
                Seen::Stdin(l) => Some(serde_json::from_str(l.trim_end()).unwrap()),
                _ => None,
            })
            .collect();
        assert_eq!(seen[0], Seen::Line("held".into()));
        assert_eq!(lines.len(), 3, "{seen:?}");
        assert_eq!(lines[0]["text"], "abc");
        assert!(lines[1]["bytes"].is_string());
        assert_eq!(lines[2]["text"], "de");
    }

    #[test]
    fn without_a_bridge_the_run_waits_then_falls_back_as_one_call() {
        let script = Script::default();
        let writers = Writers::with_wait(Duration::from_millis(30));
        for c in ["x", "y", "z"] {
            writers
                .send(&key(), 1, WriteJob::Text(c.into()), || script.clone())
                .unwrap();
        }
        flush(&writers, &script);
        // However the jobs were split across waits, nothing is lost or reordered,
        // and each wait ends in one fallback call.
        let seen = script.seen();
        let all: String = seen
            .iter()
            .map(|s| match s {
                Seen::Fallback(t) => t.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(all, "xyz");
    }

    #[test]
    fn input_waiting_for_a_bridge_goes_down_the_new_one() {
        let script = Script::default();
        let writers = Writers::with_wait(Duration::from_secs(30));
        writers
            .send(&key(), 1, WriteJob::Text("a".into()), || script.clone())
            .unwrap();
        writers
            .send(&key(), 1, WriteJob::Text("b".into()), || script.clone())
            .unwrap();
        // The writer is now waiting (no bridge); a bridge comes up.
        script.attach();
        flush(&writers, &script);
        let seen = script.seen();
        assert_eq!(stdin_text(&seen), "ab");
        assert!(seen.iter().all(|s| matches!(s, Seen::Stdin(_))), "{seen:?}");
    }

    #[test]
    fn a_failed_bridge_write_drops_the_bridge_and_falls_back() {
        let script = Script::default();
        let dead = script.attach();
        *dead.lock().unwrap() = None; // the child's stdin is closed
        let writers = Writers::with_wait(Duration::from_millis(10));
        writers
            .send(&key(), 1, WriteJob::Text("q".into()), || script.clone())
            .unwrap();
        flush(&writers, &script);
        assert_eq!(
            script.seen(),
            vec![Seen::DroppedBridge, Seen::Fallback("q".into())]
        );
        assert!(script.stdin().is_none());
        // The writer is still alive.
        script.attach();
        writers
            .send(&key(), 1, WriteJob::Text("r".into()), || script.clone())
            .unwrap();
        flush(&writers, &script);
        assert_eq!(stdin_text(&script.seen()), "r");
    }

    #[test]
    fn refused_input_is_dropped_and_lines_report_their_result() {
        let script = Script::default();
        script.attach();
        *script.refuse.lock().unwrap() = true;
        let writers = Writers::default();
        writers
            .send(&key(), 1, WriteJob::Text("nope".into()), || script.clone())
            .unwrap();
        let (job, reply) = WriteJob::line("fail");
        writers.send(&key(), 1, job, || script.clone()).unwrap();
        assert!(matches!(
            reply.wait(Duration::from_secs(30)),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert_eq!(script.seen(), vec![Seen::Line("fail".into())]);
    }

    #[test]
    fn removing_a_writer_ends_its_thread() {
        let script = Script::default();
        script.attach();
        let writers = Writers::default();
        writers
            .send(&key(), 1, WriteJob::Text("a".into()), || script.clone())
            .unwrap();
        assert!(writers.contains(&key()));
        let thread = writers.remove(&key()).unwrap();
        thread.join().unwrap();
        assert!(!writers.contains(&key()));
        assert_eq!(stdin_text(&script.seen()), "a", "queued input still ran");
    }

    #[test]
    fn removing_a_writer_waiting_for_a_bridge_ends_it_promptly() {
        let script = Script::default();
        let writers = Writers::with_wait(Duration::from_secs(60));
        writers
            .send(&key(), 1, WriteJob::Text("a".into()), || script.clone())
            .unwrap();
        writers.remove(&key()).unwrap().join().unwrap();
        assert!(script.seen().is_empty(), "{:?}", script.seen());
    }

    /// A session with a bridge up at 80x24.
    fn sized_script() -> Script {
        let script = Script::default();
        *script.size.lock().unwrap() = (80, 24);
        script.attach();
        script
    }

    #[test]
    fn resize_writes_a_resize_line_and_records_the_size() {
        let script = sized_script();
        let writers = Writers::default();
        writers
            .send(
                &key(),
                1,
                WriteJob::Resize {
                    cols: 120,
                    rows: 40,
                },
                || script.clone(),
            )
            .unwrap();
        flush(&writers, &script);
        let seen = script.seen();
        assert_eq!(stdin_lines(&seen), vec![resize_json(120, 40)]);
        assert_eq!(seen.last(), Some(&Seen::Resized(120, 40)));
        assert_eq!(*script.size.lock().unwrap(), (120, 40));
    }

    #[test]
    fn an_unchanged_size_writes_nothing() {
        let script = sized_script();
        let writers = Writers::default();
        writers
            .send(&key(), 1, WriteJob::Resize { cols: 80, rows: 24 }, || {
                script.clone()
            })
            .unwrap();
        flush(&writers, &script);
        assert!(script.seen().is_empty(), "{:?}", script.seen());
    }

    #[test]
    fn repaint_is_a_same_size_resize() {
        let script = sized_script();
        let writers = Writers::default();
        writers
            .send(&key(), 1, WriteJob::Repaint, || script.clone())
            .unwrap();
        flush(&writers, &script);
        assert_eq!(stdin_lines(&script.seen()), vec![resize_json(80, 24)]);
    }

    #[test]
    fn without_a_bridge_resize_and_repaint_do_nothing() {
        let script = Script::default();
        let writers = Writers::default();
        writers
            .send(
                &key(),
                1,
                WriteJob::Resize {
                    cols: 100,
                    rows: 30,
                },
                || script.clone(),
            )
            .unwrap();
        writers
            .send(&key(), 1, WriteJob::Repaint, || script.clone())
            .unwrap();
        flush(&writers, &script);
        assert!(script.seen().is_empty(), "{:?}", script.seen());
    }

    #[test]
    fn a_failed_resize_write_restarts_the_bridge_at_the_new_size() {
        let script = sized_script();
        let dead = script.stdin().unwrap();
        *dead.lock().unwrap() = None; // the child's stdin is closed
        let writers = Writers::default();
        writers
            .send(
                &key(),
                1,
                WriteJob::Resize {
                    cols: 100,
                    rows: 30,
                },
                || script.clone(),
            )
            .unwrap();
        flush(&writers, &script);
        assert_eq!(script.seen(), vec![Seen::Restarted(100, 30)]);
        // The writer is still alive and uses the new bridge.
        writers
            .send(&key(), 1, WriteJob::Repaint, || script.clone())
            .unwrap();
        flush(&writers, &script);
        assert_eq!(stdin_lines(&script.seen()), vec![resize_json(100, 30)]);
    }

    #[test]
    fn refused_resizes_are_dropped() {
        let script = sized_script();
        *script.refuse.lock().unwrap() = true;
        let writers = Writers::default();
        writers
            .send(
                &key(),
                1,
                WriteJob::Resize {
                    cols: 100,
                    rows: 30,
                },
                || script.clone(),
            )
            .unwrap();
        writers
            .send(&key(), 1, WriteJob::Repaint, || script.clone())
            .unwrap();
        flush(&writers, &script);
        assert!(script.seen().is_empty(), "{:?}", script.seen());
    }

    #[test]
    fn a_resize_is_ordered_between_the_input_around_it() {
        let script = sized_script();
        let writers = Writers::default();
        let gate_tx = hold_writer(&writers, &script);
        for job in [
            WriteJob::Text("a".into()),
            WriteJob::Text("b".into()),
            WriteJob::Resize { cols: 90, rows: 30 },
            WriteJob::Text("c".into()),
            WriteJob::Repaint,
        ] {
            writers.send(&key(), 1, job, unused).unwrap();
        }
        gate_tx.send(()).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 1, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();
        let lines = stdin_lines(&script.seen());
        assert_eq!(
            lines,
            vec![
                serde_json::json!({"type": "terminal.input", "text": "ab"}),
                resize_json(90, 30),
                serde_json::json!({"type": "terminal.input", "text": "c"}),
                resize_json(90, 30),
            ]
        );
    }

    #[test]
    fn a_line_not_taken_in_time_is_withdrawn_and_never_sent() {
        let script = Script::default();
        script.attach();
        let writers = Writers::default();
        let gate_tx = hold_writer(&writers, &script);
        let (job, reply) = WriteJob::line("late");
        writers.send(&key(), 1, job, unused).unwrap();
        // The writer is held, so the line is still queued when the wait ends.
        assert!(matches!(
            reply.wait(Duration::from_millis(1)),
            Err(PtyBackendError::Other(_))
        ));
        gate_tx.send(()).unwrap();
        flush(&writers, &script);
        assert_eq!(script.seen(), vec![Seen::Line("held".into())]);
    }

    #[test]
    fn a_removed_writer_still_answers_the_lines_it_had_queued() {
        let script = Script::default();
        script.attach();
        let writers = Writers::default();
        let gate_tx = hold_writer(&writers, &script);
        let (job, reply) = WriteJob::line("queued");
        writers.send(&key(), 1, job, unused).unwrap();
        let thread = writers.remove(&key()).unwrap();
        // One go-ahead for the held line, one for the queued one.
        gate_tx.send(()).unwrap();
        gate_tx.send(()).unwrap();
        thread.join().unwrap();
        // Removing a writer lets it finish its queue: the line still ran.
        assert!(reply.wait(Duration::from_secs(30)).is_ok());
    }

    #[test]
    fn a_newer_generation_replaces_the_writer() {
        let script = Script::default();
        script.attach();
        let writers = Writers::default();
        writers
            .send(&key(), 1, WriteJob::Text("old".into()), || script.clone())
            .unwrap();
        let fresh = Script::default();
        fresh.attach();
        writers
            .send(&key(), 2, WriteJob::Text("new".into()), || fresh.clone())
            .unwrap();
        writers.join_retired();
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 2, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();
        assert_eq!(stdin_text(&script.seen()), "old");
        assert_eq!(stdin_text(&fresh.seen()), "new");
    }

    /// A bridge stdin whose writes block until its bridge is killed, then fail
    /// with a broken pipe (a hung control child that is still alive).
    struct StuckStdin(Arc<(StdMutex<bool>, std::sync::Condvar)>);
    impl Write for StuckStdin {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            let (killed, cv) = &*self.0;
            let mut k = killed.lock().unwrap();
            while !*k {
                k = cv.wait(k).unwrap();
            }
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A session whose first bridge is stuck; restarting it kills the stuck
    /// child (breaking its pipe) and brings up a working bridge.
    struct Stuck(Script, Arc<(StdMutex<bool>, std::sync::Condvar)>);
    impl SessionIo for Stuck {
        fn admit(&self) -> Result<(), PtyBackendError> {
            self.0.admit()
        }
        fn stdin(&self) -> Option<BridgeStdin> {
            self.0.stdin()
        }
        fn drop_bridge(&self, s: &BridgeStdin) {
            self.0.drop_bridge(s)
        }
        fn fallback(&self, d: &[u8]) -> Result<(), PtyBackendError> {
            self.0.fallback(d)
        }
        fn stdin_written(&self) {}
        fn write_line(&self, text: &str) -> Result<(), PtyBackendError> {
            self.0.write_line(text)
        }
        fn bridge(&self) -> Option<(BridgeStdin, (u16, u16))> {
            self.0.bridge()
        }
        fn resized(&self, s: &BridgeStdin, cols: u16, rows: u16) {
            self.0.resized(s, cols, rows)
        }
        fn restart_bridge(
            &self,
            s: &BridgeStdin,
            cols: u16,
            rows: u16,
        ) -> Result<(), PtyBackendError> {
            let (killed, cv) = &*self.1;
            *killed.lock().unwrap() = true;
            cv.notify_all();
            self.0.restart_bridge(s, cols, rows)
        }
    }

    #[test]
    fn a_write_blocked_past_the_stall_limit_restarts_the_bridge() {
        let script = sized_script();
        let kill = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
        let stuck: BridgeStdin =
            Arc::new(StdMutex::new(Some(Box::new(StuckStdin(Arc::clone(&kill))))));
        *script.stdin.lock().unwrap() = Some(stuck);
        // Only the watchdog can end the blocked write, so this cannot pass by timing.
        let writers =
            Writers::with_wait(Duration::from_secs(30)).with_stall_limit(Duration::from_millis(20));
        let stuck_io = Stuck(script.clone(), Arc::clone(&kill));
        writers
            .send(&key(), 1, WriteJob::Text("typed".into()), move || stuck_io)
            .unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 1, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();
        let seen = script.seen();
        assert!(seen.contains(&Seen::Restarted(80, 24)), "{seen:?}");
        // The input that was stuck goes down the new bridge.
        assert_eq!(stdin_text(&seen), "typed");
        assert!(
            !seen.iter().any(|s| matches!(s, Seen::Fallback(_))),
            "{seen:?}"
        );
    }

    #[test]
    fn writers_share_one_stall_watchdog_thread() {
        let writers = Writers::default();
        for w in ["a", "b", "c"] {
            let script = Script::default();
            let k = SessionKey {
                project: "p".into(),
                worktree: w.into(),
            };
            writers
                .send(&k, 1, WriteJob::Text("x".into()), || script.clone())
                .unwrap();
        }
        assert_eq!(writers.watchdog.threads.load(Ordering::SeqCst), 1);
        assert_eq!(writers.watchdog.watched.lock().unwrap().len(), 3);
    }

    /// A watch over `script` on the clock origin `epoch`.
    fn watch_over(script: &Script, epoch: Instant) -> Watch<Script> {
        Watch {
            io: Arc::new(script.clone()),
            name: "t".into(),
            epoch,
            since: AtomicU64::new(IDLE),
            stdin: AtomicUsize::new(0),
        }
    }

    #[test]
    fn the_watchdog_restarts_a_stalled_bridge_once_and_only_past_the_limit() {
        let script = sized_script();
        let stdin = script.stdin().unwrap();
        let watch = watch_over(&script, Instant::now());
        // A write in flight since t=100 (the clock is injected through `check`).
        watch.stdin.store(stdin_addr(&stdin), Ordering::Relaxed);
        watch.since.store(100, Ordering::Release);
        watch.check(100 + 4_999, 5_000);
        assert!(script.seen().is_empty(), "not stalled yet");
        watch.check(100 + 5_000, 5_000);
        assert_eq!(script.seen(), vec![Seen::Restarted(80, 24)]);
        // Still blocked at the next scan: acted on once only.
        watch.check(100 + 9_000, 5_000);
        assert_eq!(script.seen(), vec![Seen::Restarted(80, 24)]);
    }

    #[test]
    fn the_watchdog_leaves_idle_writers_and_replaced_bridges_alone() {
        let script = sized_script();
        let old = script.stdin().unwrap();
        let watch = watch_over(&script, Instant::now());
        watch.check(1_000_000, 5_000);
        // A write stalled on a bridge that was replaced meanwhile.
        script.attach();
        watch.stdin.store(stdin_addr(&old), Ordering::Relaxed);
        watch.since.store(1, Ordering::Release);
        watch.check(1_000_000, 5_000);
        assert!(script.seen().is_empty(), "{:?}", script.seen());
    }

    #[test]
    fn a_job_for_an_older_generation_is_refused_and_keeps_the_newer_writer() {
        let fresh = Script::default();
        fresh.attach();
        let writers = Writers::default();
        writers
            .send(&key(), 2, WriteJob::Text("new".into()), || fresh.clone())
            .unwrap();
        // A caller that read the previous generation before the respawn.
        assert!(matches!(
            writers.send(&key(), 1, WriteJob::Text("stale".into()), unused),
            Err(PtyBackendError::NoSuchSession)
        ));
        let (job, reply) = WriteJob::line("stale line");
        assert!(matches!(
            writers.send(&key(), 1, job, unused),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert!(matches!(
            reply.wait(Duration::from_secs(30)),
            Err(PtyBackendError::NoSuchSession)
        ));
        // The newer writer is still the one in place.
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 2, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();
        writers.join_retired();
        assert_eq!(stdin_text(&fresh.seen()), "new");
        assert!(!fresh.seen().contains(&Seen::Line("stale line".into())));
    }

    #[test]
    fn a_taken_line_not_answered_in_time_counts_as_sent() {
        let (job, reply) = WriteJob::line("maybe");
        let WriteJob::Line {
            reply: answer,
            claim,
            ..
        } = job
        else {
            unreachable!()
        };
        // The writer took the line and is still sending it.
        assert!(claim.take());
        // Not a failure: the line may still land, and a retry would send it twice.
        assert!(reply.wait(Duration::from_millis(1)).is_ok());
        drop(answer);
    }

    /// A wheel scroll job.
    fn scroll(direction: ScrollDirection, lines: u16, column: u16, row: u16) -> WriteJob {
        WriteJob::Scroll {
            direction,
            source: ScrollSource::Wheel,
            lines,
            column,
            row,
        }
    }

    /// A page-key scroll job.
    fn page(direction: ScrollDirection, lines: u16) -> WriteJob {
        WriteJob::Scroll {
            direction,
            source: ScrollSource::PageKey,
            lines,
            column: 0,
            row: 0,
        }
    }

    fn scroll_json_from(
        direction: &str,
        source: &str,
        lines: u16,
        column: u16,
        row: u16,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": "terminal.scroll",
            "direction": direction,
            "source": source,
            "lines": lines,
            "column": column,
            "row": row,
        })
    }

    /// The `lines` one-line wheel commands a wheel scroll of `lines` is written as.
    fn wheel_json(direction: &str, lines: u16, column: u16, row: u16) -> Vec<serde_json::Value> {
        vec![scroll_json_from(direction, "wheel", 1, column, row); usize::from(lines)]
    }

    /// Runs `jobs` queued up behind a held writer, so they coalesce where they
    /// can, and returns once all have run.
    fn run_queued(script: &Script, jobs: Vec<WriteJob>) {
        let writers = Writers::default();
        let gate_tx = hold_writer(&writers, script);
        for job in jobs {
            writers.send(&key(), 1, job, unused).unwrap();
        }
        gate_tx.send(()).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 1, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();
    }

    #[test]
    fn a_scroll_writes_a_scroll_line() {
        let script = sized_script();
        let writers = Writers::default();
        writers
            .send(&key(), 1, scroll(ScrollDirection::Up, 3, 10, 5), || {
                script.clone()
            })
            .unwrap();
        flush(&writers, &script);
        let seen = script.seen();
        assert_eq!(stdin_lines(&seen), wheel_json("up", 3, 10, 5));
        assert_eq!(stdin_writes(&seen), 1, "one write for the whole scroll");
    }

    #[test]
    fn queued_scrolls_one_way_merge_into_one_at_the_last_cell() {
        let script = sized_script();
        run_queued(
            &script,
            vec![
                scroll(ScrollDirection::Down, 1, 1, 1),
                scroll(ScrollDirection::Down, 2, 2, 2),
                scroll(ScrollDirection::Down, 4, 3, 9),
            ],
        );
        let seen = script.seen();
        assert_eq!(stdin_lines(&seen), wheel_json("down", 7, 3, 9));
        assert_eq!(stdin_writes(&seen), 1, "merged into one write");
    }

    #[test]
    fn queued_scrolls_keep_each_turn_of_direction() {
        let script = sized_script();
        run_queued(
            &script,
            vec![
                scroll(ScrollDirection::Up, 1, 0, 0),
                scroll(ScrollDirection::Up, 2, 0, 0),
                scroll(ScrollDirection::Down, 3, 0, 0),
                scroll(ScrollDirection::Up, 4, 0, 0),
            ],
        );
        let seen = script.seen();
        assert_eq!(
            stdin_lines(&seen),
            [
                wheel_json("up", 3, 0, 0),
                wheel_json("down", 3, 0, 0),
                wheel_json("up", 4, 0, 0),
            ]
            .concat()
        );
        assert_eq!(stdin_writes(&seen), 3);
    }

    #[test]
    fn merged_scroll_lines_saturate_at_u16_max() {
        let script = sized_script();
        run_queued(
            &script,
            vec![
                scroll(ScrollDirection::Up, 40_000, 0, 0),
                scroll(ScrollDirection::Up, 40_000, 0, 0),
                scroll(ScrollDirection::Up, 40_000, 0, 0),
            ],
        );
        assert_eq!(
            stdin_lines(&script.seen()),
            vec![scroll_json_from("up", "wheel", u16::MAX, 0, 0)],
            "past the split max: one command"
        );
    }

    #[test]
    fn queued_scrolls_do_not_merge_across_sources() {
        let script = sized_script();
        run_queued(
            &script,
            vec![
                scroll(ScrollDirection::Up, 2, 0, 0),
                page(ScrollDirection::Up, 30),
                page(ScrollDirection::Up, 30),
                scroll(ScrollDirection::Up, 1, 0, 0),
            ],
        );
        let seen = script.seen();
        assert_eq!(
            stdin_lines(&seen),
            [
                wheel_json("up", 2, 0, 0),
                vec![scroll_json_from("up", "page_key", 60, 0, 0)],
                wheel_json("up", 1, 0, 0),
            ]
            .concat()
        );
        assert_eq!(stdin_writes(&seen), 3);
    }

    #[test]
    fn a_page_key_scroll_is_one_command() {
        let script = sized_script();
        run_queued(&script, vec![page(ScrollDirection::Down, 40)]);
        assert_eq!(
            stdin_lines(&script.seen()),
            vec![scroll_json_from("down", "page_key", 40, 0, 0)]
        );
    }

    #[test]
    fn a_scroll_is_ordered_between_the_input_around_it() {
        let script = sized_script();
        run_queued(
            &script,
            vec![
                WriteJob::Text("a".into()),
                scroll(ScrollDirection::Up, 2, 0, 0),
                WriteJob::Text("b".into()),
            ],
        );
        assert_eq!(
            stdin_lines(&script.seen()),
            vec![
                serde_json::json!({"type": "terminal.input", "text": "a"}),
                scroll_json_from("up", "wheel", 1, 0, 0),
                scroll_json_from("up", "wheel", 1, 0, 0),
                serde_json::json!({"type": "terminal.input", "text": "b"}),
            ]
        );
    }

    #[test]
    fn a_failed_scroll_write_is_dropped_without_restarting_the_bridge() {
        let script = sized_script();
        let dead = script.stdin().unwrap();
        *dead.lock().unwrap() = None; // the child's stdin is closed
        let writers = Writers::with_wait(Duration::from_millis(10));
        writers
            .send(&key(), 1, scroll(ScrollDirection::Up, 1, 0, 0), || {
                script.clone()
            })
            .unwrap();
        flush(&writers, &script);
        // The dead bridge is dropped as for input; never restarted, never retried.
        assert_eq!(script.seen(), vec![Seen::DroppedBridge]);
        assert!(script.stdin().is_none());
        // The writer is still alive and scrolls down a new bridge.
        script.attach();
        writers
            .send(&key(), 1, scroll(ScrollDirection::Down, 2, 0, 0), || {
                script.clone()
            })
            .unwrap();
        flush(&writers, &script);
        assert_eq!(stdin_lines(&script.seen()), wheel_json("down", 2, 0, 0));
    }

    #[test]
    fn scrolls_without_a_bridge_or_refused_do_nothing() {
        let script = Script::default();
        let writers = Writers::default();
        writers
            .send(&key(), 1, scroll(ScrollDirection::Up, 1, 0, 0), || {
                script.clone()
            })
            .unwrap();
        flush(&writers, &script);
        script.attach();
        *script.refuse.lock().unwrap() = true;
        writers
            .send(&key(), 1, scroll(ScrollDirection::Up, 1, 0, 0), || {
                script.clone()
            })
            .unwrap();
        flush(&writers, &script);
        assert!(script.seen().is_empty(), "{:?}", script.seen());
    }

    #[test]
    fn back_to_back_resizes_collapse_to_the_last() {
        let script = sized_script();
        let writers = Writers::default();
        let gate_tx = hold_writer(&writers, &script);
        for (cols, rows) in [(81, 24), (82, 25), (83, 26)] {
            writers
                .send(&key(), 1, WriteJob::Resize { cols, rows }, unused)
                .unwrap();
        }
        gate_tx.send(()).unwrap();
        let (tx, rx) = mpsc::sync_channel(1);
        writers
            .send(&key(), 1, WriteJob::Flush(tx), unused)
            .unwrap();
        rx.recv().unwrap();
        assert_eq!(stdin_lines(&script.seen()), vec![resize_json(83, 26)]);
        assert_eq!(*script.size.lock().unwrap(), (83, 26));
    }
}
