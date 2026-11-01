//! `herdr` — the [`PtyBackend`](super::PtyBackend) implementation backed by a real
//! herdr session server driven through the `herdr` CLI.
//!
//! This is the backend half of the herdr vertical slice. A herdr-backed pane can be
//! spawned (`workspace create` + `pane run`), its output streamed (one
//! `terminal session control` bridge per attached pane delivering frames to the
//! pane's output sink), input sent (through a per-session [`writer`] queue down the
//! bridge's stdin, falling back to `pane send-text` / `pane send-keys` via
//! [`input::translate_input`]), and closed (`pane close`) — all through the same
//! trait the app already uses, so no caller changes.
//!
//! Submodules:
//! - [`env`] — resolve the herdr exe, build the server's isolated env and the
//!   env panes start with (the user's own `XDG_*`);
//! - [`ops`] — the [`ops::HerdrOps`] seam every herdr interaction goes through;
//! - [`real`] — the production [`ops::HerdrOps`] that shells out to `herdr`;
//! - [`reconcile`] — bring the keymap in line with the running server instance;
//! - [`cli`] — pure parsers/types for the control-plane CLI output;
//! - [`supervisor`] — start/stop the isolated server;
//! - [`keymap`] — durable `SessionKey -> HerdrPane` map (+ gen);
//! - [`resume`] — which agent conversation each session resumes;
//! - [`input`] — pure keystroke → herdr input translation;
//! - [`paste`] — pure paste bracketing, marker stripping and size caps;
//! - [`bridge`] — per-pane output/input I/O bridge;
//! - [`writer`] — the ordered per-session input queue in front of the bridge.
//!
//! `HerdrBackend` wires herdr's first-class agent model in: `spawn_agent` launches
//! the configured agent via `agent.start --kind` (PATH resolved from the configured
//! exe dir), `write_line` dispatches via `agent.prompt`, and `list_sessions` sources
//! the busy `state_hint` from `pane list`'s per-pane `agent_status`.
//!
//! Known slice limitations (see the submodule TODOs): the keymap is in-memory
//! (not yet persisted); exotic-key
//! fidelity is deferred to the human sign-off; DB-offline-on-exit is not yet
//! ported.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::backend::EmitSink;
use crate::headroom::Headroom;
use crate::lifecycle::{self, LifecycleEvent, ResumeResult, ServerStartHow};

use super::{
    AgentLaunch, AttachResult, DaemonStats, OutputSink, OutputSinks, PtyBackend, PtyBackendError,
    ScrollDirection, ScrollSource, SessionKey, SessionSummary, StatusPayload,
};

pub mod agent;
pub mod boot;
pub mod bridge;
pub mod cli;
pub mod env;
pub mod exe_missing;
pub mod exec;
#[cfg(test)]
pub(crate) mod fake;
pub mod gate;
pub mod input;
pub mod keymap;
pub mod ops;
pub mod paste;
pub mod perf_source;
pub mod real;
pub mod reconcile;
pub mod recovery;
pub mod resume;
pub mod status_loop;
pub mod supervisor;
pub mod watchdog;
pub mod writer;

use boot::BootClass;
use bridge::{Bridge, BridgeStdin, ExitAuthority};
use cli::{PaneInfo, ServerStatus};
use env::HerdrEnv;
use exe_missing::ExeMissing;
use gate::ServerGate;
use input::HerdrInput;
use keymap::KeyMap;
use ops::HerdrOps;
use real::RealHerdr;
use reconcile::{ReconcileCtx, ReconcileOutcome};
use recovery::driver::{Executor, ResumeReport, ResumeRunner};
use recovery::machine::{RecoveryParams, ResumeMode, ResumeOutcomeKind};
use recovery::{NoopNotifier, Notifier, Parked, RecoveryGate, RecoveryHandle};
use resume::{observation_pairs, trusted_observations, ResolveCtx, ResumeDecision, ResumeStore};
use supervisor::Supervisor;
use watchdog::{DeathEvent, Watchdog};
use writer::{WriteJob, Writers};

/// `agent.start` readiness timeout (ms). herdr requires `> 3000`; its own default is
/// 30 000. We keep it bounded tighter because `spawn_agent` runs on a synchronous
/// Tauri command thread: `agent.start` can block this thread up to this long.
/// That is within the server start timeout, so it is accepted;
/// "blocked during startup" returns immediately, so the full wait only applies
/// to a genuinely slow launch.
const AGENT_START_TIMEOUT_MS: u64 = 10_000;

/// A freshly created workspace's root pane is a brand-new shell that may not be
/// at its interactive prompt yet, and `agent.start` requires the pane be at a shell
/// prompt. We keep the readiness handling simple + bounded: one brief sleep after
/// `workspace.create`, then proceed. If this proves too short against real herdr the
/// gated test records the actual `agent.start` error code to tighten it (per the plan
/// a bounded re-issue retry is the fallback).
const SHELL_READY_DELAY_MS: u64 = 300;

/// How often a reconcile asks for the running server's identity before giving up
/// and only re-adopting the live panes.
const INSTANCE_PROBE_ATTEMPTS: u32 = 3;
/// Delay between [`INSTANCE_PROBE_ATTEMPTS`] probes.
const INSTANCE_PROBE_INTERVAL_MS: u64 = 200;

/// Decides whether a session's worktree still exists (its project is registered
/// and the worktree is in its config).
pub type WorktreeFilter = Arc<dyn Fn(&SessionKey) -> bool + Send + Sync>;

/// Finds the worktree path and agent launch for a session the backend resumes on
/// its own (no pane asked for it). `None` when it cannot be resolved.
pub type LaunchResolver = Arc<dyn Fn(&SessionKey) -> Option<(String, AgentLaunch)> + Send + Sync>;

/// Bridge size for a resumed session whose size was never measured this run; the
/// real pane's attach restarts the bridge at its own size, so the value is cosmetic.
const RESUME_COLS: u16 = 120;
const RESUME_ROWS: u16 = 32;

/// Takeover size (cols, rows) when nothing better is known (see [`takeover_size`]).
const DEFAULT_TAKEOVER_SIZE: (u16, u16) = (80, 24);

/// The size (cols, rows) a takeover uses: the frontend's measured size, else the
/// session's last measured size, else the pane's own rows from herdr (default
/// cols), else the default. A takeover at a guessed size can shrink the pane, and
/// a shrink pushes its live screen into history.
fn takeover_size(
    requested: Option<(u16, u16)>,
    remembered: Option<(u16, u16)>,
    pane_rows: Option<u16>,
) -> (u16, u16) {
    requested
        .or(remembered)
        .or_else(|| pane_rows.map(|rows| (DEFAULT_TAKEOVER_SIZE.0, rows)))
        .unwrap_or(DEFAULT_TAKEOVER_SIZE)
}

/// The last size (cols, rows) the frontend measured for each session this run.
/// Each call takes and releases the lock, so it never nests under another one.
#[derive(Clone, Default)]
struct SizeMemory(Arc<Mutex<HashMap<SessionKey, (u16, u16)>>>);

impl SizeMemory {
    /// Remember `size` for `key`, clamped like every takeover.
    /// Resizes repeat the same size often, so an existing entry is updated in place.
    fn record(&self, key: &SessionKey, (cols, rows): (u16, u16)) {
        let size = (cols.max(1), rows.max(1));
        let mut sizes = self.0.lock().unwrap();
        match sizes.get_mut(key) {
            Some(known) => *known = size,
            None => {
                sizes.insert(key.clone(), size);
            }
        }
    }

    fn get(&self, key: &SessionKey) -> Option<(u16, u16)> {
        self.0.lock().unwrap().get(key).copied()
    }

    /// Remember a measured `requested` size, then pick the takeover size.
    fn takeover(
        &self,
        key: &SessionKey,
        requested: Option<(u16, u16)>,
        pane_rows: Option<u16>,
    ) -> (u16, u16) {
        if let Some(size) = requested {
            self.record(key, size);
        }
        takeover_size(requested, self.get(key), pane_rows)
    }
}

/// Apply the platform spawn flags for a short-lived herdr child (CLI call or
/// bridge): on Windows, `CREATE_NO_WINDOW` so no console flashes; a no-op
/// elsewhere. The long-lived server launches through [`crate::backend::launch`].
pub(crate) fn configure_command(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::backend::launch::CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// The memory probe a backend starts with. Unit tests never read the real
/// machine's memory: their probe is unavailable, which passes every check.
fn default_headroom() -> Arc<dyn Headroom> {
    #[cfg(test)]
    {
        Arc::new(crate::headroom::FakeHeadroom::unavailable())
    }
    #[cfg(not(test))]
    {
        Arc::new(crate::headroom::SystemHeadroom)
    }
}

/// The recovery parameters a backend starts with. Unit tests recover without
/// waits (no sustain, stagger or backoff) so they never sleep for real.
fn default_recovery_params() -> RecoveryParams {
    #[cfg(test)]
    {
        RecoveryParams {
            sustain: Duration::ZERO,
            stagger: Duration::ZERO,
            backoff: vec![Duration::ZERO],
            ..RecoveryParams::default()
        }
    }
    #[cfg(not(test))]
    {
        RecoveryParams::default()
    }
}

/// The herdr-backed [`PtyBackend`].
pub struct HerdrBackend {
    ops: Arc<dyn HerdrOps>,
    supervisor: Arc<Supervisor>,
    /// `Arc` so `start`'s background thread can run restore reconciliation
    /// against the same persisted keymap `spawn_agent`/`attach` use.
    keymap: Arc<KeyMap>,
    /// `Arc` so a reconcile on `start`/`restart`'s thread can stop the bridges of a
    /// replaced server.
    bridges: Arc<Mutex<HashMap<SessionKey, Bridge>>>,
    /// `Arc` for the same reason as `keymap` — the reconcile thread adopts restored
    /// keys into the live set before the frontend attaches.
    live: Arc<Mutex<HashSet<SessionKey>>>,
    sink: Arc<dyn EmitSink>,
    /// Each attached pane's output sink. Every bridge start (attach, spawn, a
    /// resume, a writer restart) takes the sink registered for its key.
    outputs: OutputSinks,
    /// Set by `shutdown`; cleared by `spawn`/`restart`. Reflected as a
    /// non-`Connected` status so the frontend does not auto-respawn.
    explicit_stop: Arc<AtomicBool>,
    /// Who may use the server right now: the recovery controller's view.
    gate: Arc<RecoveryGate>,
    /// Talks to the recovery controller (deaths, restarts, cancels, focus).
    recovery: RecoveryHandle,
    /// The controller's driver, waiting for `start` to spawn it.
    recovery_driver: Mutex<Option<Parked>>,
    /// Shows the controller's OS notifications.
    notifier: Arc<dyn Notifier>,
    /// The memory probe the controller's headroom checks read.
    headroom: Arc<dyn Headroom>,
    /// Set when the backend is dropped; ends the adopted-server poller.
    shutting_down: Arc<AtomicBool>,
    /// Serialises reconciles (the start thread and a spawn-triggered one can race).
    reconcile_lock: Arc<Mutex<()>>,
    /// Which sessions' worktrees still exist; a replaced server's sessions in a
    /// removed worktree are dropped instead of resumed.
    worktree_exists: WorktreeFilter,
    /// One-shot boot-readiness latch: `start`'s background thread sets the bool + notifies
    /// once it has brought the server up and run the reconcile (on EVERY exit path, including
    /// a start failure — see [`ReadySignal`]). `attach` reads it (non-blocking via
    /// [`latch_is_set`]) and returns `NotConnected` until it is set, so a boot attach does
    /// not race ahead of the reconcile and bind to a stale pane id. Stays "ready" after the
    /// first signal (a later `restart` records fresh ids synchronously, so it needs no gate).
    ready: Arc<(Mutex<bool>, Condvar)>,
    /// The session ids herdr reports, fed from every `pane_list` the backend makes.
    store: Arc<ResumeStore>,
    /// One lock per session key: at most one agent launch per key at a time.
    spawn_locks: SpawnLocks,
    /// Resolves the launch of a session resumed without a pane asking for it.
    launcher: Option<LaunchResolver>,
    /// Claude's projects dir for transcript checks; `None` = the user's own.
    projects_dir: Option<PathBuf>,
    /// Front end for server-death reports (shared with the supervisor).
    watchdog: Arc<Watchdog>,
    /// The death channel's receiving end, taken once by `start` for the pump.
    death_rx: Mutex<Option<DeathRx>>,
    /// The last server status and stats the status loop learned.
    status_cache: status_loop::SharedStatusCache,
    /// The commit margin the recovery gate uses; the status loop slows down
    /// below it.
    base_margin_bytes: u64,
    /// The bridges' shared "the server is gone" verdict (see
    /// [`HerdrBackend::exit_authority`]).
    exit_verdict: ExitVerdict,
    /// One ordered input queue per session: `write`/`write_line` only enqueue.
    writers: Writers,
    /// Whether the herdr executable is known to be missing, and the message
    /// shown while it is.
    exe_missing: Arc<ExeMissing>,
    /// Each session's last measured size, for takeovers sent without one.
    sizes: SizeMemory,
}

/// When a bridge stream's end last proved the server gone: in which
/// generation, and when. Shared by every bridge so a server death that ends
/// them all costs one status probe, not one per bridge.
type ExitVerdict = Arc<Mutex<Option<(ops::Generation, Instant)>>>;

/// How long a "server gone" verdict answers further bridge ends without a new
/// probe (bounded, so a probe that only timed out does not suppress real pane
/// exits for the rest of the generation).
const EXIT_VERDICT_TTL: Duration = Duration::from_secs(10);

type SpawnLocks = Arc<Mutex<HashMap<SessionKey, Arc<Mutex<()>>>>>;
type DeathRx = Receiver<(ops::Generation, DeathEvent)>;

/// The death channel: a watchdog stamping reports with `keymap`'s generation,
/// and the receiving end for the pump.
fn death_channel(headroom: Arc<dyn Headroom>, keymap: &Arc<KeyMap>) -> (Arc<Watchdog>, DeathRx) {
    let (tx, rx) = mpsc::channel();
    let km = Arc::clone(keymap);
    let watchdog = Watchdog::new(tx, headroom, Arc::new(move || km.generation()));
    (Arc::new(watchdog), rx)
}

/// Handles a server death off the backend (the pump thread owns one).
struct DeathHandler {
    keymap: Arc<KeyMap>,
    live: Arc<Mutex<HashSet<SessionKey>>>,
    store: Arc<ResumeStore>,
    bridges: Arc<Mutex<HashMap<SessionKey, Bridge>>>,
    recovery: RecoveryHandle,
    sink: Arc<dyn EmitSink>,
    ops: Arc<dyn HerdrOps>,
    supervisor: Arc<Supervisor>,
    explicit_stop: Arc<AtomicBool>,
    status_cache: status_loop::SharedStatusCache,
    exe_missing: Arc<ExeMissing>,
}

impl DeathHandler {
    /// One reported death: drop it when its generation is no longer current (a
    /// duplicate or stale report; the first handled death bumps the generation),
    /// otherwise record it, note the death, forget the cached running status,
    /// hand it to the recovery controller with the bumped generation and push
    /// `daemon://status`.
    fn handle_death(&self, generation: ops::Generation, ev: DeathEvent) {
        if generation != self.keymap.generation() {
            log::info!(
                "herdr: ignoring a stale server death report (generation {}, current {})",
                generation.0,
                self.keymap.generation().0
            );
            return;
        }
        lifecycle::record(LifecycleEvent::ServerDeath((&ev).into()));
        self.note_server_death();
        // A pulled snapshot must not answer from a running status taken before
        // the death (the status loop stops probing while recovery runs).
        {
            let mut cache = self.status_cache.lock().unwrap();
            if let Some((s, _)) = cache.server.as_mut() {
                s.running = false;
            }
            cache.stats = DaemonStats::default();
        }
        self.recovery.death(ev, self.keymap.generation());
        emit_status(
            self.sink.as_ref(),
            self.explicit_stop.load(Ordering::SeqCst),
            &self.supervisor,
            &*self.ops,
            &self.live,
            &self.exe_missing,
            &|| self.recovery.snapshot(),
        );
    }

    /// The server died. Remember which live sessions are owed a resume (minus
    /// those whose agent the user had quit or that were terminated), bump the
    /// server generation once, stop every bridge and clear the live set. The
    /// keymap itself is kept: the next reconcile needs it to find the restored
    /// shells. Idempotent: the recovery set is a union and the bump is a no-op
    /// until a reconcile consumes it. The death pump is its only caller.
    fn note_server_death(&self) {
        let live: Vec<SessionKey> = self.live.lock().unwrap().iter().cloned().collect();
        let owed: Vec<SessionKey> = live
            .into_iter()
            .filter(|k| self.keymap.get(k).is_some() && !self.store.skip_resume(k))
            .collect();
        self.keymap.begin_recovery(&owed);
        let generation = self.keymap.note_death_bump();
        stop_all_bridges_in(&self.bridges);
        self.live.lock().unwrap().clear();
        log::info!(
            "herdr: server death noted; {} session(s) owed a resume, generation {}",
            owed.len(),
            generation.0
        );
    }
}

/// Stop every bridge (app shutdown / server stop / server death).
fn stop_all_bridges_in(bridges: &Mutex<HashMap<SessionKey, Bridge>>) {
    stop_bridges_except_in(bridges, &HashSet::new());
}

/// Stop every bridge but those of `keep` (a reconcile against a replaced
/// server keeps the sessions already running on the new one).
fn stop_bridges_except_in(
    bridges: &Mutex<HashMap<SessionKey, Bridge>>,
    keep: &HashSet<SessionKey>,
) {
    let drained: Vec<Bridge> = {
        let mut map = bridges.lock().unwrap();
        let stop: Vec<SessionKey> = map.keys().filter(|k| !keep.contains(*k)).cloned().collect();
        stop.iter().filter_map(|k| map.remove(k)).collect()
    };
    for mut b in drained {
        b.stop();
    }
}

/// Start (or replace) the I/O bridge for generation `gen` of `key`, delivering
/// to the output sink registered for `key` now. With none (no pane attached
/// yet) any old bridge is stopped and none is started: nobody would see its
/// frames, and the pane that attaches later starts its own. `authority` decides
/// whether the stream ending without a close is a real pane exit.
///
/// The old bridge keeps streaming until the new one has started and replaces
/// it, so a failed start leaves the pane its old bridge. The new bridge is
/// installed only while `gen` and the attach are still current (see
/// [`install_bridge_in`]).
#[allow(clippy::too_many_arguments)]
fn start_bridge_in(
    ops: &dyn HerdrOps,
    bridges: &Mutex<HashMap<SessionKey, Bridge>>,
    outputs: &OutputSinks,
    keymap: &KeyMap,
    authority: &ExitAuthority,
    key: &SessionKey,
    gen: u64,
    terminal_id: &str,
    cols: u16,
    rows: u16,
) -> Result<(), PtyBackendError> {
    let Some((attach_id, out)) = outputs.current(key) else {
        stop_bridge_in(bridges, key);
        return Ok(());
    };
    // herdr hands the terminal to the new bridge (`--takeover`), which can end
    // the old one's stream before it is stopped: not a pane exit.
    let muted = bridges.lock().unwrap().get(key).map(Bridge::mute_exit);
    let started = Bridge::start(
        ops,
        terminal_id,
        cols,
        rows,
        &key.project,
        &key.worktree,
        out,
        Arc::clone(authority),
    );
    let installed = match started {
        Ok(bridge) => install_bridge_in(
            bridges,
            keymap,
            outputs,
            key,
            gen,
            attach_id,
            bridge.for_session(gen),
        ),
        Err(e) => {
            if let Some(m) = muted {
                m.unmute();
            }
            return Err(e);
        }
    };
    if !installed {
        if let Some(m) = muted {
            m.unmute();
        }
    }
    Ok(())
}

/// Whether a bridge for generation `gen` of `key` and attach `attach_id` may be
/// installed: the session is still that generation and the attach is still
/// `key`'s. Checked under the bridges map lock by every installer, so an older
/// generation or a replaced attach never displaces a newer one's bridge.
fn bridge_still_wanted(
    keymap: &KeyMap,
    outputs: &OutputSinks,
    key: &SessionKey,
    gen: u64,
    attach_id: u64,
) -> bool {
    keymap.is_current_gen(key, gen) && outputs.current_id(key) == Some(attach_id)
}

/// Put `bridge`, started for generation `gen` of `key` and attach `attach_id`,
/// in `key`'s slot, unless either is no longer current (checked under the map
/// lock). Whether it was installed. The bridge it displaced, or the surplus
/// bridge itself, is stopped (kill, wait, join) only after the map lock is
/// released, so a `write` is never held up by its teardown.
fn install_bridge_in(
    bridges: &Mutex<HashMap<SessionKey, Bridge>>,
    keymap: &KeyMap,
    outputs: &OutputSinks,
    key: &SessionKey,
    gen: u64,
    attach_id: u64,
    bridge: Bridge,
) -> bool {
    let (installed, stop) = {
        let mut map = bridges.lock().unwrap();
        if bridge_still_wanted(keymap, outputs, key, gen, attach_id) {
            (true, map.insert(key.clone(), bridge))
        } else {
            (false, Some(bridge))
        }
    };
    drop(stop);
    installed
}

/// Stop and drop the bridge for `key`, if any.
/// The bridge leaves the map under the lock, but `stop` (kill, wait, join)
/// runs after the guard is dropped so a concurrent `write` is never held up
/// by a teardown.
fn stop_bridge_in(bridges: &Mutex<HashMap<SessionKey, Bridge>>, key: &SessionKey) {
    let taken = bridges.lock().unwrap().remove(key);
    if let Some(mut b) = taken {
        b.stop();
    }
}

/// Stop `key`'s bridge only while `stdin` is still its stdin: a restart may
/// already have replaced the bridge a failed write was aimed at. Whether it
/// was stopped.
fn stop_bridge_if_in(
    bridges: &Mutex<HashMap<SessionKey, Bridge>>,
    key: &SessionKey,
    stdin: &BridgeStdin,
) -> bool {
    let taken = {
        let mut map = bridges.lock().unwrap();
        let current = map
            .get(key)
            .is_some_and(|b| Arc::ptr_eq(&b.stdin_handle(), stdin));
        if current {
            map.remove(key)
        } else {
            None
        }
    };
    match taken {
        Some(mut b) => {
            b.stop();
            true
        }
        None => false,
    }
}

/// The per-session event `pty://{kind}/{project}/{worktree}` (e.g. `resume`,
/// `paste-refused`) a pane listens on.
fn session_event(kind: &str, key: &SessionKey) -> String {
    format!("pty://{kind}/{}/{}", key.project, key.worktree)
}

/// The backend parts one session's writer thread drives, for one session
/// generation: the bridge and pane of any other generation are out of reach.
struct WriterIo {
    key: SessionKey,
    /// The session generation (keymap `gen`) this writer serves.
    gen: u64,
    ops: Arc<dyn HerdrOps>,
    bridges: Arc<Mutex<HashMap<SessionKey, Bridge>>>,
    keymap: Arc<KeyMap>,
    gate: Arc<RecoveryGate>,
    outputs: OutputSinks,
    authority: ExitAuthority,
    /// Where the writer reports a paste it refused.
    sink: Arc<dyn EmitSink>,
}

impl WriterIo {
    /// This writer's pane: of the current server, and of its own generation.
    fn pane(&self) -> Option<keymap::HerdrPane> {
        self.keymap
            .get_current(&self.key)
            .filter(|p| p.gen == self.gen)
    }
}

impl writer::SessionIo for WriterIo {
    fn admit(&self) -> Result<(), PtyBackendError> {
        // Fail fast during recovery rather than timing out against a dead server
        // (a session already resumed this episode is usable).
        Ok(self.gate.admit_session(&self.key)?)
    }

    fn stdin(&self) -> Option<BridgeStdin> {
        // Only a clone of the handle is taken under the map lock; the write
        // happens after it is released.
        let bridges = self.bridges.lock().unwrap();
        bridges
            .get(&self.key)
            .filter(|b| b.session_gen() == self.gen)
            .map(|b| b.stdin_handle())
    }

    fn drop_bridge(&self, stdin: &BridgeStdin) {
        stop_bridge_if_in(&self.bridges, &self.key, stdin);
    }

    fn bridge(&self) -> Option<(BridgeStdin, (u16, u16))> {
        let bridges = self.bridges.lock().unwrap();
        bridges
            .get(&self.key)
            .filter(|b| b.session_gen() == self.gen)
            .map(|b| (b.stdin_handle(), b.size()))
    }

    fn resized(&self, stdin: &BridgeStdin, cols: u16, rows: u16) {
        let mut bridges = self.bridges.lock().unwrap();
        if let Some(b) = bridges.get_mut(&self.key) {
            if Arc::ptr_eq(&b.stdin_handle(), stdin) {
                b.set_size(cols, rows);
            }
        }
    }

    fn restart_bridge(
        &self,
        stdin: &BridgeStdin,
        cols: u16,
        rows: u16,
    ) -> Result<(), PtyBackendError> {
        // The attach the failed bridge streamed to. With none, nobody would see
        // a new bridge: just drop the failed one.
        let Some((attach_id, out)) = self.outputs.current(&self.key) else {
            stop_bridge_if_in(&self.bridges, &self.key, stdin);
            return Ok(());
        };
        // A bridge already replaced (attach, spawn) or removed (detach, kill) is
        // left alone: restarting it would undo that.
        if !stop_bridge_if_in(&self.bridges, &self.key, stdin) {
            return Ok(());
        }
        // A pane of a replaced server is absent (its id may now name another
        // pane), and so is one of another generation.
        let Some(pane) = self.pane() else {
            return Ok(());
        };
        let bridge = Bridge::start(
            &*self.ops,
            &pane.terminal_id,
            cols,
            rows,
            &self.key.project,
            &self.key.worktree,
            out,
            Arc::clone(&self.authority),
        )?
        .for_session(self.gen);
        // Installed only if, under the map lock, the slot is still empty (no
        // attach or spawn started a bridge meanwhile), the same attach is still
        // registered, and the session is still this generation. Otherwise the
        // new bridge is surplus and is stopped once the lock is released.
        let surplus = {
            let mut map = self.bridges.lock().unwrap();
            let still_ours = !map.contains_key(&self.key)
                && bridge_still_wanted(&self.keymap, &self.outputs, &self.key, self.gen, attach_id);
            if still_ours {
                map.insert(self.key.clone(), bridge);
                None
            } else {
                Some(bridge)
            }
        };
        drop(surplus);
        Ok(())
    }

    fn fallback(&self, data: &[u8]) -> Result<(), PtyBackendError> {
        // No live bridge: translate to CLI send-* ops. A pane of a replaced
        // server is absent (its id may now name another pane), and so is one of
        // another generation (the session was killed or replaced).
        let pane = self.pane().ok_or(PtyBackendError::NoSuchSession)?;
        for op in input::translate_input(data) {
            match op {
                HerdrInput::Text(text) => self.ops.pane_send_text(&pane.pane_id, &text)?,
                HerdrInput::Keys(keys) => self.ops.pane_send_keys(&pane.pane_id, &keys)?,
            }
        }
        Ok(())
    }

    fn fallback_paste(&self, block: &str) -> Result<(), PtyBackendError> {
        // A session that is gone is not told its paste was too large.
        let pane = self.pane().ok_or(PtyBackendError::NoSuchSession)?;
        // Too long for one CLI call: refused whole (never a part) and reported
        // to the pane. Never logs the pasted text.
        if let Some(cost) = paste::fallback_overage(block) {
            let limit = paste::FALLBACK_PASTE_LIMIT;
            log::warn!(
                "herdr writer [{}]: paste refused on the fallback: {cost} > {limit}",
                self.key
            );
            self.sink.emit(
                &session_event("paste-refused", &self.key),
                serde_json::json!({ "bytes": cost, "limit": limit }),
            );
            return Ok(());
        }
        // One `send-text` with the markers as real ESC bytes: `translate_input`
        // would turn each marker's ESC into an `esc` key and each `\r` into Enter.
        Ok(self.ops.pane_send_text(&pane.pane_id, block)?)
    }

    fn stdin_written(&self) {
        crate::latency_probe::on_stdin_written(&self.key);
    }

    fn write_line(&self, text: &str) -> Result<(), PtyBackendError> {
        // Refused during recovery (unless this session already resumed); the
        // dispatcher queues the line.
        self.gate.admit_session(&self.key)?;
        // A line queued for a session since killed or replaced is not sent to
        // its successor.
        let pane = self.pane().ok_or(PtyBackendError::NoSuchSession)?;
        // Empty text = submit-only (Enter at the agent's current prompt); no
        // `agent.prompt` content to send.
        if text.is_empty() {
            return self
                .ops
                .pane_send_keys(&pane.pane_id, &["enter".to_string()])
                .map_err(PtyBackendError::from);
        }
        // `agent.prompt` sends text + Enter as one ordered submission and owns submit
        // timing (bracketed-paste / Codex-on-Windows boundary), so Keitora's
        // best-effort `submit_delay_ms` is intentionally dropped on the herdr path.
        // `agent_blocked` / `agent_not_ready` already map to `NoSuchSession` (→ the
        // dispatcher queues the line) via `cli::agent_prompt_error_from`.
        self.ops
            .agent_prompt(&pane.pane_id, text)
            .map_err(cli::agent_prompt_error_from)
    }
}

/// The backend parts an agent launch needs, cloneable into a background thread.
#[derive(Clone)]
struct Spawner {
    ops: Arc<dyn HerdrOps>,
    keymap: Arc<KeyMap>,
    live: Arc<Mutex<HashSet<SessionKey>>>,
    store: Arc<ResumeStore>,
    bridges: Arc<Mutex<HashMap<SessionKey, Bridge>>>,
    sink: Arc<dyn EmitSink>,
    outputs: OutputSinks,
    locks: SpawnLocks,
    launcher: Option<LaunchResolver>,
    projects_dir: Option<PathBuf>,
    /// Whether a bridge stream that just ended is a real pane exit.
    exit_authority: ExitAuthority,
    /// Each session's last measured size (a resume takes over at it).
    sizes: SizeMemory,
}

impl Spawner {
    /// The launch lock of `key`.
    fn lock_for(&self, key: &SessionKey) -> Arc<Mutex<()>> {
        Arc::clone(self.locks.lock().unwrap().entry(key.clone()).or_default())
    }

    /// Which conversation `key` comes back in.
    fn resolve(&self, key: &SessionKey, cwd: &str, launch: &AgentLaunch) -> ResumeDecision {
        self.store.resolve(
            key,
            &ResolveCtx {
                cli: launch.cli,
                cwd: Path::new(cwd),
                registry_id: launch.resume_target.as_deref(),
                fresh: launch.fresh,
                projects_dir: self.projects_dir.as_deref(),
            },
        )
    }

    /// Launch `key`'s agent per `decision` under the key's lock.
    fn spawn_agent_with(
        &self,
        key: &SessionKey,
        cwd: String,
        cols: u16,
        rows: u16,
        launch: &AgentLaunch,
        decision: ResumeDecision,
    ) -> Result<(u64, ResumeResult, bool), PtyBackendError> {
        let lock = self.lock_for(key);
        let _held = lock.lock().unwrap();
        self.spawn_agent_locked(key, cwd, cols, rows, launch, decision)
    }

    /// Launch `key`'s agent (the caller holds the key's lock). Each attempt gets a
    /// fresh pane; an agent that exits during startup is retried with the next,
    /// weaker decision (resume, then continue, then fresh). Every other error
    /// closes the pane and is returned. Returns the session generation, the
    /// outcome, and whether an early exit made it fall back to a weaker decision.
    fn spawn_agent_locked(
        &self,
        key: &SessionKey,
        cwd: String,
        cols: u16,
        rows: u16,
        launch: &AgentLaunch,
        decision: ResumeDecision,
    ) -> Result<(u64, ResumeResult, bool), PtyBackendError> {
        let first = decision.clone();
        let mut decision = decision;
        let kind = agent::herdr_kind(launch.cli);
        let name = agent::agent_name(kind, key);
        // A PATH that resolves the configured exe (on unix always built on herdr's
        // own PATH; on Windows a bare `claude`/`codex` adds none and the pane
        // inherits herdr's), plus the user's own XDG dirs.
        let pane_env = env::pane_env(
            &launch.path,
            &self.ops.env().original_xdg,
            self.ops.env().path(),
        );
        let (id, gen) = loop {
            // Replace any live same-key pane (frees the old agent name so
            // `agent.start` can re-use it). Drop the keymap entry + live membership
            // NOW: if anything below fails before `record()` re-adds the key, a
            // stale entry would point `attach`/`write_line` at a dead pane.
            if let Some(old) = self.keymap.get(key) {
                stop_bridge_in(&self.bridges, key);
                let _ = self.ops.pane_close(&old.pane_id);
                self.keymap.remove(key);
                self.live.lock().unwrap().remove(key);
            }
            let id = self.ops.workspace_create(&cwd, &pane_env)?;
            // Give the fresh shell a brief, bounded moment to reach its prompt before
            // `agent.start` (which requires an interactive shell prompt).
            std::thread::sleep(Duration::from_millis(SHELL_READY_DELAY_MS));
            let args = agent::build_agent_args(launch.cli, launch.autonomy, &decision);
            match self
                .ops
                .agent_start(&name, kind, &id.pane_id, &args, AGENT_START_TIMEOUT_MS)
            {
                Ok(()) => {
                    // Keep the worktree path for restore reconciliation: herdr
                    // reassigns pane ids on a restore, so cwd is the stable key.
                    let gen = self.keymap.record(
                        key.clone(),
                        id.workspace_id.clone(),
                        id.pane_id.clone(),
                        id.terminal_id.clone(),
                        cwd.clone(),
                    );
                    break (id, gen);
                }
                Err(e) if agent::is_early_exit(&e) => {
                    let _ = self.ops.pane_close(&id.pane_id);
                    let Some(next) = agent::next_fallback(&decision) else {
                        return Err(e.into());
                    };
                    lifecycle::record(LifecycleEvent::ResumeOutcome {
                        key: key.clone(),
                        outcome: ResumeResult::Failed {
                            reason: format!(
                                "{decision:?} exited early: {}",
                                e.code().unwrap_or_default()
                            ),
                        },
                    });
                    decision = next;
                }
                Err(e) => {
                    // Close the leaked pane before returning the error.
                    let _ = self.ops.pane_close(&id.pane_id);
                    return Err(e.into());
                }
            }
        };
        self.live.lock().unwrap().insert(key.clone());
        // Start the output bridge NOW, so a pane already attached to this key (a
        // restart, a resume) streams from spawn. With no pane attached, none is
        // started: the pane's attach starts it.
        if let Err(e) = start_bridge_in(
            &*self.ops,
            &self.bridges,
            &self.outputs,
            &self.keymap,
            &self.exit_authority,
            key,
            gen,
            &id.terminal_id,
            cols.max(1),
            rows.max(1),
        ) {
            log::warn!(
                "herdr: spawn_agent bridge start failed for {}: {e}",
                key.worktree
            );
        }
        // Launched: nothing is owed a resume any more, whatever the decision was.
        self.keymap.resolve_recovered(key);
        let (result, word) = match &decision {
            ResumeDecision::Resume { id, .. } => {
                (ResumeResult::Resumed { id: id.clone() }, "resumed")
            }
            ResumeDecision::Continue => (ResumeResult::Continued, "continued"),
            ResumeDecision::Fresh => (ResumeResult::Fresh, "fresh"),
        };
        lifecycle::record(LifecycleEvent::ResumeOutcome {
            key: key.clone(),
            outcome: result.clone(),
        });
        let fell_back = decision != first;
        if fell_back || decision != ResumeDecision::Fresh {
            self.sink.emit(
                &session_event("resume", key),
                serde_json::json!({ "outcome": word, "fell_back": fell_back }),
            );
        }
        Ok((gen, result, fell_back))
    }

    /// Resume `key` with no pane asking for it, under the key's lock. `None`
    /// when the session already runs again (the user started it after it was
    /// owed a resume): its pane is left alone. `Terminated` when the user
    /// terminated it: nothing is started. The flag is `fell_back` (see
    /// [`Self::spawn_agent_locked`]).
    fn resume_session(
        &self,
        key: &SessionKey,
        decision: Option<ResumeDecision>,
    ) -> Result<Option<(ResumeResult, bool)>, PtyBackendError> {
        let lock = self.lock_for(key);
        let _held = lock.lock().unwrap();
        self.resume_locked(key, decision)
    }

    /// [`Self::resume_session`] with the key's lock already held.
    fn resume_locked(
        &self,
        key: &SessionKey,
        decision: Option<ResumeDecision>,
    ) -> Result<Option<(ResumeResult, bool)>, PtyBackendError> {
        // A terminated session stays down, however it got here: "resume all",
        // a focus, a terminate racing a resume already in flight, or a requeue.
        // Read under the key's lock, so a terminate that has set the flag wins.
        if self.store.is_terminated(key) {
            log::info!("herdr: {key} was terminated; not resuming it");
            self.keymap.resolve_recovered(key);
            return Err(PtyBackendError::Terminated);
        }
        // A pane of the current server generation was recorded after the
        // session was queued (the queue only holds sessions whose pane went
        // away): replacing it would close the user's own session.
        if self.keymap.get_current(key).is_some() && self.live.lock().unwrap().contains(key) {
            log::info!("herdr: {key} already runs again; not resuming it");
            self.keymap.resolve_recovered(key);
            return Ok(None);
        }
        let (cwd, launch) = self
            .launcher
            .as_ref()
            .and_then(|l| l(key))
            .ok_or(PtyBackendError::NoSuchSession)?;
        let decision = decision.unwrap_or_else(|| self.resolve(key, &cwd, &launch));
        let (cols, rows) = self.sizes.get(key).unwrap_or((RESUME_COLS, RESUME_ROWS));
        self.spawn_agent_locked(key, cwd, cols, rows, &launch, decision)
            .map(|(_, result, fell_back)| Some((result, fell_back)))
    }
}

/// The backend parts a reconcile needs, cloneable into `start`/`restart`'s
/// background threads.
#[derive(Clone)]
struct Reconciler {
    ops: Arc<dyn HerdrOps>,
    keymap: Arc<KeyMap>,
    live: Arc<Mutex<HashSet<SessionKey>>>,
    store: Arc<ResumeStore>,
    bridges: Arc<Mutex<HashMap<SessionKey, Bridge>>>,
    lock: Arc<Mutex<()>>,
    worktree_exists: WorktreeFilter,
}

impl Reconciler {
    /// Reconcile against whatever server is running now. When its identity
    /// cannot be read, only the live panes are re-adopted (non-destructive).
    fn reconcile_current(&self) -> Result<ReconcileOutcome, String> {
        let _serial = self.lock.lock().unwrap();
        let mut observed = None;
        for attempt in 0..INSTANCE_PROBE_ATTEMPTS {
            observed = self.ops.server_instance();
            if observed.is_some() {
                break;
            }
            if attempt + 1 < INSTANCE_PROBE_ATTEMPTS {
                let ms = if cfg!(test) {
                    1
                } else {
                    INSTANCE_PROBE_INTERVAL_MS
                };
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
        let Some(observed) = observed else {
            log::warn!("herdr: cannot identify the running server; re-adopting live panes only");
            reconcile::adopt_live_panes(&*self.ops, &self.keymap, &self.live, &self.store);
            return Ok(ReconcileOutcome::Adopted);
        };
        // A death already moved the generation on; otherwise a replaced server
        // starts the next one.
        let target = if self.keymap.take_death_bumped() {
            self.keymap.generation()
        } else {
            self.keymap.generation().next()
        };
        self.reconcile_locked(target, observed, false)
    }

    /// Reconcile against `observed` in `generation`, which the caller already
    /// moved to (a death, restart or boot bumped it); consumes that bump.
    fn reconcile_at(
        &self,
        generation: ops::Generation,
        observed: ops::ServerInstance,
    ) -> Result<ReconcileOutcome, String> {
        let _serial = self.lock.lock().unwrap();
        self.keymap.take_death_bumped();
        self.reconcile_locked(generation, observed, true)
    }

    /// The reconcile itself; the caller holds the reconcile lock. `restarted`:
    /// the recovery controller just (re)started the server or found it
    /// replaced (see [`reconcile::reconcile_restarted`]).
    fn reconcile_locked(
        &self,
        target: ops::Generation,
        observed: ops::ServerInstance,
        restarted: bool,
    ) -> Result<ReconcileOutcome, String> {
        let exists = |k: &SessionKey| (self.worktree_exists)(k);
        let ctx = ReconcileCtx {
            ops: &*self.ops,
            keymap: &self.keymap,
            live: &self.live,
            store: &self.store,
            bridges: &self.bridges,
            worktree_exists: &exists,
        };
        let outcome = if restarted {
            reconcile::reconcile_restarted(&ctx, target, observed)
        } else {
            reconcile::reconcile(&ctx, target, observed)
        };
        if let Ok(ReconcileOutcome::NewGeneration { to_resume }) = &outcome {
            if !to_resume.is_empty() {
                let keys: Vec<String> = to_resume.iter().map(|k| k.to_string()).collect();
                log::info!("herdr: sessions owed a resume: {}", keys.join(", "));
            }
        }
        outcome
    }
}

/// How long a caller that just handed the controller an event waits for its
/// phase to leave `Healthy`, so the frontend never reads a stale `Healthy`
/// right after a restart or a boot decision.
const PUBLISH_WAIT: Duration = Duration::from_secs(2);

/// The backend parts the recovery entry points and the controller's executor
/// need, cloneable into the boot thread and the driver thread.
#[derive(Clone)]
struct RecoveryParts {
    ops: Arc<dyn HerdrOps>,
    supervisor: Arc<Supervisor>,
    keymap: Arc<KeyMap>,
    live: Arc<Mutex<HashSet<SessionKey>>>,
    store: Arc<ResumeStore>,
    bridges: Arc<Mutex<HashMap<SessionKey, Bridge>>>,
    gate: Arc<RecoveryGate>,
    sink: Arc<dyn EmitSink>,
    explicit_stop: Arc<AtomicBool>,
    reconciler: Reconciler,
    spawner: Spawner,
    exe_missing: Arc<ExeMissing>,
    /// The controller's published payload (read only: the controller's own
    /// executor holds these parts, and must not keep the controller alive).
    recovery_payload: recovery::PayloadReader,
}

impl RecoveryParts {
    /// Probe the server and push `daemon://status` (and `daemon://recovery`
    /// when the probe changed the not-found state).
    fn emit_status(&self) {
        emit_status(
            self.sink.as_ref(),
            self.explicit_stop.load(Ordering::SeqCst),
            &self.supervisor,
            &*self.ops,
            &self.live,
            &self.exe_missing,
            &|| self.recovery_payload.snapshot(),
        );
    }

    /// Push `daemon://status` for a server already known not to answer, without
    /// probing it again.
    fn emit_status_down(&self) {
        push_status(
            self.sink.as_ref(),
            self.explicit_stop.load(Ordering::SeqCst),
            &self.supervisor,
            &*self.ops,
            None,
            &self.live,
            &self.exe_missing,
        );
    }

    /// Push the controller's current payload again, as the UI should see it
    /// now that the not-found state changed (the controller itself publishes
    /// only on its own transitions).
    fn emit_recovery(&self) {
        emit_recovery(
            self.sink.as_ref(),
            self.recovery_payload.snapshot(),
            &self.exe_missing,
        );
    }

    /// Resume `key` (admitted by the gate only while recovery is idle or `key`
    /// is among the sessions in flight); never starts the server. The launch is
    /// derived now, so a setting changed since the death applies. `None` when
    /// the session already runs again; the flag is whether an early exit made
    /// the launch fall back to a weaker decision.
    fn resume_session(
        &self,
        key: &SessionKey,
        decision: Option<ResumeDecision>,
    ) -> Result<Option<(ResumeResult, bool)>, PtyBackendError> {
        self.gate.admit(Some(key))?;
        self.supervisor.require_running(&*self.ops)?;
        self.spawner.resume_session(key, decision)
    }

    /// Every remembered session (minus those whose agent was quit or that were
    /// terminated) is owed a resume, and the next server is a new generation (a
    /// no-op bump when a death already moved it). Returns the generation the controller works in.
    fn mark_owed(&self) -> ops::Generation {
        let owed: Vec<SessionKey> = self
            .keymap
            .entries()
            .into_iter()
            .map(|(k, _)| k)
            .filter(|k| !self.store.skip_resume(k))
            .collect();
        self.keymap.begin_recovery(&owed);
        self.keymap.note_death_bump()
    }

    /// Wait (bounded) until the gate has left `Healthy`.
    fn wait_left_healthy(&self) {
        let deadline = std::time::Instant::now() + PUBLISH_WAIT;
        while self.gate.phase() == recovery::RecoveryPhase::Healthy
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Decide at launch what to do about herdr, and hand anything but adopting
    /// a known server to the recovery controller. `ready` is released once the
    /// gate reflects the decision (for an adopted server, once its panes are
    /// reconciled), so a boot attach never binds a stale pane.
    fn boot(&self, handle: &RecoveryHandle, ready: ReadySignal) -> BootClass {
        let sleep = |d: Duration| {
            std::thread::sleep(if cfg!(test) {
                Duration::from_millis(1)
            } else {
                d
            })
        };
        if self.exe_missing.login_deferred() {
            // The launch search found nothing and, being on the setup thread,
            // left out the login shell's `PATH`: finish it here, where waiting
            // for the user's shell delays no window.
            let discovery = self.ops.rediscover();
            log::info!(
                "herdr: searched again with the login PATH: {}",
                if discovery.found {
                    discovery.exe.display().to_string()
                } else {
                    "not found".into()
                }
            );
            // The status loop may already have reported the launch search.
            if self.exe_missing.searched_again(discovery) {
                self.emit_recovery();
            }
        }
        let (live, err) = boot::boot_probe(&*self.ops, boot::BOOT_PROBE_INTERVAL, &sleep);
        // herdr could not even be spawned: say so, with where it was looked for.
        let exe_missing = err.as_ref().is_some_and(ops::HerdrError::is_exe_missing);
        let persisted = self.keymap.instance();
        let has_sessions =
            !self.keymap.entries().is_empty() || !self.keymap.should_be_running().is_empty();
        let class = boot::classify_boot(persisted.as_ref(), &live, has_sessions);
        log::info!("herdr: at launch the server is {live:?} (remembered {persisted:?}): {class:?}");
        match class {
            BootClass::Indeterminate if exe_missing => {
                // Nothing can be started. The user installs or configures
                // herdr and presses Restart, which looks for it again.
                self.exe_missing.set(None);
                log::warn!(
                    "herdr: the herdr executable was not found at launch; searched: {}",
                    display_paths(&self.exe_missing.searched())
                );
                drop(ready);
                self.emit_status();
                self.emit_recovery();
            }
            BootClass::Indeterminate => {
                // Nothing is started against a server that may be alive; the
                // adopted-server watch reports it if it really is gone.
                log::warn!(
                    "herdr: could not reach herdr at launch: {}",
                    err.map_or_else(|| "no answer".into(), |e| e.to_string())
                );
                drop(ready);
                self.emit_status();
            }
            BootClass::Adopt => {
                self.supervisor.adopt(&*self.ops);
                self.emit_status();
                // The integration hooks record each agent's session id, which a
                // later resume relies on: install them before reconciling.
                self.supervisor.ensure_ready(&*self.ops);
                // Re-adopts the panes and remembers the live instance. A server
                // that turns out to be another one (its identity could not be
                // read a moment ago), or sessions still owed a resume from an
                // earlier run, go to the controller: it resumes them one at a
                // time through the gate instead of every pane spawning at once.
                let live_instance = match live {
                    boot::LiveServer::Present(i) => i,
                    _ => watchdog::UNKNOWN_INSTANCE,
                };
                let handover = match self.reconciler.reconcile_current() {
                    Ok(_) if self.keymap.should_be_running().is_empty() => None,
                    Ok(_) => Some((
                        self.keymap.generation(),
                        self.keymap.instance().unwrap_or(live_instance),
                    )),
                    Err(e) => {
                        log::warn!("herdr: reconciling the adopted server failed: {e}");
                        Some((self.mark_owed(), live_instance))
                    }
                };
                match handover {
                    None => {
                        drop(ready);
                        self.emit_status();
                    }
                    Some((generation, instance)) => {
                        log::info!("herdr: sessions are owed a resume at launch; recovering them");
                        handle.boot_recovery(generation, Some(instance), false, 0);
                        self.wait_left_healthy();
                        drop(ready);
                    }
                }
            }
            BootClass::Fresh | BootClass::DownNoSessions => {
                // Nothing is owed a resume (and an upgraded keymap without an
                // instance must not keep a stale resume set): start at once.
                let generation = self.keymap.note_death_bump();
                handle.boot_recovery(generation, None, true, 0);
                self.wait_left_healthy();
                drop(ready);
            }
            BootClass::RecoverDown => {
                // Through the memory check: Keitora may be launched mid-build.
                let generation = self.mark_owed();
                // The sessions now owed a resume: memory ample for all of them
                // lets recovery skip the sustained wait.
                let owed = self.keymap.should_be_running().len() as u32;
                handle.boot_recovery(generation, None, false, owed);
                self.wait_left_healthy();
                drop(ready);
            }
            BootClass::RecoverDifferentInstance => {
                let instance = match live {
                    boot::LiveServer::Present(i) => i,
                    _ => watchdog::UNKNOWN_INSTANCE,
                };
                let generation = self.mark_owed();
                handle.boot_recovery(generation, Some(instance), false, 0);
                self.wait_left_healthy();
                drop(ready);
            }
        }
        class
    }
}

/// The controller's hands: every command the recovery machine issues runs
/// through these backend parts.
struct BackendExecutor {
    parts: RecoveryParts,
    headroom: Arc<dyn Headroom>,
    notifier: Arc<dyn Notifier>,
}

impl BackendExecutor {
    /// Stop the running server when asked to (or when one still answers), then
    /// start a new one.
    fn stop_and_start(
        &self,
        stop_first: bool,
        how: ServerStartHow,
    ) -> Result<ops::ServerInstance, String> {
        let p = &self.parts;
        stop_all_bridges_in(&p.bridges);
        // A server reported dead may still answer (a false death): it is
        // stopped like any live one, so a restart never runs two servers.
        let still_up = !stop_first && p.ops.status_server().is_ok_and(|s| s.running);
        if still_up {
            log::warn!("herdr: the server still answers after it was reported down; stopping it");
        }
        if stop_first || still_up {
            p.supervisor
                .stop_and_wait(&*p.ops)
                .map_err(|e| e.to_string())?;
        }
        let instance = p
            .supervisor
            .start(&*p.ops, how)
            .map_err(|e| format!("herdr failed to start: {e}"))?;
        // The new pid is the frontend's restart signal.
        p.emit_status();
        Ok(instance)
    }
}

impl Executor for BackendExecutor {
    fn sample_headroom(&mut self) -> Option<crate::headroom::HeadroomSnapshot> {
        self.headroom.sample()
    }

    fn start_server(
        &mut self,
        stop_first: bool,
        how: ServerStartHow,
    ) -> Result<ops::ServerInstance, String> {
        let started = self.stop_and_start(stop_first, how);
        if started.is_err() {
            // Learn whether herdr itself is missing before the controller
            // publishes the failure, so that publish says so.
            let p = &self.parts;
            if p.exe_missing.observe(&p.ops.status_server()) {
                p.emit_recovery();
            }
        }
        started
    }

    fn reconcile(
        &mut self,
        generation: ops::Generation,
        observed: ops::ServerInstance,
    ) -> Result<ReconcileOutcome, String> {
        let p = &self.parts;
        p.supervisor.ensure_ready(&*p.ops);
        p.reconciler.reconcile_at(generation, observed)
    }

    fn resume_runner(&self) -> Arc<dyn ResumeRunner> {
        Arc::new(BackendResumeRunner {
            parts: self.parts.clone(),
        })
    }

    fn notify(&mut self, title: &str, body: &str) {
        self.notifier.notify(title, body);
    }

    fn emit(&mut self, payload: &recovery::RecoveryPayload) {
        // The controller's payload, with the not-found message while herdr is
        // missing: applied here on every publish, never stored, so it cannot
        // outlive the state nor be dropped by a later publish.
        let payload = &self.parts.exe_missing.overlay(payload.clone());
        if let Ok(value) = serde_json::to_value(payload) {
            self.parts.sink.emit(gate::RECOVERY_EVENT, value);
        }
        // Back to healthy: panes re-attach on the status push.
        if payload.phase == recovery::RecoveryPhase::Healthy {
            self.parts.emit_status();
        }
    }
}

/// Whether a failure text is herdr's agent-startup timeout: its JSON error
/// code, or the message it gives.
fn is_timeout_text(text: &str) -> bool {
    let text = text.to_lowercase();
    text.contains("\"code\":\"timeout\"") || text.contains("timed out waiting for agent startup")
}

/// Whether a resume error points at an overloaded machine. A herdr call that
/// ran out of time (`HerdrError::Timeout`) reaches here as `NotConnected`. So
/// does the running-server check when the server is down because the user
/// stopped or is restarting it (`stop_expected`): that is no sign of load.
fn is_load_related_error(e: &PtyBackendError, stop_expected: bool) -> bool {
    match e {
        PtyBackendError::NotConnected => !stop_expected,
        PtyBackendError::Other(m) => is_timeout_text(m),
        _ => false,
    }
}

/// Runs the recovery controller's resumes on its worker threads.
///
/// Locks: a resume takes only its key's spawn lock and, inside it, the short
/// map locks (`keymap`, `live`, `bridges`) that every other path also takes
/// last and releases at once. It never takes the reconcile lock, and a
/// reconcile never takes a spawn lock, so a resume running next to a reconcile
/// (or several resumes side by side) cannot deadlock.
struct BackendResumeRunner {
    parts: RecoveryParts,
}

impl ResumeRunner for BackendResumeRunner {
    fn resume(
        &self,
        key: &SessionKey,
        generation: ops::Generation,
        mode: ResumeMode,
    ) -> Option<ResumeReport> {
        let p = &self.parts;
        // A death or restart since the machine issued this resume: the session
        // belongs to the next episode, so touch neither its stored
        // conversation nor the (possibly new) server.
        if p.keymap.generation() != generation {
            log::info!(
                "herdr: skipping the resume of {key}: generation {} is over",
                generation.0
            );
            return None;
        }
        let decision = match mode {
            ResumeMode::Stored => None,
            ResumeMode::Fresh => {
                p.store.clear(key);
                Some(ResumeDecision::Fresh)
            }
        };
        let report = |outcome, load_related| {
            Some(ResumeReport {
                outcome,
                load_related,
            })
        };
        match p.resume_session(key, decision) {
            Ok(None) => report(ResumeOutcomeKind::AlreadyRunning, false),
            // Terminated by the user: nothing runs, so it neither settles nor
            // counts as a failure.
            Err(PtyBackendError::Terminated) => report(ResumeOutcomeKind::Cancelled, false),
            Ok(Some((result, fell_back))) => {
                let outcome = match result {
                    ResumeResult::Resumed { .. } => ResumeOutcomeKind::Resumed,
                    ResumeResult::Continued => ResumeOutcomeKind::Continued,
                    ResumeResult::Fresh => ResumeOutcomeKind::StartedFresh,
                    ResumeResult::Failed { reason } => ResumeOutcomeKind::Failed(reason),
                };
                let timed_out =
                    matches!(&outcome, ResumeOutcomeKind::Failed(r) if is_timeout_text(r));
                report(outcome, fell_back || timed_out)
            }
            Err(e) => {
                let load_related = is_load_related_error(&e, p.supervisor.stop_expected());
                let reason = match e {
                    PtyBackendError::NoSuchSession => "worktree no longer exists".to_string(),
                    e => e.to_string(),
                };
                log::warn!("herdr: resuming {key} failed: {reason}");
                lifecycle::record(LifecycleEvent::ResumeOutcome {
                    key: key.clone(),
                    outcome: ResumeResult::Failed {
                        reason: reason.clone(),
                    },
                });
                report(ResumeOutcomeKind::Failed(reason), load_related)
            }
        }
    }
}

/// Drop guard that flips the readiness latch and wakes any `attach` observer. Held by
/// `start`'s thread so the signal fires no matter which path the thread returns on.
struct ReadySignal(Arc<(Mutex<bool>, Condvar)>);

impl Drop for ReadySignal {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.0;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

/// Read the readiness latch without blocking. `attach` uses this to fail fast with
/// `NotConnected` (→ frontend retry) rather than parking a runtime thread pre-reconcile.
fn latch_is_set(ready: &(Mutex<bool>, Condvar)) -> bool {
    *ready.0.lock().unwrap()
}

/// Build the `daemon://status` [`StatusPayload`] from the backend's parts and a
/// server `status` the caller already has (`None`: unknown, reported as not
/// running). Free (no `&self`) so background threads can emit it from cloned `Arc`s,
/// and shared with [`HerdrBackend::status_payload_snapshot`] and the status loop so
/// the pushed event and the pulled command never drift.
fn build_status_payload(
    explicit_stop: bool,
    supervisor: &Supervisor,
    ops: &dyn HerdrOps,
    status: Option<ServerStatus>,
    live: &Mutex<HashSet<SessionKey>>,
    exe_missing: &ExeMissing,
) -> StatusPayload {
    // After an explicit Stop, report Disconnected even if some server still answers the
    // socket — otherwise the frontend treats it as Connected and auto-respawns the
    // sessions the user just stopped.
    if explicit_stop {
        return StatusPayload {
            state: "Disconnected",
            pid: Some(supervisor.restart_signal()),
            started_at: None,
            daemon_version: None,
            session_count: Some(0),
            reason: None,
            restartable: true,
            log_path: ops.env().log_path().to_string_lossy().into_owned(),
        };
    }
    let running = status.as_ref().map(|s| s.running).unwrap_or(false);
    StatusPayload {
        state: if running { "Connected" } else { "Disconnected" },
        // MUST change across server restarts (the frontend re-attach signal).
        pid: Some(supervisor.restart_signal()),
        started_at: None,
        daemon_version: status.as_ref().and_then(|s| s.protocol).map(|p| p as u32),
        session_count: Some(live.lock().unwrap().len()),
        // Why it is not running, when that is known: herdr was not found.
        reason: if running { None } else { exe_missing.message() },
        restartable: true,
        log_path: ops.env().log_path().to_string_lossy().into_owned(),
    }
}

/// `paths` on one line, for the log.
fn display_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Emit `payload` (the recovery controller's current one) on `daemon://recovery`
/// as the UI should see it (see [`ExeMissing::overlay`]). The one place a
/// payload the controller did not just publish is pushed from.
fn emit_recovery(
    sink: &dyn EmitSink,
    payload: recovery::RecoveryPayload,
    exe_missing: &ExeMissing,
) {
    if let Ok(value) = serde_json::to_value(exe_missing.overlay(payload)) {
        sink.emit(gate::RECOVERY_EVENT, value);
    }
}

/// Learn from the result of one herdr call whether the herdr executable is
/// there (see [`ExeMissing::observe`]) and, when that changed, push the recovery
/// payload again: it carries the not-found reason code, which would otherwise
/// stay as it was until the controller's next transition. Every caller that
/// observes goes through here.
fn observe_exe<T>(
    sink: &dyn EmitSink,
    exe_missing: &ExeMissing,
    result: &Result<T, ops::HerdrError>,
    recovery: &dyn Fn() -> recovery::RecoveryPayload,
) {
    if exe_missing.observe(result) {
        emit_recovery(sink, recovery(), exe_missing);
    }
}

/// Emit a `daemon://status` snapshot built from a server `status` the caller
/// already has (`None`: not answering).
fn push_status(
    sink: &dyn EmitSink,
    explicit_stop: bool,
    supervisor: &Supervisor,
    ops: &dyn HerdrOps,
    status: Option<ServerStatus>,
    live: &Mutex<HashSet<SessionKey>>,
    exe_missing: &ExeMissing,
) {
    if let Ok(value) = serde_json::to_value(build_status_payload(
        explicit_stop,
        supervisor,
        ops,
        status,
        live,
        exe_missing,
    )) {
        sink.emit("daemon://status", value);
    }
}

/// Emit a fresh `daemon://status` snapshot through `sink`. The herdr
/// backend is otherwise pull-only (the frontend would only resync every 30s), so this
/// push is what makes a pane re-attach promptly once the reconcile corrects its ids.
/// The status call it makes also keeps the not-found state current, and a change
/// of that state goes out on `daemon://recovery` too (`recovery` is the
/// controller's current payload).
fn emit_status(
    sink: &dyn EmitSink,
    explicit_stop: bool,
    supervisor: &Supervisor,
    ops: &dyn HerdrOps,
    live: &Mutex<HashSet<SessionKey>>,
    exe_missing: &ExeMissing,
    recovery: &dyn Fn() -> recovery::RecoveryPayload,
) {
    let status = ops.status_server();
    observe_exe(sink, exe_missing, &status, recovery);
    push_status(
        sink,
        explicit_stop,
        supervisor,
        ops,
        status.ok(),
        live,
        exe_missing,
    );
}

/// Pure liveness check used by `attach`: is `pane_id` among the server's
/// live panes? Factored out so the attach decision is unit-testable without a live herdr.
fn pane_is_live(panes: &[PaneInfo], pane_id: &str) -> bool {
    panes.iter().any(|p| p.pane_id == pane_id)
}

/// The row count herdr lists for `pane_id`, when it reports one.
fn pane_viewport_rows(panes: &[PaneInfo], pane_id: &str) -> Option<u16> {
    panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .and_then(|p| p.viewport_rows)
}

/// Sample CPU/memory/uptime for `pid` via the shared `sysinfo::System`.
/// Returns `running: false` / `version: None`; the caller overlays those. A `None` pid or
/// an exited process yields null stats (the card then shows `—`). The pure math lives in
/// [`super::normalize_cpu`] / [`super::uptime_from`]; this is the thin sysinfo shell.
fn sample_process_stats(sys: &mut sysinfo::System, pid: Option<u32>) -> DaemonStats {
    let Some(pid) = pid else {
        return DaemonStats::default();
    };
    let spid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[spid]),
        true, // remove dead
        sysinfo::ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory(),
    );
    let Some(proc_) = sys.process(spid) else {
        // pid known but the process is gone (exited/reused): report the pid, null stats.
        return DaemonStats {
            pid: Some(pid),
            ..DaemonStats::default()
        };
    };
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    DaemonStats {
        running: false,
        pid: Some(pid),
        uptime_secs: Some(super::uptime_from(now, proc_.start_time())),
        cpu_percent: Some(super::normalize_cpu(proc_.cpu_usage(), cores)),
        mem_bytes: Some(proc_.memory()),
        version: None,
    }
}

impl HerdrBackend {
    /// Construct from the app handle: load the user config, resolve the herdr exe +
    /// isolated env. Fails (so the caller can fall back to `UnavailableBackend`)
    /// when the user config cannot be loaded or the isolated env cannot be set up
    /// (its directories, its config file, a socket path that is too long). A
    /// herdr executable that is not found is not a failure here: the backend
    /// starts, reports it as not found, and looks again on `restart`.
    pub fn new(app: tauri::AppHandle) -> Result<Self, PtyBackendError> {
        let cfg = keitora_tasks::UserConfig::load()
            .map_err(|e| PtyBackendError::Other(format!("loading user config: {e}")))?;
        let env = HerdrEnv::resolve(&cfg.pty)?;
        let launcher: LaunchResolver = Arc::new(crate::host::resolve_agent_launch);
        let params = RecoveryParams::from_config(&cfg.pty.herdr.recovery);
        let notifier: Arc<dyn Notifier> = Arc::new(app.clone());
        Ok(Self::with_parts(env, Arc::new(app), Some(launcher))
            .with_worktree_filter(Arc::new(crate::host::worktree_exists))
            .with_recovery_params(params)
            .with_notifier(notifier))
    }

    /// Construct from an explicit env + emit sink (the headless path), driving the
    /// real `herdr` binary.
    pub fn with_parts(
        env: HerdrEnv,
        sink: Arc<dyn EmitSink>,
        launcher: Option<LaunchResolver>,
    ) -> Self {
        // A resume DB that cannot be opened degrades resume; it never stops the backend.
        let store = Arc::new(ResumeStore::open_or_degraded(&env.resume_db_path()));
        Self::with_ops(Arc::new(RealHerdr::new(env)), sink, store, launcher)
    }

    /// Construct over any [`HerdrOps`] (tests pass a scripted double).
    pub fn with_ops(
        ops: Arc<dyn HerdrOps>,
        sink: Arc<dyn EmitSink>,
        store: Arc<ResumeStore>,
        launcher: Option<LaunchResolver>,
    ) -> Self {
        // Unit tests never look at the user's real Claude transcripts.
        let projects_dir = cfg!(test).then(|| ops.env().home.join("claude-projects"));
        // `with_path` loads any prior mapping so panes survive an app restart.
        let keymap = Arc::new(KeyMap::with_path(ops.env().keymap_path()));
        // A session pending recovery still shows its old (restored) shell; that
        // shell's session ref must not overwrite the record the resume will use.
        let pending = Arc::clone(&keymap);
        store.set_skip_filter(Arc::new(move |k: &SessionKey| {
            pending.is_pending_recovery(k)
        }));
        let headroom = default_headroom();
        let (watchdog, death_rx) = death_channel(Arc::clone(&headroom), &keymap);
        let (recovery, gate, parked) = recovery::controller(default_recovery_params());
        HerdrBackend {
            exe_missing: Arc::new(ExeMissing::new(ops.env().discovery.clone())),
            ops,
            supervisor: Arc::new(
                Supervisor::with_headroom(Arc::clone(&headroom))
                    .with_watchdog(Arc::clone(&watchdog)),
            ),
            keymap,
            bridges: Arc::new(Mutex::new(HashMap::new())),
            live: Arc::new(Mutex::new(HashSet::new())),
            gate,
            recovery,
            recovery_driver: Mutex::new(Some(parked)),
            notifier: Arc::new(NoopNotifier),
            headroom,
            shutting_down: Arc::new(AtomicBool::new(false)),
            sink,
            outputs: OutputSinks::default(),
            explicit_stop: Arc::new(AtomicBool::new(false)),
            reconcile_lock: Arc::new(Mutex::new(())),
            worktree_exists: Arc::new(|_: &SessionKey| true),
            ready: Arc::new((Mutex::new(false), Condvar::new())),
            store,
            spawn_locks: Arc::new(Mutex::new(HashMap::new())),
            launcher,
            projects_dir,
            watchdog,
            death_rx: Mutex::new(Some(death_rx)),
            status_cache: Arc::default(),
            base_margin_bytes: default_recovery_params().base_margin,
            exit_verdict: Arc::default(),
            writers: Writers::default(),
            sizes: SizeMemory::default(),
        }
    }

    /// Sample memory headroom from `headroom` (tests inject a fake). Rebuilds the
    /// supervisor and the death channel, so call it before `start`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_headroom(mut self, headroom: Arc<dyn Headroom>) -> Self {
        let (watchdog, death_rx) = death_channel(Arc::clone(&headroom), &self.keymap);
        self.supervisor = Arc::new(
            Supervisor::with_headroom(Arc::clone(&headroom)).with_watchdog(Arc::clone(&watchdog)),
        );
        self.watchdog = watchdog;
        self.death_rx = Mutex::new(Some(death_rx));
        self.headroom = headroom;
        self
    }

    /// Recover with `params` instead of the defaults. Rebuilds the controller,
    /// so call it before `start`.
    pub fn with_recovery_params(mut self, params: RecoveryParams) -> Self {
        self.base_margin_bytes = params.base_margin;
        let (recovery, gate, parked) = recovery::controller(params);
        self.recovery = recovery;
        self.gate = gate;
        self.recovery_driver = Mutex::new(Some(parked));
        self
    }

    /// Show the controller's notifications through `notifier`.
    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Spawn the recovery controller's driver thread (once).
    fn start_recovery_driver(&self) {
        let Some(parked) = self.recovery_driver.lock().unwrap().take() else {
            return;
        };
        if let Err(e) = parked.spawn(Box::new(self.executor())) {
            log::error!("herdr: could not start the recovery controller: {e}");
        }
    }

    fn executor(&self) -> BackendExecutor {
        BackendExecutor {
            parts: self.recovery_parts(),
            headroom: Arc::clone(&self.headroom),
            notifier: Arc::clone(&self.notifier),
        }
    }

    /// Swap in a recovery handle with no driver, returning what it is sent.
    #[cfg(test)]
    fn with_recording_recovery(mut self) -> (Self, mpsc::Receiver<recovery::machine::Event>) {
        let (handle, rx) = recovery::recording_handle();
        self.recovery = handle;
        (self, rx)
    }

    /// Adjust the supervisor (timings, killer). Call before anything shares it.
    #[cfg(test)]
    fn tune_supervisor(mut self, f: impl FnOnce(Supervisor) -> Supervisor) -> Self {
        let shared = std::mem::replace(&mut self.supervisor, Arc::new(Supervisor::new()));
        let sup = Arc::try_unwrap(shared)
            .ok()
            .expect("the supervisor is not shared yet");
        self.supervisor = Arc::new(f(sup));
        self
    }

    fn death_handler(&self) -> DeathHandler {
        DeathHandler {
            keymap: Arc::clone(&self.keymap),
            live: Arc::clone(&self.live),
            store: Arc::clone(&self.store),
            bridges: Arc::clone(&self.bridges),
            recovery: self.recovery.clone(),
            sink: Arc::clone(&self.sink),
            ops: Arc::clone(&self.ops),
            supervisor: Arc::clone(&self.supervisor),
            explicit_stop: Arc::clone(&self.explicit_stop),
            status_cache: Arc::clone(&self.status_cache),
            exe_missing: Arc::clone(&self.exe_missing),
        }
    }

    /// Start the single death pump (once): it handles every reported server death
    /// and ends when the backend's watchdog is dropped.
    fn start_death_pump(&self) {
        let Some(rx) = self.death_rx.lock().unwrap().take() else {
            return;
        };
        let handler = self.death_handler();
        std::thread::spawn(move || {
            for (generation, ev) in rx {
                handler.handle_death(generation, ev);
            }
        });
    }

    fn spawner(&self) -> Spawner {
        Spawner {
            ops: Arc::clone(&self.ops),
            keymap: Arc::clone(&self.keymap),
            live: Arc::clone(&self.live),
            store: Arc::clone(&self.store),
            bridges: Arc::clone(&self.bridges),
            sink: Arc::clone(&self.sink),
            outputs: self.outputs.clone(),
            locks: Arc::clone(&self.spawn_locks),
            launcher: self.launcher.clone(),
            projects_dir: self.projects_dir.clone(),
            exit_authority: self.exit_authority(),
            sizes: self.sizes.clone(),
        }
    }

    /// Close `key`'s session of generation `gen` and forget it. Gen-gated: a
    /// stale generation after a respawn is a no-op `Ok`. The caller checked the
    /// gate.
    fn close_session(&self, key: &SessionKey, gen: u64) -> Result<(), PtyBackendError> {
        if !self.keymap.is_current_gen(key, gen) {
            return Ok(());
        }
        let pane = self.keymap.get_current(key);
        // The session is gone for good: its writer ends once its queue is done.
        drop(self.writers.remove(key));
        self.stop_bridge(key);
        if let Some(p) = pane {
            match self.ops.pane_close(&p.pane_id) {
                // Already gone is the desired post-state.
                Ok(()) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.keymap.remove(key);
        self.live.lock().unwrap().remove(key);
        Ok(())
    }

    /// Resume `key`'s agent with no pane asking for it: its worktree and launch
    /// come from the launch resolver, and its conversation from `decision` or, when
    /// `None`, the resume store. Returns the final outcome (never `Failed`), or
    /// `None` when the session already runs again; an unresolvable launch is
    /// `NoSuchSession` and a terminated session `Terminated`. Admitted by the gate only while recovery is idle or `key`
    /// is the session being resumed; it never starts the server.
    // The recovery controller takes the same path through its executor.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn resume_session(
        &self,
        key: &SessionKey,
        decision: Option<ResumeDecision>,
    ) -> Result<Option<ResumeResult>, PtyBackendError> {
        self.recovery_parts()
            .resume_session(key, decision)
            .map(|resumed| resumed.map(|(result, _)| result))
    }

    /// The status loop over this backend's parts (sharing its status cache).
    fn status_loop(&self) -> status_loop::StatusLoop {
        status_loop::StatusLoop::new(status_loop::StatusLoopDeps {
            ops: Arc::clone(&self.ops),
            gate: Arc::clone(&self.gate) as Arc<dyn ServerGate>,
            headroom: Arc::clone(&self.headroom),
            base_margin_bytes: self.base_margin_bytes,
            store: Arc::clone(&self.store),
            keymap: Arc::clone(&self.keymap),
            recovery: self.recovery.clone(),
            sink: Arc::clone(&self.sink),
            explicit_stop: Arc::clone(&self.explicit_stop),
            supervisor: Arc::clone(&self.supervisor),
            live: Arc::clone(&self.live),
            watchdog: Arc::clone(&self.watchdog),
            cache: Arc::clone(&self.status_cache),
            pid_sample: None,
            exe_missing: Arc::clone(&self.exe_missing),
            boot_ready: {
                let ready = Arc::clone(&self.ready);
                Arc::new(move || latch_is_set(&ready))
            },
        })
    }

    /// Run the status loop on its own thread until the backend is dropped.
    fn spawn_status_loop(&self) -> Option<std::thread::JoinHandle<()>> {
        let status_loop = self.status_loop();
        let stop = Arc::clone(&self.shutting_down);
        std::thread::Builder::new()
            .name("herdr-status-loop".into())
            .spawn(move || status_loop.run(stop))
            .map_err(|e| log::error!("herdr: could not start the status loop: {e}"))
            .ok()
    }

    fn recovery_parts(&self) -> RecoveryParts {
        RecoveryParts {
            ops: Arc::clone(&self.ops),
            supervisor: Arc::clone(&self.supervisor),
            keymap: Arc::clone(&self.keymap),
            live: Arc::clone(&self.live),
            store: Arc::clone(&self.store),
            bridges: Arc::clone(&self.bridges),
            gate: Arc::clone(&self.gate),
            sink: Arc::clone(&self.sink),
            explicit_stop: Arc::clone(&self.explicit_stop),
            reconciler: self.reconciler(),
            spawner: self.spawner(),
            exe_missing: Arc::clone(&self.exe_missing),
            recovery_payload: self.recovery.reader(),
        }
    }

    /// Use `f` to decide whether a session's worktree still exists (default:
    /// every worktree exists).
    pub fn with_worktree_filter(mut self, f: WorktreeFilter) -> Self {
        self.worktree_exists = f;
        self
    }

    fn reconciler(&self) -> Reconciler {
        Reconciler {
            ops: Arc::clone(&self.ops),
            keymap: Arc::clone(&self.keymap),
            live: Arc::clone(&self.live),
            store: Arc::clone(&self.store),
            bridges: Arc::clone(&self.bridges),
            lock: Arc::clone(&self.reconcile_lock),
            worktree_exists: Arc::clone(&self.worktree_exists),
        }
    }

    /// Reconcile the keymap against the running server instance: re-adopt its
    /// panes when it is the same server, otherwise close the restored shells and
    /// report the sessions owed a resume. Serialised with every other reconcile.
    // Production reconciles run on the start/restart threads through
    // `Reconciler`; this entry point serves tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reconcile_current(&self) -> Result<ReconcileOutcome, String> {
        self.reconciler().reconcile_current()
    }

    /// Whether `start`'s background thread has brought the server up + run the first
    /// reconcile (non-blocking read). `attach` fails fast with `NotConnected` until then.
    fn is_ready(&self) -> bool {
        latch_is_set(&self.ready)
    }

    /// Start (or replace) the I/O bridge for generation `gen` of `key` (none
    /// while no pane is attached; see [`start_bridge_in`]).
    fn start_bridge(
        &self,
        key: &SessionKey,
        gen: u64,
        terminal_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), PtyBackendError> {
        start_bridge_in(
            &*self.ops,
            &self.bridges,
            &self.outputs,
            &self.keymap,
            &self.exit_authority(),
            key,
            gen,
            terminal_id,
            cols,
            rows,
        )
    }

    /// A bridge stream that ends without a close is a real pane exit only while
    /// the gate is healthy and the server still answers; otherwise the server
    /// died (perhaps before the death was handled) and the session will be
    /// resumed, not ended. A death already reported answers without a probe,
    /// and a probe that finds the server gone answers every bridge ending in
    /// the same generation (for a while): the bridges wait for one probe
    /// instead of each spawning their own.
    fn exit_authority(&self) -> ExitAuthority {
        let gate = Arc::clone(&self.gate);
        let ops = Arc::clone(&self.ops);
        let watchdog = Arc::clone(&self.watchdog);
        let keymap = Arc::clone(&self.keymap);
        let verdict = Arc::clone(&self.exit_verdict);
        Arc::new(move || {
            if gate.admit(None).is_err() || watchdog.death_reported() {
                return false;
            }
            let generation = keymap.generation();
            // Held across the probe: concurrent bridge ends share its answer.
            let mut gone = verdict.lock().unwrap();
            if gone.is_some_and(|(g, at)| g == generation && at.elapsed() < EXIT_VERDICT_TTL) {
                return false;
            }
            let running = ops.status_server().is_ok_and(|s| s.running);
            if !running {
                *gone = Some((generation, Instant::now()));
            }
            running
        })
    }

    /// What the writer of generation `gen` of `key` drives.
    fn writer_io(&self, key: &SessionKey, gen: u64) -> WriterIo {
        WriterIo {
            key: key.clone(),
            gen,
            ops: Arc::clone(&self.ops),
            bridges: Arc::clone(&self.bridges),
            keymap: Arc::clone(&self.keymap),
            gate: Arc::clone(&self.gate),
            outputs: self.outputs.clone(),
            authority: self.exit_authority(),
            sink: Arc::clone(&self.sink),
        }
    }

    /// Enqueue `job` on the writer of `key`'s current session generation
    /// (started on first use, replacing the writer of an older generation).
    /// Never blocks on I/O. No session mapped: `NoSuchSession`.
    fn enqueue(&self, key: &SessionKey, job: WriteJob) -> Result<(), PtyBackendError> {
        let gen = self.session_gen(key)?;
        self.enqueue_at(key, gen, job)
    }

    /// The generation of `key`'s current session. No session mapped: `NoSuchSession`.
    fn session_gen(&self, key: &SessionKey) -> Result<u64, PtyBackendError> {
        self.keymap
            .get(key)
            .map(|p| p.gen)
            .ok_or(PtyBackendError::NoSuchSession)
    }

    /// [`Self::enqueue`] for a generation already looked up.
    fn enqueue_at(&self, key: &SessionKey, gen: u64, job: WriteJob) -> Result<(), PtyBackendError> {
        self.writers
            .send(key, gen, job, || self.writer_io(key, gen))
    }

    /// Stop and drop the bridge for `key`, if any.
    fn stop_bridge(&self, key: &SessionKey) {
        stop_bridge_in(&self.bridges, key);
    }

    /// Stop every bridge (app shutdown / server stop).
    fn stop_all_bridges(&self) {
        stop_all_bridges_in(&self.bridges);
    }

    /// One summary per keymap entry whose pane is in `panes`, with the busy
    /// hint read from the pane's agent status.
    fn summaries_from(&self, panes: &[PaneInfo]) -> Vec<SessionSummary> {
        let by_id: HashMap<&str, &PaneInfo> =
            panes.iter().map(|p| (p.pane_id.as_str(), p)).collect();
        self.keymap
            .entries()
            .into_iter()
            .filter_map(|(key, pane)| {
                let p = by_id.get(pane.pane_id.as_str())?;
                Some(SessionSummary {
                    key,
                    gen: pane.gen,
                    state_hint: state_hint_for_pane(p),
                })
            })
            .collect()
    }
}

impl Drop for HerdrBackend {
    fn drop(&mut self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // The driver holds a sender of its own channel (for its resume
        // workers), so dropping the handles no longer ends it.
        self.recovery.shutdown();
    }
}

/// A pane's `state_hint` from the agent status `pane list` reports. Pure, so
/// the `list_sessions` busy-hint mapping is unit-testable without a live herdr:
/// a pane with no agent status (or one that is idle/done/unknown) is `None`;
/// `working` / `blocked` are `Some(HINT_BUSY)` (see
/// [`agent::agent_status_to_state_hint`]).
fn state_hint_for_pane(pane: &PaneInfo) -> Option<String> {
    pane.agent_status
        .as_deref()
        .and_then(|s| agent::agent_status_to_state_hint(s, false))
}

impl PtyBackend for HerdrBackend {
    fn start(&self) {
        self.start_death_pump();
        // The controller runs before anything can hand it a boot decision.
        self.start_recovery_driver();
        // The one polling thread (status, stats, liveness of an adopted server,
        // resume-store freshness). Started first so it runs even when the server
        // never comes up. Unit tests drive it explicitly instead, so their fake
        // herdr call logs stay deterministic.
        if !cfg!(test) {
            self.spawn_status_loop();
        }
        // Non-blocking: classify the server's state off the setup thread and
        // adopt it or hand it to the controller. The readiness latch is released
        // on every path (it is a drop guard), so a boot `attach` never waits on a
        // failed start.
        let parts = self.recovery_parts();
        let handle = self.recovery.clone();
        let ready = ReadySignal(Arc::clone(&self.ready));
        std::thread::spawn(move || {
            parts.boot(&handle, ready);
        });
    }

    fn spawn(
        &self,
        key: SessionKey,
        cwd: String,
        _db_path: String,
        size: Option<(u16, u16)>,
        command: Option<String>,
    ) -> Result<u64, PtyBackendError> {
        // A new pane has no rows of its own in herdr to fall back on.
        let (cols, rows) = self.sizes.takeover(&key, size, None);
        // A spawn never starts the server: only recovery does.
        if !self.is_ready() {
            return Err(PtyBackendError::NotConnected);
        }
        self.gate.admit(None)?;
        // A terminated session stays down until it is launched again.
        if self.store.is_terminated(&key) {
            return Err(PtyBackendError::Terminated);
        }
        self.supervisor.require_running(&*self.ops)?;
        // Once per process; non-fatal; opt-out via KEITORA_HERDR_NO_INTEGRATION.
        self.supervisor.ensure_ready(&*self.ops);
        self.explicit_stop.store(false, Ordering::SeqCst);

        // Replace any live same-key pane (spawn replaces).
        if let Some(old) = self.keymap.get(&key) {
            self.stop_bridge(&key);
            let _ = self.ops.pane_close(&old.pane_id);
        }

        let id = self.ops.workspace_create(
            &cwd,
            &env::pane_shell_env(&self.ops.env().original_xdg, self.ops.env().path()),
        )?;
        if let Some(launch) = command {
            // The pane is an interactive shell; type the launch line into it.
            self.ops.pane_run(&id.pane_id, &[launch.as_str()])?;
        }
        let terminal_id = id.terminal_id.clone();
        let gen = self.keymap.record(
            key.clone(),
            id.workspace_id,
            id.pane_id,
            id.terminal_id,
            cwd,
        );
        self.live.lock().unwrap().insert(key.clone());
        // Running again: nothing is owed a resume any more.
        self.keymap.resolve_recovered(&key);
        // Start the output bridge NOW so output streams immediately after spawn to a
        // pane already attached to this key (a restart keeps its attach). With no
        // attach yet none is started: the pane attaches after the spawn, and that
        // attach starts the bridge (one control process, not two).
        if let Err(e) = self.start_bridge(&key, gen, &terminal_id, cols.max(1), rows.max(1)) {
            log::warn!("herdr: spawn bridge start failed for {}: {e}", key.worktree);
        }
        Ok(gen)
    }

    fn spawn_agent(
        &self,
        key: SessionKey,
        cwd: String,
        _db_path: String,
        size: Option<(u16, u16)>,
        launch: AgentLaunch,
    ) -> Result<u64, PtyBackendError> {
        // A new pane has no rows of its own in herdr to fall back on.
        let (cols, rows) = self.sizes.takeover(&key, size, None);
        // A spawn never starts the server: only recovery does.
        if !self.is_ready() {
            return Err(PtyBackendError::NotConnected);
        }
        self.gate.admit(None)?;
        // A terminated session stays down until it is launched again.
        if self.store.is_terminated(&key) {
            return Err(PtyBackendError::Terminated);
        }
        self.supervisor.require_running(&*self.ops)?;
        // Once per process; non-fatal; opt-out via KEITORA_HERDR_NO_INTEGRATION. This is
        // the PRIMARY launch path (`host::spawn_pty` routes the default agent launch
        // here, not through `spawn`), so the integration install that powers herdr-owned
        // restore must run here too — not only in `spawn`.
        self.supervisor.ensure_ready(&*self.ops);
        self.explicit_stop.store(false, Ordering::SeqCst);

        // `agent.start` launches the agent by kind, resuming the conversation the
        // resume store picks. `agent_not_ready` / "blocked during startup" is already
        // folded to `Ok` (launched).
        let spawner = self.spawner();
        let decision = spawner.resolve(&key, &cwd, &launch);
        let (gen, _, _) = spawner.spawn_agent_with(&key, cwd, cols, rows, &launch, decision)?;
        Ok(gen)
    }

    fn attach(
        &self,
        key: SessionKey,
        size: Option<(u16, u16)>,
        out: Arc<dyn OutputSink>,
    ) -> Result<AttachResult, PtyBackendError> {
        // A measured size is remembered even when the attach is refused below.
        if let Some(measured) = size {
            self.sizes.record(&key, measured);
        }
        // First: during recovery the pane must hear "recovering", not
        // `NotConnected` (which the frontend eventually answers with a spawn),
        // unless its session already resumed this episode.
        self.gate.admit_session(&key)?;
        // On boot the frontend attaches the instant a pane mounts, which can race ahead of
        // `start`'s server start + reconcile — binding the bridge to a pane id the restored
        // server has since reassigned. The one-shot readiness latch guards that, but must
        // NOT be waited on synchronously: a Tauri command runs on the caller's runtime and a
        // 15 s park here (×N panes) stalls everything. Instead, if we're not ready yet,
        // return `NotConnected` immediately — `attach_err_kind` maps it to the "NotConnected"
        // token the frontend retries on (250 ms), and the post-reconcile `daemon://status`
        // push prompts a re-attach. (`NoSuchSession` would wrongly trigger a respawn.)
        if !self.is_ready() {
            return Err(PtyBackendError::NotConnected);
        }
        let pane = self
            .keymap
            .get(&key)
            .ok_or(PtyBackendError::NoSuchSession)?;
        // Validate the pane is actually live before binding a bridge to it. Use
        // `pane_list` (JSON, proper error-code mapping) — NOT `pane_read`'s error code,
        // which `run_raw` discards on a non-zero exit (so a dead pane reads as `Other`,
        // not `NoSuchSession`). A stale/dead id → drop the keymap entry and report
        // `NoSuchSession`, which `attach_err_kind` maps to the `"NoLiveSession"` token the
        // frontend respawns on. A `pane_list` transport error is NOT treated as "dead"
        // (that would wrongly respawn a live pane on a transient blip): fall through and
        // attach best-effort.
        // The same listing gives the pane's rows, for a takeover sent without a size.
        let pane_rows = match self.ops.pane_list() {
            Ok(panes) => {
                // Records session ids only: whether an agent was quit is the
                // status loop's to decide, which knows the server it lists.
                self.store.observe(&trusted_observations(
                    observation_pairs(&self.keymap.entries(), &panes),
                    false,
                ));
                if !pane_is_live(&panes, &pane.pane_id) {
                    self.keymap.remove(&key);
                    self.live.lock().unwrap().remove(&key);
                    return Err(PtyBackendError::NoSuchSession);
                }
                pane_viewport_rows(&panes, &pane.pane_id)
            }
            Err(e) => {
                log::warn!(
                    "herdr: attach pane_list check failed ({}); attaching best-effort",
                    PtyBackendError::from(e)
                );
                None
            }
        };
        // Paint the current screen synchronously as `scrollback`:
        // `pane read --source visible --format ansi` gives the live
        // viewport as renderable ANSI, and bridge frames update it in place. The
        // per-attach sink removed the subscribe race that once lost the bridge's first
        // frame, but whether herdr's first takeover frame is always `full` is not
        // verified against a real server, so this read stays as the guaranteed first
        // paint for an idle pane. The pane is validated-live above, so a read error
        // here is logged (not fatal): the bridge's first full frame still repaints.
        let scrollback = match self.ops.pane_read(&pane.pane_id, "visible", "ansi", None) {
            Ok(s) => s.into_bytes(),
            Err(e) => {
                log::warn!(
                    "herdr: attach pane_read failed for {} ({}); relying on bridge frame",
                    key.worktree,
                    PtyBackendError::from(e)
                );
                Vec::new()
            }
        };
        // The new sink replaces the old attach's before the bridge restarts, so every
        // frame of the new bridge reaches this attach.
        let attach_id = self.outputs.replace(&key, out);
        // `size` was recorded on entry; recording it again is harmless.
        let (cols, rows) = self.sizes.takeover(&key, size, pane_rows);
        // Clamp to a minimum of 1 (as spawn/spawn_agent do): a boot attach that races the
        // first fit could pass 0, and `--cols 0 --rows 0` is not a valid takeover size.
        if let Err(e) =
            self.start_bridge(&key, pane.gen, &pane.terminal_id, cols.max(1), rows.max(1))
        {
            // The replaced attach is still its pane's, with its bridge still running
            // (a failed start stops nothing): its own detach releases both. If that
            // detach already came (skipped, this attach being current), nobody is
            // attached any more: stop the bridge for it.
            if self.outputs.revert(&key, attach_id) {
                self.stop_bridge(&key);
            }
            return Err(e);
        }
        self.outputs.settle(&key, attach_id);
        Ok(AttachResult {
            gen: pane.gen,
            scrollback,
            attach_id,
            // A same-size `terminal.resize` makes herdr send a full frame.
            repaints: true,
        })
    }

    fn detach(&self, key: SessionKey, attach_id: Option<u64>) -> Result<(), PtyBackendError> {
        // A detach from a pane a newer attach replaced must not stop the newer
        // attach's bridge.
        if self.outputs.release(&key, attach_id) {
            self.stop_bridge(&key);
        }
        Ok(())
    }

    fn repaint(&self, key: &SessionKey) -> Result<(), PtyBackendError> {
        self.gate.admit_session(key)?;
        // A same-size `terminal.resize` on the pane's bridge: herdr answers every resize
        // with a fresh `full:true` viewport frame. The frontend calls this once its
        // renderer is live and after a settle/fit, so:
        //   - an idle restored pane paints instead of showing a blank block cursor, and
        //   - a pane switched back to is redrawn at the correct dimensions, fixing the
        //     "typing above the input line" misalignment.
        // Queued behind the input already typed, and returns once queued: the frame
        // arrives on the output stream like any other, and a failed write re-takes-over
        // the bridge in the writer. Best-effort: not attached → no-op.
        if !self.bridges.lock().unwrap().contains_key(key) {
            return Ok(());
        }
        self.enqueue(key, WriteJob::Repaint)
    }

    fn write(&self, key: &SessionKey, data: &[u8]) -> Result<(), PtyBackendError> {
        // Fail fast during recovery rather than queueing input for a dead server
        // (a session already resumed this episode takes input).
        self.gate.admit_session(key)?;
        // Runs on the main thread (`pty_write`): only enqueue. The session's writer
        // does the bridge write (or the CLI fallback) in order; its errors are logged.
        self.enqueue(key, WriteJob::input(data))
    }

    fn paste(&self, key: &SessionKey, text: &str) -> Result<(), PtyBackendError> {
        // A gated or missing session is refused before the text is scanned.
        self.gate.admit_session(key)?;
        let gen = self.session_gen(key)?;
        let payload = paste::sanitize(text);
        if payload.is_empty() {
            return Ok(());
        }
        // Checked here, on the command thread, so the caller sees the refusal.
        let bytes = paste::bracketed_len(&payload);
        if bytes > paste::BRIDGE_PASTE_LIMIT {
            log::warn!(
                "herdr paste [{key}]: refused {bytes} bytes (limit {})",
                paste::BRIDGE_PASTE_LIMIT
            );
            return Err(PtyBackendError::PasteTooLarge {
                bytes,
                limit: paste::BRIDGE_PASTE_LIMIT,
            });
        }
        self.enqueue_at(key, gen, WriteJob::Paste(paste::bracketed(&payload)))
    }

    fn write_line(
        &self,
        key: &SessionKey,
        text: &str,
        _submit_delay_ms: u32,
    ) -> Result<(), PtyBackendError> {
        // Refused during recovery (unless this session already resumed); the
        // dispatcher queues the line.
        self.gate.admit_session(key)?;
        self.keymap
            .get_current(key)
            .ok_or(PtyBackendError::NoSuchSession)?;
        // Through the session's queue, so the line lands after the keys typed
        // before it; the writer sends back the result (callers are off the main
        // thread and need it to decide whether the line was dispatched). Bounded:
        // a wedged writer withdraws the line and reports an error instead of
        // blocking the caller for good. No answer at all: the session was killed
        // or replaced with the line still queued.
        let (job, reply) = WriteJob::line(text);
        self.enqueue(key, job)?;
        reply.wait(self.writers.line_timeout())
    }

    fn resize(&self, key: &SessionKey, cols: u16, rows: u16) -> Result<(), PtyBackendError> {
        // Remembered first, even when refused or with no bridge yet: a resize that
        // races an attach still sizes the next takeover sent without a size.
        self.sizes.record(key, (cols, rows));
        self.gate.admit_session(key)?;
        // A `terminal.resize` on the existing bridge, queued so it lands between the keys
        // typed before and after it. No-op when nothing is attached. The writer skips an
        // unchanged size (a boot attach is followed by same-size resizes), comparing with
        // the size the resizes queued before it left, and restarts the bridge at the new
        // size if the write fails, so this returns once queued. herdr rejects a zero
        // size (a resize racing the first fit), hence the clamp.
        if !self.bridges.lock().unwrap().contains_key(key) {
            return Ok(());
        }
        self.enqueue(
            key,
            WriteJob::Resize {
                cols: cols.max(1),
                rows: rows.max(1),
            },
        )
    }

    fn scroll(
        &self,
        key: &SessionKey,
        direction: ScrollDirection,
        source: ScrollSource,
        lines: u16,
        column: u16,
        row: u16,
    ) -> Result<(), PtyBackendError> {
        self.gate.admit_session(key)?;
        // A `terminal.scroll` on the bridge, queued between the keys around it; the
        // writer merges a burst of same-way, same-source ticks and drops one it
        // cannot write. herdr routes it (app mouse report or page key, alternate
        // scroll, or its own history).
        // A wheel over a pane with no bridge is not an error.
        if !self.bridges.lock().unwrap().contains_key(key) {
            return Ok(());
        }
        self.enqueue(
            key,
            WriteJob::Scroll {
                direction,
                source,
                lines,
                column,
                row,
            },
        )
    }

    fn scroll_offset(&self, key: &SessionKey) -> Result<Option<u32>, PtyBackendError> {
        self.gate.admit_session(key)?;
        // One `pane get` read (herdr reports the pane's viewport position there);
        // the caller asks once after a scroll burst, so it is off the typing path.
        // A pane that is unmapped or gone from herdr has nothing to report.
        let Some(pane) = self.keymap.get(key) else {
            return Ok(None);
        };
        match self.ops.pane_get(&pane.pane_id) {
            Ok(info) => Ok(info.and_then(|p| p.scroll)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn read_history(&self, key: &SessionKey, lines: u32) -> Result<String, PtyBackendError> {
        self.gate.admit_session(key)?;
        // One `pane read` of herdr's own history; unlike `scroll_offset`, an unmapped
        // session is an error, since the caller asked for text it cannot get.
        let pane = self.keymap.get(key).ok_or(PtyBackendError::NoSuchSession)?;
        Ok(self
            .ops
            .pane_read(&pane.pane_id, "recent", "text", Some(lines))?)
    }

    fn kill(&self, key: &SessionKey, gen: u64) -> Result<(), PtyBackendError> {
        // A session that is live during recovery (resumed already, or the
        // driver's own resume in flight) may be killed. Asked before the cancel
        // below, which takes a resumed key out of the set the gate admits by.
        let admitted = self.gate.admit_session(key);
        // A killed session is not resumed by recovery, whatever recovery is
        // doing and even when its pane is already gone (a session waiting to be
        // resumed has no keymap entry). Only a kill of an older incarnation
        // while a newer one is mapped is stale and cancels nothing.
        if self.keymap.get(key).is_none_or(|p| p.gen == gen) {
            self.recovery.cancel(key);
            self.keymap.resolve_recovered(key);
        }
        admitted?;
        self.close_session(key, gen)
    }

    fn kill_live(&self, key: &SessionKey, _gen: Option<u64>) -> Result<(), PtyBackendError> {
        // The generation mapped now, not the one the caller saw: a resume that
        // held the key's lock when the terminate flag was set has started a
        // newer one, and nothing starts one after the flag.
        let admitted = self.gate.admit_session(key);
        let live = self.keymap.get(key).map(|p| p.gen);
        // Recovery drops the key even with nothing live to kill: a queued or
        // suspended entry leaves the queue (and the suspended count), and a
        // resumed one is no longer usable.
        self.recovery.cancel(key);
        self.keymap.resolve_recovered(key);
        admitted?;
        match live {
            Some(gen) => self.close_session(key, gen),
            None => Ok(()),
        }
    }

    fn list_sessions(&self) -> Result<Vec<SessionSummary>, PtyBackendError> {
        // Polled every few seconds (the activity engine): nothing reaches herdr
        // while recovery owns the server, and a recent pane list (the status
        // loop's, or an earlier call's) answers without a herdr call. Keeping
        // the resume store current is the status loop's job, not this one's.
        self.gate.admit(None)?;
        let panes = status_loop::session_panes(
            &*self.ops,
            &self.keymap,
            &self.status_cache,
            Instant::now(),
        )?;
        let out = self.summaries_from(&panes);
        let mut live = self.live.lock().unwrap();
        live.clear();
        live.extend(out.iter().map(|s| s.key.clone()));
        drop(live);
        Ok(out)
    }

    fn cached_sessions(&self) -> Result<Vec<SessionSummary>, PtyBackendError> {
        // Called from a frequent UI poll: never reaches herdr. The last pane
        // list (however old) answers; refreshing it is the status loop's and
        // `list_sessions`' job. Nothing cached yet reports no sessions, and
        // the live set is left alone (only a fresh list may rebuild it).
        self.gate.admit(None)?;
        let panes = status_loop::cached_session_panes(&self.status_cache).unwrap_or_default();
        Ok(self.summaries_from(&panes))
    }

    fn terminated_keys(&self, project: &str) -> HashSet<String> {
        self.store.terminated_in(project)
    }

    fn set_terminated(&self, key: &SessionKey, terminated: bool) -> Result<(), PtyBackendError> {
        // Under the key's spawn lock, so the flag serializes with a resume: one
        // already launching finishes first (its session is live when the
        // terminate kills), and one that has not started yet reads the flag
        // and launches nothing.
        let lock = Arc::clone(
            self.spawn_locks
                .lock()
                .unwrap()
                .entry(key.clone())
                .or_default(),
        );
        let _held = lock.lock().unwrap();
        self.store
            .set_terminated(key, terminated)
            .map_err(PtyBackendError::Other)
    }

    fn live_set_snapshot(&self) -> HashSet<SessionKey> {
        self.live.lock().unwrap().clone()
    }

    fn is_live(&self, key: &SessionKey) -> bool {
        self.live.lock().unwrap().contains(key)
    }

    fn status_payload_snapshot(&self) -> StatusPayload {
        // Shared with the pushed `daemon://status` event so the pulled
        // command and the push never drift. A fresh running status from the
        // status loop answers without a herdr call; otherwise (cold, down or
        // stale, e.g. a retry click) one live probe refreshes the cache.
        let explicit_stop = self.explicit_stop.load(Ordering::SeqCst);
        let status = if explicit_stop {
            None
        } else {
            status_loop::snapshot_status(&*self.ops, &self.status_cache, Instant::now())
        };
        build_status_payload(
            explicit_stop,
            &self.supervisor,
            &*self.ops,
            status,
            &self.live,
            &self.exe_missing,
        )
    }

    fn recovery_snapshot(&self) -> gate::RecoveryPayload {
        // Shared with the pushed `daemon://recovery` event (see `emit`).
        self.exe_missing.overlay(self.recovery.snapshot())
    }

    fn take_xdg_leak_notice(&self) -> Option<Vec<String>> {
        env::take_xdg_leak_notice(&self.ops.env().home)
    }

    fn recovery(&self) -> Option<RecoveryHandle> {
        Some(self.recovery.clone())
    }

    fn daemon_stats(&self) -> DaemonStats {
        // Only the herdr backend fills the rail card, from
        // what the status loop last sampled (no herdr call, no process scan). An
        // explicit Stop → not running, no stats.
        if self.explicit_stop.load(Ordering::SeqCst) {
            return DaemonStats::default();
        }
        self.status_cache.lock().unwrap().stats.clone()
    }

    fn perf_source(&self) -> Option<Arc<dyn crate::perf::PerfSource>> {
        Some(Arc::new(perf_source::HerdrPerfSource::new(
            Arc::clone(&self.ops),
            Arc::clone(&self.keymap),
            Arc::clone(&self.status_cache),
        )))
    }

    fn shutdown(&self) -> Result<(), PtyBackendError> {
        // Before anything stops: this exit is requested, not a death.
        self.supervisor.expect_stop();
        // Nothing is owed a resume after a stop the user asked for.
        self.recovery.explicit_stop();
        self.explicit_stop.store(true, Ordering::SeqCst);
        self.stop_all_bridges();
        let r = self.supervisor.stop(&*self.ops);
        // The server (and every pane) is gone: clear the live set and key map so
        // is_live/live_set_snapshot/session_count stop reporting dead sessions as
        // alive (list_sessions can't rebuild them — pane_list errors while down).
        // An explicit stop owes nothing a resume, so the recovery set goes too.
        self.live.lock().unwrap().clear();
        self.keymap.clear_for_shutdown();
        r
    }

    fn restart(&self) -> Result<(), PtyBackendError> {
        self.explicit_stop.store(false, Ordering::SeqCst);
        let parts = self.recovery_parts();
        // A restart is for an idle controller: marking the live sessions owed
        // mid-run would orphan the ones already resumed, and a restart queued
        // behind the run would replay once it finished.
        let idle = matches!(
            self.gate.phase(),
            recovery::RecoveryPhase::Healthy
                | recovery::RecoveryPhase::Down
                | recovery::RecoveryPhase::AwaitHeadroom
                | recovery::RecoveryPhase::Paused
        );
        if !idle {
            // A second click while a run is in progress is a no-op.
            return Ok(());
        }
        // Look for herdr again: it may have been installed, or configured,
        // since launch. The search covers the process `PATH`, so it finds
        // herdr wherever a spawn would. Only when it finds nothing is one
        // herdr call made to confirm (the bare name is still tried, and the OS
        // may resolve it somewhere the search does not look): a spawn that
        // fails for lack of the file is what counts as missing.
        let discovery = self.ops.rediscover();
        let missing =
            !discovery.found && self.ops.status_server().is_err_and(|e| e.is_exe_missing());
        if missing {
            // Nothing can be stopped or started: say so at once instead of
            // handing the controller a restart that can only fail.
            log::warn!(
                "herdr: restart requested but the herdr executable is still not found; \
                 searched: {}",
                display_paths(&discovery.searched)
            );
            self.exe_missing.set(Some(discovery));
            // The probe just made is the answer: no second one for the push.
            parts.emit_status_down();
            parts.emit_recovery();
            return Err(PtyBackendError::Other(
                self.exe_missing.message().unwrap_or_default(),
            ));
        }
        if self.exe_missing.clear(Some(discovery)) {
            log::info!("herdr: the herdr executable was found; restarting");
            parts.emit_recovery();
        }
        let generation = parts.mark_owed();
        // Stop, start, reconcile and resume on the controller's thread; the
        // breaker and the memory check do not apply to the user's own restart.
        self.recovery.user_restart(generation);
        // So the frontend never sees a stale `Healthy` after this returns.
        if self.recovery_driver.lock().unwrap().is_none() {
            parts.wait_left_healthy();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use std::time::Instant;

    /// Everything a [`CaptureSink`] saw, in order.
    type Events = Arc<StdMutex<Vec<(String, Value)>>>;

    /// Capturing [`EmitSink`] for the integration test.
    struct CaptureSink(Arc<StdMutex<Vec<(String, Value)>>>);
    impl EmitSink for CaptureSink {
        fn emit(&self, event: &str, payload: Value) {
            self.0.lock().unwrap().push((event.to_string(), payload));
        }
    }

    /// Pure busy-hint folding used by `list_sessions`: a pane's agent status
    /// maps through [`agent::agent_status_to_state_hint`]; a pane without one is
    /// `None`.
    #[test]
    fn state_hint_for_pane_folds_agent_status() {
        let busy = Some(crate::backend::HINT_BUSY.to_string());
        let with = |status: Option<&str>| PaneInfo {
            agent: status.map(|_| "claude".to_string()),
            agent_status: status.map(str::to_string),
            ..pane("w1:p1")
        };
        assert_eq!(state_hint_for_pane(&with(Some("working"))), busy, "working");
        assert_eq!(state_hint_for_pane(&with(Some("blocked"))), busy, "blocked");
        assert_eq!(state_hint_for_pane(&with(Some("idle"))), None, "idle");
        assert_eq!(state_hint_for_pane(&with(Some("done"))), None, "done");
        assert_eq!(state_hint_for_pane(&with(None)), None, "a bare shell");
    }

    fn pane(id: &str) -> PaneInfo {
        PaneInfo {
            workspace_id: "w".into(),
            pane_id: id.into(),
            terminal_id: "t".into(),
            cwd: None,
            ..Default::default()
        }
    }

    /// The seam is usable by backend tests: a backend over the scripted fake answers
    /// `list_sessions` from the fake's pane list without any real herdr.
    #[test]
    fn backend_over_fake_herdr_lists_sessions() {
        let fake = fake::FakeHerdr::new();
        let events: Arc<StdMutex<Vec<(String, Value)>>> = Arc::new(StdMutex::new(Vec::new()));
        let backend = HerdrBackend::with_ops(
            Arc::clone(&fake) as Arc<dyn HerdrOps>,
            Arc::new(CaptureSink(Arc::clone(&events))),
            Arc::new(ResumeStore::open(&fake.env().resume_db_path()).unwrap()),
            None,
        );
        assert_eq!(backend.list_sessions().unwrap(), Vec::new());
        let ops: Vec<ops::Op> = fake.calls().into_iter().map(|c| c.op).collect();
        assert_eq!(ops, vec![ops::Op::PaneList]);
        assert_eq!(
            backend.status_payload_snapshot().state,
            "Connected",
            "the fake server is up"
        );
    }

    fn up_status() -> ServerStatus {
        ServerStatus {
            running: true,
            version: Some("0.9.3".into()),
            protocol: Some(7),
            socket: None,
        }
    }

    #[test]
    fn build_status_payload_branches() {
        let fake = fake::FakeHerdr::new();
        let sup = Supervisor::new();
        let live = Mutex::new(HashSet::from([skey("a"), skey("b")]));
        let log_path = fake.env().log_path().to_string_lossy().into_owned();
        let exe_missing = ExeMissing::new(fake.env().discovery.clone());
        let build = |stop: bool, status: Option<ServerStatus>| {
            build_status_payload(stop, &sup, &*fake, status, &live, &exe_missing)
        };

        let stopped = build(true, Some(up_status()));
        assert_eq!(
            stopped,
            StatusPayload {
                state: "Disconnected",
                pid: Some(sup.restart_signal()),
                started_at: None,
                daemon_version: None,
                session_count: Some(0),
                reason: None,
                restartable: true,
                log_path: log_path.clone(),
            }
        );
        let up = build(false, Some(up_status()));
        assert_eq!(
            up,
            StatusPayload {
                state: "Connected",
                pid: Some(sup.restart_signal()),
                started_at: None,
                daemon_version: Some(7),
                session_count: Some(2),
                reason: None,
                restartable: true,
                log_path: log_path.clone(),
            }
        );
        let down = ServerStatus {
            running: false,
            ..up_status()
        };
        assert_eq!(build(false, Some(down)).state, "Disconnected");
        let unknown = build(false, None);
        assert_eq!(unknown.state, "Disconnected");
        assert_eq!(unknown.daemon_version, None);
        assert_eq!(unknown.session_count, Some(2));
        // Equality is by value: the same inputs build equal payloads.
        assert_eq!(build(false, Some(up_status())), up);
        assert_ne!(up, unknown);
        assert_eq!(unknown.reason, None);

        // herdr not found: the message is the reason of a server that is not
        // running, and of nothing else.
        exe_missing.set(None);
        let message = exe_missing.message();
        assert!(message.is_some());
        assert_eq!(build(false, None).reason, message);
        let down = ServerStatus {
            running: false,
            ..up_status()
        };
        assert_eq!(build(false, Some(down)).reason, message);
        assert_eq!(build(false, Some(up_status())).reason, None);
        assert_eq!(build(true, None).reason, None, "the user stopped it");
        assert!(build(false, None).restartable);
        // Building never calls herdr itself.
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn status_payload_snapshot_reads_a_warm_cache_without_calling_herdr() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend.status_cache.lock().unwrap().server = Some((up_status(), Instant::now()));
        let p = backend.status_payload_snapshot();
        assert_eq!(p.state, "Connected");
        assert_eq!(p.daemon_version, Some(7));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn status_payload_snapshot_probes_once_when_cold_and_caches() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        assert_eq!(backend.status_payload_snapshot().state, "Connected");
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(backend.status_payload_snapshot().state, "Connected");
        assert_eq!(fake.calls().len(), 1, "the second read hits the cache");
        assert!(backend.status_cache.lock().unwrap().server.is_some());
    }

    /// `list_sessions` is polled every few seconds: it writes nothing to the
    /// resume store (the status loop observes), stays off herdr while recovery
    /// owns the server, reuses a recent pane list, and reads the busy hint from
    /// the pane list's agent status.
    #[test]
    fn list_sessions_neither_observes_nor_calls_herdr_while_recovering() {
        use cli::{AgentSession, AgentSessionKind};
        let fake = fake::FakeHerdr::new();
        let session = AgentSession {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: AgentSessionKind::Id,
            value: "sess-1".into(),
        };
        let pane = fake.add_pane("/repos/wt", Some(("claude", session)));
        let (backend, _) = fake_backend(&fake);
        backend.keymap.record(
            skey("wt"),
            pane.workspace_id.clone(),
            pane.pane_id.clone(),
            pane.terminal_id.clone(),
            "/repos/wt".into(),
        );

        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );
        assert!(matches!(
            backend.list_sessions(),
            Err(PtyBackendError::RecoveryPending)
        ));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());

        backend.gate.set_view(
            recovery::RecoveryPhase::Healthy,
            HashSet::new(),
            HashSet::new(),
        );
        let sessions = backend.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state_hint, None, "idle");
        assert_eq!(backend.list_sessions().unwrap(), sessions);
        assert_eq!(
            count(&fake, ops::Op::PaneList),
            1,
            "the recent list is reused"
        );
        assert_eq!(count(&fake, ops::Op::AgentList), 0);
        let row = keitora_tasks::LocalStore::open(&fake.env().resume_db_path().to_string_lossy())
            .unwrap()
            .resume_get("p", "wt")
            .unwrap();
        assert!(row.is_none(), "nothing written to the resume store");
    }

    /// `cached_sessions` answers from the last pane list however old (and
    /// whatever keymap version it was taken at), never calls `pane list`,
    /// reports nothing before the first list, leaves the live set alone and
    /// stays off herdr's data while recovery owns the server.
    #[test]
    fn cached_sessions_reads_only_the_cache() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend.keymap.record(
            skey("wt"),
            "w".into(),
            "pane_busy".into(),
            "t".into(),
            "/repos/wt".into(),
        );

        // Cold cache: no sessions, no herdr call.
        assert_eq!(backend.cached_sessions().unwrap(), Vec::new());
        assert!(crate::backend::cached_background_set(&backend).is_empty());

        // A stale list taken at an older keymap version still answers.
        let mut busy = pane("pane_busy");
        busy.agent_status = Some("working".into());
        // Past every max age; `checked_sub` because `Instant` can't go before boot.
        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        backend.status_cache.lock().unwrap().panes = Some((vec![busy], long_ago, 0));
        let sessions = backend.cached_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].key, skey("wt"));
        assert_eq!(
            sessions[0].state_hint.as_deref(),
            Some(crate::backend::HINT_BUSY)
        );
        assert_eq!(
            crate::backend::cached_background_set(&backend),
            HashSet::from([skey("wt")])
        );
        assert!(
            backend.live_set_snapshot().is_empty(),
            "only a fresh list rebuilds the live set"
        );

        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );
        assert!(matches!(
            backend.cached_sessions(),
            Err(PtyBackendError::RecoveryPending)
        ));
        assert!(crate::backend::cached_background_set(&backend).is_empty());

        assert_eq!(count(&fake, ops::Op::PaneList), 0, "{:?}", fake.calls());
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    /// `stop_bridge_in` takes the bridge out of the map and drops the lock
    /// before the teardown, so a concurrent `write` is never held up by it.
    #[test]
    fn stop_bridge_in_releases_the_map_lock_before_stopping() {
        let fake = fake::FakeHerdr::new();
        let bridges: Arc<StdMutex<HashMap<SessionKey, Bridge>>> = Arc::default();
        let free_during_kill = Arc::new(StdMutex::new(None::<bool>));
        {
            let bridges = Arc::clone(&bridges);
            let free = Arc::clone(&free_during_kill);
            fake.on_bridge_kill(Arc::new(move || {
                *free.lock().unwrap() = Some(bridges.try_lock().is_ok());
            }));
        }
        let authority: ExitAuthority = Arc::new(|| false);
        let outputs = OutputSinks::default();
        outputs.register(&skey("wt"), output());
        let keymap = KeyMap::with_path(fake.env().keymap_path());
        let gen = record(&keymap, "wt");
        start_bridge_in(
            &*fake,
            &bridges,
            &outputs,
            &keymap,
            &authority,
            &skey("wt"),
            gen,
            "term_1",
            80,
            24,
        )
        .unwrap();
        assert_eq!(bridges.lock().unwrap().len(), 1);

        stop_bridge_in(&bridges, &skey("wt"));
        assert_eq!(
            *free_during_kill.lock().unwrap(),
            Some(true),
            "the map lock is free while the bridge is stopped"
        );
        assert!(bridges.lock().unwrap().is_empty());
    }

    /// A fresh recording pane output, for an attach.
    fn output() -> Arc<dyn OutputSink> {
        Arc::new(fake::CaptureOutput::default())
    }

    /// A bridge for `terminal` on `fake`, delivering nowhere.
    fn fake_bridge(fake: &fake::FakeHerdr, terminal: &str) -> Bridge {
        Bridge::start(
            fake,
            terminal,
            80,
            24,
            "p",
            "wt",
            output(),
            Arc::new(|| false),
        )
        .unwrap()
    }

    #[test]
    fn a_displaced_bridge_is_stopped_after_the_map_lock_is_released() {
        let fake = fake::FakeHerdr::new();
        let bridges: Arc<StdMutex<HashMap<SessionKey, Bridge>>> = Arc::default();
        let free_during_kill = Arc::new(StdMutex::new(Vec::new()));
        {
            let bridges = Arc::clone(&bridges);
            let free = Arc::clone(&free_during_kill);
            fake.on_bridge_kill(Arc::new(move || {
                free.lock().unwrap().push(bridges.try_lock().is_ok());
            }));
        }
        let keymap = KeyMap::with_path(fake.env().keymap_path());
        let gen = record(&keymap, "wt");
        let outputs = OutputSinks::default();
        let attach = outputs.register(&skey("wt"), output());
        let install =
            |b| install_bridge_in(&bridges, &keymap, &outputs, &skey("wt"), gen, attach, b);
        assert!(install(fake_bridge(&fake, "term_1")));
        // A second start that slipped in displaces the first.
        assert!(install(fake_bridge(&fake, "term_1")));
        assert_eq!(*free_during_kill.lock().unwrap(), vec![true]);
        assert_eq!(bridges.lock().unwrap().len(), 1);
    }

    /// Map `skey(w)` to a new pane in `keymap`; its generation.
    fn record(keymap: &KeyMap, w: &str) -> u64 {
        keymap.record(
            skey(w),
            "ws".into(),
            "pane".into(),
            "term".into(),
            format!("/repos/{w}"),
        )
    }

    #[test]
    fn a_stale_generation_or_attach_never_displaces_the_installed_bridge() {
        let fake = fake::FakeHerdr::new();
        let bridges: Arc<StdMutex<HashMap<SessionKey, Bridge>>> = Arc::default();
        let free_during_kill = Arc::new(StdMutex::new(Vec::new()));
        {
            let bridges = Arc::clone(&bridges);
            let free = Arc::clone(&free_during_kill);
            fake.on_bridge_kill(Arc::new(move || {
                free.lock().unwrap().push(bridges.try_lock().is_ok());
            }));
        }
        let keymap = KeyMap::with_path(fake.env().keymap_path());
        let outputs = OutputSinks::default();
        let old_gen = record(&keymap, "wt");
        let old_attach = outputs.register(&skey("wt"), output());
        // A respawn (newer generation) and its attach install their bridge first.
        let gen = record(&keymap, "wt");
        let attach = outputs.register(&skey("wt"), output());
        let newer = fake_bridge(&fake, "term_2").for_session(gen);
        assert!(install_bridge_in(
            &bridges,
            &keymap,
            &outputs,
            &skey("wt"),
            gen,
            attach,
            newer
        ));
        // The slow start of the older generation finishes afterwards.
        let stale = fake_bridge(&fake, "term_1").for_session(old_gen);
        assert!(!install_bridge_in(
            &bridges,
            &keymap,
            &outputs,
            &skey("wt"),
            old_gen,
            attach,
            stale
        ));
        // So does one for the current generation but a replaced attach.
        let replaced = fake_bridge(&fake, "term_2").for_session(gen);
        assert!(!install_bridge_in(
            &bridges,
            &keymap,
            &outputs,
            &skey("wt"),
            gen,
            old_attach,
            replaced
        ));
        assert_eq!(bridges.lock().unwrap()[&skey("wt")].session_gen(), gen);
        assert_eq!(
            *free_during_kill.lock().unwrap(),
            vec![true, true],
            "each surplus bridge is stopped outside the map lock"
        );
    }

    /// A backend over `fake` with a capturing sink and a temp resume store.
    fn fake_backend(fake: &Arc<fake::FakeHerdr>) -> (HerdrBackend, Events) {
        fake_backend_with(fake, None)
    }

    /// [`fake_backend`] with a launch resolver for backend-driven resumes. The
    /// boot latch is already set, as after `start`.
    fn fake_backend_with(
        fake: &Arc<fake::FakeHerdr>,
        launcher: Option<LaunchResolver>,
    ) -> (HerdrBackend, Events) {
        let (backend, events) = unready_backend(fake, launcher);
        drop(ReadySignal(Arc::clone(&backend.ready)));
        (backend, events)
    }

    /// A backend that has not booted yet (for tests that call `start`).
    fn unready_backend(
        fake: &Arc<fake::FakeHerdr>,
        launcher: Option<LaunchResolver>,
    ) -> (HerdrBackend, Events) {
        let events: Events = Arc::new(StdMutex::new(Vec::new()));
        let backend = HerdrBackend::with_ops(
            Arc::clone(fake) as Arc<dyn HerdrOps>,
            Arc::new(CaptureSink(Arc::clone(&events))),
            Arc::new(ResumeStore::open(&fake.env().resume_db_path()).unwrap()),
            launcher,
        );
        (backend, events)
    }

    fn skey(w: &str) -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: w.into(),
        }
    }

    /// Record a quit agent for `key` straight into the resume DB.
    fn record_quit(fake: &fake::FakeHerdr, key: &SessionKey) {
        keitora_tasks::LocalStore::open(&fake.env().resume_db_path().to_string_lossy())
            .unwrap()
            .resume_observe(
                &key.project,
                &key.worktree,
                keitora_tasks::CliKind::Claude,
                Some("quit-1"),
                "herdr:claude",
                false,
                1,
            )
            .unwrap();
    }

    #[test]
    fn kill_cancels_the_sessions_recovery() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (backend, rx) = backend.with_recording_recovery();
        let gen = backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        // A stale kill changes nothing, recovery included.
        backend.kill(&skey("a"), gen + 1).unwrap();
        backend.kill(&skey("a"), gen).unwrap();
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(sent, vec![recovery::machine::Event::Cancel(skey("a"))]);
    }

    /// A session waiting to be resumed has no pane (its entry went with the
    /// replaced server), and recovery may be running: the kill still cancels it.
    #[test]
    fn kill_cancels_recovery_without_a_pane_and_while_recovering() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (backend, rx) = backend.with_recording_recovery();
        backend.keymap.begin_recovery(&[skey("a"), skey("b")]);
        backend.kill(&skey("a"), 1).unwrap();
        backend.gate.set_view(
            recovery::RecoveryPhase::Resuming,
            HashSet::from([skey("x")]),
            HashSet::new(),
        );
        assert!(matches!(
            backend.kill(&skey("b"), 1),
            Err(PtyBackendError::RecoveryPending)
        ));
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            sent,
            vec![
                recovery::machine::Event::Cancel(skey("a")),
                recovery::machine::Event::Cancel(skey("b"))
            ]
        );
        assert!(backend.keymap.should_be_running().is_empty());
    }

    /// The user started a session again while it waited for its resume: the
    /// resume leaves the user's pane alone.
    #[test]
    fn a_resume_for_a_session_the_user_restarted_keeps_the_users_pane() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        backend
            .spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
            .unwrap();
        let users = backend.keymap.get(&skey("a")).unwrap();
        backend.gate.set_view(
            recovery::RecoveryPhase::Resuming,
            HashSet::from([skey("a")]),
            HashSet::new(),
        );
        assert_eq!(backend.resume_session(&skey("a"), None).unwrap(), None);
        assert_eq!(count(&fake, ops::Op::AgentStart), 1, "nothing relaunched");
        assert!(fake.closed_pane_ids().is_empty(), "the user's pane is kept");
        assert_eq!(backend.keymap.get(&skey("a")), Some(users));
        let runner = backend.executor().resume_runner();
        assert_eq!(
            runner.resume(&skey("a"), backend.keymap.generation(), ResumeMode::Stored),
            Some(ResumeReport {
                outcome: ResumeOutcomeKind::AlreadyRunning,
                load_related: false,
            })
        );
    }

    #[test]
    fn kill_removes_the_key_from_should_be_running() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let gen = backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend.keymap.begin_recovery(&[skey("a"), skey("b")]);
        backend.kill(&skey("a"), gen).unwrap();
        assert_eq!(backend.keymap.should_be_running(), vec![skey("b")]);
    }

    // ---- ordered input queue ----

    /// A spawned session (its bridge up) on a backend whose writers wait `wait`
    /// for a bridge. Returns the backend, the key's gen and its terminal id.
    fn typing_backend(fake: &Arc<fake::FakeHerdr>, wait: Duration) -> (HerdrBackend, u64, String) {
        typing_backend_with(fake, Writers::with_wait(wait))
    }

    /// [`typing_backend`] over `writers`. A pane output is registered first (a
    /// restart from an attached pane), so the spawn starts the bridge.
    fn typing_backend_with(
        fake: &Arc<fake::FakeHerdr>,
        writers: Writers,
    ) -> (HerdrBackend, u64, String) {
        let (mut backend, _) = fake_backend(fake);
        backend.writers = writers;
        backend.outputs.register(&skey("a"), output());
        let gen = backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        (backend, gen, terminal)
    }

    /// Returns once every job queued for `key` so far has run.
    fn flush_writer(backend: &HerdrBackend, key: &SessionKey) {
        let (tx, rx) = mpsc::sync_channel(1);
        backend.enqueue(key, WriteJob::Flush(tx)).unwrap();
        rx.recv().unwrap();
    }

    /// Every line the bridge stdin of `terminal` received, parsed.
    fn stdin_lines(fake: &fake::FakeHerdr, terminal: &str) -> Vec<Value> {
        fake.bridge_input(terminal)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Each `terminal.input` line the bridge stdin of `terminal` received.
    fn input_lines(fake: &fake::FakeHerdr, terminal: &str) -> Vec<Value> {
        stdin_lines(fake, terminal)
            .into_iter()
            .filter(|v| v["type"] == "terminal.input")
            .collect()
    }

    /// The text of every input line, concatenated.
    fn typed_text(fake: &fake::FakeHerdr, terminal: &str) -> String {
        input_lines(fake, terminal)
            .iter()
            .map(|v| v["text"].as_str().expect("a text line").to_string())
            .collect()
    }

    fn send_text_args(fake: &fake::FakeHerdr) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::PaneSendText)
            .map(|c| c.args)
            .collect()
    }

    #[test]
    fn typed_input_stays_in_order_across_bridge_restarts() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let backend = Arc::new(backend);
        let typist = {
            let backend = Arc::clone(&backend);
            std::thread::spawn(move || {
                let mut sent = String::new();
                for i in 0..400 {
                    let s = format!("{i},");
                    backend.write(&skey("a"), s.as_bytes()).unwrap();
                    sent.push_str(&s);
                }
                sent
            })
        };
        // Each attach stops the bridge and starts a new one mid-stream.
        for _ in 0..20 {
            backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        }
        let sent = typist.join().unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(typed_text(&fake, &terminal), sent);
        assert!(send_text_args(&fake).is_empty(), "no CLI fallback");
    }

    #[test]
    fn input_queued_behind_a_busy_writer_is_coalesced() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let mut sent = String::new();
        {
            // The writer cannot look up the bridge while this is held, so the
            // keys typed meanwhile queue up behind it.
            let _held = backend.bridges.lock().unwrap();
            for c in 'a'..='z' {
                backend.write(&skey("a"), c.to_string().as_bytes()).unwrap();
                sent.push(c);
            }
        }
        flush_writer(&backend, &skey("a"));
        let lines = input_lines(&fake, &terminal);
        assert!(lines.len() <= 2, "{lines:?}");
        assert_eq!(typed_text(&fake, &terminal), sent);
    }

    #[test]
    fn text_goes_as_text_and_other_bytes_as_base64() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let text = "echo \"a\\b\"\r\n\t\u{1b}[A é";
        backend.write(&skey("a"), text.as_bytes()).unwrap();
        flush_writer(&backend, &skey("a"));
        backend.write(&skey("a"), &[0xff, 0xfe]).unwrap();
        flush_writer(&backend, &skey("a"));
        let lines = input_lines(&fake, &terminal);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["text"], text);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(lines[1]["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, [0xff, 0xfe]);
    }

    #[test]
    fn without_a_bridge_input_falls_back_after_the_wait_as_one_call() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_millis(30));
        backend.stop_bridge(&skey("a"));
        let pane = backend.keymap.get(&skey("a")).unwrap().pane_id;
        {
            // Everything is queued before the writer finds there is no bridge.
            let _held = backend.bridges.lock().unwrap();
            for s in ["ab", "c", "d"] {
                backend.write(&skey("a"), s.as_bytes()).unwrap();
            }
        }
        flush_writer(&backend, &skey("a"));
        assert_eq!(send_text_args(&fake), vec![format!("{pane} abcd")]);
        assert!(fake.bridge_input(&terminal).is_empty());
    }

    // ---- paste ----

    #[test]
    fn paste_goes_down_the_bridge_as_one_bracketed_block() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.paste(&skey("a"), "line1\rline2").unwrap();
        flush_writer(&backend, &skey("a"));
        let lines = input_lines(&fake, &terminal);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["text"], "\x1b[200~line1\rline2\x1b[201~");
    }

    #[test]
    fn paste_strips_embedded_markers_and_stays_one_block() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend
            .paste(&skey("a"), "a\x1b[201~b\x1b[200~c\x1b[20\x1b[201~1~d")
            .unwrap();
        flush_writer(&backend, &skey("a"));
        let lines = input_lines(&fake, &terminal);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["text"], "\x1b[200~abcd\x1b[201~");
    }

    #[test]
    fn paste_queued_behind_a_busy_writer_is_never_coalesced() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        {
            // The writer cannot look up the bridge while this is held, so all
            // three queue up behind it.
            let _held = backend.bridges.lock().unwrap();
            backend.write(&skey("a"), b"x").unwrap();
            backend.paste(&skey("a"), "p").unwrap();
            backend.write(&skey("a"), b"y").unwrap();
        }
        flush_writer(&backend, &skey("a"));
        let texts: Vec<Value> = input_lines(&fake, &terminal)
            .into_iter()
            .map(|v| v["text"].clone())
            .collect();
        assert_eq!(texts, vec!["x", "\x1b[200~p\x1b[201~", "y"]);
    }

    #[test]
    fn paste_over_the_bridge_cap_is_refused_and_nothing_is_queued() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let err = backend
            .paste(&skey("a"), &"a".repeat(paste::BRIDGE_PASTE_LIMIT - 11))
            .unwrap_err();
        // The cap counts the whole block, markers included.
        assert!(
            matches!(
                err,
                PtyBackendError::PasteTooLarge {
                    bytes: 1_048_577,
                    limit: 1_048_576
                }
            ),
            "{err:?}"
        );
        // A block of exactly the cap is still accepted.
        backend
            .paste(&skey("a"), &"a".repeat(paste::BRIDGE_PASTE_LIMIT - 12))
            .unwrap();
        flush_writer(&backend, &skey("a"));
        let lines = input_lines(&fake, &terminal);
        assert_eq!(lines.len(), 1, "only the paste at the cap");
    }

    #[test]
    fn paste_that_is_empty_or_only_markers_sends_nothing() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.paste(&skey("a"), "").unwrap();
        backend.paste(&skey("a"), "\x1b[200~\x1b[201~\0").unwrap();
        flush_writer(&backend, &skey("a"));
        assert!(input_lines(&fake, &terminal).is_empty());
        assert!(send_text_args(&fake).is_empty());
    }

    #[test]
    fn paste_without_a_session_is_refused() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        assert!(matches!(
            backend.paste(&skey("nope"), "p"),
            Err(PtyBackendError::NoSuchSession)
        ));
    }

    #[test]
    fn paste_fallback_is_one_send_text_with_both_markers() {
        let fake = fake::FakeHerdr::new();
        let (backend, gen, _) = typing_backend(&fake, Duration::from_secs(30));
        let pane = backend.keymap.get(&skey("a")).unwrap().pane_id;
        let io = backend.writer_io(&skey("a"), gen);
        use writer::SessionIo as _;
        let block = paste::bracketed("l1\rl2");
        io.fallback_paste(&block).unwrap();
        assert_eq!(send_text_args(&fake), vec![format!("{pane} {block}")]);
        assert_eq!(
            count(&fake, ops::Op::PaneSendKeys),
            0,
            "no enter or esc keys"
        );
    }

    #[test]
    fn paste_without_a_bridge_falls_back_through_the_writer_as_one_block() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_millis(10));
        backend.stop_bridge(&skey("a"));
        let pane = backend.keymap.get(&skey("a")).unwrap().pane_id;
        backend.paste(&skey("a"), "l1\rl2").unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(
            send_text_args(&fake),
            vec![format!("{pane} \x1b[200~l1\rl2\x1b[201~")]
        );
        assert_eq!(count(&fake, ops::Op::PaneSendKeys), 0);
        assert!(fake.bridge_input(&terminal).is_empty());
    }

    /// A backend with session `a` spawned, that session's writer io, and the
    /// events the backend emitted.
    fn spawned_writer_io(fake: &Arc<fake::FakeHerdr>) -> (HerdrBackend, WriterIo, Events) {
        let (backend, events) = fake_backend(fake);
        let gen = backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let io = backend.writer_io(&skey("a"), gen);
        (backend, io, events)
    }

    fn paste_refusals(events: &Events) -> Vec<(String, Value)> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.starts_with("pty://paste-refused/"))
            .cloned()
            .collect()
    }

    #[test]
    fn paste_over_the_fallback_cap_is_refused_whole_with_a_per_session_event() {
        let fake = fake::FakeHerdr::new();
        let (_backend, io, events) = spawned_writer_io(&fake);
        use writer::SessionIo as _;
        // Under the cap raw, over it once each quote counts twice.
        let quotes = "\"".repeat(paste::FALLBACK_PASTE_LIMIT / 2 + 1);
        let cost = paste::fallback_cost(&quotes);
        io.fallback_paste(&paste::bracketed(&quotes)).unwrap();
        assert!(send_text_args(&fake).is_empty(), "nothing is sent");
        assert_eq!(
            paste_refusals(&events),
            vec![(
                "pty://paste-refused/p/a".to_string(),
                serde_json::json!({"bytes": cost, "limit": 24_576})
            )]
        );
    }

    #[test]
    fn paste_over_the_fallback_cap_for_a_gone_session_is_not_reported_as_too_large() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        // No session `a` of this generation: its pane is gone.
        let io = backend.writer_io(&skey("a"), 1);
        use writer::SessionIo as _;
        let over = "a".repeat(paste::FALLBACK_PASTE_LIMIT + 1);
        assert!(matches!(
            io.fallback_paste(&paste::bracketed(&over)),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert!(paste_refusals(&events).is_empty());
        assert!(send_text_args(&fake).is_empty());
    }

    #[test]
    fn paste_at_the_fallback_cap_is_still_sent() {
        let fake = fake::FakeHerdr::new();
        let (_backend, io, events) = spawned_writer_io(&fake);
        use writer::SessionIo as _;
        let block = paste::bracketed(&"a".repeat(paste::FALLBACK_PASTE_LIMIT));
        io.fallback_paste(&block).unwrap();
        assert_eq!(send_text_args(&fake).len(), 1);
        assert!(paste_refusals(&events).is_empty());
    }

    #[test]
    fn write_line_lands_after_the_text_typed_before_it() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        {
            let _held = backend.bridges.lock().unwrap();
            backend.write(&skey("a"), b"draft").unwrap();
        }
        // Submit-only line: the writer runs it after the queued text.
        backend.write_line(&skey("a"), "", 0).unwrap();
        assert_eq!(typed_text(&fake, &terminal), "draft");
        assert_eq!(count(&fake, ops::Op::PaneSendKeys), 1);
        // The caller still gets the line's own result.
        assert!(matches!(
            backend.write_line(&skey("nope"), "hi", 0),
            Err(PtyBackendError::NoSuchSession)
        ));
    }

    #[test]
    fn a_failed_bridge_write_stops_the_bridge_and_falls_back() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_millis(10));
        let pane = backend.keymap.get(&skey("a")).unwrap().pane_id;
        fake.fail_bridge_input(&terminal, true);
        backend.write(&skey("a"), b"x").unwrap();
        flush_writer(&backend, &skey("a"));
        assert!(!backend.bridges.lock().unwrap().contains_key(&skey("a")));
        assert_eq!(send_text_args(&fake), vec![format!("{pane} x")]);
        // The writer survives: a new bridge gets the next keys.
        fake.fail_bridge_input(&terminal, false);
        backend.repaint(&skey("a")).unwrap(); // no bridge: a no-op
        backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        backend.write(&skey("a"), b"y").unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(typed_text(&fake, &terminal), "y");
    }

    // ---- in-place resize and repaint ----

    fn resize_line(cols: u16, rows: u16) -> Value {
        serde_json::json!({"type": "terminal.resize", "cols": cols, "rows": rows})
    }

    fn bridge_size(backend: &HerdrBackend, key: &SessionKey) -> Option<(u16, u16)> {
        backend.bridges.lock().unwrap().get(key).map(Bridge::size)
    }

    #[test]
    fn resize_writes_a_resize_line_on_the_running_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.resize(&skey("a"), 120, 40).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(stdin_lines(&fake, &terminal), vec![resize_line(120, 40)]);
        assert_eq!(bridge_size(&backend, &skey("a")), Some((120, 40)));
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1, "only the spawn's");
        // The same size again writes nothing.
        backend.resize(&skey("a"), 120, 40).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(stdin_lines(&fake, &terminal).len(), 1);
    }

    #[test]
    fn repaint_writes_a_same_size_resize_and_spawns_nothing() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.repaint(&skey("a")).unwrap();
        backend.repaint(&skey("a")).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(
            stdin_lines(&fake, &terminal),
            vec![resize_line(80, 24), resize_line(80, 24)]
        );
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1, "only the spawn's");
    }

    #[test]
    fn resize_and_repaint_without_a_bridge_do_nothing() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.stop_bridge(&skey("a"));
        backend.resize(&skey("a"), 100, 30).unwrap();
        backend.repaint(&skey("a")).unwrap();
        flush_writer(&backend, &skey("a"));
        assert!(fake.bridge_input(&terminal).is_empty());
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1);
        assert_eq!(bridge_size(&backend, &skey("a")), None);
    }

    // ---- wheel scroll ----

    #[test]
    fn scroll_writes_a_scroll_line_on_the_running_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend
            .scroll(
                &skey("a"),
                ScrollDirection::Up,
                ScrollSource::Wheel,
                3,
                4,
                10,
            )
            .unwrap();
        backend
            .scroll(
                &skey("a"),
                ScrollDirection::Down,
                ScrollSource::PageKey,
                20,
                0,
                0,
            )
            .unwrap();
        flush_writer(&backend, &skey("a"));
        let wheel_tick = serde_json::json!({
            "type": "terminal.scroll",
            "direction": "up",
            "source": "wheel",
            "lines": 1,
            "column": 4,
            "row": 10,
        });
        assert_eq!(
            stdin_lines(&fake, &terminal),
            vec![
                wheel_tick.clone(),
                wheel_tick.clone(),
                wheel_tick,
                serde_json::json!({
                    "type": "terminal.scroll",
                    "direction": "down",
                    "source": "page_key",
                    "lines": 20,
                    "column": 0,
                    "row": 0,
                }),
            ]
        );
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1, "only the spawn's");
    }

    #[test]
    fn scroll_on_an_unknown_or_detached_pane_is_ok_and_writes_nothing() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend
            .scroll(
                &skey("nope"),
                ScrollDirection::Down,
                ScrollSource::Wheel,
                1,
                0,
                0,
            )
            .unwrap();
        backend.stop_bridge(&skey("a"));
        backend
            .scroll(
                &skey("a"),
                ScrollDirection::Down,
                ScrollSource::Wheel,
                1,
                0,
                0,
            )
            .unwrap();
        flush_writer(&backend, &skey("a"));
        assert!(fake.bridge_input(&terminal).is_empty());
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1);
    }

    #[test]
    fn scroll_offset_reads_the_panes_offset_from_pane_get() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let pane_id = backend.keymap.get(&skey("a")).unwrap().pane_id;
        assert_eq!(
            backend.scroll_offset(&skey("a")).unwrap(),
            None,
            "no scroll"
        );
        fake.set_pane_scroll(&pane_id, Some(42));
        assert_eq!(backend.scroll_offset(&skey("a")).unwrap(), Some(42));
        fake.set_pane_scroll(&pane_id, Some(0));
        assert_eq!(backend.scroll_offset(&skey("a")).unwrap(), Some(0));
        assert_eq!(count(&fake, ops::Op::PaneList), 0, "one pane, not the list");
        assert_eq!(backend.scroll_offset(&skey("nope")).unwrap(), None);
        // A pane herdr no longer has reads as nothing to report.
        fake.pane_close(&pane_id).unwrap();
        assert_eq!(backend.scroll_offset(&skey("a")).unwrap(), None);
    }

    #[test]
    fn read_history_returns_the_panes_recent_text() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let pane_id = backend.keymap.get(&skey("a")).unwrap().pane_id;
        fake.set_pane_text(&pane_id, "one\ntwo\nthree");
        assert_eq!(
            backend.read_history(&skey("a"), 200).unwrap(),
            "one\ntwo\nthree"
        );
        let reads: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::PaneRead)
            .map(|c| c.args)
            .collect();
        assert_eq!(reads, [format!("{pane_id} recent text --lines 200")]);
    }

    #[test]
    fn read_history_of_an_unknown_session_errors() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        assert!(matches!(
            backend.read_history(&skey("nope"), 10),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert_eq!(count(&fake, ops::Op::PaneRead), 0);
    }

    #[test]
    fn a_failed_scroll_write_does_not_restart_the_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        fake.fail_bridge_input(&terminal, true);
        backend
            .scroll(
                &skey("a"),
                ScrollDirection::Up,
                ScrollSource::Wheel,
                1,
                0,
                0,
            )
            .unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1, "no restart");
        assert!(!backend.bridges.lock().unwrap().contains_key(&skey("a")));
    }

    #[test]
    fn a_failed_resize_write_restarts_the_bridge_at_the_new_size() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        fake.fail_bridge_input(&terminal, true);
        backend.resize(&skey("a"), 100, 30).unwrap();
        flush_writer(&backend, &skey("a"));
        let spawns: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::SpawnBridge)
            .map(|c| c.args)
            .collect();
        assert_eq!(
            spawns,
            vec![format!("{terminal} 80x24"), format!("{terminal} 100x30")]
        );
        assert_eq!(bridge_size(&backend, &skey("a")), Some((100, 30)));
        // A failed repaint re-takes-over at the current size.
        backend.repaint(&skey("a")).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 3);
        assert_eq!(bridge_size(&backend, &skey("a")), Some((100, 30)));
        // The new bridge takes resizes in place again.
        fake.fail_bridge_input(&terminal, false);
        backend.repaint(&skey("a")).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(stdin_lines(&fake, &terminal), vec![resize_line(100, 30)]);
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 3);
    }

    #[test]
    fn a_resize_lands_after_the_input_queued_before_it() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.write(&skey("a"), b"ab").unwrap();
        backend.resize(&skey("a"), 90, 30).unwrap();
        backend.write(&skey("a"), b"cd").unwrap();
        flush_writer(&backend, &skey("a"));
        let kinds: Vec<Value> = stdin_lines(&fake, &terminal)
            .into_iter()
            .map(|v| v["text"].clone())
            .collect();
        assert_eq!(
            kinds,
            vec![Value::from("ab"), Value::Null, Value::from("cd")]
        );
        assert_eq!(stdin_lines(&fake, &terminal)[1], resize_line(90, 30));
    }

    #[test]
    fn a_zero_resize_is_clamped() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        backend.resize(&skey("a"), 0, 0).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(stdin_lines(&fake, &terminal), vec![resize_line(1, 1)]);
    }

    // ---- pane output ----

    /// Puts a full frame of `text` on `terminal`'s current bridge stream.
    fn push_text(fake: &fake::FakeHerdr, terminal: &str, text: &str) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        fake.push_frame(
            terminal,
            &format!(r#"{{"type":"terminal.frame","bytes":"{b64}","full":true}}"#),
        );
    }

    fn has_frame(e: &[fake::OutputEvent]) -> bool {
        e.iter()
            .any(|e| matches!(e, fake::OutputEvent::Frame { .. }))
    }

    #[test]
    fn the_attached_output_receives_frames_and_the_exit() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let out = Arc::new(fake::CaptureOutput::default());
        backend
            .attach(skey("a"), Some((80, 24)), out.clone())
            .unwrap();
        push_text(&fake, &terminal, "hello");
        fake.push_frame(&terminal, r#"{"type":"terminal.closed","reason":"exited"}"#);
        assert!(out.wait_for(|e| e.contains(&fake::OutputEvent::Exit)));
        assert_eq!(out.frames(), vec!["hello".to_string()]);
        assert_eq!(out.exits(), 1);
    }

    #[test]
    fn attach_paints_the_visible_screen_without_a_lines_cap() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let pane_id = backend.keymap.get(&skey("a")).unwrap().pane_id;
        backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        let reads: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::PaneRead)
            .map(|c| c.args)
            .collect();
        assert_eq!(reads, [format!("{pane_id} visible ansi")]);
    }

    #[test]
    fn a_reattach_replaces_the_output() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let first = Arc::new(fake::CaptureOutput::default());
        let second = Arc::new(fake::CaptureOutput::default());
        let a = backend
            .attach(skey("a"), Some((80, 24)), first.clone())
            .unwrap();
        let b = backend
            .attach(skey("a"), Some((80, 24)), second.clone())
            .unwrap();
        assert!(b.attach_id > a.attach_id, "attach ids are monotonic");
        push_text(&fake, &terminal, "after");
        assert!(second.wait_for(has_frame));
        backend.detach(skey("a"), Some(b.attach_id)).unwrap();
        assert_eq!(second.frames(), vec!["after".to_string()]);
        assert!(first.events().is_empty(), "{:?}", first.events());
    }

    #[test]
    fn a_stale_attach_id_detach_is_a_no_op() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let old = backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        let out = Arc::new(fake::CaptureOutput::default());
        let new = backend
            .attach(skey("a"), Some((80, 24)), out.clone())
            .unwrap();
        // The old pane's late detach leaves the newer attach running.
        backend.detach(skey("a"), Some(old.attach_id)).unwrap();
        assert!(backend.bridges.lock().unwrap().contains_key(&skey("a")));
        push_text(&fake, &terminal, "still here");
        assert!(out.wait_for(has_frame));
        // Its own detach stops it; repeating it changes nothing.
        backend.detach(skey("a"), Some(new.attach_id)).unwrap();
        assert!(!backend.bridges.lock().unwrap().contains_key(&skey("a")));
        assert!(backend.outputs.get(&skey("a")).is_none());
        backend.detach(skey("a"), Some(new.attach_id)).unwrap();
        assert_eq!(
            count(&fake, ops::Op::SpawnBridge),
            3,
            "spawn + two attaches"
        );
    }

    #[test]
    fn an_unscoped_detach_stops_the_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        backend.detach(skey("a"), None).unwrap();
        assert!(!backend.bridges.lock().unwrap().contains_key(&skey("a")));
        assert!(backend.outputs.get(&skey("a")).is_none());
    }

    #[test]
    fn a_writer_restart_keeps_the_attached_output() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, terminal) = typing_backend(&fake, Duration::from_secs(30));
        let out = Arc::new(fake::CaptureOutput::default());
        backend
            .attach(skey("a"), Some((80, 24)), out.clone())
            .unwrap();
        // A failed resize write restarts the bridge from the writer thread.
        fake.fail_bridge_input(&terminal, true);
        backend.resize(&skey("a"), 100, 30).unwrap();
        flush_writer(&backend, &skey("a"));
        assert_eq!(
            count(&fake, ops::Op::SpawnBridge),
            3,
            "spawn, attach, restart"
        );
        push_text(&fake, &terminal, "restarted");
        assert!(out.wait_for(has_frame));
        assert_eq!(out.frames(), vec!["restarted".to_string()]);
    }

    #[test]
    fn a_respawn_streams_to_the_pane_already_attached() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let out = Arc::new(fake::CaptureOutput::default());
        backend
            .attach(skey("a"), Some((80, 24)), out.clone())
            .unwrap();
        // A restart: the session is replaced while its pane stays attached.
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        push_text(&fake, &terminal, "fresh");
        assert!(out.wait_for(has_frame));
        assert_eq!(out.frames(), vec!["fresh".to_string()]);
    }

    #[test]
    fn a_spawn_with_no_attached_pane_starts_no_bridge_and_the_attach_does() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 0, "nobody to stream to");
        assert!(!backend.bridges.lock().unwrap().contains_key(&skey("a")));
        let out = Arc::new(fake::CaptureOutput::default());
        backend
            .attach(skey("a"), Some((100, 30)), out.clone())
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        let spawns: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::SpawnBridge)
            .map(|c| c.args)
            .collect();
        assert_eq!(
            spawns,
            vec![format!("{terminal} 100x30")],
            "one, at the pane's size"
        );
        push_text(&fake, &terminal, "seen");
        assert!(out.wait_for(has_frame));
        assert_eq!(out.frames(), vec!["seen".to_string()]);
    }

    /// The `terminal_id COLSxROWS` of every bridge started so far.
    fn bridge_takeovers(fake: &fake::FakeHerdr) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::SpawnBridge)
            .map(|c| c.args)
            .collect()
    }

    /// A backend with session "a" spawned without a size, and its terminal id.
    fn unsized_spawn(fake: &Arc<fake::FakeHerdr>) -> (HerdrBackend, String) {
        let (backend, _) = fake_backend(fake);
        backend
            .spawn(skey("a"), "/repos/a".into(), String::new(), None, None)
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        (backend, terminal)
    }

    #[test]
    fn takeover_size_prefers_measured_then_remembered_then_pane_rows() {
        let cases = [
            (Some((100, 30)), Some((90, 40)), Some(50), (100, 30)),
            (None, Some((90, 40)), Some(50), (90, 40)),
            (None, None, Some(50), (80, 50)),
            (None, None, None, (80, 24)),
        ];
        for (requested, remembered, pane_rows, want) in cases {
            assert_eq!(
                takeover_size(requested, remembered, pane_rows),
                want,
                "{requested:?} {remembered:?} {pane_rows:?}"
            );
        }
    }

    #[test]
    fn an_attach_with_no_size_and_nothing_known_takes_over_at_the_default() {
        let fake = fake::FakeHerdr::new();
        let (backend, terminal) = unsized_spawn(&fake);
        backend.attach(skey("a"), None, output()).unwrap();
        assert_eq!(bridge_takeovers(&fake), vec![format!("{terminal} 80x24")]);
    }

    #[test]
    fn an_attach_with_no_size_takes_over_at_the_panes_own_rows() {
        let fake = fake::FakeHerdr::new();
        let (backend, terminal) = unsized_spawn(&fake);
        let pane = backend.keymap.get(&skey("a")).unwrap().pane_id;
        fake.set_viewport_rows(&pane, Some(50));
        backend.attach(skey("a"), None, output()).unwrap();
        assert_eq!(bridge_takeovers(&fake), vec![format!("{terminal} 80x50")]);
    }

    #[test]
    fn an_attach_with_no_size_takes_over_at_a_resize_sent_before_any_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, terminal) = unsized_spawn(&fake);
        let pane = backend.keymap.get(&skey("a")).unwrap().pane_id;
        fake.set_viewport_rows(&pane, Some(50));
        // No bridge yet: the resize is a no-op for herdr, but remembered.
        backend.resize(&skey("a"), 120, 40).unwrap();
        assert!(bridge_takeovers(&fake).is_empty());
        backend.attach(skey("a"), None, output()).unwrap();
        assert_eq!(bridge_takeovers(&fake), vec![format!("{terminal} 120x40")]);
    }

    #[test]
    fn a_measured_attach_sizes_the_next_unsized_one() {
        let fake = fake::FakeHerdr::new();
        let (backend, terminal) = unsized_spawn(&fake);
        backend
            .attach(skey("a"), Some((132, 44)), output())
            .unwrap();
        backend.attach(skey("a"), None, output()).unwrap();
        assert_eq!(
            bridge_takeovers(&fake),
            vec![format!("{terminal} 132x44"), format!("{terminal} 132x44")]
        );
    }

    #[test]
    fn a_measured_spawn_sizes_the_next_unsized_attach() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((100, 30)),
                None,
            )
            .unwrap();
        backend.attach(skey("a"), None, output()).unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        assert_eq!(bridge_takeovers(&fake), vec![format!("{terminal} 100x30")]);
    }

    #[test]
    fn an_unsized_respawn_of_an_attached_pane_keeps_its_size() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((110, 35)),
                claude_launch(),
            )
            .unwrap();
        // A restart of the attached pane, sent before it was measured again.
        backend.outputs.register(&skey("a"), output());
        backend
            .spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                None,
                claude_launch(),
            )
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        assert_eq!(
            bridge_takeovers(&fake).last(),
            Some(&format!("{terminal} 110x35"))
        );
    }

    #[test]
    fn a_resume_takes_over_at_the_remembered_size() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        // The pane is attached (its output registered) and was measured.
        backend.outputs.register(&skey("a"), output());
        backend.resize(&skey("a"), 140, 45).unwrap();
        backend
            .resume_session(&skey("a"), Some(ResumeDecision::Continue))
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        assert_eq!(bridge_takeovers(&fake), vec![format!("{terminal} 140x45")]);
    }

    #[test]
    fn a_resume_with_no_remembered_size_uses_the_resume_default() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        backend.outputs.register(&skey("a"), output());
        backend
            .resume_session(&skey("a"), Some(ResumeDecision::Continue))
            .unwrap();
        let terminal = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        assert_eq!(
            bridge_takeovers(&fake),
            vec![format!("{terminal} {RESUME_COLS}x{RESUME_ROWS}")]
        );
    }

    #[test]
    fn an_agent_spawn_with_no_attached_pane_starts_no_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
            .unwrap();
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 0);
        // A pane attached to the key (a restart) streams from the spawn.
        backend.outputs.register(&skey("a"), output());
        backend
            .spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
            .unwrap();
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 1);
        assert!(backend.bridges.lock().unwrap().contains_key(&skey("a")));
    }

    // ---- writer stalls, wedged lines, generations ----

    #[test]
    fn a_bridge_write_blocked_past_the_stall_limit_restarts_the_bridge() {
        let fake = fake::FakeHerdr::new();
        let writers =
            Writers::with_wait(Duration::from_secs(30)).with_stall_limit(Duration::from_millis(20));
        let (backend, _, terminal) = typing_backend_with(&fake, writers);
        // The control child stops reading: the write blocks until it is killed.
        fake.block_bridge_input(&terminal, true);
        backend.write(&skey("a"), b"x").unwrap();
        // Only the watchdog's restart (killing the stuck child) can end that write.
        wait_for("the stalled bridge to be restarted", || {
            count(&fake, ops::Op::SpawnBridge) >= 2
        });
        fake.block_bridge_input(&terminal, false);
        flush_writer(&backend, &skey("a"));
        assert_eq!(
            typed_text(&fake, &terminal),
            "x",
            "sent once, on the new bridge"
        );
        assert!(send_text_args(&fake).is_empty(), "no CLI fallback");
        assert!(backend.bridges.lock().unwrap().contains_key(&skey("a")));
    }

    #[test]
    fn write_line_gives_up_on_a_wedged_writer_and_withdraws_the_line() {
        let fake = fake::FakeHerdr::new();
        let writers = Writers::with_wait(Duration::from_secs(30))
            .with_stall_limit(Duration::from_secs(3600))
            .with_line_timeout(Duration::from_millis(10));
        let (backend, _, terminal) = typing_backend_with(&fake, writers);
        fake.block_bridge_input(&terminal, true);
        backend.write(&skey("a"), b"x").unwrap();
        // The writer is stuck in the write above, so the line cannot be taken.
        assert!(matches!(
            backend.write_line(&skey("a"), "hi", 0),
            Err(PtyBackendError::Other(_))
        ));
        fake.block_bridge_input(&terminal, false);
        flush_writer(&backend, &skey("a"));
        assert_eq!(typed_text(&fake, &terminal), "x");
        assert_eq!(
            count(&fake, ops::Op::AgentPrompt),
            0,
            "the line was withdrawn"
        );
    }

    #[test]
    fn an_old_generations_writer_cannot_reach_the_new_session() {
        let fake = fake::FakeHerdr::new();
        let (backend, gen1, _) = typing_backend(&fake, Duration::from_secs(30));
        let old = backend.writer_io(&skey("a"), gen1);
        use writer::SessionIo as _;
        assert!(old.stdin().is_some(), "its own bridge is reachable");
        backend.kill(&skey("a"), gen1).unwrap();
        let gen2 = backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        assert_ne!(gen1, gen2);
        assert!(backend.bridges.lock().unwrap().contains_key(&skey("a")));
        assert!(old.stdin().is_none());
        assert!(old.bridge().is_none());
        assert!(matches!(
            old.fallback(b"x"),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert!(matches!(
            old.write_line("hi"),
            Err(PtyBackendError::NoSuchSession)
        ));
        assert!(send_text_args(&fake).is_empty());
        assert_eq!(count(&fake, ops::Op::AgentPrompt), 0);
        // The new generation's writer reaches it.
        let new = backend.writer_io(&skey("a"), gen2);
        assert!(new.stdin().is_some());
    }

    #[test]
    fn jobs_queued_before_a_kill_never_reach_the_respawned_session() {
        let fake = fake::FakeHerdr::new();
        let (backend, gen1, term1) = typing_backend(&fake, Duration::from_secs(30));
        // The old writer is stuck in a write with more queued behind it.
        fake.block_bridge_input(&term1, true);
        backend.write(&skey("a"), b"x").unwrap();
        backend.write(&skey("a"), b"y").unwrap();
        let (job, line) = WriteJob::line("queued");
        backend.enqueue(&skey("a"), job).unwrap();
        backend.kill(&skey("a"), gen1).unwrap();
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let term2 = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        assert_ne!(term1, term2);
        backend.write(&skey("a"), b"z").unwrap();
        flush_writer(&backend, &skey("a"));
        // The old writer ends without sending the line (or answers it against
        // its own, gone, session).
        assert!(matches!(
            line.wait(Duration::from_secs(30)),
            Err(PtyBackendError::NoSuchSession)
        ));
        backend.writers.join_retired();
        assert_eq!(typed_text(&fake, &term2), "z");
        assert_eq!(fake.bridge_input(&term1), "", "the stuck write failed");
        assert_eq!(count(&fake, ops::Op::AgentPrompt), 0);
        assert!(send_text_args(&fake).is_empty());
    }

    #[test]
    fn a_new_generation_replaces_the_writer_without_a_kill() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, term1) = typing_backend(&fake, Duration::from_secs(30));
        // The old writer waits for a bridge with input in hand.
        fake.block_bridge_input(&term1, true);
        backend.write(&skey("a"), b"old").unwrap();
        // A same-key spawn replaces the session (and stops its bridge).
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let term2 = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        backend.write(&skey("a"), b"new").unwrap();
        flush_writer(&backend, &skey("a"));
        backend.writers.join_retired();
        assert_eq!(typed_text(&fake, &term2), "new");
        assert!(send_text_args(&fake).is_empty(), "the old pane is gone");
    }

    #[test]
    fn a_writer_restart_does_not_install_a_bridge_for_a_replaced_attach() {
        let fake = fake::FakeHerdr::new();
        let (backend, gen, _) = typing_backend(&fake, Duration::from_secs(30));
        let io = backend.writer_io(&skey("a"), gen);
        use writer::SessionIo as _;
        let (stdin, (cols, rows)) = io.bridge().unwrap();
        // While the restart's bridge is starting, a new attach takes the session.
        {
            let outputs = backend.outputs.clone();
            fake.on_call(
                ops::Op::SpawnBridge,
                Arc::new(move || {
                    outputs.register(&skey("a"), Arc::new(fake::CaptureOutput::default()));
                }),
            );
        }
        let free_during_kill = Arc::new(StdMutex::new(Vec::new()));
        {
            let bridges = Arc::clone(&backend.bridges);
            let free = Arc::clone(&free_during_kill);
            fake.on_bridge_kill(Arc::new(move || {
                free.lock().unwrap().push(bridges.try_lock().is_ok());
            }));
        }
        io.restart_bridge(&stdin, cols, rows).unwrap();
        assert_eq!(count(&fake, ops::Op::SpawnBridge), 2);
        assert!(
            !backend.bridges.lock().unwrap().contains_key(&skey("a")),
            "the surplus bridge is not installed over the newer attach's slot"
        );
        assert_eq!(
            *free_during_kill.lock().unwrap(),
            vec![true],
            "the surplus bridge is stopped outside the map lock"
        );
    }

    #[test]
    fn a_writer_restart_leaves_a_bridge_an_attach_installed_meanwhile() {
        let fake = fake::FakeHerdr::new();
        let (backend, gen, _) = typing_backend(&fake, Duration::from_secs(30));
        let io = backend.writer_io(&skey("a"), gen);
        use writer::SessionIo as _;
        let (stdin, _) = io.bridge().unwrap();
        // The failed bridge was already replaced by an attach at a newer size.
        backend
            .attach(skey("a"), Some((132, 50)), output())
            .unwrap();
        io.restart_bridge(&stdin, 80, 24).unwrap();
        assert_eq!(
            count(&fake, ops::Op::SpawnBridge),
            2,
            "spawn and attach only"
        );
        assert_eq!(bridge_size(&backend, &skey("a")), Some((132, 50)));
    }

    #[test]
    fn a_failed_attach_keeps_the_attach_it_would_have_replaced() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let live = backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        fake.fail_next(ops::Op::SpawnBridge, ops::HerdrError::Timeout);
        assert!(backend.attach(skey("a"), Some((80, 24)), output()).is_err());
        assert_eq!(backend.outputs.current_id(&skey("a")), Some(live.attach_id));
        // Its own detach still releases it.
        backend.detach(skey("a"), Some(live.attach_id)).unwrap();
        assert!(backend.outputs.get(&skey("a")).is_none());
    }

    #[test]
    fn a_failed_attach_leaves_the_replaced_attach_its_running_bridge() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let live_out = Arc::new(fake::CaptureOutput::default());
        backend
            .attach(skey("a"), Some((80, 24)), live_out.clone())
            .unwrap();
        let stdin = backend.bridges.lock().unwrap()[&skey("a")].stdin_handle();
        fake.fail_next(ops::Op::SpawnBridge, ops::HerdrError::Timeout);
        assert!(backend.attach(skey("a"), Some((80, 24)), output()).is_err());
        // The same bridge is still running, and its frames reach the live attach.
        let bridges = backend.bridges.lock().unwrap();
        assert!(Arc::ptr_eq(&bridges[&skey("a")].stdin_handle(), &stdin));
        drop(bridges);
        let term = backend.keymap.get(&skey("a")).unwrap().terminal_id;
        live_out.clear();
        fake.push_frame(
            &term,
            r#"{"type":"terminal.frame","bytes":"aGk=","full":false}"#,
        );
        assert!(
            live_out.wait_for(|e| !e.is_empty()),
            "the pane is not frozen"
        );
    }

    #[test]
    fn a_failed_attach_stops_the_bridge_of_an_attach_detached_meanwhile() {
        let fake = fake::FakeHerdr::new();
        let (backend, _, _) = typing_backend(&fake, Duration::from_secs(30));
        let live = backend.attach(skey("a"), Some((80, 24)), output()).unwrap();
        // While the next attach's bridge is starting (and failing), the live
        // attach's owner detaches: skipped, as the new attach is current.
        {
            let outputs = backend.outputs.clone();
            fake.on_call(
                ops::Op::SpawnBridge,
                Arc::new(move || {
                    assert!(!outputs.release(&skey("a"), Some(live.attach_id)));
                }),
            );
        }
        fake.fail_next(ops::Op::SpawnBridge, ops::HerdrError::Timeout);
        assert!(backend.attach(skey("a"), Some((80, 24)), output()).is_err());
        assert!(
            backend.outputs.current(&skey("a")).is_none(),
            "the detached attach is not brought back"
        );
        assert!(
            !backend.bridges.lock().unwrap().contains_key(&skey("a")),
            "its bridge is stopped, as its detach asked"
        );
    }

    #[test]
    fn kill_ends_the_sessions_writer() {
        let fake = fake::FakeHerdr::new();
        let (backend, gen, _) = typing_backend(&fake, Duration::from_secs(30));
        backend.write(&skey("a"), b"x").unwrap();
        assert!(backend.writers.contains(&skey("a")));
        backend.kill(&skey("a"), gen).unwrap();
        assert!(!backend.writers.contains(&skey("a")));
    }

    #[test]
    fn shutdown_clears_should_be_running() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend.keymap.begin_recovery(&[skey("a")]);
        backend.shutdown().unwrap();
        assert!(backend.keymap.should_be_running().is_empty());
        assert_eq!(backend.keymap.instance(), None);
    }

    #[test]
    fn note_server_death_persists_the_set_bumps_once_and_keeps_the_keymap() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        for w in ["a", "b", "quit"] {
            // Each pane is attached, so each session runs a bridge.
            backend.outputs.register(&skey(w), output());
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        record_quit(&fake, &skey("quit"));
        assert_eq!(backend.bridges.lock().unwrap().len(), 3);

        backend.death_handler().note_server_death();
        backend.death_handler().note_server_death();

        assert_eq!(
            KeyMap::with_path(fake.env().keymap_path()).should_be_running(),
            vec![skey("a"), skey("b")],
            "persisted, without the quit agent"
        );
        assert_eq!(backend.keymap.generation(), ops::Generation(1), "one bump");
        assert!(
            backend.bridges.lock().unwrap().is_empty(),
            "bridges stopped"
        );
        assert!(backend.live_set_snapshot().is_empty(), "live cleared");
        assert_eq!(backend.keymap.entries().len(), 3, "keymap kept");
        assert!(
            backend.keymap.get_current(&skey("a")).is_none(),
            "old-generation panes read as absent"
        );
        assert!(matches!(
            backend.write_line(&skey("a"), "hi", 0),
            Err(PtyBackendError::NoSuchSession)
        ));
    }

    fn death_event(source: watchdog::DeathSource) -> DeathEvent {
        DeathEvent {
            exit_code: Some(0xC000_0409u32 as i32),
            signal: None,
            source,
            uptime: Duration::from_secs(42),
            headroom_at_death: None,
            instance: ops::ServerInstance {
                pid: 1000,
                start_time: 1_000_000,
            },
            detected_at: std::time::SystemTime::now(),
        }
    }

    fn deaths(events: &[lifecycle::LifecycleEvent]) -> Vec<lifecycle::ServerDeathRecord> {
        events
            .iter()
            .filter_map(|e| match e {
                lifecycle::LifecycleEvent::ServerDeath(d) => Some(d.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn handle_death_records_and_notes_the_death_once() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let handler = backend.death_handler();
        let g0 = backend.keymap.generation();

        let ((), got) = lifecycle::with_capture(|| {
            // A report stamped with any other generation is stale.
            handler.handle_death(
                ops::Generation(g0.0 + 7),
                death_event(watchdog::DeathSource::Child),
            );
            assert_eq!(backend.keymap.generation(), g0, "stale report ignored");
            // The child's report is handled; the adopted report of the same death,
            // stamped with the same generation, is then a duplicate.
            handler.handle_death(g0, death_event(watchdog::DeathSource::Child));
            handler.handle_death(g0, death_event(watchdog::DeathSource::Adopted));
        });

        let recorded = deaths(&got);
        assert_eq!(recorded.len(), 1, "{got:?}");
        assert_eq!(recorded[0].source, "child");
        assert_eq!(recorded[0].pid, Some(1000));
        assert_eq!(recorded[0].exit_code, Some(0xC000_0409u32 as i32));
        assert_eq!(recorded[0].uptime_secs, Some(42));
        assert_eq!(backend.keymap.generation(), g0.next(), "noted exactly once");
        assert_eq!(backend.keymap.should_be_running(), vec![skey("a")]);
        assert!(backend.live_set_snapshot().is_empty());
    }

    #[test]
    fn bridge_exits_on_a_dead_server_share_one_status_probe() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.kill_server(Some(1));
        let before = count(&fake, ops::Op::StatusServer);
        // Each bridge holds its own authority; they decide together.
        let authorities: Vec<ExitAuthority> = (0..4).map(|_| backend.exit_authority()).collect();
        let threads: Vec<_> = authorities
            .iter()
            .map(|a| {
                let a = Arc::clone(a);
                std::thread::spawn(move || a())
            })
            .collect();
        for t in threads {
            assert!(!t.join().unwrap(), "not a pane exit: the server is gone");
        }
        assert!(!authorities[0]());
        assert_eq!(count(&fake, ops::Op::StatusServer) - before, 1);
    }

    #[test]
    fn a_reported_death_decides_bridge_exits_without_a_probe() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .watchdog
            .note_instance(fake.server_instance().unwrap());
        backend.watchdog.report_down_now();
        let before = count(&fake, ops::Op::StatusServer);
        assert!(!backend.exit_authority()());
        assert_eq!(count(&fake, ops::Op::StatusServer), before, "no probe");
    }

    #[test]
    fn a_dead_verdict_does_not_outlive_its_generation() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.kill_server(Some(1));
        assert!(!backend.exit_authority()());
        // The next server, in the next generation: a real pane exit again.
        backend.keymap.note_death_bump();
        let _ = fake.spawn_server().unwrap();
        assert!(backend.exit_authority()());
        assert!(backend.exit_authority()(), "a live answer is not memoised");
    }

    #[test]
    fn a_death_drops_the_warm_status_so_a_pulled_snapshot_sees_it() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend.status_cache.lock().unwrap().server = Some((up_status(), Instant::now()));
        backend.status_cache.lock().unwrap().stats.running = true;
        assert_eq!(backend.status_payload_snapshot().state, "Connected");

        fake.kill_server(Some(1));
        let ((), _) = lifecycle::with_capture(|| {
            backend.death_handler().handle_death(
                backend.keymap.generation(),
                death_event(watchdog::DeathSource::Child),
            )
        });
        assert_eq!(
            backend.status_payload_snapshot().state,
            "Disconnected",
            "the cached answer predates the death"
        );
        assert!(!backend.daemon_stats().running);
    }

    #[test]
    fn a_dead_child_server_reaches_the_death_pump() {
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        let (backend, events) = unready_backend(&fake, None);
        let backend = backend.with_headroom(Arc::new(crate::headroom::FakeHeadroom::unavailable()));
        backend.start();
        wait_for("the boot start", || backend.is_ready());
        wait_recovered(&backend);
        assert!(
            backend.supervisor.spawned_pid().is_some(),
            "boot spawned the server"
        );
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        let g = backend.keymap.generation();

        fake.kill_server(Some(0xC000_0409u32 as i32));
        // The pump bumps the generation first, then stops the bridges and clears
        // the live set: wait for the whole post-death state.
        wait_for("the death to be noted", || {
            backend.keymap.generation() == g.next() && backend.live_set_snapshot().is_empty()
        });
        assert_eq!(backend.keymap.should_be_running(), vec![skey("a")]);
        assert!(
            backend.bridges.lock().unwrap().is_empty(),
            "bridges stopped"
        );
        // The controller brings it back by itself (no memory wait or backoff in
        // tests) and pushes the healthy state last.
        wait_for("the automatic restart", || {
            count(&fake, ops::Op::SpawnServer) == 2
        });
        wait_recovered(&backend);
        wait_for("the healthy push", || {
            recovery_phases(&events).last().map(String::as_str) == Some("Healthy")
        });
        assert_eq!(
            count(&fake, ops::Op::SpawnServer),
            2,
            "boot, then the restart"
        );
    }

    #[test]
    fn shutdown_is_not_a_death() {
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        let (backend, _) = unready_backend(&fake, None);
        backend.start();
        wait_for("the boot start", || backend.is_ready());
        wait_recovered(&backend);
        assert!(backend.supervisor.spawned_pid().is_some());
        let g = backend.keymap.generation();
        backend.shutdown().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(backend.keymap.generation(), g, "no death noted");
    }

    fn claude_launch() -> AgentLaunch {
        AgentLaunch {
            cli: keitora_tasks::CliKind::Claude,
            path: "claude".into(),
            autonomy: false,
            resume_target: None,
            fresh: false,
        }
    }

    /// Every caller asks the gate first; while it is down they are refused with
    /// the recovering error, and none of them ever starts the server.
    #[test]
    fn a_down_gate_refuses_every_caller_and_nothing_starts_herdr() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let gen = backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );

        let pending =
            |r: Result<(), PtyBackendError>| matches!(r, Err(PtyBackendError::RecoveryPending));
        assert!(pending(
            backend
                .spawn(
                    skey("b"),
                    "/repos/b".into(),
                    String::new(),
                    Some((80, 24)),
                    None
                )
                .map(|_| ())
        ));
        assert!(pending(
            backend
                .spawn_agent(
                    skey("b"),
                    "/repos/b".into(),
                    String::new(),
                    Some((80, 24)),
                    claude_launch()
                )
                .map(|_| ())
        ));
        assert!(pending(
            backend
                .attach(skey("a"), Some((80, 24)), output())
                .map(|_| ())
        ));
        let line = backend.write_line(&skey("a"), "hi", 0);
        assert!(matches!(line, Err(PtyBackendError::RecoveryPending)));
        assert_eq!(
            crate::backend::map_write_line_result(&line),
            crate::backend::WriteLineOutcome::Queued
        );
        assert!(pending(backend.write(&skey("a"), b"x")));
        assert!(pending(backend.resize(&skey("a"), 100, 30)));
        assert!(pending(backend.repaint(&skey("a"))));
        assert!(pending(backend.kill(&skey("a"), gen)));
        assert!(backend.keymap.get(&skey("a")).is_some(), "kill was refused");
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
    }

    /// While the rest of the queue resumes, a session whose resume already
    /// succeeded takes attach, input, resizes and repaints; every other session
    /// and every keyless caller still waits, and a pause stops them all.
    #[test]
    fn gate_admits_a_settled_session_while_the_queue_resumes() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        for w in ["a", "b"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        backend.gate.set_view(
            recovery::RecoveryPhase::Resuming,
            HashSet::new(),
            HashSet::from([skey("a")]),
        );
        let refused = |r: Result<(), PtyBackendError>| {
            matches!(
                r,
                Err(PtyBackendError::RecoveryPending | PtyBackendError::RecoveryPaused)
            )
        };
        // The settled session.
        assert!(!refused(
            backend
                .attach(skey("a"), Some((80, 24)), output())
                .map(|_| ())
        ));
        assert!(!refused(backend.write(&skey("a"), b"x")));
        assert!(!refused(backend.resize(&skey("a"), 100, 30)));
        assert!(!refused(backend.repaint(&skey("a"))));
        assert!(!refused(backend.write_line(&skey("a"), "hi", 0)));
        // A session still queued.
        assert!(refused(
            backend
                .attach(skey("b"), Some((80, 24)), output())
                .map(|_| ())
        ));
        assert!(refused(backend.write(&skey("b"), b"x")));
        assert!(refused(backend.write_line(&skey("b"), "hi", 0)));
        // Keyless callers wait for Healthy, the settled key's own included.
        assert!(refused(
            backend
                .spawn(
                    skey("c"),
                    "/repos/c".into(),
                    String::new(),
                    Some((80, 24)),
                    None
                )
                .map(|_| ())
        ));
        // A kill of the queued session waits too.
        let gen_b = backend.keymap.get(&skey("b")).unwrap().gen;
        assert!(refused(backend.kill(&skey("b"), gen_b)));
        assert!(backend.keymap.get(&skey("b")).is_some());

        backend.gate.set_view(
            recovery::RecoveryPhase::Paused,
            HashSet::new(),
            HashSet::from([skey("a")]),
        );
        assert!(matches!(
            backend.write(&skey("a"), b"x"),
            Err(PtyBackendError::RecoveryPaused)
        ));
        assert!(matches!(
            backend
                .attach(skey("a"), Some((80, 24)), output())
                .map(|_| ()),
            Err(PtyBackendError::RecoveryPaused)
        ));
        let gen_a = backend.keymap.get(&skey("a")).unwrap().gen;
        assert!(matches!(
            backend.kill(&skey("a"), gen_a),
            Err(PtyBackendError::RecoveryPaused)
        ));
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
    }

    /// A session that is live during recovery (resumed already, or the
    /// driver's own resume in flight) can be killed; the kill closes its pane.
    #[test]
    fn kill_takes_down_a_settled_or_in_flight_session_during_recovery() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let mut gens = HashMap::new();
        for w in ["settled", "inflight", "queued", "waiting"] {
            let gen = backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
            gens.insert(w, gen);
        }
        backend.gate.set_view(
            recovery::RecoveryPhase::Resuming,
            HashSet::from([skey("inflight")]),
            HashSet::from([skey("settled")]),
        );
        backend.kill(&skey("settled"), gens["settled"]).unwrap();
        backend.kill(&skey("inflight"), gens["inflight"]).unwrap();
        assert!(backend.keymap.get(&skey("settled")).is_none());
        assert!(backend.keymap.get(&skey("inflight")).is_none());
        assert!(!backend.is_live(&skey("settled")));
        assert_eq!(fake.closed_pane_ids().len(), 2);
        assert!(matches!(
            backend.kill(&skey("queued"), gens["queued"]),
            Err(PtyBackendError::RecoveryPending)
        ));
        assert!(backend.keymap.get(&skey("queued")).is_some());
        // Waiting for memory: a resumed session is still live and killable.
        backend.gate.set_view(
            recovery::RecoveryPhase::AwaitHeadroom,
            HashSet::new(),
            HashSet::from([skey("waiting")]),
        );
        backend.kill(&skey("waiting"), gens["waiting"]).unwrap();
        assert!(backend.keymap.get(&skey("waiting")).is_none());
        assert_eq!(fake.closed_pane_ids().len(), 3);
    }

    /// The terminate flag takes the key's spawn lock: it waits for a resume
    /// that is launching the key.
    #[test]
    fn set_terminated_waits_for_the_keys_spawn_lock() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let backend = Arc::new(backend);
        let lock = Arc::clone(
            backend
                .spawn_locks
                .lock()
                .unwrap()
                .entry(skey("a"))
                .or_default(),
        );
        let held = lock.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let b = Arc::clone(&backend);
        let t = std::thread::spawn(move || {
            b.set_terminated(&skey("a"), true).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the flag waited for the lock"
        );
        assert!(!backend.store.is_terminated(&skey("a")));
        drop(held);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("the flag is set once the lock is free");
        t.join().unwrap();
        assert!(backend.store.is_terminated(&skey("a")));
    }

    /// A resume that won the key's lock before the terminate started a new
    /// incarnation: the terminate kills that one, not the generation the caller
    /// saw, and drops the key from recovery.
    #[test]
    fn terminate_after_a_racing_resume_kills_the_session_it_started() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        let (backend, rx) = backend.with_recording_recovery();
        backend.keymap.begin_recovery(&[skey("a")]);
        backend.gate.set_view(
            recovery::RecoveryPhase::Resuming,
            HashSet::from([skey("a")]),
            HashSet::new(),
        );
        // The resume ran first (it held the key's lock).
        let report = run_resume(&backend, "a");
        assert_ne!(report.outcome, ResumeOutcomeKind::Cancelled);
        let started = backend.keymap.get(&skey("a")).unwrap().gen;
        // The terminate, with the generation the rail saw before the resume.
        backend.set_terminated(&skey("a"), true).unwrap();
        backend.kill_live(&skey("a"), Some(started + 7)).unwrap();
        assert!(
            backend.keymap.get(&skey("a")).is_none(),
            "the new one is gone"
        );
        assert_eq!(fake.closed_pane_ids().len(), 1);
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(sent, vec![recovery::machine::Event::Cancel(skey("a"))]);
    }

    /// A terminate with nothing live (the key is queued or suspended) still
    /// drops it from recovery, while recovery refuses the kill itself.
    #[test]
    fn terminate_with_nothing_live_still_cancels_the_keys_recovery() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (backend, rx) = backend.with_recording_recovery();
        backend.keymap.begin_recovery(&[skey("a"), skey("b")]);
        backend.gate.set_view(
            recovery::RecoveryPhase::AwaitHeadroom,
            HashSet::new(),
            HashSet::new(),
        );
        backend.set_terminated(&skey("a"), true).unwrap();
        assert!(matches!(
            backend.kill_live(&skey("a"), None),
            Err(PtyBackendError::RecoveryPending)
        ));
        let sent: Vec<_> = rx.try_iter().collect();
        assert_eq!(sent, vec![recovery::machine::Event::Cancel(skey("a"))]);
        assert_eq!(backend.keymap.should_be_running(), vec![skey("b")]);
        assert_eq!(count(&fake, ops::Op::PaneClose), 0);
    }

    /// In no gate state, and not with the server down either, does a spawn or
    /// attach start herdr.
    #[test]
    fn server_not_started_by_spawn_while_gate_refuses() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.kill_server(Some(1));
        let try_all = |backend: &HerdrBackend| {
            let _ = backend.spawn(
                skey("x"),
                "/repos/x".into(),
                String::new(),
                Some((80, 24)),
                None,
            );
            let _ = backend.spawn_agent(
                skey("x"),
                "/repos/x".into(),
                String::new(),
                Some((80, 24)),
                claude_launch(),
            );
            let _ = backend.attach(skey("x"), Some((80, 24)), output());
            let _ = backend.resume_session(&skey("x"), None);
        };
        try_all(&backend);
        for phase in [
            recovery::RecoveryPhase::Down,
            recovery::RecoveryPhase::AwaitHeadroom,
            recovery::RecoveryPhase::Restarting,
            recovery::RecoveryPhase::Reconciling,
            recovery::RecoveryPhase::Resuming,
            recovery::RecoveryPhase::Paused,
            recovery::RecoveryPhase::Healthy,
        ] {
            backend.gate.set_view(phase, HashSet::new(), HashSet::new());
            try_all(&backend);
            backend
                .gate
                .set_view(phase, HashSet::from([skey("x")]), HashSet::new());
            try_all(&backend);
        }
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
    }

    #[test]
    fn spawn_before_the_boot_is_ready_is_not_connected() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = unready_backend(&fake, None);
        assert!(matches!(
            backend.spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None
            ),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(matches!(
            backend.spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                claude_launch()
            ),
            Err(PtyBackendError::NotConnected)
        ));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    /// Shell and agent panes both get explicit XDG config/state values, so the
    /// server's isolated dirs never reach the tools running in them, and never a
    /// synthesised data/cache/runtime dir. Assumes the test host has a home dir.
    #[test]
    fn spawned_panes_get_the_users_xdg_env() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend
            .spawn_agent(
                skey("b"),
                "/repos/b".into(),
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
            .unwrap();
        let creates: Vec<String> = fake
            .calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::WorkspaceCreate)
            .map(|c| c.args)
            .collect();
        assert_eq!(creates.len(), 2, "{creates:?}");
        for args in &creates {
            // Never an empty state dir; an empty config dir only off Linux/BSD.
            assert!(args.contains("(\"XDG_STATE_HOME\", \""), "{args}");
            assert!(!args.contains("(\"XDG_STATE_HOME\", \"\")"), "{args}");
            let empty_config = args.contains("(\"XDG_CONFIG_HOME\", \"\")");
            assert!(args.contains("(\"XDG_CONFIG_HOME\", \""), "{args}");
            assert_eq!(
                empty_config,
                cfg!(windows) || cfg!(target_os = "macos"),
                "{args}"
            );
            for name in ["XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"] {
                assert!(!args.contains(name), "{args}");
            }
            assert!(!args.contains("HERDR_"), "{args}");
            #[cfg(unix)]
            assert!(
                args.contains("(\"PATH\", \"/fake/bin:/usr/bin\")"),
                "{args}"
            );
        }
    }

    /// A spawn that finds the server not running reports the death at once and
    /// is refused; it does not start the server.
    #[test]
    fn spawn_agent_with_the_server_down_reports_a_death_instead_of_starting_it() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.kill_server(Some(1));
        assert!(matches!(
            backend.spawn_agent(
                skey("b"),
                "/repos/b".into(),
                String::new(),
                Some((80, 24)),
                claude_launch()
            ),
            Err(PtyBackendError::RecoveryPending)
        ));
        let rx = backend.death_rx.lock().unwrap();
        let rx = rx.as_ref().unwrap();
        let (_, ev) = rx.try_recv().expect("one death report");
        assert_eq!(ev.source, watchdog::DeathSource::Adopted);
        assert!(rx.try_recv().is_err());
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert_eq!(count(&fake, ops::Op::WorkspaceCreate), 0);
    }

    #[test]
    fn handled_death_reaches_the_controller_with_the_bumped_generation() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        backend.start_recovery_driver();
        let g0 = backend.keymap.generation();
        fake.kill_server(Some(0xC000_0409u32 as i32));
        backend
            .death_handler()
            .handle_death(g0, death_event(watchdog::DeathSource::Child));
        assert_eq!(backend.keymap.generation(), g0.next());
        let status = events
            .lock()
            .unwrap()
            .iter()
            .position(|(e, _)| e == "daemon://status");
        assert!(status.is_some(), "status pushed by the pump");
        // The controller works in the bumped generation and restarts herdr.
        wait_for("the controller's down push", || {
            events.lock().unwrap().iter().any(|(e, v)| {
                e == gate::RECOVERY_EVENT
                    && v["phase"] != "Healthy"
                    && v["generation"] == g0.next().0
            })
        });
        wait_for("the restart", || count(&fake, ops::Op::SpawnServer) == 1);
        wait_recovered(&backend);
    }

    /// A keymap that never learned its server (written before identities were
    /// kept): after a death the restored shells are still replaced and the
    /// sessions resumed, never adopted as if they were the agents.
    #[test]
    fn a_death_without_a_remembered_server_still_resumes_the_sessions() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        for w in ["a", "b"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        assert_eq!(backend.keymap.instance(), None);
        backend.start_recovery_driver();
        let g0 = backend.keymap.generation();
        fake.kill_server(Some(1));
        backend
            .death_handler()
            .handle_death(g0, death_event(watchdog::DeathSource::Child));
        wait_for("both sessions resumed", || started_agents(&fake).len() == 2);
        wait_recovered(&backend);
        assert!(backend.keymap.should_be_running().is_empty());
        assert!(
            fake.panes().iter().all(|p| p.agent.is_some()),
            "no restored shell left: {:?}",
            fake.panes()
        );
    }

    /// A backend whose session "a" is spawned, with a pane output attached to it.
    fn attached_spawn(fake: &Arc<fake::FakeHerdr>) -> (HerdrBackend, Arc<fake::CaptureOutput>) {
        let (backend, _) = fake_backend(fake);
        let out = Arc::new(fake::CaptureOutput::default());
        backend.outputs.register(&skey("a"), out.clone());
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        (backend, out)
    }

    #[test]
    fn a_bridge_dropping_while_the_gate_is_down_is_not_a_pane_exit() {
        let fake = fake::FakeHerdr::new();
        let (backend, out) = attached_spawn(&fake);
        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );
        fake.kill_server(Some(1));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(out.exits(), 0);
    }

    /// The stream can end before the death is handled (the gate still
    /// healthy): the server not answering is enough to withhold the exit.
    #[test]
    fn a_bridge_dropping_before_the_death_is_handled_is_not_a_pane_exit() {
        let fake = fake::FakeHerdr::new();
        // Held so its bridges live until the end.
        let (_backend, out) = attached_spawn(&fake);
        let probes = count(&fake, ops::Op::StatusServer);
        fake.kill_server(Some(1));
        wait_for("the authority to ask the server", || {
            count(&fake, ops::Op::StatusServer) > probes
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(out.exits(), 0);
    }

    #[test]
    fn a_pane_closed_by_herdr_is_still_an_exit() {
        let fake = fake::FakeHerdr::new();
        let (backend, out) = attached_spawn(&fake);
        let pane = backend.keymap.get(&skey("a")).unwrap();
        fake.push_frame(&pane.terminal_id, r#"{"type":"terminal.closed"}"#);
        wait_for("the exit", || out.exits() == 1);
    }

    #[test]
    fn resume_session_is_admitted_only_for_the_key_in_flight() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );
        assert!(matches!(
            backend.resume_session(&skey("a"), Some(ResumeDecision::Continue)),
            Err(PtyBackendError::RecoveryPending)
        ));
        backend.gate.set_view(
            recovery::RecoveryPhase::Resuming,
            HashSet::from([skey("a")]),
            HashSet::new(),
        );
        assert!(matches!(
            backend.resume_session(&skey("b"), Some(ResumeDecision::Continue)),
            Err(PtyBackendError::RecoveryPending)
        ));
        assert_eq!(
            backend
                .resume_session(&skey("a"), Some(ResumeDecision::Continue))
                .unwrap(),
            Some(ResumeResult::Continued)
        );
        assert_eq!(count(&fake, ops::Op::AgentStart), 1);
    }

    #[test]
    fn take_xdg_leak_notice_reads_the_backend_home_once() {
        let fake = fake::FakeHerdr::new();
        std::fs::create_dir_all(fake.env().home.join("config").join("gh")).unwrap();
        let (backend, _) = fake_backend(&fake);
        assert_eq!(
            backend.take_xdg_leak_notice(),
            Some(vec!["config/gh".to_string()])
        );
        assert_eq!(backend.take_xdg_leak_notice(), None);
    }

    #[test]
    fn recovery_snapshot_and_handle_come_from_the_controller() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        assert_eq!(
            backend.recovery_snapshot().phase,
            recovery::RecoveryPhase::Healthy
        );
        let handle = PtyBackend::recovery(&backend).expect("herdr has a controller");
        assert_eq!(handle.snapshot(), backend.recovery_snapshot());
    }

    /// `restart` reconciles the restarted server's restored panes.
    #[test]
    fn restart_reconciles_the_new_server() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend.keymap.set_instance(fake.server_instance());
        fake.kill_server(Some(1));

        backend.start_recovery_driver();
        backend.restart().unwrap();

        wait_recovered(&backend);
        assert_eq!(backend.keymap.instance(), fake.server_instance());
        assert_eq!(backend.keymap.should_be_running(), vec![skey("a")]);
        assert!(backend.keymap.get(&skey("a")).is_none());
        assert_eq!(fake.closed_pane_ids().len(), 1, "the restored shell closed");
    }

    /// Wait until the controller is idle again (`Healthy`).
    fn wait_recovered(backend: &HerdrBackend) {
        wait_for("recovery to finish", || {
            backend.gate.phase() == recovery::RecoveryPhase::Healthy
        });
    }

    /// Wait until the controller pauses.
    fn wait_paused(backend: &HerdrBackend) {
        wait_for("recovery to pause", || {
            backend.recovery_snapshot().phase == recovery::RecoveryPhase::Paused
        });
    }

    /// The phases pushed on `daemon://recovery`, consecutive repeats folded.
    fn recovery_phases(events: &Events) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (e, v) in events.lock().unwrap().iter() {
            if e != gate::RECOVERY_EVENT {
                continue;
            }
            let p = v["phase"].as_str().unwrap_or_default().to_string();
            if out.last() != Some(&p) {
                out.push(p);
            }
        }
        out
    }

    /// The agent name of every `agent start` call, in order.
    fn started_agents(fake: &fake::FakeHerdr) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::AgentStart)
            .map(|c| c.args.split(' ').next().unwrap_or_default().to_string())
            .collect()
    }

    fn agent_name_of(w: &str) -> String {
        agent::agent_name(agent::herdr_kind(keitora_tasks::CliKind::Claude), &skey(w))
    }

    /// Records each pid it is asked to kill; kills nothing.
    fn recording_killer() -> (supervisor::Killer, Arc<StdMutex<Vec<u32>>>) {
        let killed = Arc::new(StdMutex::new(Vec::new()));
        let k = Arc::clone(&killed);
        let killer: supervisor::Killer = Arc::new(move |pid| k.lock().unwrap().push(pid));
        (killer, killed)
    }

    fn fast_stop(backend: HerdrBackend, killer: supervisor::Killer) -> HerdrBackend {
        backend.tune_supervisor(|s| {
            s.with_stop(Duration::from_millis(30), Duration::from_millis(30), killer)
        })
    }

    /// Two plain sessions on the fake's first server, recorded against it.
    fn two_sessions(backend: &HerdrBackend, fake: &fake::FakeHerdr) {
        for w in ["a", "b"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        backend.keymap.set_instance(fake.server_instance());
    }

    #[test]
    fn restart_stops_starts_reconciles_and_resumes_each_session_in_order() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend_with(&fake, Some(launch_everything()));
        two_sessions(&backend, &fake);
        let first = fake.server_instance();

        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_recovered(&backend);

        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
        assert_eq!(count(&fake, ops::Op::ServerStop), 1);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 1);
        assert_ne!(fake.server_instance(), first);
        assert_eq!(
            backend.keymap.instance(),
            fake.server_instance(),
            "reconciled against the new server"
        );
        assert_eq!(
            started_agents(&fake),
            vec![agent_name_of("a"), agent_name_of("b")]
        );
        assert!(backend.keymap.should_be_running().is_empty());
        assert!(backend.keymap.get_current(&skey("a")).is_some());
        assert!(backend.keymap.get_current(&skey("b")).is_some());
        assert_eq!(
            recovery_phases(&events),
            vec!["Restarting", "Reconciling", "Resuming", "Healthy"]
        );
        let last_recovery = events
            .lock()
            .unwrap()
            .iter()
            .rposition(|(e, _)| e == gate::RECOVERY_EVENT)
            .unwrap();
        let last_status = events
            .lock()
            .unwrap()
            .iter()
            .rposition(|(e, _)| e == "daemon://status")
            .unwrap();
        assert!(last_status > last_recovery, "status pushed after Healthy");
    }

    #[test]
    fn restart_reports_restarting_before_it_returns() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        // Hold the cycle open at the stop: over the in-memory fake the whole restart
        // otherwise finishes between the wait's samples and a finished Healthy is genuine.
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(StdMutex::new(rx));
        fake.on_call(
            ops::Op::ServerStop,
            Arc::new(move || {
                let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
            }),
        );
        backend.start_recovery_driver();
        backend.restart().unwrap();
        assert_ne!(
            backend.recovery_snapshot().phase,
            recovery::RecoveryPhase::Healthy,
            "never a stale Healthy once restart returns"
        );
        tx.send(()).unwrap();
        wait_recovered(&backend);
    }

    #[test]
    fn a_second_restart_during_a_run_is_a_no_op() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let entered = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(StdMutex::new(rx));
        {
            let entered = Arc::clone(&entered);
            fake.on_call(
                ops::Op::ServerStop,
                Arc::new(move || {
                    entered.store(true, Ordering::SeqCst);
                    let _ = rx.lock().unwrap().recv_timeout(Duration::from_secs(10));
                }),
            );
        }
        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_for("the stop to begin", || entered.load(Ordering::SeqCst));
        backend.start_recovery_driver();
        backend.restart().unwrap();
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Restarting);
        tx.send(()).unwrap();
        wait_recovered(&backend);
        assert_eq!(count(&fake, ops::Op::ServerStop), 1);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 1);
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
    }

    #[test]
    fn a_server_that_will_not_stop_is_killed_by_pid_and_the_restart_retried() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (killer, killed) = recording_killer();
        let backend = fast_stop(backend, killer);
        fake.fail_next(ops::Op::ServerStop, ops::HerdrError::Timeout);

        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_for("the kill", || !killed.lock().unwrap().is_empty());
        wait_recovered(&backend);

        assert_eq!(killed.lock().unwrap()[0], 1000, "the running server's pid");
        // The failed stop counts as a failed start; the retry stops it and
        // starts a new server.
        assert_eq!(count(&fake, ops::Op::SpawnServer), 1);
    }

    #[test]
    fn a_server_that_will_not_start_pauses_recovery() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.fail_spawn_server(Some(std::io::ErrorKind::PermissionDenied));
        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_paused(&backend);
        let s = backend.recovery_snapshot();
        assert_eq!(s.reason_code, Some("start_failed"));
        assert!(s.can_resume_now);
        assert_eq!(
            count(&fake, ops::Op::SpawnServer),
            recovery::machine::MAX_START_FAILURES as usize,
            "retried, then paused instead of looping"
        );
    }

    #[test]
    fn a_death_while_resuming_is_recovered_by_the_next_run() {
        let fake = fake::FakeHerdr::new();
        let keymap_slot: Arc<StdMutex<Option<Arc<KeyMap>>>> = Arc::new(StdMutex::new(None));
        let killed = Arc::new(AtomicBool::new(false));
        let launcher: LaunchResolver = {
            let (fake, slot, killed) = (
                Arc::clone(&fake),
                Arc::clone(&keymap_slot),
                Arc::clone(&killed),
            );
            Arc::new(move |k: &SessionKey| {
                // The server dies just as the first session is resumed.
                if !killed.swap(true, Ordering::SeqCst) {
                    let km = slot.lock().unwrap().clone().unwrap();
                    let before = km.generation();
                    fake.kill_server(Some(0xC000_0409u32 as i32));
                    wait_for("the death to be handled", || km.generation() != before);
                }
                Some((format!("/repos/{}", k.worktree), claude_launch()))
            })
        };
        let (backend, _) = fake_backend_with(&fake, Some(launcher));
        *keymap_slot.lock().unwrap() = Some(Arc::clone(&backend.keymap));
        backend.start_death_pump();
        two_sessions(&backend, &fake);

        backend.start_recovery_driver();
        backend.restart().unwrap();
        // Both resumes were in flight when the server died, so the next run
        // gets half of them: it resumes `a` and leaves `b` suspended.
        wait_for("the first session resumed", || {
            started_agents(&fake).contains(&agent_name_of("a"))
        });
        wait_recovered(&backend);
        assert_eq!(
            count(&fake, ops::Op::SpawnServer),
            2,
            "the restart, then the restart after the death"
        );
        let suspended: Vec<String> = backend
            .recovery_snapshot()
            .queue
            .iter()
            .filter(|q| q.state == "suspended")
            .map(|q| q.worktree.clone())
            .collect();
        assert_eq!(suspended, vec!["b".to_string()]);
        assert!(!started_agents(&fake).contains(&agent_name_of("b")));

        // "Resume all" brings back the one the budget held back.
        backend.recovery.resume_now();
        wait_for("the second session resumed", || {
            started_agents(&fake).contains(&agent_name_of("b"))
        });
        wait_recovered(&backend);
        assert!(backend.keymap.should_be_running().is_empty());
    }

    #[test]
    fn the_resume_uses_the_launch_settings_current_at_resume_time() {
        let fake = fake::FakeHerdr::new();
        let autonomy = Arc::new(AtomicBool::new(false));
        let launcher: LaunchResolver = {
            let autonomy = Arc::clone(&autonomy);
            Arc::new(move |k: &SessionKey| {
                Some((
                    format!("/repos/{}", k.worktree),
                    AgentLaunch {
                        autonomy: autonomy.load(Ordering::SeqCst),
                        ..claude_launch()
                    },
                ))
            })
        };
        let (backend, _) = fake_backend_with(&fake, Some(launcher));
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend.keymap.set_instance(fake.server_instance());
        // Toggled after the session started, before the restart resumes it.
        autonomy.store(true, Ordering::SeqCst);

        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_recovered(&backend);

        let args = agent_start_args(&fake);
        assert_eq!(args.len(), 1);
        assert!(
            args[0].contains("--dangerously-skip-permissions"),
            "{args:?}"
        );
    }

    // ---- launch-time classification ----

    /// Boot and wait until the launch decision has played out.
    fn boot(backend: &HerdrBackend) {
        backend.start();
        // The latch is released only once the gate reflects the boot decision.
        wait_for("the boot decision", || backend.is_ready());
        wait_recovered(backend);
    }

    /// A session `w` from an earlier Keitora run on the fake's current server.
    fn remembered_session(backend: &HerdrBackend, fake: &fake::FakeHerdr, w: &str) {
        let cwd = format!("/repos/{w}");
        let pane = fake.add_pane(&cwd, None);
        backend.keymap.record(
            skey(w),
            pane.workspace_id,
            pane.pane_id,
            pane.terminal_id,
            cwd,
        );
        backend.keymap.set_instance(fake.server_instance());
    }

    #[test]
    fn boot_that_cannot_reach_herdr_starts_nothing_and_still_signals_ready() {
        let fake = fake::FakeHerdr::new();
        fake.hang(ops::Op::StatusServer, true);
        let (backend, events) = unready_backend(&fake, None);
        backend.start();
        wait_for("the boot decision", || backend.is_ready());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert!(
            recovery_phases(&events).is_empty(),
            "nothing handed to recovery"
        );
        assert_eq!(
            backend.recovery_snapshot().phase,
            recovery::RecoveryPhase::Healthy
        );
    }

    // ---- herdr not found ----

    /// A backend whose herdr executable does not exist and whose server is not
    /// running, booted until the launch decision is made.
    fn booted_without_herdr() -> (Arc<fake::FakeHerdr>, HerdrBackend, Events) {
        booted_without_herdr_with(|b| b)
    }

    /// [`booted_without_herdr`], with `tune` applied before the boot.
    fn booted_without_herdr_with(
        tune: impl FnOnce(HerdrBackend) -> HerdrBackend,
    ) -> (Arc<fake::FakeHerdr>, HerdrBackend, Events) {
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        fake.set_exe_missing(true);
        let (backend, events) = unready_backend(&fake, None);
        let backend = tune(backend);
        backend.start();
        wait_for("the boot decision", || backend.is_ready());
        wait_for("the not-found status", || {
            last_event(&events, "daemon://status").is_some_and(|v| v.get("reason").is_some())
                && last_event(&events, gate::RECOVERY_EVENT).is_some()
        });
        (fake, backend, events)
    }

    /// The latest `event` payload pushed to the frontend.
    fn last_event(events: &Events, event: &str) -> Option<Value> {
        events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(e, _)| e == event)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn boot_without_herdr_reports_not_found_on_both_payloads_and_spawns_nothing() {
        let (fake, backend, events) = booted_without_herdr();
        let message = backend.exe_missing.message().expect("the state is set");
        assert!(message.starts_with("herdr was not found."), "{message}");
        assert!(message.contains("[pty] herdr_bin"), "{message}");
        assert!(message.contains("keitora.toml"), "{message}");

        // Pushed.
        let status = last_event(&events, "daemon://status").unwrap();
        assert_eq!(status["state"], "Disconnected");
        assert_eq!(status["reason"], Value::String(message.clone()));
        assert_eq!(status["restartable"], true);
        let recovery = last_event(&events, gate::RECOVERY_EVENT).unwrap();
        assert_eq!(recovery["reason"], Value::String(message.clone()));
        assert_eq!(recovery["reason_code"], exe_missing::NOT_FOUND_CODE);
        assert_eq!(
            recovery["phase"], "Healthy",
            "nothing was handed to recovery"
        );

        // Pulled: the same.
        let status = backend.status_payload_snapshot();
        assert_eq!(status.state, "Disconnected");
        assert_eq!(status.reason, Some(message.clone()));
        assert!(status.restartable);
        let recovery = backend.recovery_snapshot();
        assert_eq!(recovery.reason, Some(message));
        assert_eq!(recovery.reason_code, Some(exe_missing::NOT_FOUND_CODE));
        assert_eq!(recovery.phase, recovery::RecoveryPhase::Healthy);

        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert_eq!(count(&fake, ops::Op::ServerStop), 0);
        assert_eq!(fake.rediscoveries(), 0, "the launch search is not repeated");
    }

    #[test]
    fn restart_while_herdr_is_still_missing_fails_at_once_and_never_restarts() {
        // Stop and kill waits long enough that sitting through either fails.
        let (killer, killed) = recording_killer();
        let (fake, backend, events) = booted_without_herdr_with(|b| {
            b.tune_supervisor(|s| {
                s.with_stop(Duration::from_secs(30), Duration::from_secs(30), killer)
            })
        });
        let phases_before = recovery_phases(&events);
        events.lock().unwrap().clear();
        let probes_before = count(&fake, ops::Op::StatusServer);

        let t0 = Instant::now();
        let err = backend.restart().expect_err("nothing can be restarted");
        assert_eq!(
            count(&fake, ops::Op::StatusServer) - probes_before,
            1,
            "one confirming probe, and none again for the status push"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "took {:?}",
            t0.elapsed()
        );
        let message = backend.exe_missing.message().expect("still set");
        assert!(matches!(&err, PtyBackendError::Other(m) if *m == message));
        // The search ran again, and its locations are the ones named now.
        assert_eq!(fake.rediscoveries(), 1);
        for place in fake::FAKE_SEARCHED {
            assert!(message.contains(place), "{message}");
        }

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
        assert!(
            recovery_phases(&events).iter().all(|p| p == "Healthy"),
            "never Restarting: {:?} (before: {phases_before:?})",
            recovery_phases(&events)
        );
        assert_eq!(count(&fake, ops::Op::ServerStop), 0);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert!(killed.lock().unwrap().is_empty());

        // Both payloads were pushed again with the new text.
        let status = last_event(&events, "daemon://status").unwrap();
        assert_eq!(status["reason"], Value::String(message.clone()));
        let recovery = last_event(&events, gate::RECOVERY_EVENT).unwrap();
        assert_eq!(recovery["reason"], Value::String(message));
        assert_eq!(recovery["reason_code"], exe_missing::NOT_FOUND_CODE);
    }

    #[test]
    fn restart_with_herdr_found_hands_off_without_probing_the_server() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (backend, rx) = backend.with_recording_recovery();
        let probes_before = count(&fake, ops::Op::StatusServer);

        backend.restart().expect("herdr is there");
        // The search found herdr, so the caller waits for no herdr call.
        assert_eq!(fake.rediscoveries(), 1);
        assert_eq!(count(&fake, ops::Op::StatusServer), probes_before);
        let sent: Vec<_> = rx.try_iter().collect();
        assert!(
            matches!(
                sent.as_slice(),
                [recovery::machine::Event::UserRestart { .. }]
            ),
            "{sent:?}"
        );
    }

    #[test]
    fn a_status_push_that_sees_herdr_vanish_or_return_also_pushes_recovery() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let parts = backend.recovery_parts();
        let pushed = || -> Vec<Value> {
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| e == gate::RECOVERY_EVENT)
                .map(|(_, v)| v.clone())
                .collect()
        };

        parts.emit_status();
        assert!(pushed().is_empty(), "nothing changed");

        // This status push is the first call to see herdr gone.
        fake.set_exe_missing(true);
        parts.emit_status();
        let message = backend.exe_missing.message().expect("the state is set");
        let seen = pushed();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0]["reason"], Value::String(message.clone()));
        assert_eq!(seen[0]["reason_code"], exe_missing::NOT_FOUND_CODE);
        let status = last_event(&events, "daemon://status").unwrap();
        assert_eq!(status["reason"], Value::String(message));

        // Still gone: nothing new to say.
        parts.emit_status();
        assert_eq!(pushed().len(), 1);

        // And it is the first to see herdr back: the code is withdrawn.
        fake.set_exe_missing(false);
        parts.emit_status();
        assert_eq!(backend.exe_missing.message(), None);
        let seen = pushed();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[1]["reason_code"].is_null(), "{}", seen[1]);
        assert!(seen[1]["reason"].is_null(), "{}", seen[1]);
        let status = last_event(&events, "daemon://status").unwrap();
        assert!(status.get("reason").is_none(), "{status}");
    }

    #[test]
    fn restart_after_herdr_appears_starts_the_server_and_clears_the_message() {
        let (fake, backend, events) = booted_without_herdr();
        assert!(backend.restart().is_err(), "still missing");

        // The user installs herdr and presses Restart again.
        fake.set_exe_missing(false);
        backend.restart().expect("herdr is there now");
        wait_for("the server to be started", || {
            count(&fake, ops::Op::SpawnServer) == 1
        });
        wait_recovered(&backend);

        assert_eq!(backend.exe_missing.message(), None);
        assert!(fake.status_server().unwrap().running);
        assert!(backend.is_ready());
        assert_eq!(backend.keymap.instance(), fake.server_instance());
        let status = backend.status_payload_snapshot();
        assert_eq!(status.state, "Connected");
        assert_eq!(status.reason, None);
        let recovery = backend.recovery_snapshot();
        assert_eq!(recovery.phase, recovery::RecoveryPhase::Healthy);
        assert_eq!((recovery.reason, recovery.reason_code), (None, None));
        // No pushed recovery payload after the restart carries the stale text.
        let pushed = last_event(&events, gate::RECOVERY_EVENT).unwrap();
        assert!(pushed["reason_code"].is_null(), "{pushed}");
        wait_for("the connected status push", || {
            last_event(&events, "daemon://status").is_some_and(|v| v["state"] == "Connected")
        });
        let pushed = last_event(&events, "daemon://status").unwrap();
        assert!(pushed.get("reason").is_none(), "{pushed}");

        // The backend is usable: a session can be spawned on the new server.
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .expect("spawn on the restarted server");
    }

    #[test]
    fn a_recovery_publish_neither_drops_a_true_message_nor_keeps_a_stale_one() {
        let (_fake, backend, events) = booted_without_herdr();
        let message = backend.exe_missing.message().unwrap();
        // What the controller publishes knows nothing of the herdr executable.
        let published = recovery::RecoveryPayload {
            phase: recovery::RecoveryPhase::Paused,
            reason: Some("herdr could not be started.".into()),
            reason_code: Some("start_failed"),
            seq: 9,
            ..recovery::RecoveryPayload::healthy()
        };
        let mut executor = backend.executor();

        events.lock().unwrap().clear();
        executor.emit(&published);
        let shown = last_event(&events, gate::RECOVERY_EVENT).unwrap();
        assert_eq!(shown["reason"], Value::String(message));
        assert_eq!(shown["reason_code"], exe_missing::NOT_FOUND_CODE);
        assert_eq!(shown["phase"], "Paused");
        assert_eq!(shown["seq"], 9);

        // herdr is back: the very same publish goes out as the controller's.
        backend.exe_missing.clear(None);
        events.lock().unwrap().clear();
        executor.emit(&published);
        let shown = last_event(&events, gate::RECOVERY_EVENT).unwrap();
        assert_eq!(shown, serde_json::to_value(&published).unwrap());
    }

    #[test]
    fn start_failures_with_herdr_missing_pause_with_the_not_found_reason() {
        // herdr vanishes after launch and a restart reaches the controller
        // anyway (here: handed to it directly, past the backend's own check).
        let fake = fake::FakeHerdr::new();
        let (backend, _events) = fake_backend(&fake);
        let (killer, killed) = recording_killer();
        let backend = backend.tune_supervisor(|s| {
            s.with_stop(Duration::from_secs(30), Duration::from_secs(30), killer)
        });
        backend.start_recovery_driver();
        assert_eq!(backend.exe_missing.message(), None);

        fake.set_exe_missing(true);
        let t0 = Instant::now();
        backend.recovery.user_restart(backend.keymap.generation());
        wait_paused(&backend);
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "no stop or kill wait: {:?}",
            t0.elapsed()
        );

        let message = backend.exe_missing.message().expect("learned on the way");
        let recovery = backend.recovery_snapshot();
        assert_eq!(recovery.phase, recovery::RecoveryPhase::Paused);
        assert_eq!(recovery.reason, Some(message));
        assert_eq!(recovery.reason_code, Some(exe_missing::NOT_FOUND_CODE));
        // The retried starts could not spawn anything, and the server that was
        // running was neither stopped nor killed.
        assert!(
            killed.lock().unwrap().is_empty(),
            "the server is left alone"
        );
        fake.set_exe_missing(false);
        assert!(fake.status_server().unwrap().running);
        assert_eq!(fake.server_instance().map(|i| i.pid), Some(1000));
    }

    #[test]
    fn boot_with_nothing_remembered_starts_the_server() {
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        let (backend, events) = unready_backend(&fake, None);
        boot(&backend);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 1);
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
        // Nothing to protect: started at once, no memory wait.
        assert_eq!(
            recovery_phases(&events),
            vec!["Restarting", "Reconciling", "Healthy"]
        );
        assert_eq!(backend.keymap.instance(), fake.server_instance());
    }

    #[test]
    fn boot_finishes_a_launch_search_that_left_out_the_login_path() {
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        let (backend, _events) = unready_backend(&fake, None);
        // What the setup thread's search reports when nothing was found and
        // the login shell was not asked.
        let launch = env::ExeDiscovery {
            exe: "herdr".into(),
            searched: Vec::new(),
            notable: Vec::new(),
            found: false,
            explicit: false,
            login_deferred: true,
        };
        backend.exe_missing.clear(Some(launch));
        assert!(backend.exe_missing.login_deferred());

        boot(&backend);
        assert_eq!(fake.rediscoveries(), 1, "searched again, once, in full");
        assert!(!backend.exe_missing.login_deferred());
        assert_eq!(backend.exe_missing.message(), None);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 1);
    }

    #[test]
    fn boot_after_herdr_died_while_closed_starts_it_and_resumes() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = unready_backend(&fake, Some(launch_everything()));
        remembered_session(&backend, &fake, "a");
        fake.kill_server(Some(1));

        boot(&backend);

        assert_eq!(count(&fake, ops::Op::SpawnServer), 1);
        assert_eq!(started_agents(&fake), vec![agent_name_of("a")]);
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
        let phases = recovery_phases(&events);
        // Sessions are at stake: the memory check runs before the start.
        assert_eq!(
            phases.first().map(String::as_str),
            Some("AwaitHeadroom"),
            "{phases:?}"
        );
        assert_eq!(phases.last().map(String::as_str), Some("Healthy"));
        assert!(phases.iter().any(|p| p == "Resuming"));
        assert_eq!(backend.keymap.instance(), fake.server_instance());
        assert!(backend.keymap.should_be_running().is_empty());
    }

    #[test]
    fn boot_against_a_replaced_server_reconciles_without_starting_one() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = unready_backend(&fake, Some(launch_everything()));
        remembered_session(&backend, &fake, "a");
        // Another server took over while Keitora was closed and restored the
        // pane as a shell.
        fake.kill_server(Some(1));
        drop(fake.spawn_server().unwrap());
        let replaced = fake.server_instance();

        boot(&backend);

        assert_eq!(count(&fake, ops::Op::SpawnServer), 1, "only the takeover");
        assert_eq!(count(&fake, ops::Op::ServerStop), 0);
        assert_eq!(fake.server_instance(), replaced);
        assert_eq!(backend.keymap.instance(), replaced);
        assert_eq!(started_agents(&fake), vec![agent_name_of("a")]);
        assert_eq!(
            recovery_phases(&events).first().map(String::as_str),
            Some("Reconciling")
        );
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
    }

    #[test]
    fn boot_against_the_same_server_adopts_it_and_stays_healthy() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = unready_backend(&fake, None);
        remembered_session(&backend, &fake, "a");
        boot(&backend);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert!(backend.is_live(&skey("a")), "the pane is adopted");
        assert!(recovery_phases(&events).is_empty(), "no recovery shown");
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
        assert_eq!(
            backend.watchdog.noted_instance(),
            fake.server_instance(),
            "the adopted server is watched"
        );
    }

    /// The launch probe could not read the server's identity, so it adopted;
    /// the reconcile then finds a replaced server. Its sessions are resumed by
    /// the controller, not left for every pane to respawn at once.
    #[test]
    fn boot_adopting_a_server_that_turns_out_replaced_resumes_through_recovery() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = unready_backend(&fake, Some(launch_everything()));
        remembered_session(&backend, &fake, "a");
        fake.kill_server(Some(1));
        drop(fake.spawn_server().unwrap());
        fake.fail_next(ops::Op::ServerInstance, ops::HerdrError::Timeout);

        boot(&backend);

        assert_eq!(count(&fake, ops::Op::SpawnServer), 1, "only the takeover");
        assert_eq!(started_agents(&fake), vec![agent_name_of("a")]);
        assert!(
            recovery_phases(&events).iter().any(|p| p == "Resuming"),
            "{:?}",
            recovery_phases(&events)
        );
        assert!(backend.keymap.should_be_running().is_empty());
        assert_eq!(backend.keymap.instance(), fake.server_instance());
        assert!(
            fake.panes().iter().all(|p| p.agent.is_some()),
            "{:?}",
            fake.panes()
        );
    }

    /// Sessions still owed a resume from an earlier run (suspended when
    /// Keitora quit) are resumed by the controller over an adopted server.
    #[test]
    fn boot_adopting_the_same_server_resumes_what_was_still_owed() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = unready_backend(&fake, Some(launch_everything()));
        remembered_session(&backend, &fake, "a");
        backend.keymap.begin_recovery(&[skey("b")]);

        boot(&backend);

        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert_eq!(started_agents(&fake), vec![agent_name_of("b")]);
        assert!(backend.is_live(&skey("a")), "the surviving pane is adopted");
        assert!(fake.closed_pane_ids().is_empty());
        assert!(backend.keymap.should_be_running().is_empty());
    }

    #[test]
    fn boot_with_a_live_server_and_nothing_remembered_adopts_and_remembers_it() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = unready_backend(&fake, None);
        boot(&backend);
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
        assert_eq!(backend.gate.phase(), recovery::RecoveryPhase::Healthy);
        assert_eq!(backend.keymap.instance(), fake.server_instance());
    }

    // ---- the status loop over the backend ----

    #[test]
    fn daemon_stats_reads_the_cache_without_calling_herdr() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let cached = DaemonStats {
            running: true,
            pid: Some(42),
            version: Some("0.9.3".into()),
            ..DaemonStats::default()
        };
        backend.status_cache.lock().unwrap().stats = cached.clone();
        assert_eq!(backend.daemon_stats(), cached);
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        backend.explicit_stop.store(true, Ordering::SeqCst);
        assert_eq!(backend.daemon_stats(), DaemonStats::default());
    }

    #[test]
    fn status_loop_thread_runs_until_the_backend_is_dropped() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let handle = backend.spawn_status_loop().expect("thread starts");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !events
            .lock()
            .unwrap()
            .iter()
            .any(|(e, _)| e == status_loop::STATS_EVENT)
        {
            assert!(Instant::now() < deadline, "no stats from the loop");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(backend.daemon_stats().running, "the loop filled the cache");
        drop(backend);
        handle.join().expect("the loop ends cleanly");
    }

    /// Feed `n` one-second wakes after `t0` to the backend's status loop.
    fn wake(lp: &mut status_loop::StatusLoop, t0: Instant, from: u64, to: u64) {
        for s in from..=to {
            lp.tick(t0 + Duration::from_secs(s));
        }
    }

    fn death(backend: &HerdrBackend) -> Option<DeathEvent> {
        let rx = backend.death_rx.lock().unwrap();
        rx.as_ref().unwrap().try_recv().ok().map(|(_, ev)| ev)
    }

    #[test]
    fn an_adopted_server_that_stops_answering_is_declared_dead_by_the_loop() {
        let fake = fake::FakeHerdr::new();
        // Its process cannot be found either.
        fake.set_instance(None);
        let (backend, _) = fake_backend(&fake);
        let mut lp = backend.status_loop();
        let t0 = Instant::now();
        lp.tick(t0);
        fake.hang(ops::Op::StatusServer, true);
        // The 10 s probe fails, then retries back off (12 s, 16 s) until K failures.
        wake(&mut lp, t0, 1, 15);
        assert!(death(&backend).is_none());
        wake(&mut lp, t0, 16, 16);
        let ev = death(&backend).expect("declared dead");
        assert_eq!(ev.source, watchdog::DeathSource::Adopted);
        assert!(death(&backend).is_none(), "exactly one death");
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0, "it never restarts");
    }

    #[test]
    fn a_hung_server_whose_process_runs_is_not_declared_dead_by_the_loop() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let mut lp = backend.status_loop();
        let t0 = Instant::now();
        lp.tick(t0);
        fake.hang(ops::Op::StatusServer, true);
        wake(&mut lp, t0, 1, 90);
        assert!(death(&backend).is_none(), "slow, not dead");
        let probes = count(&fake, ops::Op::StatusServer);
        assert!(probes <= 13, "failed probes back off ({probes})");
    }

    #[test]
    fn the_loop_does_not_report_probes_that_mean_nothing() {
        // A stop is expected: failures are not deaths.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let mut lp = backend.status_loop();
        let t0 = Instant::now();
        lp.tick(t0);
        backend.watchdog.expect_stop();
        fake.hang(ops::Op::StatusServer, true);
        wake(&mut lp, t0, 10, 20);
        assert!(death(&backend).is_none(), "a stop is expected");

        // Our own child: its waiter is authoritative.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.kill_server(None);
        backend
            .supervisor
            .start(&*backend.ops, ServerStartHow::Spawned)
            .unwrap();
        let mut lp = backend.status_loop();
        lp.tick(t0);
        fake.hang(ops::Op::StatusServer, true);
        wake(&mut lp, t0, 10, 20);
        assert!(death(&backend).is_none(), "our own child");

        // Not healthy: no probe at all.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let mut lp = backend.status_loop();
        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );
        wake(&mut lp, t0, 0, 20);
        assert_eq!(count(&fake, ops::Op::StatusServer), 0);
    }

    // ---- the recovery controller over the fake ----

    #[test]
    fn a_start_after_a_false_death_stops_the_live_server_first() {
        // The controller believes the server is down (no stop asked for), but
        // it still answers: it is stopped before another one is started.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (killer, killed) = recording_killer();
        let backend = fast_stop(backend, killer);
        let old = fake.server_instance().unwrap();
        let mut exec = backend.executor();
        let (r, _) = lifecycle::with_capture(|| exec.start_server(false, ServerStartHow::Restart));
        let started = r.unwrap();
        assert_ne!(started, old, "a new server");
        assert_eq!(Some(started), fake.server_instance());
        let ops: Vec<ops::Op> = fake.calls().into_iter().map(|c| c.op).collect();
        let stop = ops.iter().position(|o| *o == ops::Op::ServerStop);
        let spawn = ops.iter().position(|o| *o == ops::Op::SpawnServer);
        assert!(
            matches!((stop, spawn), (Some(a), Some(b)) if a < b),
            "stopped before spawning: {ops:?}"
        );
        assert!(killed.lock().unwrap().is_empty());
    }

    #[test]
    fn a_start_fails_when_the_live_server_cannot_be_stopped() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let (killer, _killed) = recording_killer();
        let backend = fast_stop(backend, killer);
        fake.fail_next(ops::Op::ServerStop, ops::HerdrError::Timeout);
        let mut exec = backend.executor();
        let (r, _) = lifecycle::with_capture(|| exec.start_server(false, ServerStartHow::Restart));
        assert!(r.is_err(), "no second server next to a live one");
        assert_eq!(count(&fake, ops::Op::SpawnServer), 0);
    }

    #[test]
    fn reconciling_same_instance_adopts_without_queue() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend
            .spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend.keymap.set_instance(fake.server_instance());
        backend.live.lock().unwrap().clear();
        let mut exec = backend.executor();
        let outcome = exec
            .reconcile(backend.keymap.generation(), fake.server_instance().unwrap())
            .unwrap();
        assert_eq!(outcome, ReconcileOutcome::Adopted);
        assert!(backend.is_live(&skey("a")), "the pane is adopted");
        assert!(fake.closed_pane_ids().is_empty());
    }

    #[test]
    fn reconciling_new_generation_builds_queue_from_should_be_running() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend_with(&fake, Some(launch_everything()));
        let backend = backend.with_worktree_filter(Arc::new(|k: &SessionKey| k.worktree != "gone"));
        for w in ["a", "b", "gone"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        backend.keymap.set_instance(fake.server_instance());

        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_for("the resumes", || started_agents(&fake).len() == 2);
        wait_recovered(&backend);

        // Every restored shell was closed; only existing worktrees resumed, in
        // order, each admitted by the gate as the key in flight.
        assert_eq!(fake.closed_pane_ids().len(), 3);
        assert_eq!(
            started_agents(&fake),
            vec![agent_name_of("a"), agent_name_of("b")]
        );
        assert!(backend.keymap.should_be_running().is_empty());
        let resuming: Vec<Value> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, v)| e == gate::RECOVERY_EVENT && v["phase"] == "Resuming")
            .map(|(_, v)| v["total"].clone())
            .collect();
        assert!(resuming.iter().all(|t| *t == 2), "{resuming:?}");
    }

    #[test]
    fn a_killed_server_is_recovered_with_one_notification_each() {
        use recovery::driver::tests::RecordingNotifier;
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        let notifier = Arc::new(RecordingNotifier::default());
        let (backend, events) = unready_backend(&fake, Some(launch_everything()));
        let backend = backend.with_notifier(Arc::clone(&notifier) as Arc<dyn Notifier>);
        boot(&backend);
        assert!(
            notifier.titles().is_empty(),
            "a boot start notifies nothing"
        );
        for w in ["a", "b"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        // The controller learns who is live from the status loop's observation.
        backend.status_loop().tick(Instant::now());

        fake.kill_server(Some(0xC000_0409u32 as i32));
        wait_for("both sessions resumed", || started_agents(&fake).len() == 2);
        wait_recovered(&backend);
        wait_for("the done notification", || notifier.titles().len() == 2);

        assert_eq!(notifier.titles(), vec!["herdr stopped", "Sessions resumed"]);
        assert_eq!(
            notifier.0.lock().unwrap()[0].1,
            "2 sessions will restart automatically."
        );
        let phases = recovery_phases(&events);
        let after_boot = phases.iter().position(|p| p == "Healthy").unwrap() + 1;
        assert_eq!(
            phases[after_boot..].first().map(String::as_str),
            Some("AwaitHeadroom"),
            "{phases:?}"
        );
        assert_eq!(phases.last().map(String::as_str), Some("Healthy"));
        assert_eq!(count(&fake, ops::Op::SpawnServer), 2);
    }

    #[test]
    fn restart_after_a_crash_loop_bypasses_the_breaker() {
        let fake = fake::FakeHerdr::new();
        fake.kill_server(None);
        let (backend, _) = unready_backend(&fake, None);
        boot(&backend);
        let breaker = RecoveryParams::default().breaker_deaths as usize;
        for n in 1..=breaker {
            let spawned = count(&fake, ops::Op::SpawnServer);
            fake.kill_server(Some(1));
            if n < breaker {
                wait_for("the automatic restart", || {
                    count(&fake, ops::Op::SpawnServer) == spawned + 1
                });
                wait_recovered(&backend);
            }
        }
        wait_paused(&backend);
        assert_eq!(
            backend.recovery_snapshot().reason_code,
            Some("crash_loop_early")
        );

        let spawned = count(&fake, ops::Op::SpawnServer);
        backend.restart().unwrap();
        wait_recovered(&backend);
        assert_eq!(count(&fake, ops::Op::SpawnServer), spawned + 1);
    }

    #[test]
    fn shutdown_tells_the_controller_to_stand_down() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        fake.fail_spawn_server(Some(std::io::ErrorKind::PermissionDenied));
        backend.start_recovery_driver();
        backend.restart().unwrap();
        wait_paused(&backend);

        // The stop itself may report that no server was running.
        let _ = backend.shutdown();
        wait_recovered(&backend);
        let s = backend.recovery_snapshot();
        assert!(s.queue.is_empty());
        assert!(!s.can_resume_now);
    }

    // ---- resume-aware spawn ----

    /// A real worktree dir under the fake's home, with a Claude transcript for
    /// `id` in the test projects dir when given.
    fn worktree_dir(fake: &fake::FakeHerdr, w: &str, id: Option<&str>) -> String {
        let cwd = fake.env().home.join(w);
        std::fs::create_dir_all(&cwd).unwrap();
        if let Some(id) = id {
            let dir = fake
                .env()
                .home
                .join("claude-projects")
                .join(crate::transfer::encoded_cwd_dir_name(&cwd).unwrap());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{id}.jsonl")), "{}").unwrap();
        }
        cwd.to_string_lossy().into_owned()
    }

    /// Record herdr reporting `id` as `key`'s running claude session.
    fn record_session(fake: &fake::FakeHerdr, key: &SessionKey, id: &str) {
        keitora_tasks::LocalStore::open(&fake.env().resume_db_path().to_string_lossy())
            .unwrap()
            .resume_observe(
                &key.project,
                &key.worktree,
                keitora_tasks::CliKind::Claude,
                Some(id),
                "herdr:claude",
                true,
                10,
            )
            .unwrap();
    }

    /// The extra args of every `agent start` call, joined by spaces.
    fn agent_start_args(fake: &fake::FakeHerdr) -> Vec<String> {
        fake.calls()
            .into_iter()
            .filter(|c| c.op == ops::Op::AgentStart)
            .map(|c| c.args.splitn(5, ' ').nth(4).unwrap_or_default().to_string())
            .collect()
    }

    fn count(fake: &fake::FakeHerdr, op: ops::Op) -> usize {
        fake.calls().iter().filter(|c| c.op == op).count()
    }

    fn resume_outcomes(events: &[lifecycle::LifecycleEvent]) -> Vec<ResumeResult> {
        events
            .iter()
            .filter_map(|e| match e {
                lifecycle::LifecycleEvent::ResumeOutcome { outcome, .. } => Some(outcome.clone()),
                _ => None,
            })
            .collect()
    }

    fn resume_events(events: &Events) -> Vec<(String, Value)> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.starts_with("pty://resume/"))
            .cloned()
            .collect()
    }

    fn wait_for(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn spawn_agent_resumes_with_flags() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let cwd = worktree_dir(&fake, "wt", Some("sess-1"));
        record_session(&fake, &skey("wt"), "sess-1");
        let launch = AgentLaunch {
            autonomy: true,
            ..claude_launch()
        };
        let (gen, got) = lifecycle::with_capture(|| {
            backend
                .spawn_agent(skey("wt"), cwd, String::new(), Some((80, 24)), launch)
                .unwrap()
        });
        assert_eq!(gen, 1);
        assert_eq!(
            agent_start_args(&fake),
            vec!["--dangerously-skip-permissions --resume sess-1"]
        );
        assert_eq!(
            resume_outcomes(&got),
            vec![ResumeResult::Resumed {
                id: "sess-1".into()
            }]
        );
        assert_eq!(
            resume_events(&events),
            vec![(
                "pty://resume/p/wt".to_string(),
                serde_json::json!({"outcome": "resumed", "fell_back": false})
            )]
        );
    }

    /// A respawn replaces the old pane but keeps counting the key's `gen`, so a
    /// late kill of the old session leaves the new one running.
    #[test]
    fn a_late_kill_of_a_replaced_session_leaves_the_new_one_running() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        let spawn = || {
            backend
                .spawn_agent(
                    skey("wt"),
                    "/repos/wt".into(),
                    String::new(),
                    Some((80, 24)),
                    claude_launch(),
                )
                .unwrap()
        };
        let old = spawn();
        let new = spawn();
        assert!(new > old, "gen {new} after {old}");
        backend.kill(&skey("wt"), old).unwrap();
        let pane = backend.keymap.get(&skey("wt")).expect("still mapped");
        assert_eq!(pane.gen, new);
        assert!(fake.panes().iter().any(|p| p.pane_id == pane.pane_id));

        // Resumed after its entry was dropped (a replaced server): newer again.
        backend
            .keymap
            .adopt_reconciled(&backend.keymap.entries(), Vec::new());
        let resumed = spawn();
        assert!(resumed > new, "gen {resumed} after {new}");
    }

    #[test]
    fn spawn_agent_early_exit_falls_back_resume_continue_fresh() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let cwd = worktree_dir(&fake, "wt", Some("sess-1"));
        record_session(&fake, &skey("wt"), "sess-1");
        fake.script_agent_start(vec![
            fake::AgentStartScript::EarlyExit,
            fake::AgentStartScript::EarlyExit,
            fake::AgentStartScript::Ok,
        ]);
        let (res, got) = lifecycle::with_capture(|| {
            backend.spawn_agent(
                skey("wt"),
                cwd,
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
        });
        res.unwrap();
        assert_eq!(
            count(&fake, ops::Op::WorkspaceCreate),
            3,
            "a new pane per attempt"
        );
        assert_eq!(
            count(&fake, ops::Op::PaneClose),
            2,
            "each failed pane closed"
        );
        assert_eq!(
            agent_start_args(&fake),
            vec!["--resume sess-1", "--continue", ""],
            "the final launch is fresh"
        );
        let outcomes = resume_outcomes(&got);
        assert_eq!(outcomes.len(), 3);
        assert!(matches!(outcomes[0], ResumeResult::Failed { .. }));
        assert!(matches!(outcomes[1], ResumeResult::Failed { .. }));
        assert_eq!(outcomes[2], ResumeResult::Fresh);
        assert_eq!(
            resume_events(&events),
            vec![(
                "pty://resume/p/wt".to_string(),
                serde_json::json!({"outcome": "fresh", "fell_back": true})
            )]
        );
        let live_pane = backend.keymap.get_current(&skey("wt")).unwrap();
        assert_eq!(fake.panes().len(), 1);
        assert_eq!(fake.panes()[0].pane_id, live_pane.pane_id);
    }

    #[test]
    fn spawn_agent_lost_name_falls_back_once() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let cwd = worktree_dir(&fake, "wt", Some("sess-1"));
        record_session(&fake, &skey("wt"), "sess-1");
        fake.script_agent_start(vec![
            fake::AgentStartScript::NameLost,
            fake::AgentStartScript::Ok,
        ]);
        backend
            .spawn_agent(
                skey("wt"),
                cwd,
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
            .unwrap();
        assert_eq!(
            agent_start_args(&fake),
            vec!["--resume sess-1", "--continue"]
        );
        assert_eq!(
            resume_events(&events),
            vec![(
                "pty://resume/p/wt".to_string(),
                serde_json::json!({"outcome": "continued", "fell_back": true})
            )]
        );
    }

    #[test]
    fn spawn_agent_timeout_is_not_retried() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let cwd = worktree_dir(&fake, "wt", Some("sess-1"));
        record_session(&fake, &skey("wt"), "sess-1");
        fake.script_agent_start(vec![fake::AgentStartScript::Err(ops::HerdrError::Api {
            code: "timeout".into(),
            message: "agent did not become ready".into(),
        })]);
        let (res, got) = lifecycle::with_capture(|| {
            backend.spawn_agent(
                skey("wt"),
                cwd,
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
        });
        assert!(res.is_err());
        assert_eq!(count(&fake, ops::Op::AgentStart), 1, "no retry");
        assert_eq!(count(&fake, ops::Op::PaneClose), 1, "the pane is closed");
        assert!(fake.panes().is_empty());
        assert!(backend.keymap.get(&skey("wt")).is_none());
        assert!(resume_outcomes(&got).is_empty());
        assert!(resume_events(&events).is_empty());
    }

    #[test]
    fn spawn_agent_fresh_clears_store_and_removes_from_should_be_running() {
        let fake = fake::FakeHerdr::new();
        let (backend, events) = fake_backend(&fake);
        let cwd = worktree_dir(&fake, "wt", Some("sess-1"));
        record_session(&fake, &skey("wt"), "sess-1");
        backend.keymap.begin_recovery(&[skey("wt")]);
        let launch = AgentLaunch {
            fresh: true,
            ..claude_launch()
        };
        backend
            .spawn_agent(skey("wt"), cwd, String::new(), Some((80, 24)), launch)
            .unwrap();
        assert_eq!(agent_start_args(&fake), vec![""]);
        let row = keitora_tasks::LocalStore::open(&fake.env().resume_db_path().to_string_lossy())
            .unwrap()
            .resume_get("p", "wt")
            .unwrap()
            .expect("row kept");
        assert_eq!(row.session_id, None, "the stored id is cleared");
        assert!(backend.keymap.should_be_running().is_empty());
        assert!(
            resume_events(&events).is_empty(),
            "a plain fresh start says nothing"
        );
    }

    /// Two sessions on a server that is then replaced: the reconcile owes both a
    /// resume.
    fn replaced_server_with_two_sessions(
        launcher: Option<LaunchResolver>,
    ) -> (Arc<fake::FakeHerdr>, HerdrBackend) {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, launcher);
        for w in ["a", "b"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        backend.keymap.set_instance(fake.server_instance());
        fake.kill_server(Some(1));
        let _ = fake.spawn_server().unwrap();
        (fake, backend)
    }

    fn launch_everything() -> LaunchResolver {
        Arc::new(|k: &SessionKey| Some((format!("/repos/{}", k.worktree), claude_launch())))
    }

    /// A reconcile only reports the sessions owed a resume; nothing launches
    /// them behind the recovery controller's back.
    #[test]
    fn reconcile_leaves_owed_sessions_to_the_recovery_sequence() {
        let (fake, backend) = replaced_server_with_two_sessions(Some(launch_everything()));
        assert_eq!(
            backend.reconcile_current(),
            Ok(ReconcileOutcome::NewGeneration {
                to_resume: vec![skey("a"), skey("b")]
            })
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count(&fake, ops::Op::AgentStart), 0);
        assert_eq!(
            backend.keymap.should_be_running(),
            vec![skey("a"), skey("b")]
        );
    }

    #[test]
    fn resume_session_without_a_launch_is_no_such_session() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        assert!(matches!(
            backend.resume_session(&skey("a"), None),
            Err(PtyBackendError::NoSuchSession)
        ));
        let (backend, _) = fake_backend_with(&fake, Some(Arc::new(|_: &SessionKey| None)));
        assert!(matches!(
            backend.resume_session(&skey("a"), Some(ResumeDecision::Continue)),
            Err(PtyBackendError::NoSuchSession)
        ));
    }

    /// Resume `w` through the recovery controller's runner, in the current
    /// generation.
    fn run_resume(backend: &HerdrBackend, w: &str) -> ResumeReport {
        let runner = backend.executor().resume_runner();
        let generation = backend.keymap.generation();
        let (report, _) =
            lifecycle::with_capture(|| runner.resume(&skey(w), generation, ResumeMode::Stored));
        report.expect("the generation is current")
    }

    /// A launch resolver that starts every session in `cwd`.
    fn launch_in(cwd: String) -> LaunchResolver {
        Arc::new(move |_: &SessionKey| Some((cwd.clone(), claude_launch())))
    }

    #[test]
    fn resume_runner_skips_a_generation_that_moved_on() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        record_session(&fake, &skey("a"), "sess-1");
        backend.keymap.begin_recovery(&[skey("a")]);
        let issued = backend.keymap.generation();
        backend.keymap.note_death_bump();
        assert_ne!(backend.keymap.generation(), issued);
        let calls_before = fake.calls().len();

        let runner = backend.executor().resume_runner();
        assert_eq!(runner.resume(&skey("a"), issued, ResumeMode::Fresh), None);
        assert_eq!(fake.calls().len(), calls_before, "herdr untouched");
        let row = keitora_tasks::LocalStore::open(&fake.env().resume_db_path().to_string_lossy())
            .unwrap()
            .resume_get("p", "a")
            .unwrap()
            .expect("the stored row");
        assert_eq!(
            row.session_id.as_deref(),
            Some("sess-1"),
            "the store is not cleared"
        );
    }

    #[test]
    fn resume_runner_reports_a_startup_timeout_as_load_related() {
        // herdr's own timeout answer.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        fake.script_agent_start(vec![fake::AgentStartScript::Err(ops::HerdrError::Api {
            code: "timeout".into(),
            message: "timed out waiting for agent startup".into(),
        })]);
        let report = run_resume(&backend, "a");
        assert!(
            matches!(report.outcome, ResumeOutcomeKind::Failed(_)),
            "{report:?}"
        );
        assert!(report.load_related);

        // The CLI call itself ran out of time.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        fake.script_agent_start(vec![fake::AgentStartScript::Err(ops::HerdrError::Timeout)]);
        let report = run_resume(&backend, "a");
        assert!(
            matches!(report.outcome, ResumeOutcomeKind::Failed(_)),
            "{report:?}"
        );
        assert!(report.load_related);
    }

    #[test]
    fn resume_runner_reports_an_early_exit_fallback_as_load_related() {
        let fake = fake::FakeHerdr::new();
        let cwd = worktree_dir(&fake, "wt", Some("sess-1"));
        let (backend, _) = fake_backend_with(&fake, Some(launch_in(cwd)));
        record_session(&fake, &skey("wt"), "sess-1");
        backend.keymap.begin_recovery(&[skey("wt")]);
        fake.script_agent_start(vec![
            fake::AgentStartScript::EarlyExit,
            fake::AgentStartScript::Ok,
        ]);
        assert_eq!(
            run_resume(&backend, "wt"),
            ResumeReport {
                outcome: ResumeOutcomeKind::Continued,
                load_related: true,
            }
        );
    }

    #[test]
    fn resume_runner_outcomes_that_are_not_load_related() {
        // A stored id whose transcript is gone: decided fresh up front, no
        // fallback.
        let fake = fake::FakeHerdr::new();
        let cwd = worktree_dir(&fake, "wt", None);
        let (backend, _) = fake_backend_with(&fake, Some(launch_in(cwd)));
        record_session(&fake, &skey("wt"), "sess-1");
        backend.keymap.begin_recovery(&[skey("wt")]);
        assert_eq!(
            run_resume(&backend, "wt"),
            ResumeReport {
                outcome: ResumeOutcomeKind::StartedFresh,
                load_related: false,
            }
        );

        // The worktree is gone (no launch).
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        assert_eq!(
            run_resume(&backend, "a"),
            ResumeReport {
                outcome: ResumeOutcomeKind::Failed("worktree no longer exists".into()),
                load_related: false,
            }
        );

        // Any other herdr error.
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        fake.script_agent_start(vec![fake::AgentStartScript::Err(ops::HerdrError::Api {
            code: "workspace_busy".into(),
            message: "the workspace is busy".into(),
        })]);
        let report = run_resume(&backend, "a");
        assert!(
            matches!(report.outcome, ResumeOutcomeKind::Failed(_)),
            "{report:?}"
        );
        assert!(!report.load_related);
    }

    #[test]
    fn resume_load_related_classification_of_errors() {
        assert!(is_timeout_text(
            r#"herdr: {"error":{"code":"timeout","message":"x"}}"#
        ));
        assert!(is_timeout_text(
            "herdr: Timed out waiting for agent startup"
        ));
        assert!(!is_timeout_text("herdr: agent did not become ready"));
        assert!(is_load_related_error(&PtyBackendError::NotConnected, false));
        assert!(is_load_related_error(
            &PtyBackendError::Other("herdr: timed out waiting for agent startup".into()),
            false
        ));
        assert!(!is_load_related_error(
            &PtyBackendError::NoSuchSession,
            false
        ));
        assert!(!is_load_related_error(
            &PtyBackendError::RecoveryPending,
            false
        ));
        assert!(!is_load_related_error(
            &PtyBackendError::Other("herdr: boom".into()),
            false
        ));
        // Unreachable because the user stopped (or is restarting) the server.
        assert!(!is_load_related_error(&PtyBackendError::NotConnected, true));
    }

    #[test]
    fn resume_runner_does_not_count_a_requested_stop_as_load() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        // The user's stop is under way and the server is already gone.
        backend.supervisor.expect_stop();
        fake.kill_server(Some(1));
        let report = run_resume(&backend, "a");
        assert!(
            matches!(report.outcome, ResumeOutcomeKind::Failed(_)),
            "{report:?}"
        );
        assert!(!report.load_related, "{report:?}");
        assert!(backend.supervisor.stop_expected());
    }

    /// Run the launch decision against a recording controller whose gate
    /// already left `Healthy` (so the boot does not wait for it), and return
    /// the class and the owed count it handed over.
    fn boot_owed(
        setup: impl FnOnce(&HerdrBackend, &Arc<fake::FakeHerdr>),
    ) -> (BootClass, Vec<u32>) {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = unready_backend(&fake, Some(launch_everything()));
        let (backend, rx) = backend.with_recording_recovery();
        setup(&backend, &fake);
        backend.gate.set_view(
            recovery::RecoveryPhase::Down,
            HashSet::new(),
            HashSet::new(),
        );
        let ready = ReadySignal(Arc::clone(&backend.ready));
        let (class, _) =
            lifecycle::with_capture(|| backend.recovery_parts().boot(&backend.recovery, ready));
        let owed = rx
            .try_iter()
            .filter_map(|ev| match ev {
                recovery::machine::Event::BootRecovery { owed, .. } => Some(owed),
                _ => None,
            })
            .collect();
        (class, owed)
    }

    #[test]
    fn boot_recovery_owed_count_only_when_herdr_is_down() {
        // Down with two remembered sessions: both are owed.
        let (class, owed) = boot_owed(|b, f| {
            remembered_session(b, f, "a");
            remembered_session(b, f, "b");
            f.kill_server(Some(1));
        });
        assert_eq!(class, BootClass::RecoverDown);
        assert_eq!(owed, vec![2]);

        // Nothing remembered: started at once.
        let (class, owed) = boot_owed(|_, f| f.kill_server(None));
        assert_eq!(class, BootClass::Fresh);
        assert_eq!(owed, vec![0]);

        // A replaced server: reconciled without a memory wait.
        let (class, owed) = boot_owed(|b, f| {
            remembered_session(b, f, "a");
            f.kill_server(Some(1));
            drop(f.spawn_server().unwrap());
        });
        assert_eq!(class, BootClass::RecoverDifferentInstance);
        assert_eq!(owed, vec![0]);

        // The same server, with a session still owed from an earlier run.
        let (class, owed) = boot_owed(|b, f| {
            remembered_session(b, f, "a");
            b.keymap.begin_recovery(&[skey("b")]);
        });
        assert_eq!(class, BootClass::Adopt);
        assert_eq!(owed, vec![0]);
    }

    #[test]
    fn resume_session_launches_the_given_decision() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        assert_eq!(
            backend
                .resume_session(&skey("a"), Some(ResumeDecision::Continue))
                .unwrap(),
            Some(ResumeResult::Continued)
        );
        assert_eq!(agent_start_args(&fake), vec!["--continue"]);
        assert!(backend.keymap.should_be_running().is_empty());
    }

    #[test]
    fn a_terminated_session_is_never_resumed() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend_with(&fake, Some(launch_everything()));
        backend.keymap.begin_recovery(&[skey("a")]);
        backend.set_terminated(&skey("a"), true).unwrap();
        // Cancelled, not "already running": nothing runs, so recovery must
        // not settle it (nor count it as failed).
        assert_eq!(
            run_resume(&backend, "a"),
            ResumeReport {
                outcome: ResumeOutcomeKind::Cancelled,
                load_related: false,
            }
        );
        assert!(backend.keymap.should_be_running().is_empty(), "resolved");
        // A pause can requeue an entry the terminate cancelled: running it
        // again still leaves the session down.
        backend.keymap.begin_recovery(&[skey("a")]);
        assert_eq!(
            run_resume(&backend, "a").outcome,
            ResumeOutcomeKind::Cancelled
        );
        assert!(matches!(
            backend.resume_session(&skey("a"), Some(ResumeDecision::Continue)),
            Err(PtyBackendError::Terminated)
        ));
        assert_eq!(count(&fake, ops::Op::AgentStart), 0);
        assert_eq!(count(&fake, ops::Op::WorkspaceCreate), 0);
        // Launched again: resumed as usual.
        backend.set_terminated(&skey("a"), false).unwrap();
        backend.keymap.begin_recovery(&[skey("a")]);
        let outcome = run_resume(&backend, "a").outcome;
        assert_ne!(outcome, ResumeOutcomeKind::AlreadyRunning);
        assert_ne!(outcome, ResumeOutcomeKind::Cancelled);
        assert_eq!(count(&fake, ops::Op::AgentStart), 1);
    }

    #[test]
    fn spawn_and_spawn_agent_refuse_a_terminated_session() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        backend.set_terminated(&skey("a"), true).unwrap();
        assert_eq!(
            backend.terminated_keys("p"),
            HashSet::from(["a".to_string()])
        );
        assert!(matches!(
            backend.spawn(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                None
            ),
            Err(PtyBackendError::Terminated)
        ));
        assert!(matches!(
            backend.spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                claude_launch()
            ),
            Err(PtyBackendError::Terminated)
        ));
        assert_eq!(count(&fake, ops::Op::WorkspaceCreate), 0);
        assert_eq!(count(&fake, ops::Op::AgentStart), 0);
        // Another session spawns as usual, and so does `a` once launched.
        backend
            .spawn(
                skey("b"),
                "/repos/b".into(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .unwrap();
        backend.set_terminated(&skey("a"), false).unwrap();
        assert!(backend.terminated_keys("p").is_empty());
        backend
            .spawn_agent(
                skey("a"),
                "/repos/a".into(),
                String::new(),
                Some((80, 24)),
                claude_launch(),
            )
            .unwrap();
        assert_eq!(count(&fake, ops::Op::AgentStart), 1);
    }

    #[test]
    fn a_death_and_a_restart_owe_no_resume_to_a_terminated_session() {
        let fake = fake::FakeHerdr::new();
        let (backend, _) = fake_backend(&fake);
        for w in ["a", "term"] {
            backend
                .spawn(
                    skey(w),
                    format!("/repos/{w}"),
                    String::new(),
                    Some((80, 24)),
                    None,
                )
                .unwrap();
        }
        backend.set_terminated(&skey("term"), true).unwrap();

        backend.death_handler().note_server_death();
        assert_eq!(backend.keymap.should_be_running(), vec![skey("a")]);

        backend.keymap.resolve_recovered(&skey("a"));
        backend.recovery_parts().mark_owed();
        assert_eq!(backend.keymap.should_be_running(), vec![skey("a")]);
    }

    /// The attach liveness predicate: a pane id present in the server's live list is
    /// live; an id absent (stale/dead after a restart-restore reassign) is not — which
    /// is what drives `attach` to return `NoSuchSession` → respawn.
    #[test]
    fn pane_is_live_detects_present_and_stale() {
        let panes = vec![pane("p1"), pane("p2")];
        assert!(pane_is_live(&panes, "p1"));
        assert!(pane_is_live(&panes, "p2"));
        assert!(!pane_is_live(&panes, "p3"), "a stale id is not live");
        assert!(!pane_is_live(&[], "p1"), "no live panes → nothing is live");
    }

    /// The readiness latch: unset until a `ReadySignal` guard drops (the all-paths signal
    /// `start`'s thread relies on), then set. `attach` reads it without blocking.
    #[test]
    fn ready_latch_set_by_signal_guard() {
        let latch = Arc::new((Mutex::new(false), Condvar::new()));
        assert!(
            !latch_is_set(&latch),
            "latch starts unset → attach returns NotConnected"
        );
        // Dropping the guard is exactly how `start`'s thread signals on every path.
        drop(ReadySignal(Arc::clone(&latch)));
        assert!(
            latch_is_set(&latch),
            "after the guard drops, attach may proceed"
        );
    }

    /// Stats sampling for a `None` pid (externally started server we didn't spawn, or
    /// after a stop) yields the empty default — the card then shows `—` rather than
    /// numbers for the wrong process.
    #[test]
    fn sample_process_stats_none_pid_is_empty() {
        let mut sys = sysinfo::System::new();
        let stats = sample_process_stats(&mut sys, None);
        assert_eq!(stats, DaemonStats::default());
        assert!(stats.cpu_percent.is_none() && stats.mem_bytes.is_none());
    }

    /// Sampling our own live process returns real memory + a sane uptime (it started
    /// before now), exercising the sysinfo shell end-to-end without a herdr server.
    #[test]
    fn sample_process_stats_self_has_memory() {
        let mut sys = sysinfo::System::new();
        let me = std::process::id();
        let stats = sample_process_stats(&mut sys, Some(me));
        assert_eq!(stats.pid, Some(me));
        // The current process is obviously resident, so RSS must be > 0.
        assert!(
            stats.mem_bytes.unwrap_or(0) > 0,
            "own process must report non-zero RSS"
        );
        // cpu_percent is populated (may be 0.0 on the first sample) and within range.
        let cpu = stats.cpu_percent.expect("cpu sampled");
        assert!((0.0..=100.0).contains(&cpu));
    }

    // ---- real herdr (gated) ----

    /// A real herdr on its own home for one gated test, with every backend
    /// built over it. The home sits directly under the temp dir with a short
    /// name: herdr's socket lives below it and unix caps that path's length.
    /// Dropping the rig stops the server and removes the home, also when the
    /// test fails.
    struct RealHerdrRig {
        exe: PathBuf,
        home: PathBuf,
        backends: Vec<Arc<HerdrBackend>>,
    }

    impl RealHerdrRig {
        /// `None` (the test skips) unless `KEITORA_HERDR_BIN` names a herdr.
        /// `tag` tells the tests' homes apart; keep it to a few characters.
        fn new(test: &str, tag: &str) -> Option<Self> {
            let Some(exe) = std::env::var_os(env::HERDR_BIN_ENV) else {
                eprintln!("skipping {test}: KEITORA_HERDR_BIN not set");
                return None;
            };
            let home = std::env::temp_dir().join(format!("kh-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            Some(RealHerdrRig {
                exe: PathBuf::from(exe),
                home,
                backends: Vec::new(),
            })
        }

        fn env(&self) -> Result<HerdrEnv, PtyBackendError> {
            let mut e = HerdrEnv::with_exe_and_home(self.exe.clone(), self.home.clone())?;
            // `herdr integration install` writes the user's real ~/.claude and
            // ~/.codex: opted out here as well as by `cfg(test)`.
            e.env
                .push((supervisor::NO_INTEGRATION_ENV.to_string(), "1".to_string()));
            Ok(e)
        }

        /// A backend over the home that has not booted (a second one stands
        /// for Keitora restarted while the server kept running).
        fn backend(&mut self) -> (Arc<HerdrBackend>, Events) {
            let events: Events = Arc::new(StdMutex::new(Vec::new()));
            let sink: Arc<dyn EmitSink> = Arc::new(CaptureSink(Arc::clone(&events)));
            let env = self.env().expect("env builds");
            let backend = Arc::new(HerdrBackend::with_parts(env, sink, None));
            self.backends.push(Arc::clone(&backend));
            (backend, events)
        }

        /// A backend that booted: the server runs and the first reconcile is
        /// done, as after launch.
        fn booted(&mut self) -> (Arc<HerdrBackend>, Events) {
            let (backend, events) = self.backend();
            boot(&backend);
            (backend, events)
        }

        /// Whether the home's server answers as running.
        fn server_running(&self) -> bool {
            self.env()
                .is_ok_and(|env| RealHerdr::new(env).status_server().is_ok_and(|s| s.running))
        }

        /// The servers the backends spawned and still own.
        fn spawned_pids(&self) -> Vec<u32> {
            self.backends
                .iter()
                .filter_map(|b| b.supervisor.spawned_pid())
                .collect()
        }
    }

    impl Drop for RealHerdrRig {
        fn drop(&mut self) {
            use crate::backend::platform;
            // The only processes ever killed: servers these backends spawned.
            let mut spawned = self.spawned_pids();
            // Every backend learns of the stop before the server goes: one
            // told only afterwards would take it for a death and restart it.
            for backend in &self.backends {
                backend.supervisor.expect_stop();
                backend.recovery.explicit_stop();
            }
            for backend in self.backends.iter().rev() {
                let _ = backend.shutdown();
            }
            // A server a recovery already under way still brought up is asked
            // to stop as well: wait until none has answered for a while.
            if let Ok(env) = self.env() {
                let herdr = RealHerdr::new(env);
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut quiet = 0;
                while quiet < 5 && Instant::now() < deadline {
                    if herdr.status_server().is_ok_and(|s| s.running) {
                        quiet = 0;
                        let _ = herdr.server_stop();
                    } else {
                        quiet += 1;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            spawned.extend(self.spawned_pids());
            for pid in spawned {
                let deadline = Instant::now() + Duration::from_secs(5);
                while platform::process_is_alive(pid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if platform::process_is_alive(pid) {
                    eprintln!("herdr server {pid} outlived its stop; killing it");
                    platform::kill_group_kill(pid);
                }
            }
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    fn real_key(worktree: &str) -> SessionKey {
        SessionKey {
            project: "it".to_string(),
            worktree: worktree.to_string(),
        }
    }

    /// Poll (bounded by `limit`) until a captured frame contains `marker`.
    fn frame_arrives(output: &fake::CaptureOutput, marker: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if output.frames().iter().any(|f| f.contains(marker)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// End-to-end slice against a REAL herdr, gated behind `KEITORA_HERDR_BIN` so CI
    /// without a binary skips it (returns early). Boot an isolated server, spawn a
    /// workspace, run `echo HELLO_HERDR_SLICE`, attach the bridge, and assert a
    /// decoded output frame contains the marker.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn herdr_backend_slice_roundtrip_gated() {
        let Some(mut rig) = RealHerdrRig::new("herdr_backend_slice_roundtrip_gated", "slice")
        else {
            return;
        };
        let (backend, _events) = rig.booted();

        let key = real_key("slice");
        // The pane's output, as a mounted pane would have attached it.
        let output = Arc::new(fake::CaptureOutput::default());
        backend.outputs.register(&key, output.clone());
        let cwd = env!("CARGO_MANIFEST_DIR").to_string();
        let gen = backend
            .spawn(
                key.clone(),
                cwd,
                String::new(),
                Some((80, 24)),
                Some("echo HELLO_HERDR_SLICE".to_string()),
            )
            .expect("spawn");
        assert!(gen >= 1);

        // Let the echo land, then attach (the first full frame mirrors the viewport).
        std::thread::sleep(Duration::from_millis(1500));
        backend
            .attach(key.clone(), Some((80, 24)), output.clone())
            .expect("attach");
        assert!(
            frame_arrives(&output, "HELLO_HERDR_SLICE", Duration::from_secs(6)),
            "expected an output frame containing HELLO_HERDR_SLICE"
        );

        // Fast input path: write raw bytes via the bridge's persistent stdin (no
        // process spawn) and assert they reach the pane. Guards the latency fix.
        backend
            .write(&key, b"echo WRITE_VIA_BRIDGE\r")
            .expect("write via bridge");
        assert!(
            frame_arrives(&output, "WRITE_VIA_BRIDGE", Duration::from_secs(6)),
            "bridge.send_input bytes must reach the pane (WRITE_VIA_BRIDGE)"
        );
    }

    /// herdr accepts the `agent prompt` argv `agent_prompt` builds: against a plain
    /// shell pane (no agent, so no LLM turn) herdr parses the argv and answers with
    /// an agent error code, never a usage exit. A dash-leading text exercises the
    /// positional text argument. Gated behind `KEITORA_HERDR_BIN`.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn agent_prompt_argv_is_accepted_by_herdr_gated() {
        let Some(mut rig) =
            RealHerdrRig::new("agent_prompt_argv_is_accepted_by_herdr_gated", "argv")
        else {
            return;
        };
        let (backend, _events) = rig.booted();

        let key = real_key("argv");
        backend
            .spawn(
                key.clone(),
                env!("CARGO_MANIFEST_DIR").to_string(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .expect("spawn");
        let pane = backend.keymap.get_current(&key).expect("spawned pane");

        let err = backend
            .ops
            .agent_prompt(&pane.pane_id, "-herdr argv check")
            .expect_err("a shell pane has no agent to prompt");
        assert!(
            matches!(err.code(), Some("agent_not_found" | "agent_not_ready")),
            "herdr must parse the argv and answer with an agent error, got {err:?}"
        );
    }

    /// Regression for the blank-pane bug: `spawn` must start the output bridge by
    /// itself, because the frontend's fresh-spawn path (`TerminalPane::spawn`) never
    /// calls `attach` afterwards. So output must reach the sink after spawn ALONE —
    /// no explicit `attach` here. Gated behind `KEITORA_HERDR_BIN`.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn spawn_starts_the_bridge_without_attach_gated() {
        let Some(mut rig) =
            RealHerdrRig::new("spawn_starts_the_bridge_without_attach_gated", "spawn")
        else {
            return;
        };
        let (backend, _events) = rig.booted();

        let key = real_key("spawnonly");
        // The pane's output, as a mounted pane would have attached it.
        let output = Arc::new(fake::CaptureOutput::default());
        backend.outputs.register(&key, output.clone());
        let gen = backend
            .spawn(
                key.clone(),
                env!("CARGO_MANIFEST_DIR").to_string(),
                String::new(),
                Some((80, 24)),
                Some("echo SPAWN_ONLY_MARKER".to_string()),
            )
            .expect("spawn");
        assert!(gen >= 1);

        // NO attach() — the bridge must already be streaming from spawn.
        assert!(
            frame_arrives(&output, "SPAWN_ONLY_MARKER", Duration::from_secs(8)),
            "spawn alone (no attach) must stream an output frame containing SPAWN_ONLY_MARKER"
        );
    }

    /// `spawn_agent` launches via `agent.start --kind`, `list_sessions`
    /// folds the `agent.list` status into `state_hint`, and an empty `write_line`
    /// submits Enter. Gated behind `KEITORA_HERDR_BIN`.
    ///
    /// Robustness / cost notes:
    /// - `--kind claude` needs a real `claude` on the pane PATH. If it is not resolvable
    ///   in this isolated env, `agent.start` fails hard and `spawn_agent` closes the
    ///   leaked pane; the test then SKIPS the remaining assertions rather than failing
    ///   CI on a machine without claude.
    /// - We assert on launch/stream MECHANICS (a non-empty output frame), not an LLM
    ///   turn, to avoid cost + R1 blocked-startup flakiness.
    /// - `write_line` is exercised on the empty (submit-only) path only: it sends the
    ///   Enter key and no prompt text (no LLM turn).
    ///
    /// Starts the user's REAL `claude` in this checkout, so it also needs
    /// `KEITORA_HERDR_TEST_AGENT` set; without it the test skips.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn spawn_agent_launches_and_lists_gated() {
        const TEST: &str = "spawn_agent_launches_and_lists_gated";
        if std::env::var_os("KEITORA_HERDR_TEST_AGENT").is_none() {
            eprintln!(
                "skipping {TEST}: KEITORA_HERDR_TEST_AGENT not set (it starts a real claude)"
            );
            return;
        }
        let Some(mut rig) = RealHerdrRig::new(TEST, "agent") else {
            return;
        };
        let (backend, _events) = rig.booted();

        let key = real_key("agent");
        // The pane's output, as a mounted pane would have attached it.
        let output = Arc::new(fake::CaptureOutput::default());
        backend.outputs.register(&key, output.clone());
        let launch = AgentLaunch {
            cli: keitora_tasks::CliKind::Claude,
            // Bare name → pane_env adds only herdr's own PATH (unix) and the XDG
            // pairs, so `claude` resolves on that PATH.
            path: "claude".to_string(),
            autonomy: false,
            resume_target: None,
            fresh: false,
        };

        let spawned = backend.spawn_agent(
            key.clone(),
            env!("CARGO_MANIFEST_DIR").to_string(),
            String::new(),
            Some((80, 24)),
            launch,
        );

        let gen = match spawned {
            Ok(g) => g,
            Err(e) => {
                eprintln!(
                    "skipping spawn_agent assertions: agent.start failed (claude not resolvable?): {e}"
                );
                return;
            }
        };

        // Launch mechanics: the agent's pane is live.
        let panes_nonempty = backend
            .ops
            .pane_list()
            .map(|p| !p.is_empty())
            .unwrap_or(false);

        // Bridge streams at least one (non-empty) frame from spawn alone — no attach,
        // no LLM marker (mechanics only, to avoid LLM cost + R1 flakiness).
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut streamed = false;
        while Instant::now() < deadline && !streamed {
            std::thread::sleep(Duration::from_millis(200));
            streamed = output.frames().iter().any(|f| !f.is_empty());
        }

        // list_sessions reflects the agent: present, with a VALID hint (busy or none).
        let sessions = backend.list_sessions().unwrap_or_default();
        let summary = sessions.iter().find(|s| s.key == key).cloned();
        let present = summary.is_some();
        let hint = summary.and_then(|s| s.state_hint.clone());
        let hint_valid = hint.is_none() || hint.as_deref() == Some(crate::backend::HINT_BUSY);

        // write_line, empty text → submit-only (Enter): no LLM content at all.
        let empty = backend.write_line(&key, "", 0);

        eprintln!(
            "spawn_agent_launches_and_lists_gated: gen={gen} present={present} \
             state_hint={hint:?} streamed={streamed} empty_write={empty:?}"
        );

        // R1: even a "blocked during startup" agent LAUNCHED → a valid generation.
        assert!(
            gen >= 1,
            "spawn_agent must return a generation even when the agent is blocked at startup (R1)"
        );
        assert!(
            panes_nonempty,
            "the spawned agent pane must appear in pane_list"
        );
        assert!(present, "the spawned agent must appear in list_sessions");
        assert!(hint_valid, "state_hint must be None or busy, got {hint:?}");
        assert!(
            empty.is_ok(),
            "empty write_line (submit-only) must be Ok, got {empty:?}"
        );
        assert!(
            streamed,
            "spawn_agent must stream at least one non-empty output frame from the bridge"
        );
    }

    /// Boot-attach regression (the blinking-block-cursor bug): when the persisted keymap
    /// holds ids for a pane the live server does NOT have (herdr reassigns pane ids on a
    /// restart-restore, so the on-disk ids go stale), `attach` must detect the dead id via
    /// `pane_list` and return `NoSuchSession` — NOT `Ok` with an empty, silently-dead
    /// bridge (which rendered as a blinking cursor until a manual restart). `NoSuchSession`
    /// is what the frontend respawns on. Gated behind `KEITORA_HERDR_BIN`.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn attach_stale_pane_returns_no_such_session_gated() {
        let Some(mut rig) =
            RealHerdrRig::new("attach_stale_pane_returns_no_such_session_gated", "stale")
        else {
            return;
        };
        let (backend, _events) = rig.booted();

        // Persist a key whose pane/terminal ids are NOT live on this server.
        let key = real_key("stale");
        backend.keymap.record(
            key.clone(),
            "ws-does-not-exist".to_string(),
            "pane-does-not-exist".to_string(),
            "term-does-not-exist".to_string(),
            "/no/such/cwd".to_string(),
        );

        let res = backend.attach(
            key.clone(),
            Some((80, 24)),
            Arc::new(fake::CaptureOutput::default()),
        );

        assert!(
            matches!(res, Err(PtyBackendError::NoSuchSession)),
            "attach to a stale/dead pane id must return NoSuchSession, got {res:?}"
        );
        assert!(
            backend.keymap.get(&key).is_none(),
            "attach must drop the dead keymap entry so it can't poison later boots"
        );
    }

    /// Boot-repaint: an idle pane streams no new output, so the frontend asks for one
    /// fresh viewport frame once its renderer is live. `repaint` must re-read the pane and
    /// deliver an output frame carrying the current screen — WITHOUT any new input or a
    /// bridge restart. Gated behind `KEITORA_HERDR_BIN`.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn repaint_emits_output_frame_gated() {
        let Some(mut rig) = RealHerdrRig::new("repaint_emits_output_frame_gated", "paint") else {
            return;
        };
        let (backend, _events) = rig.booted();

        let key = real_key("repaint");
        // The pane's output, as a mounted pane would have attached it.
        let output = Arc::new(fake::CaptureOutput::default());
        backend.outputs.register(&key, output.clone());
        let gen = backend
            .spawn(
                key.clone(),
                env!("CARGO_MANIFEST_DIR").to_string(),
                String::new(),
                Some((80, 24)),
                Some("echo REPAINT_MARKER".to_string()),
            )
            .expect("spawn");
        assert!(gen >= 1);
        // Let the echo land, then clear captured output so we only see the repaint frame.
        std::thread::sleep(Duration::from_millis(1500));
        output.clear();

        let res = backend.repaint(&key);

        assert!(res.is_ok(), "repaint returns Ok, got {res:?}");
        assert!(
            frame_arrives(&output, "REPAINT_MARKER", Duration::from_secs(4)),
            "repaint must deliver an output frame carrying the current viewport"
        );
    }

    /// What a second backend over the first one's home sees after a reconcile:
    /// Keitora restarted while the detached server, and `key`'s pane in `cwd`,
    /// kept running.
    struct Readopted {
        panes_before: usize,
        panes_after: usize,
        loaded: bool,
        live: bool,
        listed: bool,
    }

    /// Spawn `key` in `cwd` on a booted backend, then reconcile a fresh backend
    /// over the same home (its keymap loads from `keymap.json`).
    fn readopt_after_app_restart(rig: &mut RealHerdrRig, key: &SessionKey, cwd: &str) -> Readopted {
        let (backend1, _events1) = rig.booted();
        let gen1 = backend1
            .spawn(
                key.clone(),
                cwd.to_string(),
                String::new(),
                Some((80, 24)),
                Some("echo RECONCILE_SPAWN".to_string()),
            )
            .expect("spawn");
        assert!(gen1 >= 1);

        std::thread::sleep(Duration::from_millis(800));
        let panes = backend1.ops.pane_list().expect("pane list");
        eprintln!(
            "readopt: spawned in {cwd}; herdr reports {:?}",
            panes.iter().map(|p| p.cwd.clone()).collect::<Vec<_>>()
        );

        let (backend2, _events2) = rig.backend();
        let loaded = backend2.keymap.get(key).is_some();
        backend2.reconcile_current().expect("reconciled");

        Readopted {
            panes_before: panes.len(),
            panes_after: backend2.ops.pane_list().map(|p| p.len()).unwrap_or(0),
            loaded,
            live: backend2.is_live(key),
            listed: backend2
                .list_sessions()
                .unwrap_or_default()
                .iter()
                .any(|s| &s.key == key),
        }
    }

    impl Readopted {
        fn assert_adopted_once(&self) {
            assert!(
                self.panes_before >= 1,
                "spawn must create at least one pane"
            );
            assert!(
                self.loaded,
                "backend2 must LOAD the persisted key from keymap.json across the restart"
            );
            assert!(
                self.live && self.listed,
                "backend2 must re-adopt the existing session after reconcile (so attach \
                 succeeds and the frontend does not spawn a duplicate): live={} listed={}",
                self.live,
                self.listed
            );
            assert_eq!(
                self.panes_after, self.panes_before,
                "reconcile must NOT create a second pane for an already-live worktree"
            );
        }
    }

    /// Duplicate-workspace fix: spawn a pane via `backend1`
    /// (which persists the keymap), then construct `backend2` over the SAME herdr home
    /// — simulating an app restart while the detached server survives — and run
    /// `backend2.reconcile()`. `backend2` must re-adopt the existing pane BY CWD (so it
    /// is `is_live` / in `list_sessions`) WITHOUT creating a second pane (the pane_list
    /// count is unchanged). Gated behind `KEITORA_HERDR_BIN`.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn reconcile_adopts_persisted_pane_without_duplicate_gated() {
        let Some(mut rig) = RealHerdrRig::new(
            "reconcile_adopts_persisted_pane_without_duplicate_gated",
            "adopt",
        ) else {
            return;
        };
        let key = real_key("reconcile");
        let seen = readopt_after_app_restart(&mut rig, &key, env!("CARGO_MANIFEST_DIR"));
        seen.assert_adopted_once();
    }

    /// The same re-adoption when the worktree is reached through a symlink:
    /// herdr reports the pane's resolved cwd, Keitora holds the path it was
    /// given, and the two must still match.
    #[cfg(unix)]
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn reconcile_adopts_a_pane_whose_cwd_is_a_symlink_gated() {
        let Some(mut rig) = RealHerdrRig::new(
            "reconcile_adopts_a_pane_whose_cwd_is_a_symlink_gated",
            "link",
        ) else {
            return;
        };
        // Both live in the home, so the rig removes them.
        let target = rig.home.join("wt");
        let link = rig.home.join("ln");
        std::fs::create_dir_all(&target).expect("worktree dir");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let key = real_key("symlink");
        let seen = readopt_after_app_restart(&mut rig, &key, &link.to_string_lossy());
        seen.assert_adopted_once();
    }

    /// A server killed under a running backend is noticed and restarted by
    /// recovery: another process answers, the remembered instance follows it
    /// and the server generation moves on. Only the server this test started
    /// is killed, by the pid the supervisor spawned.
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn a_killed_server_is_restarted_by_recovery_gated() {
        use crate::backend::platform;
        let Some(mut rig) =
            RealHerdrRig::new("a_killed_server_is_restarted_by_recovery_gated", "kill")
        else {
            return;
        };
        let (backend, events) = rig.booted();

        let pid = backend.supervisor.spawned_pid().expect("boot spawned it");
        let before = backend.ops.server_instance().expect("server identified");
        assert_eq!(before.pid, pid, "the scan finds the server boot spawned");
        assert!(before.start_time > 0, "its start time is read: {before:?}");
        assert_eq!(backend.keymap.instance(), Some(before), "and remembered");
        let generation = backend.keymap.generation();

        let key = real_key("kill");
        backend
            .spawn(
                key.clone(),
                env!("CARGO_MANIFEST_DIR").to_string(),
                String::new(),
                Some((80, 24)),
                None,
            )
            .expect("spawn");

        let killed_at = Instant::now();
        platform::kill_group_kill(pid);
        wait_for("the death to be noticed", || {
            backend.keymap.generation() != generation
        });
        let noticed = killed_at.elapsed();
        wait_recovered(&backend);
        let recovered = killed_at.elapsed();

        let after = backend.ops.server_instance();
        let panes = backend.ops.pane_list().map(|p| p.len());
        eprintln!(
            "kill_gated: before={before:?} after={after:?} noticed={noticed:?} \
             recovered={recovered:?} generation {}->{} panes={panes:?} entry={:?} \
             owed={:?} phases={:?}",
            generation.0,
            backend.keymap.generation().0,
            backend.keymap.get(&key),
            backend.keymap.should_be_running(),
            recovery_phases(&events),
        );

        assert!(!platform::process_is_alive(pid), "the kill landed");
        assert!(rig.server_running(), "recovery started a server");
        let after = after.expect("new server identified");
        assert_ne!(after.pid, pid, "it is another process");
        assert!(after.start_time > 0, "its start time is read: {after:?}");
        assert_eq!(backend.supervisor.spawned_pid(), Some(after.pid));
        assert_eq!(
            backend.keymap.instance(),
            Some(after),
            "the new server is the one remembered"
        );
        assert!(
            backend.keymap.generation().0 > generation.0,
            "the server generation advanced"
        );
    }

    /// Keitora relaunched over a server that kept running adopts it instead of
    /// starting another. Nothing waits on an adopted server's process, so when
    /// it is killed the status loop's probes are what notice, and recovery
    /// starts a new one. (The server is found by scanning for the herdr
    /// binary, so no other server of that binary may be running.)
    #[test]
    #[ignore = "spawns a REAL herdr server; isolated/CI only — never run on a machine with a live herdr (run with --ignored in a dedicated env)"]
    fn an_adopted_server_that_is_killed_is_restarted_by_recovery_gated() {
        use crate::backend::platform;
        let Some(mut rig) = RealHerdrRig::new(
            "an_adopted_server_that_is_killed_is_restarted_by_recovery_gated",
            "adoptkill",
        ) else {
            return;
        };
        let (first, _events) = rig.booted();
        let pid = first.supervisor.spawned_pid().expect("boot spawned it");
        let before = first.keymap.instance().expect("server remembered");
        // The first Keitora is gone: it no longer reacts to its server.
        first.supervisor.expect_stop();
        first.recovery.explicit_stop();

        let (second, events) = rig.booted();
        assert_eq!(
            second.supervisor.spawned_pid(),
            None,
            "the running server is adopted, not replaced"
        );
        assert_eq!(
            second.ops.server_instance(),
            Some(before),
            "a process scan finds the same server"
        );
        assert_eq!(second.keymap.instance(), Some(before));
        let generation = second.keymap.generation();

        let mut lp = second.status_loop();
        let t0 = Instant::now();
        lp.tick(t0);
        platform::kill_group_kill(pid);
        wait_for("the kill to land", || !platform::process_is_alive(pid));
        // One-second wakes, as the loop's own thread would make them.
        let mut wakes = 0;
        while second.keymap.generation() == generation {
            wakes += 1;
            assert!(wakes <= 120, "the status loop never reported the death");
            lp.tick(t0 + Duration::from_secs(wakes));
            std::thread::sleep(Duration::from_millis(20));
        }
        wait_recovered(&second);

        let after = second.ops.server_instance();
        eprintln!(
            "adopted_kill_gated: before={before:?} after={after:?} wakes={wakes} \
             generation {}->{} phases={:?}",
            generation.0,
            second.keymap.generation().0,
            recovery_phases(&events),
        );
        assert!(rig.server_running(), "recovery started a server");
        let after = after.expect("new server identified");
        assert_ne!(after.pid, pid, "it is another process");
        assert_eq!(second.supervisor.spawned_pid(), Some(after.pid));
        assert_eq!(second.keymap.instance(), Some(after));
        assert!(second.keymap.generation().0 > generation.0);
    }
}
