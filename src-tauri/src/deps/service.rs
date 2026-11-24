//! The dependency service: one per app, managed in Tauri state. It owns the status
//! snapshot every window reads, the upgrade prompt, and the single install mutex; the
//! install/upgrade sequence itself lives in [`super::upgrade`].

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::agent_updates::probe::Prober;
use crate::backend::herdr::exe_missing::NOT_FOUND_CODE;
use crate::backend::herdr::recovery::RecoveryPhase;
use crate::backend::SharedBackend;

use super::floor::{FloorSlot, FloorState, HERDR_FLOOR};
use super::manifest::{Net, Platform};
use super::manual::{self, DetailsInput, ManualRoute};
use super::preflight::{
    classify_layout, upgrade_route, InstallLayout, ManualReason, Tier, UpgradeRoute,
};
use super::{binary_version, FailureKind, Fs, NextStep, Proc, Stage};

pub const STATUS_EVENT: &str = "deps://status";
pub const PROGRESS_EVENT: &str = "deps://progress";

/// Same-stage progress (download chunks, script output) is sent at most this often.
const PROGRESS_THROTTLE: Duration = Duration::from_millis(250);
/// Install log lines kept for "Copy details".
const LOG_TAIL: usize = 200;
/// The file under `KEITORA_HOME` that records the herdr paths Keitora installed itself.
const INSTALLED_RECORD: &str = "deps-installed.json";
/// How often recovery's phase is read while waiting for it to settle.
const SETTLE_POLL: Duration = Duration::from_millis(50);

/// How long a restart waits for recovery to settle, and how a restart it could not
/// make is retried later. Shortened by tests.
#[derive(Debug, Clone, Copy)]
pub(super) struct Timing {
    /// The wait for recovery to settle before a restart (one made while it is busy
    /// would be a no-op).
    pub settle: Duration,
    /// How often a deferred restart re-checks recovery.
    pub retry_poll: Duration,
    /// How long a deferred restart keeps trying before the run is marked failed.
    pub retry_limit: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            settle: Duration::from_secs(30),
            retry_poll: Duration::from_secs(1),
            retry_limit: Duration::from_secs(600),
        }
    }
}

/// Where the current (or last) install run stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunState {
    Idle,
    Running {
        run_id: u64,
        tier: Tier,
        stage: Stage,
        upgrade: bool,
    },
    Done {
        run_id: u64,
        path: String,
        /// The new herdr is pinned but recovery was busy, so it starts once
        /// recovery settles (or on the next restart).
        restart_pending: bool,
    },
    Failed {
        run_id: u64,
        kind: FailureKind,
        message: String,
        next: NextStep,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    TooOld,
    ProtocolMismatch,
}

/// The upgrade dialog the frontend should show. `hard` prompts have no "Later".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpgradePrompt {
    pub kind: PromptKind,
    pub found: String,
    pub hard: bool,
}

/// `deps_status` reply and the whole `deps://status` payload (replaces, never merges).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DepsSnapshot {
    /// False iff the backend reports herdr as not found.
    pub herdr_installed: bool,
    pub floor: FloorState,
    /// `[pty] herdr_bin`.
    pub pinned_bin: Option<String>,
    /// `Platform::key()`; `None` on an OS Keitora can't install herdr on.
    pub platform: Option<String>,
    pub run: RunState,
    pub prompt: Option<UpgradePrompt>,
    /// Live sessions an upgrade would restart (for the confirm dialog).
    pub live_sessions: usize,
    /// How an upgrade of the herdr in use would go; `None` when none is found.
    pub route: Option<UpgradeRoute>,
    /// A herdr the vendor script would overwrite that Keitora did not install
    /// (macOS `~/.local/bin/herdr`). Ask before passing `confirm_overwrite`.
    pub overwrite_target: Option<String>,
    /// Copyable install commands and the guide link for the manual route, with the
    /// reason the last run fell back to it.
    pub manual: ManualRoute,
}

/// `deps://progress` payload: sent on every stage change and on throttled
/// same-stage updates; byte counts are best effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    pub run_id: u64,
    pub stage: Stage,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// The latest install log line, when there is one.
    pub line: Option<String>,
}

/// Where status and progress go: the webview in the app, a recorder in tests.
/// Called with the service's lock held (so events stay in order); it must not call
/// back into the service.
pub trait DepsSink: Send + Sync {
    fn emit_status(&self, snapshot: &DepsSnapshot);
    fn emit_progress(&self, event: &ProgressEvent);
}

/// The `[pty] herdr_bin` pin and herdr discovery, behind a trait so tests never touch
/// the real config file or search the machine.
pub trait HerdrConfig: Send + Sync {
    fn pinned(&self) -> Option<PathBuf>;
    /// Write (`Some`) or clear (`None`) the pin.
    fn set_pinned(&self, path: Option<PathBuf>) -> Result<(), String>;
    /// The herdr Keitora would launch now (the pin, else discovery) as an absolute
    /// path, or `None` when none is found.
    fn current_exe(&self) -> Option<PathBuf>;
}

/// Everything about the machine and build the service needs besides its I/O traits.
pub struct DepsContext {
    /// `KEITORA_HOME` (the managed install root lives under it).
    pub keitora_home: PathBuf,
    /// The user's home; `None` when it can't be found (tier 1 is then unavailable).
    pub home: Option<PathBuf>,
    /// A snapshot of the process environment.
    pub env: Vec<(String, String)>,
    pub release_build: bool,
    pub app_version: String,
    pub config: Box<dyn HerdrConfig>,
    /// Finds claude/codex for the doctor.
    pub prober: Box<dyn Prober>,
}

impl DepsContext {
    pub(super) fn env_set(&self, key: &str) -> bool {
        self.env.iter().any(|(k, v)| k == key && !v.is_empty())
    }
}

/// What the service last learned about the herdr binary on disk.
#[derive(Debug, Clone, Default)]
pub(super) struct Machine {
    pub pinned: Option<PathBuf>,
    pub current: Option<PathBuf>,
    /// `--version` of `current`.
    pub current_version: Option<semver::Version>,
    pub route: Option<UpgradeRoute>,
    pub overwrite_target: Option<PathBuf>,
}

/// What the service last read from the backend (cached so the floor observer never
/// calls into it).
#[derive(Debug, Clone, Copy)]
struct BackendView {
    herdr_installed: bool,
    live_sessions: usize,
}

/// Progress bookkeeping of the current run.
struct Track {
    run_id: u64,
    stage: Stage,
    downloaded: u64,
    total: Option<u64>,
    last_emit: Instant,
    terminal: bool,
}

pub(super) struct State {
    next_run_id: u64,
    run: RunState,
    track: Option<Track>,
    prompt: Option<UpgradePrompt>,
    /// Versions a soft prompt was already raised for this session.
    prompted: HashSet<String>,
    pub machine: Machine,
    view: BackendView,
    log_tail: VecDeque<String>,
    last_tier: Option<Tier>,
    last_failure: Option<(FailureKind, String)>,
    /// Why the last run found no automatic tier.
    manual_reason: Option<ManualReason>,
    /// A restart recovery was too busy to take; a retry thread owes it.
    restart_owed: bool,
    /// Bumped by every restart the service makes, so a deferred restart can tell
    /// that one was made after it was deferred.
    restart_epoch: u64,
    /// `set_herdr_bin` is validating and writing the pin; installs are refused.
    pinning: bool,
}

pub(super) struct Inner {
    pub backend: SharedBackend,
    pub floor: Arc<FloorSlot>,
    pub net: Box<dyn Net>,
    pub proc_: Box<dyn Proc>,
    pub fs: Box<dyn Fs>,
    sink: Box<dyn DepsSink>,
    pub platform: Option<Platform>,
    pub ctx: DepsContext,
    state: Mutex<State>,
    /// Serializes emits made after the state lock is released (see [`Inner::emit_after`]).
    emit: Mutex<()>,
    /// This instance, for the threads it spawns (a deferred restart).
    me: OnceLock<Weak<Inner>>,
    timing: Mutex<Timing>,
}

/// The dependency service. Cheap to clone (one shared instance).
#[derive(Clone)]
pub struct DepsService {
    inner: Arc<Inner>,
}

/// Recovery phases in which `PtyBackend::restart` actually restarts (in any other
/// phase it is a silent no-op).
pub(super) fn restart_takes_effect(phase: RecoveryPhase) -> bool {
    matches!(
        phase,
        RecoveryPhase::Healthy
            | RecoveryPhase::Down
            | RecoveryPhase::AwaitHeadroom
            | RecoveryPhase::Paused
    )
}

/// The prompt after the floor became `floor`: a protocol mismatch is always a hard
/// prompt; a too-old server raises a soft prompt once per version per session;
/// anything else clears it.
fn next_prompt(
    prompt: &Option<UpgradePrompt>,
    prompted: &mut HashSet<String>,
    floor: &FloorState,
) -> Option<UpgradePrompt> {
    match floor {
        FloorState::ProtocolMismatch { found, .. } => Some(UpgradePrompt {
            kind: PromptKind::ProtocolMismatch,
            found: found.to_string(),
            hard: true,
        }),
        FloorState::TooOld { found } => {
            if prompted.insert(found.clone()) {
                Some(UpgradePrompt {
                    kind: PromptKind::TooOld,
                    found: found.clone(),
                    hard: false,
                })
            } else {
                // Already asked about this version (maybe snoozed); a hard prompt
                // from an earlier mismatch no longer applies.
                prompt.clone().filter(|p| !p.hard)
            }
        }
        _ => None,
    }
}

/// The floor the frontend sees: a running server other than the pinned binary only
/// needs a restart when that binary is acceptable (at least the floor). This holds
/// for a too-old server, a protocol mismatch (the pin is the fix, e.g. a compatible
/// managed copy) and an ok server whose pin was changed while it ran. `server_version`
/// is what the running server reported; a too-old judgement carries its own.
fn effective_floor(
    slot: FloorState,
    server_version: Option<&str>,
    machine: &Machine,
) -> FloorState {
    let server = match &slot {
        FloorState::TooOld { found } => found.clone(),
        FloorState::Ok | FloorState::ProtocolMismatch { .. } => match server_version {
            Some(v) => {
                super::floor::parse_version(v).map_or_else(|| v.to_string(), |p| p.to_string())
            }
            None => return slot,
        },
        _ => return slot,
    };
    let binary = match (&machine.pinned, &machine.current, &machine.current_version) {
        (Some(pin), Some(cur), Some(v)) if pin == cur => v,
        _ => return slot,
    };
    let differs = super::floor::parse_version(&server).is_none_or(|s| *binary != s);
    if *binary >= HERDR_FLOOR && differs {
        FloorState::RestartToFinish {
            binary: binary.to_string(),
            server,
        }
    } else {
        slot
    }
}

/// Releases `set_herdr_bin`'s claim on the pin when it returns, however it returns.
struct PinClaim<'a>(&'a Inner);

impl Drop for PinClaim<'_> {
    fn drop(&mut self) {
        self.0.lock().pinning = false;
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InstalledRecord {
    #[serde(default)]
    keitora_installed: Vec<String>,
}

fn same_path(a: &Path, b: &str) -> bool {
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    norm(&a.to_string_lossy()) == norm(b)
}

impl Inner {
    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Release the state lock, then emit what was built under it: the snapshot first, then
    /// the progress event. The emit lock is taken before the state lock drops, so events
    /// still leave in the order the state changed, but no IPC runs under the state lock
    /// (the floor observer takes it from the status loop).
    fn emit_after(
        &self,
        st: MutexGuard<'_, State>,
        snap: Option<&DepsSnapshot>,
        progress: Option<&ProgressEvent>,
    ) {
        let _emit = self.emit.lock().unwrap_or_else(|e| e.into_inner());
        drop(st);
        if let Some(snap) = snap {
            self.sink.emit_status(snap);
        }
        if let Some(event) = progress {
            self.sink.emit_progress(event);
        }
    }

    fn read_view(&self) -> BackendView {
        BackendView {
            herdr_installed: self.backend.recovery_snapshot().reason_code != Some(NOT_FOUND_CODE),
            live_sessions: self.backend.live_set_snapshot().len(),
        }
    }

    fn build(&self, st: &State) -> DepsSnapshot {
        let m = &st.machine;
        let floor = effective_floor(self.floor.get(), self.floor.server_version().as_deref(), m);
        // The upgrade is already on disk; asking for it again would mislead.
        let prompt = if matches!(floor, FloorState::RestartToFinish { .. }) {
            None
        } else {
            st.prompt.clone()
        };
        DepsSnapshot {
            herdr_installed: st.view.herdr_installed,
            floor,
            pinned_bin: m.pinned.as_ref().map(|p| p.display().to_string()),
            platform: self.platform.map(|p| p.key().to_string()),
            run: st.run.clone(),
            prompt,
            live_sessions: st.view.live_sessions,
            route: m.route.clone(),
            overwrite_target: m.overwrite_target.as_ref().map(|p| p.display().to_string()),
            manual: manual::manual_route(self.platform, st.manual_reason),
        }
    }

    /// Record why the current run found no automatic tier.
    pub(super) fn set_manual_reason(&self, reason: ManualReason) {
        self.lock().manual_reason = Some(reason);
    }

    /// Re-read the backend, then build and emit the snapshot.
    pub(super) fn publish(&self) -> DepsSnapshot {
        let view = self.read_view();
        let mut st = self.lock();
        st.view = view;
        let snap = self.build(&st);
        self.emit_after(st, Some(&snap), None);
        snap
    }

    fn on_floor(&self, floor: &FloorState) {
        let mut guard = self.lock();
        let st = &mut *guard;
        st.prompt = next_prompt(&st.prompt, &mut st.prompted, floor);
        let snap = self.build(st);
        self.emit_after(guard, Some(&snap), None);
    }

    // ── Machine view ──

    fn record_path(&self) -> PathBuf {
        self.ctx.keitora_home.join(INSTALLED_RECORD)
    }

    fn installed_record(&self) -> InstalledRecord {
        self.fs
            .read_to_string(&self.record_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Whether Keitora itself installed the herdr at `path`.
    pub(super) fn keitora_installed(&self, path: &Path) -> bool {
        self.installed_record()
            .keitora_installed
            .iter()
            .any(|p| same_path(path, p))
    }

    /// Remember that Keitora installed `path` (so a later script run may replace it).
    pub(super) fn record_installed(&self, path: &Path) {
        let mut rec = self.installed_record();
        if rec.keitora_installed.iter().any(|p| same_path(path, p)) {
            return;
        }
        rec.keitora_installed.push(path.display().to_string());
        let write = serde_json::to_vec_pretty(&rec)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                self.fs
                    .create_dir_all(&self.ctx.keitora_home)
                    .and_then(|_| self.fs.write(&self.record_path(), &bytes))
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = write {
            log::warn!("[deps] could not record the installed herdr: {e}");
        }
    }

    /// macOS `~/.local/bin/herdr`, where the vendor script installs in place.
    fn script_target(&self) -> Option<PathBuf> {
        let platform = self.platform?;
        if platform.is_windows() {
            return None;
        }
        Some(
            self.ctx
                .home
                .as_ref()?
                .join(".local")
                .join("bin")
                .join("herdr"),
        )
    }

    /// The layout of `path`, where a `~/.local/bin` herdr counts as the vendor's only
    /// when Keitora installed it (otherwise it is the user's own copy).
    fn layout(&self, path: &Path) -> Option<InstallLayout> {
        let platform = self.platform?;
        let home = self.ctx.home.as_ref()?;
        let layout = classify_layout(path, &self.ctx.keitora_home, home, platform);
        let in_local_bin = self
            .script_target()
            .and_then(|t| t.parent().map(|d| path.starts_with(d)))
            .unwrap_or(false);
        if layout == InstallLayout::Standalone && in_local_bin && !self.keitora_installed(path) {
            return Some(InstallLayout::External);
        }
        Some(layout)
    }

    /// Read the pin, find the herdr in use, and run its `--version`. Never holds the
    /// lock while it does I/O.
    pub(super) fn refresh_machine(&self) {
        let pinned = self.ctx.config.pinned();
        let current = self.ctx.config.current_exe();
        let current_version = current
            .as_ref()
            .and_then(|exe| binary_version(&*self.proc_, exe).ok())
            .map(|(v, _)| v);
        let route = current
            .as_ref()
            .and_then(|exe| Some(upgrade_route(self.layout(exe)?, exe)));
        let overwrite_target = self
            .script_target()
            .filter(|t| self.fs.exists(t) && !self.keitora_installed(t));
        self.lock().machine = Machine {
            pinned,
            current,
            current_version,
            route,
            overwrite_target,
        };
    }

    // ── Runs ──

    /// Whether the backend's server answers right now.
    pub(super) fn server_running(&self) -> bool {
        self.backend.status_payload_snapshot().state == "Connected"
    }

    /// Whether herdr speaks a newer protocol than Keitora (upgrading herdr can't help).
    pub(super) fn newer_protocol(&self) -> bool {
        matches!(
            self.floor.get(),
            FloorState::ProtocolMismatch { newer: true, .. }
        )
    }

    pub(super) fn timing(&self) -> Timing {
        *self.timing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait (bounded by [`Timing::settle`]) until a restart would take effect.
    fn wait_restartable(&self) -> bool {
        let deadline = Instant::now() + self.timing().settle;
        loop {
            if restart_takes_effect(self.backend.recovery_snapshot().phase) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(SETTLE_POLL);
        }
    }

    /// Restart the backend and refresh the agent integration. Recovery may be busy
    /// (resuming sessions, or a restart someone else asked for), in which case
    /// `PtyBackend::restart` would be a silent no-op, so this waits for it to settle
    /// first; only if it never does is the restart handed to a retry thread, and
    /// `Ok(true)` says it is pending. herdr is never left down silently.
    pub(super) fn restart_backend(&self) -> Result<bool, String> {
        if !self.wait_restartable() {
            self.defer_restart();
            return Ok(true);
        }
        // This restart supersedes any one still owed: the retry thread must not
        // restart herdr a second time behind it.
        {
            let mut st = self.lock();
            st.restart_owed = false;
            st.restart_epoch += 1;
        }
        self.backend.restart().map_err(|e| e.to_string())?;
        self.backend.refresh_integration();
        // A finished run that was waiting on a deferred restart has had it now.
        if let RunState::Done {
            restart_pending, ..
        } = &mut self.lock().run
        {
            *restart_pending = false;
        }
        Ok(false)
    }

    /// Owe a restart to a retry thread that makes it once recovery settles.
    fn defer_restart(&self) {
        let epoch = {
            let mut st = self.lock();
            if st.restart_owed {
                return;
            }
            st.restart_owed = true;
            st.restart_epoch
        };
        let server_was_running = self.server_running();
        log::warn!("[deps] recovery is busy; herdr restarts once it settles");
        let Some(inner) = self.me.get().and_then(Weak::upgrade) else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("deps-restart".into())
            .spawn(move || inner.retry_restart(epoch, server_was_running));
        if let Err(e) = spawned {
            self.lock().restart_owed = false;
            self.restart_failed(format!("could not schedule the herdr restart: {e}"));
        }
    }

    /// Whether the restart owed since `epoch` is still owed: no restart was made
    /// since it was deferred.
    fn still_owed(st: &State, epoch: u64) -> bool {
        st.restart_owed && st.restart_epoch == epoch
    }

    /// The deferred restart: poll recovery until a restart takes effect, then make it
    /// and refresh the integration. It is dropped when a restart was made since it
    /// was deferred, or when herdr was down then and something else has started it
    /// since (a restart re-reads the pin, so that server already runs the new binary).
    fn retry_restart(&self, epoch: u64, server_was_running: bool) {
        let timing = self.timing();
        let deadline = Instant::now() + timing.retry_limit;
        loop {
            std::thread::sleep(timing.retry_poll);
            if !Self::still_owed(&self.lock(), epoch) {
                return;
            }
            if !restart_takes_effect(self.backend.recovery_snapshot().phase) {
                if Instant::now() >= deadline {
                    {
                        let mut st = self.lock();
                        if !Self::still_owed(&st, epoch) {
                            return;
                        }
                        st.restart_owed = false;
                    }
                    self.restart_failed(
                        "herdr did not restart because recovery never settled; restart it \
                         from Settings"
                            .into(),
                    );
                    return;
                }
                continue;
            }
            let started_since = !server_was_running && self.server_running();
            // Claim the owed restart under the lock, so a direct restart made
            // meanwhile and this one never both happen.
            {
                let mut st = self.lock();
                if !Self::still_owed(&st, epoch) {
                    return;
                }
                st.restart_owed = false;
                st.restart_epoch += 1;
            }
            let restarted = if started_since {
                log::info!("[deps] herdr was started meanwhile; the deferred restart is dropped");
                Ok(())
            } else {
                self.backend.restart().map_err(|e| e.to_string())
            };
            match restarted {
                Ok(()) => {
                    self.backend.refresh_integration();
                    log::info!("[deps] the deferred herdr restart was made");
                    let mut st = self.lock();
                    if let RunState::Done {
                        restart_pending, ..
                    } = &mut st.run
                    {
                        *restart_pending = false;
                    }
                    drop(st);
                    self.refresh_machine();
                    self.publish();
                }
                Err(e) => self.restart_failed(format!("herdr did not restart: {e}")),
            }
            return;
        }
    }

    /// A deferred restart failed: a run still waiting on it ends failed (so the UI
    /// says herdr is down), and the snapshot is published.
    fn restart_failed(&self, message: String) {
        log::warn!("[deps] {message}");
        {
            let mut st = self.lock();
            if let RunState::Done {
                run_id,
                restart_pending: true,
                ..
            } = st.run
            {
                let tier = st.last_tier.unwrap_or(Tier::Direct);
                st.last_failure = Some((FailureKind::Incomplete, message.clone()));
                st.run = RunState::Failed {
                    run_id,
                    kind: FailureKind::Incomplete,
                    message,
                    next: super::after_failure(FailureKind::Incomplete, tier, false),
                };
            }
        }
        self.publish();
    }

    /// The run's tier, once chosen.
    pub(super) fn set_tier(&self, run_id: u64, tier: Tier) {
        let mut st = self.lock();
        st.last_tier = Some(tier);
        if let RunState::Running {
            run_id: id,
            tier: t,
            ..
        } = &mut st.run
        {
            if *id == run_id {
                *t = tier;
                let snap = self.build(&st);
                self.emit_after(st, Some(&snap), None);
            }
        }
    }

    /// Report progress for `run_id`. Stages only move forward; a same-stage update
    /// is throttled unless it completes a download.
    pub(super) fn progress(
        &self,
        run_id: u64,
        stage: Stage,
        downloaded: u64,
        total: Option<u64>,
        line: Option<String>,
    ) {
        debug_assert!(!stage.is_terminal());
        let mut st = self.lock();
        let now = Instant::now();
        let Some(track) = st
            .track
            .as_mut()
            .filter(|t| t.run_id == run_id && !t.terminal)
        else {
            return;
        };
        if stage < track.stage {
            return;
        }
        let changed = stage != track.stage;
        let completes = total.is_some_and(|t| t == downloaded && t > 0);
        if !changed && !completes && now.duration_since(track.last_emit) < PROGRESS_THROTTLE {
            return;
        }
        track.stage = stage;
        track.downloaded = downloaded;
        track.total = total;
        track.last_emit = now;
        let event = ProgressEvent {
            run_id,
            stage,
            downloaded,
            total,
            line,
        };
        let snap = changed.then(|| {
            if let RunState::Running { stage: s, .. } = &mut st.run {
                *s = stage;
            }
            self.build(&st)
        });
        self.emit_after(st, snap.as_ref(), Some(&event));
    }

    /// One line of install output: kept for "Copy details" and sent as progress.
    pub(super) fn log_line(&self, run_id: u64, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let stage = {
            let mut st = self.lock();
            st.log_tail.push_back(line.to_string());
            while st.log_tail.len() > LOG_TAIL {
                st.log_tail.pop_front();
            }
            match &st.track {
                Some(t) if t.run_id == run_id && !t.terminal => t.stage,
                _ => return,
            }
        };
        self.progress(run_id, stage, 0, None, Some(line.to_string()));
    }

    /// End `run_id` with exactly one terminal progress event, then refresh what the
    /// service knows about the binary and publish the new snapshot.
    pub(super) fn finish(&self, run_id: u64, outcome: super::upgrade::Outcome) {
        use super::upgrade::Outcome;
        {
            let mut st = self.lock();
            let Some(track) = st
                .track
                .as_mut()
                .filter(|t| t.run_id == run_id && !t.terminal)
            else {
                return;
            };
            track.terminal = true;
            let (downloaded, total) = (track.downloaded, track.total);
            let (stage, run) = match outcome {
                Outcome::Done {
                    path,
                    restart_pending,
                } => {
                    st.last_failure = None;
                    (
                        Stage::Done,
                        RunState::Done {
                            run_id,
                            path: path.display().to_string(),
                            restart_pending,
                        },
                    )
                }
                Outcome::Failed {
                    tier,
                    failure,
                    direct_viable,
                } => {
                    log::warn!(
                        "[deps] install run {run_id} failed ({:?}): {}",
                        failure.kind,
                        failure.message
                    );
                    st.last_failure = Some((failure.kind, failure.message.clone()));
                    (
                        Stage::Failed,
                        RunState::Failed {
                            run_id,
                            kind: failure.kind,
                            message: failure.message,
                            next: super::after_failure(failure.kind, tier, direct_viable),
                        },
                    )
                }
            };
            st.run = run;
            let event = ProgressEvent {
                run_id,
                stage,
                downloaded,
                total,
                line: None,
            };
            self.emit_after(st, None, Some(&event));
        }
        self.refresh_machine();
        self.publish();
    }
}

impl DepsService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: SharedBackend,
        floor: Arc<FloorSlot>,
        net: Box<dyn Net>,
        proc_: Box<dyn Proc>,
        fs: Box<dyn Fs>,
        sink: Box<dyn DepsSink>,
        platform: Option<Platform>,
        ctx: DepsContext,
    ) -> Self {
        let mut prompted = HashSet::new();
        let prompt = next_prompt(&None, &mut prompted, &floor.get());
        let inner = Arc::new(Inner {
            backend,
            floor: Arc::clone(&floor),
            net,
            proc_,
            fs,
            sink,
            platform,
            ctx,
            state: Mutex::new(State {
                next_run_id: 1,
                run: RunState::Idle,
                track: None,
                prompt,
                prompted,
                machine: Machine::default(),
                view: BackendView {
                    herdr_installed: true,
                    live_sessions: 0,
                },
                log_tail: VecDeque::new(),
                last_tier: None,
                last_failure: None,
                manual_reason: None,
                restart_owed: false,
                restart_epoch: 0,
                pinning: false,
            }),
            emit: Mutex::new(()),
            me: OnceLock::new(),
            timing: Mutex::new(Timing::default()),
        });
        let _ = inner.me.set(Arc::downgrade(&inner));
        // Weak: the slot lives inside the backend, which outlives nothing here.
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        floor.on_change(Box::new(move |state| {
            if let Some(inner) = weak.upgrade() {
                inner.on_floor(state);
            }
        }));
        DepsService { inner }
    }

    /// The current snapshot (re-reads the backend's not-found state and live count).
    pub fn snapshot(&self) -> DepsSnapshot {
        let view = self.inner.read_view();
        let mut st = self.inner.lock();
        st.view = view;
        self.inner.build(&st)
    }

    /// Re-read the pin and the binary's `--version`, then publish.
    pub fn recheck(&self) -> DepsSnapshot {
        self.inner.refresh_machine();
        self.inner.publish()
    }

    /// Start an install or upgrade, or attach to the one already running. `tier`:
    /// `None` lets preflight decide; `Some(Direct)` forces a managed download (also
    /// the "install a Keitora-managed copy instead" route). `confirm_overwrite`: the
    /// user agreed to replace [`DepsSnapshot::overwrite_target`].
    pub fn start_install(&self, tier: Option<Tier>, confirm_overwrite: bool) -> Result<(), String> {
        if tier == Some(Tier::Manual) {
            return Err("the manual route has nothing to install".into());
        }
        // A herdr ahead of Keitora is never upgraded: the fix is a compatible
        // Keitora-managed copy.
        let tier = if tier.is_none() && self.inner.newer_protocol() {
            Some(Tier::Direct)
        } else {
            tier
        };
        let upgrade = self.inner.server_running();
        let run_id = {
            let mut st = self.inner.lock();
            if matches!(st.run, RunState::Running { .. }) {
                return Ok(());
            }
            if st.pinning {
                return Err("the herdr path is being changed; try again in a moment".into());
            }
            if tier.is_none() {
                if let Some(UpgradeRoute::External { instruction }) = &st.machine.route {
                    return Err(instruction.clone());
                }
            }
            let run_id = st.next_run_id;
            st.next_run_id += 1;
            let guess = match (&tier, &st.machine.route) {
                (Some(t), _) => *t,
                (None, Some(UpgradeRoute::ScriptThenDirect)) => Tier::Script,
                _ => Tier::Direct,
            };
            st.run = RunState::Running {
                run_id,
                tier: guess,
                stage: Stage::Checking,
                upgrade,
            };
            st.track = Some(Track {
                run_id,
                stage: Stage::Checking,
                downloaded: 0,
                total: None,
                last_emit: Instant::now(),
                terminal: false,
            });
            st.log_tail.clear();
            st.manual_reason = None;
            let snap = self.inner.build(&st);
            let event = ProgressEvent {
                run_id,
                stage: Stage::Checking,
                downloaded: 0,
                total: None,
                line: None,
            };
            self.inner.emit_after(st, Some(&snap), Some(&event));
            run_id
        };
        let inner = Arc::clone(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("deps-install".into())
            .spawn(move || inner.run(run_id, tier, upgrade, confirm_overwrite));
        if let Err(e) = spawned {
            self.inner.finish(
                run_id,
                super::upgrade::Outcome::Failed {
                    tier: tier.unwrap_or(Tier::Direct),
                    failure: super::InstallFailure::new(
                        FailureKind::Other,
                        format!("could not start the install: {e}"),
                    ),
                    direct_viable: false,
                },
            );
        }
        Ok(())
    }

    /// "Later": clears a soft prompt for the rest of the session. A hard
    /// (protocol mismatch) prompt stays.
    pub fn snooze_prompt(&self) {
        let mut st = self.inner.lock();
        if st.prompt.as_ref().is_some_and(|p| !p.hard) {
            st.prompt = None;
            let snap = self.inner.build(&st);
            self.inner.emit_after(st, Some(&snap), None);
        }
    }

    /// Pin (`Some`, validated by `--version` and the floor before anything is written)
    /// or clear (`None`) the herdr Keitora runs. With no server running the backend is
    /// restarted onto it; with one running only the pin changes and the snapshot shows
    /// `restart_to_finish`.
    pub fn set_herdr_bin(&self, path: Option<PathBuf>) -> Result<DepsSnapshot, String> {
        // Claimed under the lock and held to the end, so an install can't start (and
        // repin) between the check and the write.
        let _claim = {
            let mut st = self.inner.lock();
            if matches!(st.run, RunState::Running { .. }) {
                return Err("an install is in progress".into());
            }
            if st.pinning {
                return Err("the herdr path is already being changed".into());
            }
            st.pinning = true;
            PinClaim(&self.inner)
        };
        if let Some(p) = &path {
            manual::validate_browse_path(p, &*self.inner.proc_).map_err(|e| e.to_string())?;
        }
        let running = self.inner.server_running();
        self.inner.ctx.config.set_pinned(path)?;
        let restarted = if running {
            Ok(())
        } else {
            self.inner.restart_backend().map(|_| ())
        };
        self.inner.refresh_machine();
        let snap = self.inner.publish();
        restarted.map_err(|e| format!("herdr is pinned but did not start: {e}"))?;
        Ok(snap)
    }

    /// "Restart to finish": restart herdr onto the pinned binary and refresh the agent
    /// integration, as an upgrade's own restart does. A restart recovery is too busy
    /// for is made once it settles.
    pub fn restart_to_finish(&self) -> Result<DepsSnapshot, String> {
        if matches!(self.inner.lock().run, RunState::Running { .. }) {
            return Err("an install is in progress".into());
        }
        let restarted = self.inner.restart_backend();
        self.inner.refresh_machine();
        let snap = self.inner.publish();
        restarted.map_err(|e| format!("herdr did not restart: {e}"))?;
        Ok(snap)
    }

    /// Shorten the restart waits.
    #[cfg(test)]
    pub(super) fn set_timing(&self, timing: Timing) {
        *self.inner.timing.lock().unwrap() = timing;
    }

    /// The plain-text "Copy details" report.
    pub fn details_report(&self) -> String {
        let st = self.inner.lock();
        let failure = st.last_failure.clone();
        let current = st.machine.current.as_ref().map(|p| p.display().to_string());
        let log: Vec<String> = st.log_tail.iter().cloned().collect();
        manual::details_report(&DetailsInput {
            app_version: &self.inner.ctx.app_version,
            platform: self.inner.platform,
            tier: st.last_tier,
            failure: failure.as_ref().map(|(k, m)| (*k, m.as_str())),
            herdr_path: current.as_deref(),
            log_tail: &log,
        })
    }

    /// Rows for herdr, git, claude and codex. Installs nothing.
    pub fn doctor(&self) -> Vec<super::doctor::DoctorRow> {
        super::doctor::rows(&self.inner)
    }
}
