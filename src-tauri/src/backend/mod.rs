//! `backend` — the PTY/session backend seam.
//!
//! App code (`host.rs`, `dispatch.rs`, `transfer.rs`, `tasks.rs`) depends on the
//! neutral [`PtyBackend`] trait and the owned types in this module — never on a
//! concrete backend. The implementation is [`herdr::HerdrBackend`];
//! [`unavailable::UnavailableBackend`] stands in when herdr cannot be constructed,
//! so the app still starts and reports why.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use keitora_tasks::WorktreeSession;

pub mod emit;
pub mod herdr;
pub mod launch;
pub mod platform;
pub mod unavailable;

pub use emit::EmitSink;

#[cfg(test)]
pub mod fake;

// ── Neutral session identity + summaries ─────────────────────────────────────

/// Which way a [`PtyBackend::scroll`] moves: `up` is back into history. Its wire
/// spelling (IPC and herdr's `terminal.scroll`) is lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollDirection {
    Up,
    Down,
}

impl ScrollDirection {
    /// The wire spelling: `"up"` / `"down"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ScrollDirection::Up => "up",
            ScrollDirection::Down => "down",
        }
    }
}

/// What a [`PtyBackend::scroll`] came from, which decides how herdr routes it to an
/// app that handles scrolling itself: a `wheel` tick becomes one mouse report (or
/// arrow key), a `page_key` becomes one PageUp/PageDown keypress. Over a plain
/// shell both scroll herdr's own history. Its wire spelling is snake case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollSource {
    Wheel,
    PageKey,
}

impl ScrollSource {
    /// The wire spelling: `"wheel"` / `"page_key"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ScrollSource::Wheel => "wheel",
            ScrollSource::PageKey => "page_key",
        }
    }
}

/// Canonical identity of a PTY session, `"project/worktree"`. Owned by `backend`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub project: String,
    pub worktree: String,
}

impl fmt::Display for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.project, self.worktree)
    }
}

/// [`SessionSummary::state_hint`] value meaning the backend observed background
/// work running in the session. The app matches this string; the backend sets it.
pub const HINT_BUSY: &str = "busy";

/// Per-session summary returned by [`PtyBackend::list_sessions`]. Mirrors the real
/// proto shape; `state_hint` carries [`HINT_BUSY`] for a background-running session.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub key: SessionKey,
    pub gen: u64,
    pub state_hint: Option<String>,
}

/// A live-session hint surfaced to the frontend by `daemon_list_sessions` (mirrors
/// the TS `DaemonSessionHint`). Splits the [`SessionKey`] into its `project` /
/// `worktree` parts so the GUI can group + count by project.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionHint {
    pub project: String,
    pub worktree: String,
    pub gen: u64,
}

/// Map a [`SessionSummary`] to a frontend [`SessionHint`].
pub fn session_hint(s: &SessionSummary) -> SessionHint {
    SessionHint {
        project: s.key.project.clone(),
        worktree: s.key.worktree.clone(),
        gen: s.gen,
    }
}

/// Result of [`PtyBackend::attach`]: the backend-side generation plus the
/// initial screen blob (raw VT bytes).
#[derive(Debug, Clone)]
pub struct AttachResult {
    pub gen: u64,
    pub scrollback: Vec<u8>,
    /// Identifies this attach for a scoped [`PtyBackend::detach`].
    pub attach_id: u64,
    /// Whether [`PtyBackend::repaint`] really repaints this session. A pane whose
    /// backend cannot repaint must never drop output it has not written.
    pub repaints: bool,
}

// ── Pane output delivery ─────────────────────────────────────────────────────

/// Where one attached pane's output goes (the webview's per-attach channel in
/// the app). Called from backend reader threads, so it must not block for long.
pub trait OutputSink: Send + Sync {
    /// One output chunk: base64 VT bytes. `full` marks a whole-viewport repaint.
    fn frame(&self, b64: String, full: bool);
    /// The session's process ended.
    fn exit(&self);
}

/// One session's current attach: its id and its output sink.
pub type Attached = (u64, Arc<dyn OutputSink>);

/// What an attach still in progress replaced, until it succeeds or fails.
enum Replaced {
    /// Nothing was attached.
    Nothing,
    /// That attach, put back if this one fails.
    Attach(Attached),
    /// An attach whose owner has since detached: nothing to put back, and the
    /// attach's resources are still to be released.
    Released,
}

/// One session's current attach, and what it replaced while still in progress.
struct Slot {
    current: Attached,
    replaced: Replaced,
}

/// The output sink attached to each session, with the id of the attach that
/// registered it. Cloning shares the same registry. Ids come from one monotonic
/// counter, so an id is never reused.
#[derive(Clone)]
pub struct OutputSinks {
    by_key: Arc<Mutex<HashMap<SessionKey, Slot>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for OutputSinks {
    fn default() -> Self {
        OutputSinks {
            by_key: Arc::default(),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

impl OutputSinks {
    /// Make `sink` the output of `key`, replacing any earlier one; returns the
    /// new attach id.
    #[cfg(test)]
    pub fn register(&self, key: &SessionKey, sink: Arc<dyn OutputSink>) -> u64 {
        let id = self.replace(key, sink);
        self.settle(key, id);
        id
    }

    /// Make `sink` the output of `key` for an attach in progress; returns the
    /// new attach id. The attach it replaced is kept until [`Self::settle`] (the
    /// attach succeeded) or [`Self::revert`] (it failed, putting it back).
    pub fn replace(&self, key: &SessionKey, sink: Arc<dyn OutputSink>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut map = self.by_key.lock().unwrap();
        let replaced = match map.remove(key) {
            Some(old) => Replaced::Attach(old.current),
            None => Replaced::Nothing,
        };
        map.insert(
            key.clone(),
            Slot {
                current: (id, sink),
                replaced,
            },
        );
        id
    }

    /// Attach `id` succeeded: forget what it replaced.
    pub fn settle(&self, key: &SessionKey, id: u64) {
        if let Some(slot) = self.by_key.lock().unwrap().get_mut(key) {
            if slot.current.0 == id {
                slot.replaced = Replaced::Nothing;
            }
        }
    }

    /// Undo the [`Self::replace`] of a failed attach `id`: while `id` is still
    /// `key`'s attach, the one it replaced comes back (or nothing, if none did),
    /// so that attach's own scoped detach still releases it. `true` when the
    /// replaced attach was detached meanwhile: it is not put back, and the
    /// caller owes the release its detach skipped (the session is unattached).
    pub fn revert(&self, key: &SessionKey, id: u64) -> bool {
        let mut map = self.by_key.lock().unwrap();
        if map.get(key).is_none_or(|s| s.current.0 != id) {
            return false;
        }
        let Some(slot) = map.remove(key) else {
            return false;
        };
        match slot.replaced {
            Replaced::Attach(p) => {
                map.insert(
                    key.clone(),
                    Slot {
                        current: p,
                        replaced: Replaced::Nothing,
                    },
                );
                false
            }
            Replaced::Nothing => false,
            Replaced::Released => true,
        }
    }

    /// The id of `key`'s current attach, if any.
    pub fn current_id(&self, key: &SessionKey) -> Option<u64> {
        self.by_key.lock().unwrap().get(key).map(|s| s.current.0)
    }

    /// `key`'s current attach (its id and sink), if any.
    pub fn current(&self, key: &SessionKey) -> Option<Attached> {
        self.by_key
            .lock()
            .unwrap()
            .get(key)
            .map(|s| (s.current.0, Arc::clone(&s.current.1)))
    }

    /// The sink attached to `key`, if any.
    #[cfg(test)]
    pub fn get(&self, key: &SessionKey) -> Option<Arc<dyn OutputSink>> {
        self.by_key
            .lock()
            .unwrap()
            .get(key)
            .map(|s| Arc::clone(&s.current.1))
    }

    /// Whether a detach scoped to `attach_id` should go ahead, dropping `key`'s
    /// sink if so. `None` (an unscoped detach) always goes ahead. `Some(id)` goes
    /// ahead only while `id` is `key`'s current attach: an id that a newer attach
    /// replaced, or that was already released, returns `false`. An id replaced
    /// by an attach still in progress is marked released, so that attach failing
    /// does not bring it back (see [`Self::revert`]).
    pub fn release(&self, key: &SessionKey, attach_id: Option<u64>) -> bool {
        let mut map = self.by_key.lock().unwrap();
        match attach_id {
            None => {
                map.remove(key);
                true
            }
            Some(id) => match map.get_mut(key) {
                Some(slot) if slot.current.0 == id => {
                    map.remove(key);
                    true
                }
                Some(slot) => {
                    if matches!(&slot.replaced, Replaced::Attach((p, _)) if *p == id) {
                        slot.replaced = Replaced::Released;
                    }
                    false
                }
                None => false,
            },
        }
    }
}

/// Everything [`PtyBackend::spawn_agent`] needs beyond the plain spawn params
/// (`key`/`cwd`/`db_path`/`cols`/`rows`). Resolved by `host::resolve_agent_launch`
/// from the worktree registry + config. herdr uses `cli` + `autonomy` (+ `path` for
/// the pane PATH), takes `resume_target` as the registry fallback when choosing
/// which conversation to resume, and starts a new conversation when `fresh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLaunch {
    pub cli: keitora_tasks::CliKind,
    /// Resolved agent executable (absolute, or the bare `claude`/`codex` fallback).
    pub path: String,
    pub autonomy: bool,
    /// Persisted Claude session id / Codex thread id to resume, if any.
    pub resume_target: Option<String>,
    /// The user asked for a new, empty conversation instead of a resume.
    pub fresh: bool,
}

/// JSON payload emitted on the `daemon://status` Tauri event / returned by the
/// `daemon_status` command. The JSON shape is the webview contract: fields may
/// be added, never renamed or removed. `log_path` is the path of the active
/// daemon's log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusPayload {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_count: Option<usize>,
    /// Why the backend is in this state, for the user (set when herdr is
    /// unavailable or was not found). Omitted from the JSON when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether `restart` can do anything about a `Disconnected` state (always
    /// serialized). False only when herdr could not be set up at all, which
    /// takes a relaunch.
    pub restartable: bool,
    pub log_path: String,
}

/// Live process stats for the daemon, returned by the `daemon_stats` command and
/// rendered in the rail daemon card. Separate from [`StatusPayload`] (whose
/// wire shape is frozen) because it carries fields `StatusPayload` can't: the herdr
/// `version` STRING (not the protocol number) and sampled CPU/memory. Only the herdr
/// backend fills it; any other backend returns the `running: false` default.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DaemonStats {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Seconds since the daemon process started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// Whole-machine CPU percent (per-process usage already divided by core count).
    /// `None` when the pid is unknown or the process has exited. Reads `0.0` on the very
    /// first sample after a pid first appears (sysinfo needs two refreshes for a real
    /// delta), then reflects actual usage on subsequent 2s polls. The rail card shows `—`
    /// only while no sample has arrived yet (the hook's `null` state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_percent: Option<f32>,
    /// Resident set size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_bytes: Option<u64>,
    /// The daemon's own version string (e.g. `"0.9.3"`), distinct from the protocol
    /// number in [`StatusPayload::daemon_version`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Normalize a per-process CPU reading (sysinfo reports percent-of-one-core, which can
/// exceed 100 on multi-core work) to whole-machine percent by dividing across the logical
/// core count. Pure + unit-tested. `cores == 0` is treated as 1 (never divide by
/// zero); the result is clamped to `[0, 100]`.
pub fn normalize_cpu(per_core_percent: f32, cores: usize) -> f32 {
    let cores = cores.max(1) as f32;
    (per_core_percent / cores).clamp(0.0, 100.0)
}

/// Uptime in whole seconds from a process `start_time` (epoch secs) and `now` (epoch
/// secs). Saturating so clock skew can't underflow. Pure + unit-tested.
pub fn uptime_from(now_secs: u64, start_secs: u64) -> u64 {
    now_secs.saturating_sub(start_secs)
}

// ── Dispatch-gate mapping ────────────────────────────────────────────────────

/// How the dispatch path should treat a [`PtyBackend::write_line`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteLineOutcome {
    /// `Ok(())` — text+`\r` reached the PTY.
    Dispatched,
    /// `NoSuchSession` / `NotConnected` — leave the task reserved, retry later.
    Queued,
    /// Any other error — propagate to the caller.
    Error,
}

/// Pure mapping of a `write_line` result to its dispatch outcome: `Ok` ->
/// `Dispatched`; `NoSuchSession`, `NotConnected` and a server under recovery ->
/// `Queued`; anything else -> `Error`.
pub fn map_write_line_result(result: &Result<(), PtyBackendError>) -> WriteLineOutcome {
    match result {
        Ok(()) => WriteLineOutcome::Dispatched,
        Err(PtyBackendError::NoSuchSession)
        | Err(PtyBackendError::NotConnected)
        | Err(PtyBackendError::RecoveryPending)
        | Err(PtyBackendError::RecoveryPaused) => WriteLineOutcome::Queued,
        Err(_) => WriteLineOutcome::Error,
    }
}

// ── Error type ───────────────────────────────────────────────────────────────

/// The only error the [`PtyBackend`] trait returns. Named `PtyBackendError` (not
/// `BackendError`) to avoid colliding with `keitora_tasks::BackendError`.
///
/// `Display` strings are the error text the webview commands return.
#[derive(Debug)]
pub enum PtyBackendError {
    /// Backend unreachable (herdr not running or not available).
    NotConnected,
    /// No live session for the key.
    NoSuchSession,
    /// The server is down or being recovered; only recovery may use it.
    RecoveryPending,
    /// Server recovery stopped and is waiting for the user.
    RecoveryPaused,
    /// The user terminated the session; it stays down until launched.
    Terminated,
    /// Any other error (protocol / IO / implementation-specific), message verbatim.
    Other(String),
}

impl From<herdr::gate::GateRefusal> for PtyBackendError {
    fn from(r: herdr::gate::GateRefusal) -> Self {
        match r {
            herdr::gate::GateRefusal::RecoveryPending => PtyBackendError::RecoveryPending,
            herdr::gate::GateRefusal::Paused => PtyBackendError::RecoveryPaused,
        }
    }
}

impl fmt::Display for PtyBackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtyBackendError::NotConnected => write!(f, "herdr unreachable"),
            PtyBackendError::NoSuchSession => write!(f, "no live session"),
            PtyBackendError::RecoveryPending => write!(f, "herdr is recovering"),
            PtyBackendError::RecoveryPaused => write!(f, "herdr recovery is paused"),
            PtyBackendError::Terminated => write!(f, "Terminated: the session was terminated"),
            PtyBackendError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PtyBackendError {}

// ── The trait ────────────────────────────────────────────────────────────────

/// A PTY/session backend. Object-safe (no generics, no `Self` returns) and
/// `Send + Sync + 'static` so it can live behind `Arc<dyn PtyBackend>` in Tauri
/// state. Every method is synchronous and blocking (each RPC blocks up to the
/// backend's own timeout), so there is no `async_trait`.
pub trait PtyBackend: Send + Sync + 'static {
    /// Begin connecting (non-blocking).
    fn start(&self);

    /// Spawn a new session, replacing any live same-key session; returns the new,
    /// strictly-greater generation. Clears any explicit-Stop gate.
    fn spawn(
        &self,
        key: SessionKey,
        cwd: String,
        db_path: String,
        size: Option<(u16, u16)>,
        command: Option<String>,
    ) -> Result<u64, PtyBackendError>;

    /// Spawn the worktree's default agent CLI (Claude/Codex) in a new session,
    /// replacing any live same-key session; returns the new generation. The backend
    /// starts the agent natively from `launch.cli` / `launch.autonomy` (+
    /// `launch.path` for the pane PATH).
    fn spawn_agent(
        &self,
        key: SessionKey,
        cwd: String,
        db_path: String,
        size: Option<(u16, u16)>,
        launch: AgentLaunch,
    ) -> Result<u64, PtyBackendError>;

    /// Attach to an existing live session; returns its generation, scrollback and
    /// a fresh attach id. From now on the session's output and exit go to `out`
    /// (replacing the sink of any earlier attach).
    fn attach(
        &self,
        key: SessionKey,
        size: Option<(u16, u16)>,
        out: Arc<dyn OutputSink>,
    ) -> Result<AttachResult, PtyBackendError>;

    /// Release one local subscription ref (the key is taken by value, as today).
    /// With `Some(attach_id)` it is scoped: when that is not the session's
    /// current attach (a newer one replaced it), this is a no-op.
    fn detach(&self, key: SessionKey, attach_id: Option<u64>) -> Result<(), PtyBackendError>;

    /// Write raw bytes (user keystrokes) into a session.
    fn write(&self, key: &SessionKey, data: &[u8]) -> Result<(), PtyBackendError>;

    /// Atomic server-side submit: write `text`, pause `submit_delay_ms`, write `\r`.
    fn write_line(
        &self,
        key: &SessionKey,
        text: &str,
        submit_delay_ms: u32,
    ) -> Result<(), PtyBackendError>;

    /// Resize the PTY (note: `cols, rows` order, matching the wire).
    fn resize(&self, key: &SessionKey, cols: u16, rows: u16) -> Result<(), PtyBackendError>;

    /// Push one fresh full-viewport repaint frame for a session, on demand.
    ///
    /// An idle restored pane streams no new output, so the single frame captured at
    /// attach can be lost if the webview's renderer wasn't ready yet (blank terminal +
    /// block cursor until a manual restart). The frontend calls this once its xterm/WebGL
    /// renderer is live so a current frame lands in a renderer that can paint it. Default
    /// is a no-op.
    fn repaint(&self, key: &SessionKey) -> Result<(), PtyBackendError> {
        let _ = key;
        Ok(())
    }

    /// Scroll a session's viewport by `lines` from `source` (a wheel tick or a page
    /// key), with the pointer at the 0-based cell `column`,`row`. The backend decides
    /// where it goes: the app (mouse reporting, alternate scroll, page keys) or the
    /// backend's own history. Best-effort: no session or bridge is not an error.
    /// Default is a no-op.
    fn scroll(
        &self,
        key: &SessionKey,
        direction: ScrollDirection,
        source: ScrollSource,
        lines: u16,
        column: u16,
        row: u16,
    ) -> Result<(), PtyBackendError> {
        let _ = (key, direction, source, lines, column, row);
        Ok(())
    }

    /// How many lines above the live tail a session's viewport is scrolled, when
    /// the backend can tell (`0` is the live tail). `None` when it cannot, or the
    /// session is unknown. One-shot query for the UI, never polled. Default is `None`.
    fn scroll_offset(&self, key: &SessionKey) -> Result<Option<u32>, PtyBackendError> {
        let _ = key;
        Ok(None)
    }

    /// The last `lines` lines of a session's history as plain text (what scrolled off
    /// plus the live screen). One-shot query for the UI. Default: unsupported.
    fn read_history(&self, key: &SessionKey, lines: u32) -> Result<String, PtyBackendError> {
        let _ = (key, lines);
        Err(PtyBackendError::Other(
            "history is not available on this backend".to_string(),
        ))
    }

    /// Generation-gated kill: a kill for an older generation than the live one is a
    /// no-op `Ok`.
    fn kill(&self, key: &SessionKey, gen: u64) -> Result<(), PtyBackendError>;

    /// List all sessions.
    fn list_sessions(&self) -> Result<Vec<SessionSummary>, PtyBackendError>;

    /// The sessions as last listed, without asking the server again when the
    /// backend keeps a cache (the herdr backend answers from its last pane list,
    /// however old). For frequent UI polls that must not spawn processes. The
    /// default is [`PtyBackend::list_sessions`].
    fn cached_sessions(&self) -> Result<Vec<SessionSummary>, PtyBackendError> {
        self.list_sessions()
    }

    /// The worktree names of `project` whose session the user terminated.
    fn terminated_keys(&self, project: &str) -> HashSet<String>;

    /// Persist (or clear) the user's terminate of `key`. A terminated session is
    /// never resumed by recovery and refuses a spawn until the flag is cleared.
    fn set_terminated(&self, key: &SessionKey, terminated: bool) -> Result<(), PtyBackendError>;

    /// The terminate's kill, once the flag is set: kill whatever session of
    /// `key` is live now and drop `key` from recovery. `gen` is the generation
    /// the caller last saw; a backend that knows the live one itself kills that
    /// instead (a resume may have started a newer one meanwhile). The default
    /// kills `gen`, else the generation the cached session list has for `key`,
    /// and nothing when neither knows one.
    fn kill_live(&self, key: &SessionKey, gen: Option<u64>) -> Result<(), PtyBackendError> {
        let gen = match gen {
            Some(gen) => Some(gen),
            None => self
                .cached_sessions()?
                .into_iter()
                .find(|s| s.key == *key)
                .map(|s| s.gen),
        };
        match gen {
            Some(gen) => self.kill(key, gen),
            None => Ok(()),
        }
    }

    /// Clone of the cached live-set (display / reconciliation only).
    fn live_set_snapshot(&self) -> HashSet<SessionKey>;

    /// `true` if the live-set currently contains `key` (display-only).
    fn is_live(&self, key: &SessionKey) -> bool;

    /// Serialisable `daemon://status` payload snapshot.
    fn status_payload_snapshot(&self) -> StatusPayload;

    /// Live process stats for the rail daemon card: a cheap read of what the backend
    /// last sampled (the herdr backend's status loop also pushes them as
    /// `daemon://stats`). The default impl returns `running: false` — only the herdr
    /// backend overrides it.
    fn daemon_stats(&self) -> DaemonStats {
        DaemonStats::default()
    }

    /// What the performance overlay's sampler reads from the backend: its panes'
    /// process roots and server pid. The default has none (the sampler then
    /// attributes nothing to sessions) — only the herdr backend overrides it.
    fn perf_source(&self) -> Option<Arc<dyn crate::perf::PerfSource>> {
        None
    }

    /// Stop the backend and engage the explicit-Stop gate (no auto-respawn until
    /// `restart()` or `spawn()`).
    fn shutdown(&self) -> Result<(), PtyBackendError>;

    /// Restart the backend. `Ok(())` when a restart is under way (or needs
    /// none), or the reason nothing was started (the herdr backend, when the
    /// herdr executable is still not found). It does not wait for the restart,
    /// which runs in the background, but it is not instant: the herdr backend
    /// first searches for the herdr executable again (a config reload and a
    /// scan of the usual places; on unix, when those all miss, the login shell)
    /// and, only when that search finds nothing, makes one status call (bounded
    /// by its deadline, a few seconds) to confirm herdr cannot be run. Call it
    /// off the UI thread.
    fn restart(&self) -> Result<(), PtyBackendError>;

    /// The one-time notice that pane tools now use the user's normal folders: the
    /// old isolated tool folders left in place, or `None` when there is nothing to
    /// say (or it was already said). Only the herdr backend isolates panes' tools.
    fn take_xdg_leak_notice(&self) -> Option<Vec<String>> {
        None
    }

    /// Where server recovery stands (the `daemon://recovery` payload). Backends
    /// without recovery are always healthy.
    fn recovery_snapshot(&self) -> herdr::gate::RecoveryPayload {
        herdr::gate::RecoveryPayload::healthy()
    }

    /// The recovery controller, for callers that steer it (cancels, focus,
    /// transfers). Backends without recovery have none.
    fn recovery(&self) -> Option<herdr::recovery::RecoveryHandle> {
        None
    }
}

/// Shared, type-erased backend handle managed in Tauri state.
pub type SharedBackend = std::sync::Arc<dyn PtyBackend>;

// ── Reconciliation (pure) ────────────────────────────────────────────────────

/// Downgrade non-offline sessions absent from the live-set to `offline` (in-memory
/// only). Then any session still `idle` whose key appears in `bg` (the backend's
/// background-work hint set) is upgraded to `busy`. Offline wins over `bg`.
///
/// Pure: `(&[WorktreeSession], &HashSet<SessionKey>, &HashSet<SessionKey>, project) -> Vec<…>`.
pub fn apply_reconcile(
    sessions: &[WorktreeSession],
    live_set: &HashSet<SessionKey>,
    bg: &HashSet<SessionKey>,
    project: &str,
) -> Vec<WorktreeSession> {
    sessions
        .iter()
        .cloned()
        .map(|mut s| {
            let key = SessionKey {
                project: project.to_string(),
                worktree: s.worktree.clone(),
            };
            // Offline downgrade first — a session with no live PTY cannot have
            // background work (offline wins over bg).
            if s.state != "offline" && !live_set.contains(&key) {
                s.state = "offline".to_string();
                s.current_task_id = None;
                s.pid = None;
            }
            // Background-work override: upgrade hook-reported idle -> busy.
            if s.state == "idle" && bg.contains(&key) {
                s.state = "busy".to_string();
            }
            s
        })
        .collect()
}

/// Keys the backend currently reports as background-running. Degrades gracefully to
/// an empty set on any error — never blocks the UI or dispatch on a flaky backend.
pub fn background_set(backend: &dyn PtyBackend) -> HashSet<SessionKey> {
    busy_keys(backend.list_sessions())
}

fn busy_keys(r: Result<Vec<SessionSummary>, PtyBackendError>) -> HashSet<SessionKey> {
    r.map(|v| {
        v.into_iter()
            .filter(|s| s.state_hint.as_deref() == Some(HINT_BUSY))
            .map(|s| s.key)
            .collect()
    })
    .unwrap_or_default()
}

/// [`background_set`] over [`PtyBackend::cached_sessions`]: the busy keys as
/// last listed, never asking the server. Empty on any error.
pub fn cached_background_set(backend: &dyn PtyBackend) -> HashSet<SessionKey> {
    busy_keys(backend.cached_sessions())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::FakeBackend;

    #[test]
    fn scroll_directions_use_their_lowercase_wire_spelling() {
        for (d, word) in [(ScrollDirection::Up, "up"), (ScrollDirection::Down, "down")] {
            assert_eq!(d.as_str(), word);
            assert_eq!(serde_json::to_value(d).unwrap(), word);
            assert_eq!(
                serde_json::from_value::<ScrollDirection>(word.into()).unwrap(),
                d
            );
        }
        assert!(serde_json::from_value::<ScrollDirection>("Up".into()).is_err());
        assert!(serde_json::from_value::<ScrollDirection>("left".into()).is_err());
    }

    #[test]
    fn scroll_sources_use_their_snake_case_wire_spelling() {
        for (s, word) in [
            (ScrollSource::Wheel, "wheel"),
            (ScrollSource::PageKey, "page_key"),
        ] {
            assert_eq!(s.as_str(), word);
            assert_eq!(serde_json::to_value(s).unwrap(), word);
            assert_eq!(
                serde_json::from_value::<ScrollSource>(word.into()).unwrap(),
                s
            );
        }
        assert!(serde_json::from_value::<ScrollSource>("pageKey".into()).is_err());
        assert!(serde_json::from_value::<ScrollSource>("Wheel".into()).is_err());
    }

    fn key(project: &str, worktree: &str) -> SessionKey {
        SessionKey {
            project: project.to_string(),
            worktree: worktree.to_string(),
        }
    }

    fn session(worktree: &str, state: &str) -> WorktreeSession {
        WorktreeSession {
            project: String::new(),
            worktree: worktree.to_string(),
            state: state.to_string(),
            current_task_id: Some(7),
            pid: Some(1234),
            last_heartbeat: "now".to_string(),
        }
    }

    // ── Error Display ─────────────────────────────────────────────────────────

    #[test]
    fn backend_error_display_strings() {
        assert_eq!(
            PtyBackendError::NotConnected.to_string(),
            "herdr unreachable"
        );
        assert_eq!(
            PtyBackendError::NoSuchSession.to_string(),
            "no live session"
        );
        // `Other` renders its message verbatim (e.g. the folded protocol/io text).
        assert_eq!(
            PtyBackendError::Other("protocol error: boom".to_string()).to_string(),
            "protocol error: boom"
        );
        assert_eq!(
            PtyBackendError::Other("io error: bad".to_string()).to_string(),
            "io error: bad"
        );
        assert_eq!(
            PtyBackendError::RecoveryPending.to_string(),
            "herdr is recovering"
        );
        assert_eq!(
            PtyBackendError::RecoveryPaused.to_string(),
            "herdr recovery is paused"
        );
        assert_eq!(
            PtyBackendError::Terminated.to_string(),
            "Terminated: the session was terminated"
        );
    }

    #[test]
    fn gate_refusals_map_to_recovery_errors() {
        use herdr::gate::GateRefusal;
        assert!(matches!(
            PtyBackendError::from(GateRefusal::RecoveryPending),
            PtyBackendError::RecoveryPending
        ));
        assert!(matches!(
            PtyBackendError::from(GateRefusal::Paused),
            PtyBackendError::RecoveryPaused
        ));
    }

    #[test]
    fn recovery_snapshot_defaults_to_healthy() {
        let fake = FakeBackend::new();
        assert_eq!(
            fake.recovery_snapshot(),
            herdr::gate::RecoveryPayload::healthy()
        );
    }

    #[test]
    fn session_key_display_is_project_slash_worktree() {
        assert_eq!(key("alpha", "sayo").to_string(), "alpha/sayo");
    }

    // ── Dispatch-gate mapping ─────────────────────────────────────────────────

    // ── Daemon stats helpers ──────────────────────────────────────────────────

    #[test]
    fn normalize_cpu_divides_by_cores_and_clamps() {
        // 200% of one core across 4 cores = 50% of the machine.
        assert_eq!(normalize_cpu(200.0, 4), 50.0);
        // 0 cores is treated as 1 (never divide by zero).
        assert_eq!(normalize_cpu(30.0, 0), 30.0);
        // Over-100 (all cores pegged, rounding) clamps to 100.
        assert_eq!(normalize_cpu(800.0, 4), 100.0);
        // Idle stays 0.
        assert_eq!(normalize_cpu(0.0, 8), 0.0);
    }

    #[test]
    fn uptime_from_is_saturating() {
        assert_eq!(uptime_from(1_000, 400), 600);
        // Clock skew (start in the "future") can't underflow.
        assert_eq!(uptime_from(400, 1_000), 0);
        assert_eq!(uptime_from(500, 500), 0);
    }

    #[test]
    fn daemon_stats_defaults_to_not_running() {
        assert_eq!(FakeBackend::new().daemon_stats(), DaemonStats::default());
    }

    #[test]
    fn perf_source_defaults_to_none() {
        assert!(FakeBackend::new().perf_source().is_none());
    }

    #[test]
    fn take_xdg_leak_notice_defaults_to_none() {
        assert_eq!(FakeBackend::new().take_xdg_leak_notice(), None);
    }

    #[test]
    fn daemon_stats_default_is_not_running_and_omits_nulls() {
        let d = DaemonStats::default();
        assert!(!d.running);
        let json = serde_json::to_value(&d).unwrap();
        // Only `running` serializes; every Option is skipped when None.
        assert_eq!(json, serde_json::json!({ "running": false }));
    }

    #[test]
    fn write_line_result_maps_to_dispatch_outcome() {
        assert_eq!(map_write_line_result(&Ok(())), WriteLineOutcome::Dispatched);
        assert_eq!(
            map_write_line_result(&Err(PtyBackendError::NoSuchSession)),
            WriteLineOutcome::Queued
        );
        assert_eq!(
            map_write_line_result(&Err(PtyBackendError::NotConnected)),
            WriteLineOutcome::Queued
        );
        assert_eq!(
            map_write_line_result(&Err(PtyBackendError::RecoveryPending)),
            WriteLineOutcome::Queued
        );
        assert_eq!(
            map_write_line_result(&Err(PtyBackendError::RecoveryPaused)),
            WriteLineOutcome::Queued
        );
        assert_eq!(
            map_write_line_result(&Err(PtyBackendError::Other("boom".into()))),
            WriteLineOutcome::Error
        );
    }

    // ── session_hint mapping ──────────────────────────────────────────────────

    #[test]
    fn session_hint_splits_key_into_project_worktree() {
        let summaries = [
            SessionSummary {
                key: key("alpha", "sayo"),
                gen: 1,
                state_hint: None,
            },
            SessionSummary {
                key: key("beta", "vesper"),
                gen: 7,
                state_hint: None,
            },
            SessionSummary {
                key: key("alpha", "umbra"),
                gen: 2,
                state_hint: None,
            },
        ];
        let hints: Vec<SessionHint> = summaries.iter().map(session_hint).collect();
        assert_eq!(
            hints,
            vec![
                SessionHint {
                    project: "alpha".into(),
                    worktree: "sayo".into(),
                    gen: 1
                },
                SessionHint {
                    project: "beta".into(),
                    worktree: "vesper".into(),
                    gen: 7
                },
                SessionHint {
                    project: "alpha".into(),
                    worktree: "umbra".into(),
                    gen: 2
                },
            ]
        );
        assert_eq!(hints[0].project, "alpha");
        assert_eq!(hints[0].worktree, "sayo");
        assert_eq!(hints[1].project, "beta");
        assert_eq!(hints[1].worktree, "vesper");
    }

    // ── apply_reconcile (offline downgrade) ───────────────────────────────────

    #[test]
    fn reconciled_sessions_downgrades_non_offline_when_absent_from_live_set() {
        let live = HashSet::new();
        let sessions = vec![session("sayo", "idle")];
        let result = apply_reconcile(&sessions, &live, &HashSet::new(), "proj");
        assert_eq!(result[0].state, "offline");
        assert_eq!(result[0].current_task_id, None);
        assert_eq!(result[0].pid, None);
    }

    #[test]
    fn reconciled_sessions_preserves_live_session_state() {
        let mut live = HashSet::new();
        live.insert(key("proj", "sayo"));
        let sessions = vec![session("sayo", "idle")];
        let result = apply_reconcile(&sessions, &live, &HashSet::new(), "proj");
        assert_eq!(result[0].state, "idle");
        assert_eq!(result[0].current_task_id, Some(7));
    }

    #[test]
    fn reconciled_sessions_leaves_already_offline_untouched() {
        let live = HashSet::new();
        let sessions = vec![session("sayo", "offline")];
        let result = apply_reconcile(&sessions, &live, &HashSet::new(), "proj");
        assert_eq!(result[0].state, "offline");
    }

    // ── Background-set override ────────────────────────────────────────

    #[test]
    fn bg_override_idle_in_bg_and_live_becomes_busy() {
        let mut live = HashSet::new();
        live.insert(key("proj", "sayo"));
        let mut bg = HashSet::new();
        bg.insert(key("proj", "sayo"));
        let sessions = vec![session("sayo", "idle")];
        let result = apply_reconcile(&sessions, &live, &bg, "proj");
        assert_eq!(result[0].state, "busy");
    }

    #[test]
    fn bg_override_idle_not_in_bg_stays_idle() {
        let mut live = HashSet::new();
        live.insert(key("proj", "sayo"));
        let bg = HashSet::new();
        let sessions = vec![session("sayo", "idle")];
        let result = apply_reconcile(&sessions, &live, &bg, "proj");
        assert_eq!(result[0].state, "idle");
    }

    #[test]
    fn bg_override_already_busy_in_bg_stays_busy() {
        let mut live = HashSet::new();
        live.insert(key("proj", "sayo"));
        let mut bg = HashSet::new();
        bg.insert(key("proj", "sayo"));
        let sessions = vec![session("sayo", "busy")];
        let result = apply_reconcile(&sessions, &live, &bg, "proj");
        assert_eq!(result[0].state, "busy");
    }

    #[test]
    fn bg_override_waiting_in_bg_stays_waiting() {
        let mut live = HashSet::new();
        live.insert(key("proj", "sayo"));
        let mut bg = HashSet::new();
        bg.insert(key("proj", "sayo"));
        let sessions = vec![session("sayo", "waiting")];
        let result = apply_reconcile(&sessions, &live, &bg, "proj");
        assert_eq!(result[0].state, "waiting");
    }

    #[test]
    fn bg_override_offline_wins_over_bg() {
        let live = HashSet::new();
        let mut bg = HashSet::new();
        bg.insert(key("proj", "sayo"));
        let sessions = vec![session("sayo", "idle")];
        let result = apply_reconcile(&sessions, &live, &bg, "proj");
        assert_eq!(result[0].state, "offline");
        assert_eq!(result[0].current_task_id, None);
        assert_eq!(result[0].pid, None);
    }

    // ── background_set over a FakeBackend ──────────────────────────────────────

    #[test]
    fn background_set_filters_busy_and_degrades_to_empty_on_error() {
        let busy = key("proj", "sayo");
        let idle = key("proj", "umbra");
        let fake = FakeBackend::new();
        // One busy (HINT_BUSY) and one hintless session, then a scripted error.
        fake.push_list_sessions(Ok(vec![
            SessionSummary {
                key: busy.clone(),
                gen: 1,
                state_hint: Some(HINT_BUSY.to_string()),
            },
            SessionSummary {
                key: idle.clone(),
                gen: 2,
                state_hint: None,
            },
        ]));
        fake.push_list_sessions(Err(PtyBackendError::NotConnected));

        // First call: only the busy key is reported.
        let set = background_set(&fake);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&busy));
        assert!(!set.contains(&idle));

        // Second call errors -> degrade to an empty set (never block dispatch).
        let set = background_set(&fake);
        assert!(set.is_empty(), "an RPC error degrades to an empty set");
    }

    #[test]
    fn cached_background_set_defaults_to_the_listed_sessions() {
        let busy = key("proj", "sayo");
        let fake = FakeBackend::new();
        fake.push_list_sessions(Ok(vec![SessionSummary {
            key: busy.clone(),
            gen: 1,
            state_hint: Some(HINT_BUSY.to_string()),
        }]));
        fake.push_list_sessions(Err(PtyBackendError::NotConnected));

        assert_eq!(cached_background_set(&fake), HashSet::from([busy]));
        assert!(cached_background_set(&fake).is_empty());
    }

    /// A sink that only counts what it got.
    #[derive(Default)]
    struct CountingSink(Mutex<(usize, usize)>);
    impl OutputSink for CountingSink {
        fn frame(&self, _b64: String, _full: bool) {
            self.0.lock().unwrap().0 += 1;
        }
        fn exit(&self) {
            self.0.lock().unwrap().1 += 1;
        }
    }

    #[test]
    fn output_sinks_hand_out_increasing_ids_and_replace_per_key() {
        let sinks = OutputSinks::default();
        let first = Arc::new(CountingSink::default());
        let second = Arc::new(CountingSink::default());
        let a = sinks.register(&key("p", "w"), first.clone());
        let b = sinks.register(&key("p", "w"), second.clone());
        assert!(b > a, "ids are monotonic");
        sinks
            .get(&key("p", "w"))
            .unwrap()
            .frame(String::new(), false);
        assert_eq!(
            second.0.lock().unwrap().0,
            1,
            "the newer attach gets output"
        );
        assert_eq!(first.0.lock().unwrap().0, 0);
        assert!(sinks.get(&key("p", "other")).is_none());
    }

    #[test]
    fn a_stale_attach_id_does_not_release_a_newer_attach() {
        let sinks = OutputSinks::default();
        let old = sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        let new = sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        assert!(!sinks.release(&key("p", "w"), Some(old)));
        assert!(sinks.get(&key("p", "w")).is_some());
        assert!(sinks.release(&key("p", "w"), Some(new)));
        assert!(sinks.get(&key("p", "w")).is_none());
        // Nothing attached: a scoped detach has nothing to release.
        assert!(!sinks.release(&key("p", "w"), Some(new)));
    }

    #[test]
    fn a_reverted_attach_brings_back_the_one_it_replaced() {
        let sinks = OutputSinks::default();
        let old = sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        let new = sinks.replace(&key("p", "w"), Arc::new(CountingSink::default()));
        assert_eq!(sinks.current_id(&key("p", "w")), Some(new));
        assert!(!sinks.revert(&key("p", "w"), new));
        assert_eq!(sinks.current_id(&key("p", "w")), Some(old));
        // The old attach's own detach releases it again.
        assert!(sinks.release(&key("p", "w"), Some(old)));
        // A failed first attach leaves nothing behind.
        let only = sinks.replace(&key("p", "x"), Arc::new(CountingSink::default()));
        assert!(!sinks.revert(&key("p", "x"), only));
        assert!(sinks.current(&key("p", "x")).is_none());
    }

    #[test]
    fn a_revert_after_a_newer_attach_changes_nothing() {
        let sinks = OutputSinks::default();
        let failed = sinks.replace(&key("p", "w"), Arc::new(CountingSink::default()));
        let newer = sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        assert!(!sinks.revert(&key("p", "w"), failed));
        assert_eq!(sinks.current_id(&key("p", "w")), Some(newer));
    }

    #[test]
    fn a_revert_does_not_resurrect_an_attach_detached_meanwhile() {
        let sinks = OutputSinks::default();
        let old = sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        let new = sinks.replace(&key("p", "w"), Arc::new(CountingSink::default()));
        // The old attach's owner detaches while the new attach is in progress:
        // the new one is current, so nothing is released yet.
        assert!(!sinks.release(&key("p", "w"), Some(old)));
        // The new attach fails: the old one is not put back, and the caller is
        // told it owes the release the old detach skipped.
        assert!(sinks.revert(&key("p", "w"), new));
        assert!(sinks.current(&key("p", "w")).is_none());
    }

    #[test]
    fn a_settled_attach_forgets_the_one_it_replaced() {
        let sinks = OutputSinks::default();
        let old = sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        let new = sinks.replace(&key("p", "w"), Arc::new(CountingSink::default()));
        sinks.settle(&key("p", "w"), new);
        // A late detach of the old attach marks nothing, and a (bogus) revert
        // has nothing to put back.
        assert!(!sinks.release(&key("p", "w"), Some(old)));
        assert!(!sinks.revert(&key("p", "w"), new));
        assert!(sinks.current(&key("p", "w")).is_none());
    }

    #[test]
    fn an_unscoped_release_always_goes_ahead() {
        let sinks = OutputSinks::default();
        sinks.register(&key("p", "w"), Arc::new(CountingSink::default()));
        assert!(sinks.release(&key("p", "w"), None));
        assert!(sinks.get(&key("p", "w")).is_none());
        assert!(sinks.release(&key("p", "w"), None));
    }
}
