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
//! The DB-only drive commands (`drives_list`, `drive_release`, `drive_take_over`,
//! `drive_pause`) work for any registered project, with or without a window.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::sync::mpsc;

use keitora_tasks::{
    CliKind, Drive, DriveRequest, LocalStore, ProjectConfig, DRIVE_READ_LINES_DEFAULT,
    DRIVE_READ_LINES_MAX, DRIVE_READ_RESULT_MAX, DRIVE_REQUEST_MAX_AGE_SECS, DRIVE_RETENTION_SECS,
    DRIVE_SEND_TEXT_MAX,
};

use crate::activity::lock;
use crate::backend::{
    agent_hints, map_write_line_result, HintMap, SessionKey, SharedBackend, WriteLineOutcome,
};
use crate::dispatch::{
    collapse, injection_detail, transfer_refusal, unknown_worktree_refusal, DispatchGate,
    SUBMIT_DELAY,
};
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
    /// starting: the hook's `busy` write lags the keystrokes.
    sent: Mutex<HashMap<(String, String), RecentSend>>,
    /// (project, send request id) whose target was woken for it, or that waited on a
    /// reset/swap/wake holding its target's slot: the send then waits for idle like a
    /// queued one.
    woken: Mutex<HashMap<WaiterKey, WakeMark>>,
    /// (project, send request id) with a follow-up pass scheduled, and when it fires.
    rechecks: Mutex<HashMap<WaiterKey, Instant>>,
}

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
    /// The turn the send is waiting for; a finished turn this far means it ran.
    turn: i64,
}

/// Managed state shared by the runner and the drive commands. Cheap to clone; every
/// clone sees the same state.
#[derive(Default, Clone)]
pub struct DriveRuntime(Arc<RuntimeInner>);

/// Releases a worktree's reset/swap slot on drop.
pub(crate) struct InFlightGuard {
    runtime: DriveRuntime,
    key: (String, String),
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        lock(&self.runtime.0.in_flight).remove(&self.key);
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
    /// The open set, or `None` before the window's first report.
    pub(crate) fn open_set(&self) -> Option<HashSet<String>> {
        lock(&self.0.open).clone()
    }

    /// Store the window's open projects. `true` when this is the first report or the
    /// set changed.
    pub(crate) fn set_open(&self, projects: Vec<String>) -> bool {
        let next: HashSet<String> = projects.into_iter().collect();
        let mut open = lock(&self.0.open);
        if open.as_ref() == Some(&next) {
            return false;
        }
        *open = Some(next);
        true
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

    /// Hold `worktree` busy: a `send` was just typed into it, awaiting `turn`.
    fn mark_sent(&self, project: &str, worktree: &str, turn: i64) {
        lock(&self.0.sent).insert(
            (project.to_string(), worktree.to_string()),
            RecentSend {
                at: Instant::now(),
                turn,
            },
        );
    }

    fn clear_sent(&self, project: &str, worktree: &str) {
        lock(&self.0.sent).remove(&(project.to_string(), worktree.to_string()));
    }

    /// Whether a recent `send` still holds `worktree` busy. The hold ends (and is
    /// forgotten) once the session is seen out of `idle`, the awaited turn has
    /// finished, or `settle` has elapsed.
    fn send_settling(
        &self,
        project: &str,
        worktree: &str,
        state: &str,
        finished_turn: i64,
        settle: Duration,
    ) -> bool {
        let key = (project.to_string(), worktree.to_string());
        let mut sent = lock(&self.0.sent);
        let Some(recent) = sent.get(&key) else {
            return false;
        };
        if state != "idle" || finished_turn >= recent.turn || recent.at.elapsed() >= settle {
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
    /// Get `worktree` ready to launch `cli` after a provider change: Antigravity needs
    /// its global hook entry, workspace trust and herdr's integration in place first.
    fn prepare_launch(&self, config: &ProjectConfig, worktree: &str, cli: CliKind);
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

    fn prepare_launch(&self, config: &ProjectConfig, worktree: &str, cli: CliKind) {
        if cli != CliKind::Antigravity {
            return;
        }
        use tauri::Manager as _;
        crate::antigravity_hooks::sync_worktree(&config.worktree_path(worktree));
        if let Some(backend) = self.0.try_state::<SharedBackend>() {
            backend.inner().refresh_integration();
        }
    }

    fn schedule_pass(&self, project: &str, after: Duration) {
        let app = self.0.clone();
        let project = project.to_string();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(after).await;
            run_drive_requests(app, project).await;
        });
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

/// Pure: why a `send` can't go to a session in `state`; `None` when idle.
fn state_refusal(state: &str, worktree: &str) -> Option<String> {
    match state {
        "idle" => None,
        "busy" => Some(format!(
            "@{worktree} is busy; send only goes to an idle session (use --queue to wait for it)"
        )),
        "waiting" => Some(format!(
            "@{worktree} is waiting on a prompt; answer it first (or use --queue)"
        )),
        "offline" => Some(format!("@{worktree} is offline; start its session first")),
        other => Some(format!("@{worktree} is {other}")),
    }
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
}

impl DriveEnv {
    fn store(&self) -> Result<LocalStore, String> {
        LocalStore::open(&self.db_path).map_err(|e| e.to_string())
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

    fn terminated(&self, worktree: &str) -> bool {
        self.backend
            .terminated_keys(&self.project)
            .contains(worktree)
    }

    /// Whether `worktree` has a running agent: a live pane that isn't terminated.
    fn awake(&self, worktree: &str) -> bool {
        !self.terminated(worktree)
            && self
                .backend
                .live_set_snapshot()
                .contains(&self.key(worktree))
    }

    /// The backend's session sets from `snapshot`, taken on first use and reused until
    /// the caller clears it.
    fn sets<'a>(&self, snapshot: &'a mut Option<SessionSets>) -> &'a SessionSets {
        snapshot.get_or_insert_with(|| SessionSets {
            live: self.backend.live_set_snapshot(),
            hints: agent_hints(self.backend.as_ref()),
            terminated: self.backend.terminated_keys(&self.project),
        })
    }

    /// `worktree`'s reconciled session state (`offline` without a row), from the
    /// session sets in `snapshot`.
    fn session_state(
        &self,
        local: &LocalStore,
        snapshot: &mut Option<SessionSets>,
        worktree: &str,
    ) -> Result<String, String> {
        let sets = self.sets(snapshot);
        let sessions = crate::tasks::reconciled_sessions_in(
            local,
            &sets.live,
            &sets.hints,
            &self.project,
            &self.config,
        )?;
        Ok(sessions
            .into_iter()
            .find(|s| s.worktree == worktree)
            .map(|s| s.state)
            .unwrap_or_else(|| "offline".to_string()))
    }

    /// [`Self::session_state`], but `busy` while a send just typed into `worktree`
    /// hasn't been seen starting its turn.
    fn send_state(
        &self,
        local: &LocalStore,
        snapshot: &mut Option<SessionSets>,
        worktree: &str,
    ) -> Result<String, String> {
        let state = self.session_state(local, snapshot, worktree)?;
        let finished = local
            .session_reply_get(worktree)
            .ok()
            .flatten()
            .map_or(0, |r| r.turn);
        if self.runtime.send_settling(
            &self.project,
            worktree,
            &state,
            finished,
            self.timeouts.send_settle,
        ) {
            return Ok("busy".to_string());
        }
        Ok(state)
    }
}

/// The backend's session sets, snapshotted once per pass.
struct SessionSets {
    live: HashSet<SessionKey>,
    hints: HintMap,
    /// Worktrees whose session is terminated.
    terminated: HashSet<String>,
}

impl SessionSets {
    /// Whether `key` has a running agent: a live pane that isn't terminated.
    fn awake(&self, key: &SessionKey) -> bool {
        !self.terminated.contains(&key.worktree) && self.live.contains(key)
    }
}

/// A reset/swap body, spawned by the caller.
pub(crate) type Job = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Housekeeping once per pass; errors are logged and ignored.
fn housekeeping(local: &LocalStore, env: &DriveEnv, now: chrono::DateTime<chrono::Utc>) {
    match local.drive_requests_expire(env.timeouts.max_age_secs) {
        Ok(0) => {}
        Ok(n) => log::info!("drive requests: {n} expired in {}", env.project),
        Err(e) => log::warn!("drive requests: expiry failed: {e}"),
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

/// One pass over `env`'s pending requests, oldest first. `send`/`read` run inline and
/// are resolved here; begun `reset`/`swap` bodies are returned for the caller to spawn.
pub(crate) async fn run_pass(env: &Arc<DriveEnv>, gate: &DispatchGate) -> Vec<Job> {
    let mut jobs: Vec<Job> = Vec::new();
    let local = match env.store() {
        Ok(l) => l,
        Err(e) => {
            log::warn!("drive requests: cannot open {}: {e}", env.db_path);
            return jobs;
        }
    };
    housekeeping(&local, env, chrono::Utc::now());
    let Some(open) = env.runtime.open_set() else {
        return jobs; // the window hasn't reported yet: leave rows pending
    };
    let pending = match local.drive_requests_pending() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("drive requests: cannot list pending: {e}");
            return jobs;
        }
    };
    let pending_ids: HashSet<i64> = pending.iter().map(|r| r.id).collect();
    env.runtime.retain_woken(&env.project, &pending_ids);
    let configured = env.config.worktrees();
    // Backend session sets, shared by every send in this pass; dropped after a
    // delivery so the next check sees the session the send just woke.
    let mut snapshot: Option<SessionSets> = None;
    // Drive rows by driven worktree, loaded for the first waking send.
    let mut drives: Option<HashMap<String, Drive>> = None;
    for req in pending {
        let parsed = parse_request(&req.kind, &req.payload);
        // A send to a worktree with no running agent wakes it first (unless the send
        // opted out); it then waits, un-begun, for idle like a queued send.
        let wake = if matches!(parsed, Ok(Parsed::Send { wake: true, .. }))
            && open.contains(&env.project)
        {
            let drives = drives.get_or_insert_with(|| {
                local
                    .drives_list()
                    .map(|ds| ds.into_iter().map(|d| (d.driven.clone(), d)).collect())
                    .unwrap_or_default()
            });
            let facts = GateFacts {
                open: &open,
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
            WakeGate::Wait => continue,
            WakeGate::Start(claim) => {
                jobs.push(Box::pin(run_auto_wake(env.clone(), req, claim)));
                continue;
            }
            WakeGate::Refuse(r) => {
                woke_but_down = Some(r);
                false
            }
        };
        // A queued send waits, un-begun, until its target is idle (the hook's idle
        // write is itself a DB change, so it fires promptly).
        if queued && matches!(parsed, Ok(Parsed::Send { .. })) && open.contains(&env.project) {
            let state = {
                let _g = gate.0.lock().await;
                env.send_state(&local, &mut snapshot, &req.worktree)
            };
            if state.as_deref() != Ok("idle") {
                continue;
            }
        }
        match local.drive_request_begin(req.id) {
            Ok(true) => {}
            Ok(false) => continue, // another pass took it
            Err(e) => {
                log::warn!("drive request {}: cannot begin: {e}", req.id);
                continue;
            }
        }
        let track = env.runtime.track(&env.project, req.id);
        env.runtime.forget_woken(&env.project, req.id);
        if let Some(r) = woke_but_down {
            resolve(&local, &req, &env.project, &Outcome::refused(r));
            continue;
        }
        let parsed = match parsed {
            Ok(p) => p,
            Err(r) => {
                resolve(&local, &req, &env.project, &Outcome::refused(r));
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
        let drive = local.drive_get(&req.worktree).ok().flatten();
        let refusal = common_refusal(
            &req.kind,
            &env.project,
            &req.worktree,
            &req.requested_by,
            &GateFacts {
                open: &open,
                configured: &configured,
                drive: drive.as_ref(),
                transfer: transfer_refusal(&env.registry, &env.project, &req.worktree),
                terminated: !is_wake && env.terminated(&req.worktree),
            },
        );
        if let Some(r) = refusal {
            resolve(&local, &req, &env.project, &Outcome::refused(r));
            continue;
        }
        let outcome = match parsed {
            Parsed::Send { text, .. } => match sanitize_prompt(&text) {
                Err(r) => Outcome::refused(r),
                Ok(prompt) => {
                    let _g = gate.0.lock().await;
                    match env.send_state(&local, &mut snapshot, &req.worktree) {
                        Err(e) => Outcome::refused(e),
                        Ok(state) => match state_refusal(&state, &req.worktree) {
                            Some(r) => Outcome::refused(r),
                            None => {
                                snapshot = None;
                                deliver_send(env, &local, &req.worktree, &prompt)
                            }
                        },
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
                        jobs.push(Box::pin(run_job(env.clone(), req, p, claim, track)));
                        continue;
                    }
                }
            }
        };
        resolve(&local, &req, &env.project, &outcome);
        drop(track);
    }
    jobs
}

/// What a send does about waking its target in this pass.
enum WakeGate {
    /// Go on to the send path; `woken` when the target was woken for this send, so it
    /// waits for idle like a queued send.
    Proceed { woken: bool },
    /// Leave the send pending (a wake is in flight, or just finished).
    Wait,
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
    if gated.is_some() || sets.awake(&env.key(wt)) {
        return WakeGate::Proceed {
            woken: mark.is_some(),
        };
    }
    // A reset, swap or wake holds the slot (this send's own wake, still running, or
    // another request's): wait for it. Waiting isn't waking, so it doesn't start this
    // send's grace period; the holder's end is a DB write or a scheduled pass.
    let Some(claim) = rt.claim_worktree(project, wt) else {
        rt.mark_slot_wait(project, req.id);
        return WakeGate::Wait;
    };
    match mark {
        // Woken a moment ago: give the live set a moment to show the new session, and
        // make sure a pass comes back once that moment is over.
        Some(WakeMark { own: true, at }) if at.elapsed() < env.timeouts.send_settle => {
            env.recheck_after_settle(req.id, at);
            WakeGate::Wait
        }
        Some(WakeMark { own: true, .. }) => WakeGate::Refuse(format!(
            "@{wt} was woken but its agent did not come up; check its pane"
        )),
        // Not woken for this send yet (at most it waited on another request): wake it.
        _ => {
            rt.mark_woken(project, req.id);
            WakeGate::Start(claim)
        }
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

/// Write a sanitised prompt into an idle session (the caller holds the dispatch gate).
/// `awaiting_turn` is set before the write so a fast Stop can't be missed, and
/// restored if the prompt wasn't submitted. A delivered send holds the worktree busy
/// until its turn is seen starting.
fn deliver_send(env: &DriveEnv, local: &LocalStore, worktree: &str, prompt: &Prompt) -> Outcome {
    let key = env.key(worktree);
    let previous = local
        .drive_get(worktree)
        .ok()
        .flatten()
        .and_then(|d| d.awaiting_turn);
    let turn = match local.drive_set_awaiting_turn(worktree) {
        Ok(Some(n)) => n,
        Ok(None) => return Outcome::refused(format!("@{worktree} is not driven")),
        Err(e) => return Outcome::refused(format!("could not record the turn: {e}")),
    };
    let delay = SUBMIT_DELAY.as_millis() as u32;
    let restore_and_refuse = |result: &Result<(), crate::backend::PtyBackendError>| {
        let _ = local.drive_restore_awaiting_turn(worktree, previous);
        let detail = injection_detail(result, env.terminated(worktree));
        Outcome::refused(format!("could not send to @{worktree} ({detail})"))
    };
    let applied = || {
        env.runtime.mark_sent(&env.project, worktree, turn);
        Outcome::applied(serde_json::json!({ "awaiting_turn": turn }).to_string())
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
                    let _ = local.drive_restore_awaiting_turn(worktree, previous);
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
    if mode == RestartMode::Wake && env.awake(wt) {
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
    if changed {
        // Before the restart: the new provider must find its hooks and trust in place.
        let host = env.host.clone();
        let config = env.config.clone();
        let worktree = wt.to_string();
        let prepared = tokio::task::spawn_blocking(move || {
            host.prepare_launch(&config, &worktree, now_on);
        })
        .await;
        if let Err(e) = prepared {
            log::warn!("drive reset: preparing @{wt} for {now_on:?} panicked: {e}");
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
/// `[project].name` from the registry; the tasks-DB watchers call this (through
/// `dispatch::spawn_pending_requests`) when they see a pending row. Safe to run
/// concurrently with itself: each row is taken with a compare-and-set.
pub(crate) async fn run_drive_requests<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project: String,
) {
    use tauri::Manager;

    let (Some(client), Some(gate), Some(registry), Some(runtime)) = (
        app.try_state::<SharedBackend>(),
        app.try_state::<DispatchGate>(),
        app.try_state::<TransferRegistry>(),
        app.try_state::<DriveRuntime>(),
    ) else {
        return; // still starting up; the next DB event retries
    };
    let Some(config) = keitora_tasks::UserConfig::load()
        .ok()
        .and_then(|u| u.project_config_by_name(&project))
    else {
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
    });
    for job in run_pass(&env, &gate).await {
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

/// The window's open (warm) projects, by resolved name. When the set changes, every
/// registered project's pending requests get another pass.
#[tauri::command(async)]
pub fn drive_set_open_projects(
    app: tauri::AppHandle,
    projects: Vec<String>,
    runtime: State<'_, DriveRuntime>,
) -> Result<(), String> {
    if runtime.set_open(projects) {
        retrigger_pending(&app);
    }
    Ok(())
}

/// Give every registered project's pending requests a pass.
fn retrigger_pending<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    let Ok(user) = keitora_tasks::UserConfig::load() else {
        return;
    };
    for pref in user.projects.values().filter(|p| p.path.is_dir()) {
        let Ok(pc) = keitora_tasks::effective_config(pref) else {
            continue;
        };
        // Read-only: a project whose DB doesn't exist yet has nothing pending.
        if let Ok(local) = LocalStore::open_read_only(&pc.tasks.db_path.to_string_lossy()) {
            crate::dispatch::spawn_pending_requests(app, &local, &pc.project.name);
        }
    }
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
