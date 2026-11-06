//! Claude ⇄ Codex session transfer.
//!
//! Two Tauri commands drive a worktree's live conversation from one CLI to the
//! other and flip its persisted `cli` so a subsequent restart resumes on the new
//! side:
//!
//!   * [`preview_claude_transfer`] — lock-free and side-effect-free. Runs the
//!     [live-session lookup](find_live_claude_jsonl) and returns a [`ConfirmPreview`]
//!     so the rail menu can show a blocking confirm before anything irreversible
//!     happens. It does not take the dispatch lock, spawn `app-server`, or import.
//!   * [`transfer_worktree`] — only after the user confirms.
//!       - **Claude → Codex**: a full *live* import via `codex app-server`'s
//!         `externalAgentConfig/import` (mirrors the official `openai-codex`
//!         plugin's `/codex:transfer`). Completion is judged from the app-server's
//!         own results (the completion notification's per-item successes and
//!         failures), with a path-only lookup in Codex's import ledger as the
//!         fallback when the notification carries no answer.
//!       - **Codex → Claude**: a *summary* handoff (no supported reverse importer
//!         exists). Runs `codex exec resume` under a structured output schema,
//!         writes the parsed summary to a handoff file for the frontend to inject,
//!         and flips `cli=claude`.
//!
//! The app-server client is a minimal JSON-RPC-2.0-over-stdio client, generic over
//! its streams (a spawned child in production, an in-process fake in tests): stdout
//! and stderr pump threads, an `mpsc` frame channel, and one receive loop that
//! honours a cancel flag and a single import deadline. The blocking work runs under
//! `tauri::async_runtime::spawn_blocking`. DIRECT connection only (no broker).
//! Protocol literals (the handshake `capabilities`, the migration payload, the
//! completion-notification method, the `-32601` upgrade message) are reproduced
//! verbatim from `codex-cli 0.149.0` + the reference plugin.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::State;

use keitora_tasks::{CliKind, FieldIntent};

use crate::backend::herdr::recovery::RecoveryHandle;
use crate::backend::herdr::{configure_command, truncate_at_char_boundary};
use crate::backend::{PtyBackend, ResumeRecord, SessionKey, SharedBackend};
use crate::codex_env::{CodexEnv, CodexHomeSource, ExeSource};
use crate::dispatch::DispatchGate;
use crate::transfer_registry::{TrailStage, TransferGuard, TransferRegistry};

#[cfg(test)]
mod fake_app_server;

// ---------------------------------------------------------------------------
// Protocol constants (verbatim from codex-cli 0.149.0 + the reference plugin)
// ---------------------------------------------------------------------------

/// The completion notification method (`codex.mjs:51`
/// `EXTERNAL_AGENT_IMPORT_COMPLETED`). The `externalAgentConfig/import` request
/// response alone does NOT imply the import finished — the completion is signalled
/// out-of-band by this notification.
const EXTERNAL_AGENT_IMPORT_COMPLETED: &str = "externalAgentConfig/import/completed";

/// Progress notifications Codex may emit while an import runs.
const EXTERNAL_AGENT_IMPORT_PROGRESS: &str = "externalAgentConfig/import/progress";

/// One budget for the whole import: spawn + initialize + import + completion.
const IMPORT_DEADLINE: Duration = Duration::from_secs(150);

/// The receive slice: bounds how long a cancel takes to be noticed.
const POLL_SLICE: Duration = Duration::from_millis(250);

/// How long a normal teardown waits for app-server to exit on stdin EOF before
/// killing it.
const TEARDOWN_GRACE: Duration = Duration::from_secs(3);

/// How long to keep re-reading the import ledger for a late flush. Runs outside
/// the import deadline.
const LEDGER_WINDOW: Duration = Duration::from_secs(3);

/// Ledger re-read interval inside [`LEDGER_WINDOW`].
const LEDGER_POLL: Duration = Duration::from_millis(250);

/// Codex's import ledger, under the Codex home.
const LEDGER_FILE: &str = "external_agent_session_imports.json";

/// Stderr ring caps: at most this many lines, this many bytes in total, and this
/// many bytes per line.
const STDERR_RING_MAX_LINES: usize = 200;
const STDERR_RING_MAX_BYTES: usize = 16 * 1024;
const STDERR_LINE_MAX_BYTES: usize = 2 * 1024;

/// A bounded ceiling on the headless `codex exec resume` summary run so a wedged
/// child can't hang the blocking thread forever.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(180);

/// The exact `-32601` upgrade message (`codex.mjs:1072-1076`). `npm install -g`,
/// not the abbreviated `npm i -g`.
const UPGRADE_MSG: &str = "This Codex version does not support Claude session transfer. Update Codex with `npm install -g @openai/codex@latest`, then retry.";

/// The explicit summarize instruction for the Codex → Claude handoff.
const HANDOFF_PROMPT: &str = "Summarize this conversation for a handoff to a new session: goal, key decisions, next steps, open files, and any other notes.";

/// The `--output-schema` file contents for the Codex → Claude handoff. All five
/// fields required, `additionalProperties: false`.
const HANDOFF_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "goal": {"type": "string"},
    "key_decisions": {"type": "array", "items": {"type": "string"}},
    "next_steps": {"type": "array", "items": {"type": "string"}},
    "open_files": {"type": "array", "items": {"type": "string"}},
    "notes": {"type": "string"}
  },
  "required": ["goal", "key_decisions", "next_steps", "open_files", "notes"],
  "additionalProperties": false
}"#;

// ---------------------------------------------------------------------------
// IPC payloads
// ---------------------------------------------------------------------------

/// Confirm-preview payload shown before a Claude → Codex transfer. The
/// mtime-newest lookup is the weakest link; this lets the rail show the user which
/// JSONL was selected before the irreversible import fires.
#[derive(Debug, Clone, Serialize)]
pub struct ConfirmPreview {
    /// Full JSONL path (post-normalization, containment-guard-checked).
    pub path: String,
    /// Basename only, i.e. `<uuid>.jsonl`.
    pub filename: String,
    /// Last-modified time, RFC 3339 (the value used to pick "newest").
    pub mtime: String,
    /// The first real user line (`type=user`, not `isMeta`), truncated for display.
    pub first_user_line: String,
}

/// The Codex → Claude handoff summary shape (matches [`HANDOFF_SCHEMA`]). Written
/// to [`handoff_summary_path`] for [`inject_handoff_summary`] to inject as the
/// fresh session's opening prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandoffSummary {
    pub goal: String,
    #[serde(default)]
    pub key_decisions: Vec<String>,
    #[serde(default)]
    pub next_steps: Vec<String>,
    #[serde(default)]
    pub open_files: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

impl HandoffSummary {
    /// Fallback used when the headless `codex exec resume` summarize can't run (e.g.
    /// Codex usage limit or auth). The Codex→Claude switch still completes; this note
    /// is injected into the fresh Claude session so it's clear why no prior context
    /// carried over. `reason` is truncated — a raw `codex` failure can be very long.
    fn unavailable(reason: &str) -> Self {
        HandoffSummary {
            goal: "Handed off from a Codex session; continuing on Claude.".to_string(),
            key_decisions: Vec::new(),
            next_steps: Vec::new(),
            open_files: Vec::new(),
            notes: format!(
                "An automatic Codex handoff summary could not be generated ({}). No prior context was carried over.",
                truncate_chars(reason.trim(), 200)
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Transfer outcome and progress contract
// ---------------------------------------------------------------------------
//
// Wire convention: internally tagged with `kind`, snake_case variant tags,
// camelCase field names. Serialize only; nothing comes back from the webview.

/// The structured result of a transfer. `Fresh` and `Imported` are returned only
/// after the worktree was persisted as Codex; `Cancelled` only before anything
/// was changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TransferOutcome {
    /// Claude → Codex: resume the imported thread.
    Imported { thread_id: String },
    /// Claude → Codex: the import failed; the worktree is Codex, start fresh.
    Fresh {
        reason: TransferFailure,
        reason_text: String,
    },
    /// Codex → Claude: the summary handoff is complete.
    Done,
    /// The user cancelled during the import; nothing changed, still Claude.
    Cancelled,
}

/// Why a Claude → Codex import did not produce a thread. `message` and
/// `stderr_tail` are diagnostics only; user-facing copy is [`TransferFailure::user_copy`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum TransferFailure {
    CodexNotFound {
        message: String,
        stderr_tail: String,
    },
    CodexTooOld {
        version: Option<String>,
        message: String,
        stderr_tail: String,
    },
    SessionNotDetected {
        message: String,
        stderr_tail: String,
    },
    Skipped {
        reason: SkipReason,
        message: String,
        stderr_tail: String,
    },
    Timeout {
        phase: TransferPhase,
        message: String,
        stderr_tail: String,
    },
    ProtocolError {
        message: String,
        stderr_tail: String,
    },
    Other {
        message: String,
        stderr_tail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    NoOp,
    Deferred,
    ContinuedIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Initialize,
    Import,
    Completion,
}

impl TransferFailure {
    /// The one place plain-language failure copy lives; it becomes
    /// `TransferOutcome::Fresh.reason_text`. One sentence, no trailing period (the
    /// frontend embeds it verbatim).
    pub fn user_copy(&self) -> String {
        match self {
            TransferFailure::CodexNotFound { .. } => "Codex could not be found",
            TransferFailure::CodexTooOld { .. } => {
                "This version of Codex cannot import Claude sessions"
            }
            TransferFailure::SessionNotDetected { .. } => {
                "Codex did not recognise the Claude session"
            }
            TransferFailure::Skipped { reason, .. } => match reason {
                SkipReason::NoOp => "Codex found nothing new to import",
                SkipReason::Deferred => {
                    "Codex postponed the import because the session is still in use"
                }
                SkipReason::ContinuedIn => {
                    "This Claude conversation was continued in another session"
                }
            },
            TransferFailure::Timeout { phase, .. } => match phase {
                TransferPhase::Initialize => "Codex did not start in time",
                TransferPhase::Import => "Codex did not accept the import in time",
                TransferPhase::Completion => "Codex did not finish the import in time",
            },
            TransferFailure::ProtocolError { .. } => "Codex returned an unexpected response",
            TransferFailure::Other { .. } => "The import did not complete",
        }
        .to_string()
    }

    /// Codex's own message and the captured stderr tail (diagnostics only).
    fn details(&self) -> (&str, &str) {
        match self {
            TransferFailure::CodexNotFound {
                message,
                stderr_tail,
            }
            | TransferFailure::CodexTooOld {
                message,
                stderr_tail,
                ..
            }
            | TransferFailure::SessionNotDetected {
                message,
                stderr_tail,
            }
            | TransferFailure::Skipped {
                message,
                stderr_tail,
                ..
            }
            | TransferFailure::Timeout {
                message,
                stderr_tail,
                ..
            }
            | TransferFailure::ProtocolError {
                message,
                stderr_tail,
            }
            | TransferFailure::Other {
                message,
                stderr_tail,
            } => (message, stderr_tail),
        }
    }
}

impl TransferPhase {
    fn as_str(self) -> &'static str {
        match self {
            TransferPhase::Initialize => "initialize",
            TransferPhase::Import => "import",
            TransferPhase::Completion => "completion",
        }
    }
}

/// Payload of the advisory `transfer://progress` event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub project: String,
    pub worktree: String,
    pub phase: ProgressPhase,
    /// Human-readable detail (e.g. Codex's progress text); `null` when none.
    pub message: Option<String>,
    /// Since the transfer began.
    pub elapsed_ms: u64,
    /// The overall import deadline.
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    Preflight,
    StartingCodex,
    Initializing,
    Importing,
    AwaitingCompletion,
    ReadingLedger,
    Persisting,
    Cancelling,
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

/// Resolve the worktree's live Claude JSONL and return the confirm preview.
/// Lock-free, no side effects (no `DispatchGate`, no `app-server`, no import).
#[tauri::command]
pub async fn preview_claude_transfer(
    project: String,
    worktree: String,
) -> Result<ConfirmPreview, String> {
    off_runtime(move || {
        let user = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        let config = resolve_project(&user, &project)?;
        let cwd = config.worktree_path(&worktree);
        let path = find_live_claude_jsonl(&cwd)?;
        build_preview(&path)
    })
    .await?
}

/// Run blocking file/config work on the blocking pool instead of a runtime
/// worker; a panic in it comes back as an error.
async fn off_runtime<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| format!("blocking task panicked: {e}"))
}

/// Load the user config and resolve `project` in it, off the runtime.
async fn load_project(
    project: &str,
) -> Result<(keitora_tasks::UserConfig, keitora_tasks::ProjectConfig), String> {
    let project = project.to_string();
    off_runtime(move || {
        let user = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
        let config = resolve_project(&user, &project)?;
        Ok((user, config))
    })
    .await?
}

/// Transfer the worktree to `target_cli`. `Codex` ⇒ Claude→Codex live import;
/// `Claude` ⇒ Codex→Claude summary handoff. Persists the new provider state and
/// returns the outcome; the frontend drives the PTY restart and then calls
/// `transfer_settled`.
///
/// `Err` means nothing changed: the session is terminated, the project cannot be
/// resolved, a transfer is already in flight for the worktree, Codex cannot be
/// found (Claude → Codex), or persisting failed.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // IPC arguments plus Tauri-injected state
pub async fn transfer_worktree(
    project: String,
    worktree: String,
    target_cli: CliKind,
    jsonl: Option<String>,
    gate: State<'_, DispatchGate>,
    client: State<'_, SharedBackend>,
    registry: State<'_, TransferRegistry>,
    app: tauri::AppHandle,
) -> Result<TransferOutcome, String> {
    // Declared before the recovery hold so it outlives it; dropping it releases
    // the marker unless the transfer committed.
    let guard = registry
        .begin(&project, &worktree)
        .map_err(|_| "a transfer is already in progress for this worktree".to_string())?;
    let key = SessionKey {
        project: project.clone(),
        worktree: worktree.clone(),
    };
    // Before anything is held or rewritten: a terminated session is not moved.
    refuse_terminated(client.inner().as_ref(), &key)?;
    let ctx = TransferCtx {
        project,
        worktree,
        jsonl,
        gate: gate.inner(),
        client: client.inner(),
        registry: registry.inner(),
        guard: &guard,
        app: &app,
    };
    // Recovery must not resume this session while the transfer rewrites it.
    let result = with_recovery_lock(client.recovery(), &key, async {
        match target_cli {
            CliKind::Codex => transfer_claude_to_codex(&ctx).await,
            CliKind::Claude => transfer_codex_to_claude(&ctx).await,
        }
    })
    .await;
    // The persisted provider state changed: the frontend refetches the worktree
    // list, which drives the rail glyph and the "Transfer to …" menu target.
    if changes_worktrees(&result) {
        use tauri::Emitter as _;
        let _ = app.emit("worktrees://changed", ());
    }
    result
}

/// Refuse to transfer a session the user terminated: it must be launched first.
fn refuse_terminated(client: &dyn PtyBackend, key: &SessionKey) -> Result<(), String> {
    if client.terminated_keys(&key.project).contains(&key.worktree) {
        return Err("session is terminated; launch it before transferring".into());
    }
    Ok(())
}

/// Run `transfer` with `key` held out of recovery's resume queue; the hold is
/// released when it returns, whatever the outcome.
async fn with_recovery_lock<T>(
    recovery: Option<RecoveryHandle>,
    key: &SessionKey,
    transfer: impl std::future::Future<Output = T>,
) -> T {
    let _recovery_lock = recovery.map(|r| r.transfer_lock(key));
    transfer.await
}

/// What both transfer directions need from the command.
struct TransferCtx<'a> {
    project: String,
    worktree: String,
    /// Claude → Codex: the transcript the user confirmed in the preview, if any.
    jsonl: Option<String>,
    gate: &'a DispatchGate,
    client: &'a SharedBackend,
    registry: &'a TransferRegistry,
    guard: &'a TransferGuard<'a>,
    app: &'a tauri::AppHandle,
}

/// Resolve a project by its resolved `[project].name`. Never falls back to the
/// app's current project: a transfer acts on the project the user clicked.
fn resolve_project(
    user: &keitora_tasks::UserConfig,
    name: &str,
) -> Result<keitora_tasks::ProjectConfig, String> {
    user.project_config_by_name(name)
        .ok_or_else(|| format!("project `{name}` not found"))
}

/// Whether a transfer result changed the persisted worktree state (so the
/// frontend must refetch). A cancel or an error changed nothing.
fn changes_worktrees(result: &Result<TransferOutcome, String>) -> bool {
    matches!(result, Ok(outcome) if *outcome != TransferOutcome::Cancelled)
}

/// The outcome a Claude → Codex import result becomes once it is persisted.
fn outcome_for(result: ImportResult) -> TransferOutcome {
    match result {
        ImportResult::Imported { thread_id } => TransferOutcome::Imported { thread_id },
        ImportResult::Failed(reason) => TransferOutcome::Fresh {
            reason_text: reason.user_copy(),
            reason,
        },
        ImportResult::Cancelled => TransferOutcome::Cancelled,
    }
}

/// The resume-store row matching a persist plan: a set thread is what the
/// worktree resumes, a cleared one leaves no usable stored session.
fn resume_record_for(plan: &PersistPlan) -> Option<ResumeRecord> {
    match &plan.codex_thread_id {
        FieldIntent::Set(thread_id) => Some(ResumeRecord::Codex {
            thread_id: thread_id.clone(),
        }),
        FieldIntent::Clear => Some(ResumeRecord::Cleared),
        FieldIntent::Keep => None,
    }
}

fn intent_label<T>(intent: &FieldIntent<T>) -> &'static str {
    match intent {
        FieldIntent::Keep => "keep",
        FieldIntent::Set(_) => "set",
        FieldIntent::Clear => "clear",
    }
}

/// The `classify` trail entry for the decided outcome: the failure's kind,
/// Codex's own message and the stderr tail, which the user-facing copy omits.
fn outcome_trail(result: &ImportResult) -> Value {
    match result {
        ImportResult::Imported { .. } => json!({ "outcome": "imported" }),
        ImportResult::Cancelled => json!({ "outcome": "cancelled" }),
        ImportResult::Failed(failure) => {
            let (message, stderr_tail) = failure.details();
            let kind = serde_json::to_value(failure)
                .ok()
                .and_then(|v| v.get("kind").cloned())
                .unwrap_or(Value::Null);
            json!({
                "outcome": if keeps_claude(failure) { "kept_claude" } else { "fresh" },
                "failure": kind,
                "message": message,
                "stderr_tail": stderr_tail,
            })
        }
    }
}

/// The `env` trail entry for the resolved Codex environment (paths are
/// home-scrubbed when the diagnostics are rendered).
fn env_trail(env: &CodexEnv) -> Value {
    json!({
        "exe": env.exe,
        "exe_source": match env.exe_source {
            ExeSource::Configured => "configured",
            ExeSource::Discovered => "discovered",
            ExeSource::BareName => "bare_name",
        },
        "version": env.version,
        "codex_home_source": codex_home_from(env.codex_home_source),
        "path_set": env.path.is_some(),
    })
}

/// Emits `transfer://progress` for one transfer. Cheap to clone into the
/// blocking import. `elapsed_ms` counts from the registry's `begin`: the guard's
/// elapsed time when the emitter was made plus the time since.
#[derive(Clone)]
struct ProgressEmitter {
    app: tauri::AppHandle,
    project: String,
    worktree: String,
    base_ms: u64,
    since: Instant,
}

impl ProgressEmitter {
    fn new(ctx: &TransferCtx<'_>) -> Self {
        ProgressEmitter {
            app: ctx.app.clone(),
            project: ctx.project.clone(),
            worktree: ctx.worktree.clone(),
            base_ms: ctx.guard.elapsed_ms(),
            since: Instant::now(),
        }
    }

    fn emit(&self, phase: ProgressPhase, message: Option<String>) {
        use tauri::Emitter as _;
        let payload = progress_payload(
            &self.project,
            &self.worktree,
            phase,
            message,
            self.base_ms + self.since.elapsed().as_millis() as u64,
        );
        let _ = self.app.emit("transfer://progress", &payload);
    }
}

/// Pure: the `transfer://progress` payload.
fn progress_payload(
    project: &str,
    worktree: &str,
    phase: ProgressPhase,
    message: Option<String>,
    elapsed_ms: u64,
) -> TransferProgress {
    TransferProgress {
        project: project.to_string(),
        worktree: worktree.to_string(),
        phase,
        message,
        elapsed_ms,
        deadline_ms: IMPORT_DEADLINE.as_millis() as u64,
    }
}

/// Run the short persist step with the dispatch gate held. The long work
/// (import, summarize) runs outside it, protected from dispatch by the
/// registry's transferring marker. The persist itself does file I/O, so it runs
/// on the blocking pool while the gate stays held across it.
pub(crate) async fn persist_under_gate<R: Send + 'static>(
    gate: &DispatchGate,
    persist: impl FnOnce() -> R + Send + 'static,
) -> Result<R, String> {
    let _gate = gate.0.lock().await;
    off_runtime(persist).await
}

/// Whether the commit step honours a pending cancel.
pub(crate) enum CommitCancel {
    /// Claude → Codex: a cancel requested before the commit wins.
    Honour,
    /// Codex → Claude: cancel is never observed; persist always runs.
    Ignore,
}

pub(crate) enum Committed<T> {
    Cancelled,
    Done(Result<T, String>),
}

/// The commit step, run inside [`persist_under_gate`]: mark the transfer
/// committed (atomically with the cancel check) BEFORE persisting, and settle
/// the marker if persisting then fails so an error never leaves it set.
pub(crate) fn commit_then_persist<T>(
    registry: &TransferRegistry,
    project: &str,
    worktree: &str,
    cancel: CommitCancel,
    persist: impl FnOnce() -> Result<T, String>,
) -> Committed<T> {
    let committed = registry.mark_committed(project, worktree);
    if !committed && matches!(cancel, CommitCancel::Honour) {
        return Committed::Cancelled;
    }
    let r = persist();
    if r.is_err() {
        registry.settle(project, worktree);
    }
    Committed::Done(r)
}

/// Overwrite the worktree's resume-store row after a persist. Best-effort: the
/// store only matters to a later default launch, so a failure is logged and
/// trailed, never an error.
fn record_resume(client: &dyn PtyBackend, key: &SessionKey, record: ResumeRecord) {
    if let Err(e) = client.set_resume_record(key, record) {
        crate::transfer_registry::trail_warn(
            &key.project,
            &key.worktree,
            TrailStage::Persist,
            json!({ "resume_record_error": e.to_string() }),
        );
    }
}

/// Claude → Codex: the full live import. Claude stays live and nothing is
/// persisted until the outcome is known; a failed import still switches the
/// worktree to Codex (started fresh), except when Codex cannot be found (an `Err`:
/// the worktree stays on Claude); a cancel changes nothing.
async fn transfer_claude_to_codex(ctx: &TransferCtx<'_>) -> Result<TransferOutcome, String> {
    let (user, config) = load_project(&ctx.project).await?;
    let cwd = config.worktree_path(&ctx.worktree);
    // The persisted Claude session id only goes to the trail: it is written by
    // transfers alone, so it can be stale and never chooses the file.
    let persisted = config
        .worktree_entries()
        .into_iter()
        .find(|e| e.name == ctx.worktree)
        .and_then(|e| e.claude_session_id);
    // The transcript the user confirmed must be this worktree's; anything else is
    // refused before anything runs.
    let confirmed = match ctx.jsonl.clone() {
        Some(path) => {
            let cwd = cwd.clone();
            Some(off_runtime(move || confirmed_claude_jsonl(&cwd, Path::new(&path))).await??)
        }
        None => None,
    };
    let progress = ProgressEmitter::new(ctx);
    let cancel = ctx.guard.cancel_flag();

    // Resolve Codex, pick the transcript, spawn app-server, handshake, one import
    // request, await and judge the completion. All blocking, so off the runtime,
    // and outside the dispatch gate.
    let (result, claude_session_id) = {
        let (project, worktree) = (ctx.project.clone(), ctx.worktree.clone());
        let progress = progress.clone();
        let config = config.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let trail = |stage: TrailStage, detail: Value| {
                crate::transfer_registry::trail(&project, &worktree, stage, detail)
            };
            let emit =
                |phase: ProgressPhase, message: Option<String>| progress.emit(phase, message);
            let probe = crate::codex_env::probe_codex_env_from_machine(&config, &user);
            let env = match crate::codex_env::resolve_codex_env(&probe) {
                Ok(env) => env,
                // Without a resolved environment no Codex can start, fresh or not.
                Err(message) => {
                    let failure = TransferFailure::CodexNotFound {
                        message,
                        stderr_tail: String::new(),
                    };
                    return (ImportResult::Failed(failure), None);
                }
            };
            let version = crate::codex_env::codex_version(&env.exe, env.path.as_deref());
            let env = env.with_version(version);
            trail(TrailStage::Env, env_trail(&env));
            let picked = match confirmed {
                Some(path) => Ok(path),
                None => find_live_claude_jsonl(&cwd),
            };
            let jsonl = match picked {
                Ok(jsonl) => jsonl,
                Err(message) => {
                    let failure = TransferFailure::SessionNotDetected {
                        message,
                        stderr_tail: String::new(),
                    };
                    return (ImportResult::Failed(failure), None);
                }
            };
            trail(
                TrailStage::Import,
                selection_trail(&jsonl, persisted.as_deref()),
            );
            let cwd_s = cwd.to_string_lossy().into_owned();
            let result = run_claude_import(&env, &cwd_s, &jsonl, cancel, &emit, &trail);
            (result, session_id_from_jsonl(&jsonl))
        })
        .await
        .map_err(|e| format!("import task panicked: {e}"))?
    };
    crate::transfer_registry::trail(
        &ctx.project,
        &ctx.worktree,
        TrailStage::Classify,
        outcome_trail(&result),
    );

    // A cancel observed after the import returned still changes nothing.
    if ctx.guard.cancel_flag().load(Ordering::Acquire) {
        return Ok(TransferOutcome::Cancelled);
    }
    // Codex missing: nothing is persisted and the live Claude session is untouched.
    let Some(plan) = persist_plan(&result, claude_session_id)? else {
        return Ok(TransferOutcome::Cancelled);
    };

    let key = SessionKey {
        project: ctx.project.clone(),
        worktree: ctx.worktree.clone(),
    };
    let registry = ctx.registry.clone();
    let client = ctx.client.clone();
    let (project, worktree) = (ctx.project.clone(), ctx.worktree.clone());
    let committed = persist_under_gate(ctx.gate, move || {
        commit_then_persist(&registry, &project, &worktree, CommitCancel::Honour, || {
            progress.emit(ProgressPhase::Persisting, None);
            crate::transfer_registry::trail(
                &project,
                &worktree,
                TrailStage::Persist,
                json!({
                    "cli": "codex",
                    "codex_thread_id": intent_label(&plan.codex_thread_id),
                    "claude_session_id": intent_label(&plan.claude_session_id),
                }),
            );
            persist_worktree_cli(
                &config,
                &worktree,
                plan.cli,
                plan.codex_thread_id.clone(),
                plan.claude_session_id.clone(),
            )?;
            if let Some(record) = resume_record_for(&plan) {
                record_resume(client.as_ref(), &key, record);
            }
            Ok(())
        })
    })
    .await?;
    match committed {
        Committed::Cancelled => Ok(TransferOutcome::Cancelled),
        Committed::Done(Err(e)) => {
            crate::transfer_registry::trail_warn(
                &ctx.project,
                &ctx.worktree,
                TrailStage::Persist,
                json!({ "error": e }),
            );
            Err(e)
        }
        Committed::Done(Ok(())) => Ok(outcome_for(result)),
    }
}

/// Codex → Claude: write a summary, flip `cli=claude`, and let the frontend restart
/// the captured Claude session before injecting the summary.
///
/// The worktree's interactive PTY runs `codex resume <thread_id>`, which holds that
/// thread as its exclusive "active writer" in the Codex thread-store. A headless
/// `codex exec resume <thread_id>` (the summarize step) is refused with a
/// `thread-store conflict … already has an active writer` (`-32600`) while that live
/// session exists. So the live session is KILLED and its writer confirmed released
/// BEFORE summarizing — only then can the headless resume open the thread.
async fn transfer_codex_to_claude(ctx: &TransferCtx<'_>) -> Result<TransferOutcome, String> {
    let (user, config) = load_project(&ctx.project).await?;
    let cwd = config.worktree_path(&ctx.worktree);

    // Prefer the persisted thread id over `--last` (unproven to target the same
    // thread the interactive PTY runs). Read-only.
    //
    // A NATIVE Codex worktree (default_cli=codex, launched via `codex resume --last`,
    // never imported from Claude) legitimately has NO persisted thread id — yet the
    // rail still offers "Transfer to Claude". So this is `Option`, not a hard error:
    // when present we summarize by resuming the id; when absent we summarize via
    // `codex exec resume --last`, which (after the live session is killed) resolves
    // to this cwd's most-recent rollout.
    let thread_id = config
        .worktree_entries()
        .into_iter()
        .find(|e| e.name == ctx.worktree)
        .and_then(|e| e.codex_thread_id);

    let cwd_s = cwd.to_string_lossy().into_owned();
    let key = SessionKey {
        project: ctx.project.clone(),
        worktree: ctx.worktree.clone(),
    };
    // `SharedBackend` is `Arc`-backed; the clone shares the same connection + cached
    // live-set as the app's managed instance, so `is_live` here observes the push
    // loop's `Exit` update.
    let client = ctx.client.clone();
    let probe_config = config.clone();

    // Kill the live Codex PTY (releasing its thread-store writer), wait for it to
    // actually exit, THEN summarize. All blocking → off the runtime, and outside
    // the dispatch gate: the registry's transferring marker keeps dispatch out.
    //
    // Releasing the session is fatal on failure (we must not proceed while the writer
    // is held). But the SUMMARY is best-effort: once the session is killed the
    // worktree is committed to the switch, so if `codex exec` can't produce a summary
    // (e.g. Codex usage limit or auth), fall back to a short note and still complete
    // the move to Claude rather than stranding a worktree whose session we just killed.
    let summary =
        tauri::async_runtime::spawn_blocking(move || -> Result<HandoffSummary, String> {
            release_codex_session(client.as_ref(), &key)?;
            // The same Codex environment the import resolves (executable, PATH,
            // CODEX_HOME), so the summary runs the Codex the pane runs.
            let probe = crate::codex_env::probe_codex_env_from_machine(&probe_config, &user);
            let summary = crate::codex_env::resolve_codex_env(&probe)
                .and_then(|env| summarize_with_writer_retry(&env, &cwd_s, thread_id.as_deref()));
            Ok(summary.unwrap_or_else(|e| {
                log::warn!("codex→claude handoff summary unavailable: {e}");
                HandoffSummary::unavailable(&e)
            }))
        })
        .await
        .map_err(|e| format!("summary task panicked: {e}"))??;

    // Write the summary file the frontend injects, then flip the persisted CLI.
    // Both ids are kept: the Claude session captured on the outbound leg resumes.
    let registry = ctx.registry.clone();
    let (project, worktree) = (ctx.project.clone(), ctx.worktree.clone());
    let committed = persist_under_gate(ctx.gate, move || {
        commit_then_persist(&registry, &project, &worktree, CommitCancel::Ignore, || {
            write_handoff_summary(&project, &worktree, &summary)?;
            persist_worktree_cli(
                &config,
                &worktree,
                CliKind::Claude,
                FieldIntent::Keep,
                FieldIntent::Keep,
            )
        })
    })
    .await?;
    match committed {
        Committed::Done(r) => r.map(|()| TransferOutcome::Done),
        // Unreachable with `Ignore`; nothing was persisted.
        Committed::Cancelled => Ok(TransferOutcome::Cancelled),
    }
}

/// How long to wait for a killed Codex PTY session to actually exit (dropping its
/// thread-store writer lock) before giving up.
const WRITER_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll cadence while waiting for the killed session to drop out of the live-set.
const WRITER_RELEASE_POLL: Duration = Duration::from_millis(100);

/// Kill the worktree's live Codex PTY session — the interactive `codex resume` that
/// holds the thread as its exclusive "active writer" — and block until it is gone.
///
/// The daemon's `Kill` is generation-gated (a stale gen is silently ignored), so the
/// current generation for the key is looked up via [`PtyBackend::list_sessions`]
/// first. No live session for the key ⇒ nothing holds the thread ⇒ proceed.
///
/// After the kill, poll [`PtyBackend::is_live`] until the daemon's `Exit` push
/// clears the key from the cached live-set — that push fires only once the child
/// process (and its `\r` grandchild tree on Windows) has fully exited, which is what
/// releases the thread-store lock. Bounded by [`WRITER_RELEASE_TIMEOUT`].
fn release_codex_session(client: &dyn PtyBackend, key: &SessionKey) -> Result<(), String> {
    release_codex_session_with(client, key, WRITER_RELEASE_TIMEOUT, WRITER_RELEASE_POLL)
}

/// [`release_codex_session`] with explicit timing, so the 5s-timeout path is
/// testable in milliseconds against a [`crate::backend::fake::FakeBackend`].
fn release_codex_session_with(
    client: &dyn PtyBackend,
    key: &SessionKey,
    timeout: Duration,
    poll: Duration,
) -> Result<(), String> {
    let gen = client
        .list_sessions()
        .map_err(|e| format!("list daemon sessions before Codex handoff: {e}"))?
        .into_iter()
        .find(|s| s.key == *key)
        .map(|s| s.gen);
    let Some(gen) = gen else {
        // Nothing live for this key — no interactive writer to release.
        return Ok(());
    };
    client
        .kill(key, gen)
        .map_err(|e| format!("kill live Codex session on `{}`: {e}", key.worktree))?;

    let deadline = Instant::now() + timeout;
    while client.is_live(key) {
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for the live Codex session on `{}` to exit before summarizing",
                key.worktree
            ));
        }
        std::thread::sleep(poll);
    }
    Ok(())
}

/// Total attempts for the summarize call (initial + retries). The daemon's `Exit`
/// push (which clears the live-set) can very slightly precede the OS actually
/// dropping the thread-store lock, so a first `codex exec resume` may still see the
/// stale writer. Retry ONLY that transient condition.
const SUMMARY_WRITER_ATTEMPTS: u32 = 3;
/// Backoff between summarize retries when the thread-store writer is still held.
const SUMMARY_WRITER_BACKOFF: Duration = Duration::from_millis(500);

/// Run [`run_codex_handoff_summary`], retrying up to [`SUMMARY_WRITER_ATTEMPTS`]
/// times when the error is a lingering "active writer" / `-32600` / thread-store
/// conflict (see [`is_active_writer_error`]); any other error fails immediately.
fn summarize_with_writer_retry(
    env: &CodexEnv,
    cwd: &str,
    resume: Option<&str>,
) -> Result<HandoffSummary, String> {
    let mut attempt = 1;
    loop {
        match run_codex_handoff_summary(env, cwd, resume) {
            Ok(summary) => return Ok(summary),
            Err(e) if attempt < SUMMARY_WRITER_ATTEMPTS && is_active_writer_error(&e) => {
                attempt += 1;
                std::thread::sleep(SUMMARY_WRITER_BACKOFF);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Pure: whether a summarize error string reports that the Codex thread still has a
/// live writer — the transient race where the interactive PTY's thread-store lock
/// has not dropped yet. Matches any of Codex's markers for it: `active writer`, the
/// JSON-RPC `-32600` code, or `thread-store conflict` (case-insensitive). Only these
/// are retryable; every other failure is real and must surface.
fn is_active_writer_error(err: &str) -> bool {
    let s = err.to_ascii_lowercase();
    s.contains("active writer") || s.contains("-32600") || s.contains("thread-store conflict")
}

/// Submit-delay for the handoff-summary injection (matches `dispatch.rs`'s
/// `SUBMIT_DELAY`: the daemon writes the text, pauses this long, then writes the
/// terminating `\r`).
const HANDOFF_SUBMIT_DELAY_MS: u32 = 60;

/// Best-effort Codex → Claude handoff-summary injection. The frontend calls this
/// once the freshly-restarted worktree's session reports `idle` (never blindly —
/// injecting into a still-starting or busy PTY would land mid-boot or interleave
/// with other output). Reads the summary [`write_handoff_summary`] wrote during
/// [`transfer_codex_to_claude`], flattens it to a single line (`write_line` submits
/// raw text then a trailing `\r` — embedded newlines would submit the prompt
/// early, mid-sentence), injects it via the same daemon `write_line` path
/// `dispatch_apply` uses, then deletes the file so a later re-poll/retry can't
/// double-inject.
///
/// Returns `Ok(false)` — a no-op, not an error — when there is no pending handoff
/// for this worktree (a Claude→Codex transfer never writes one; a Codex→Claude
/// transfer's summary may already have been injected and removed).
#[tauri::command]
pub async fn inject_handoff_summary(
    project: String,
    worktree: String,
    client: State<'_, SharedBackend>,
) -> Result<bool, String> {
    // The handoff file is keyed by the project name the transfer wrote it under,
    // so no config resolution is needed.
    let path = handoff_summary_path(&project, &worktree)?;
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read handoff {}: {e}", path.display()))?;
    let summary: HandoffSummary = serde_json::from_str(&text)
        .map_err(|e| format!("parse handoff {}: {e}", path.display()))?;
    let prompt = format_handoff_prompt(&summary);
    let key = SessionKey { project, worktree };
    client
        .write_line(&key, &prompt, HANDOFF_SUBMIT_DELAY_MS)
        .map_err(|e| e.to_string())?;
    // Best-effort cleanup: if this fails, the file just survives for a retry to
    // pick up (a manual re-inject is a mild annoyance, not silent data loss).
    let _ = std::fs::remove_file(&path);
    Ok(true)
}

/// Ask the in-flight Claude → Codex import for `project`/`worktree` to stop. A
/// no-op (still `Ok`) when nothing is in flight or the transfer already committed.
#[tauri::command]
pub fn cancel_transfer(
    project: String,
    worktree: String,
    registry: State<'_, TransferRegistry>,
) -> Result<(), String> {
    registry.request_cancel(&project, &worktree);
    Ok(())
}

/// The frontend finished the post-transfer restart: clear the transferring
/// marker. Idempotent, and a no-op unless the transfer committed (see
/// [`TransferRegistry::settle`]).
#[tauri::command]
pub fn transfer_settled(
    project: String,
    worktree: String,
    registry: State<'_, TransferRegistry>,
) -> Result<(), String> {
    registry.settle(&project, &worktree);
    Ok(())
}

/// The home-scrubbed diagnostics text of the worktree's last transfer trail, or
/// `None` when there is none.
#[tauri::command]
pub fn transfer_diagnostics(
    project: String,
    worktree: String,
    registry: State<'_, TransferRegistry>,
) -> Result<Option<String>, String> {
    let home = dirs::home_dir();
    Ok(crate::transfer_registry::diagnostics_text(
        &registry,
        &project,
        &worktree,
        home.as_deref().and_then(|p| p.to_str()),
    ))
}

/// Pure: flatten a [`HandoffSummary`] into ONE line suitable for `write_line`.
/// Any embedded `\r`/`\n` in the model-generated fields is scrubbed to a space —
/// `write_line` appends a single trailing `\r` to submit, so an embedded newline
/// would submit the prompt early, mid-sentence, into Claude's input box.
fn format_handoff_prompt(summary: &HandoffSummary) -> String {
    fn oneline(s: &str) -> String {
        // `split_whitespace` also collapses any run of whitespace (not just a lone
        // `\r`/`\n`) to a single space and trims the ends, so "a\r\nb" doesn't come
        // out as "a  b" (double space).
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
    let join = |items: &[String]| -> String {
        if items.is_empty() {
            "none".to_string()
        } else {
            items
                .iter()
                .map(|s| oneline(s))
                .collect::<Vec<_>>()
                .join("; ")
        }
    };
    let mut parts = vec![
        "Resuming from a Codex session handoff summary.".to_string(),
        format!("Goal: {}.", oneline(&summary.goal)),
        format!("Key decisions: {}.", join(&summary.key_decisions)),
        format!("Next steps: {}.", join(&summary.next_steps)),
        format!("Open files: {}.", join(&summary.open_files)),
    ];
    let notes = oneline(&summary.notes);
    if !notes.is_empty() {
        parts.push(format!("Notes: {notes}."));
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Claude → Codex: the blocking import core
// ---------------------------------------------------------------------------

/// What the blocking import produced. The transfer command turns it into a
/// [`PersistPlan`] and a [`TransferOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportResult {
    Imported { thread_id: String },
    Failed(TransferFailure),
    Cancelled,
}

/// Progress emitter for the blocking import (`transfer://progress` in production).
type ProgressFn<'a> = dyn Fn(ProgressPhase, Option<String>) + Send + Sync + 'a;

/// Trail emitter for the blocking import (log line plus registry trail).
type TrailFn<'a> = dyn Fn(TrailStage, Value) + Send + Sync + 'a;

/// Everything [`import_over`] needs besides the server.
struct ImportCtx<'a> {
    cwd: &'a str,
    jsonl: &'a Path,
    /// Resolved before spawn; its `codex_home` is the ledger home unless
    /// `initialize` reports another one, and its `version` goes into `CodexTooOld`.
    env: &'a CodexEnv,
    ledger_window: Duration,
    ledger_poll: Duration,
    progress: &'a ProgressFn<'a>,
    /// Stages `Import` and `Classify` only.
    trail: &'a TrailFn<'a>,
}

/// Handshake, one import request, the completion wait, judgement and the ledger
/// fallback. Pure protocol logic over any [`AppServer`], so the in-process fake
/// drives it. Does not spawn and does not preflight; never reads the live JSONL's
/// content, so the file growing during the import cannot change the result.
///
/// Every cancel and every timeout calls [`AppServer::abort`] before returning, so
/// the drop that follows kills immediately; only a normal finish or failure leaves
/// the server for the graceful teardown.
fn import_over<W: Write>(server: &mut AppServer<W>, ctx: &ImportCtx) -> ImportResult {
    let progress = ctx.progress;
    let trail = ctx.trail;

    progress(ProgressPhase::Initializing, None);
    let info = match server.handshake() {
        Ok(info) => info,
        Err(e) => return fail_with(server, ctx, e, TransferPhase::Initialize),
    };
    // The `initialize` home only redirects the ledger lookup; the child keeps the
    // environment it was spawned with.
    let ledger_env = ctx
        .env
        .clone()
        .with_initialize_home(info.codex_home.as_deref().and_then(Path::to_str));
    trail(
        TrailStage::Import,
        json!({
            "user_agent_present": info.user_agent.is_some(),
            "codex_home_from": codex_home_from(ledger_env.codex_home_source),
        }),
    );

    // The session path is the raw lookup path — Codex canonicalizes it on its own
    // end (as it also does when writing `source_path` to the ledger).
    let source_path = ctx.jsonl.to_string_lossy().into_owned();
    // What the ledger already says about this transcript, before the request: a
    // record from an earlier import must not pass for this import's answer.
    let ledger_paths = ledger_source_paths(ctx.jsonl);
    let ledger_before = ledger_snapshot(&ledger_env.codex_home, &ledger_paths);
    let payload = match serde_json::to_value(migration_payload(&source_path, ctx.cwd)) {
        Ok(v) => v,
        Err(e) => {
            return ImportResult::Failed(TransferFailure::ProtocolError {
                message: format!("encode the import request: {e}"),
                stderr_tail: String::new(),
            })
        }
    };

    progress(ProgressPhase::Importing, None);
    let response = match server.request("externalAgentConfig/import", payload) {
        Ok(v) => v,
        Err(e) => return fail_with(server, ctx, e, TransferPhase::Import),
    };
    let import_id = response
        .get("importId")
        .and_then(Value::as_str)
        .map(str::to_string);
    trail(
        TrailStage::Import,
        json!({
            "import_id_present": import_id.is_some(),
            "response_keys": sorted_keys(&response),
        }),
    );

    // The response alone does not mean the import finished: await the completion
    // for our import (any completion when the server gave no id).
    progress(ProgressPhase::AwaitingCompletion, None);
    let completed = server.wait_notification(
        EXTERNAL_AGENT_IMPORT_COMPLETED,
        |p| match &import_id {
            None => true,
            Some(id) => p
                .get("importId")
                .and_then(Value::as_str)
                .is_none_or(|x| x == id),
        },
        |m, p| {
            if m == EXTERNAL_AGENT_IMPORT_PROGRESS {
                progress(
                    ProgressPhase::AwaitingCompletion,
                    p.get("message").and_then(Value::as_str).map(str::to_string),
                );
            }
        },
    );
    let params = match completed {
        Ok(v) => v,
        Err(e) => return fail_with(server, ctx, e, TransferPhase::Completion),
    };
    let item = select_item(&params);
    let count = |key: &str| {
        item.and_then(|i| i.get(key))
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    trail(
        TrailStage::Import,
        json!({
            "completion_keys": sorted_keys(&params),
            "successes": count("successes"),
            "failures": count("failures"),
        }),
    );

    let mut verdict = judge_completion(&params);
    // A server that gives no import id predates structured results: only a
    // recognised failure is trusted, anything else goes to the ledger.
    if import_id.is_none()
        && matches!(
            verdict,
            CompletionVerdict::Failed {
                kind: FailureKind::Other,
                ..
            }
        )
    {
        verdict = CompletionVerdict::LedgerFallback { noop: false };
    }
    trail(
        TrailStage::Classify,
        json!({
            "verdict": verdict.label(),
            "noop": matches!(verdict, CompletionVerdict::LedgerFallback { noop: true }),
        }),
    );

    match verdict {
        CompletionVerdict::Thread(thread_id) => ImportResult::Imported { thread_id },
        CompletionVerdict::Failed { kind, message } => {
            ImportResult::Failed(kind.into_failure(message, server.stderr_tail()))
        }
        CompletionVerdict::LedgerFallback { noop } => {
            progress(ProgressPhase::ReadingLedger, None);
            let cancel = server.cancel.clone();
            // Only a no-op verdict ("already imported") may answer with a record
            // that predates this import.
            match ledger_lookup(
                &ledger_env.codex_home,
                &ledger_paths,
                &ledger_before,
                noop,
                ctx.ledger_window,
                ctx.ledger_poll,
                &cancel,
            ) {
                LedgerOutcome::Found(thread_id) => ImportResult::Imported { thread_id },
                LedgerOutcome::Cancelled => {
                    progress(ProgressPhase::Cancelling, None);
                    server.abort();
                    ImportResult::Cancelled
                }
                LedgerOutcome::NotFound => {
                    let stderr_tail = server.stderr_tail();
                    ImportResult::Failed(if noop {
                        TransferFailure::Skipped {
                            reason: SkipReason::NoOp,
                            message: "Codex found nothing new to import and recorded no thread"
                                .to_string(),
                            stderr_tail,
                        }
                    } else {
                        TransferFailure::Other {
                            message: "Codex completed the import without recording a thread"
                                .to_string(),
                            stderr_tail,
                        }
                    })
                }
            }
        }
    }
}

/// Map a transport error in `phase` to the result, picking the stderr tail the
/// failure carries and aborting the server on cancel and timeout.
fn fail_with<W: Write>(
    server: &mut AppServer<W>,
    ctx: &ImportCtx,
    err: ReqErr,
    phase: TransferPhase,
) -> ImportResult {
    let stderr_tail = match err {
        // The process is gone or going: give its last words a moment to land.
        ReqErr::Closed | ReqErr::Transport(_) => server.stderr_tail_settled(),
        _ => server.stderr_tail(),
    };
    match err {
        ReqErr::Cancelled => {
            (ctx.progress)(ProgressPhase::Cancelling, None);
            server.abort();
        }
        ReqErr::TimedOut => server.abort(),
        _ => {}
    }
    result_from_req_err(err, phase, ctx.env.version.as_deref(), stderr_tail)
}

/// Stderr wording that means the shell could not find `codex` at all.
const NOT_FOUND_MARKERS: [&str; 4] = [
    "is not recognized as an internal or external command",
    "command not found",
    "no such file or directory",
    "cannot find the file",
];

fn looks_like_codex_not_found(stderr_tail: &str) -> bool {
    let tail = stderr_tail.to_lowercase();
    NOT_FOUND_MARKERS.iter().any(|m| tail.contains(m))
}

/// Pure mapping of a transport error to the import result. `-32601` on the import
/// method means this Codex cannot import at all, so the ledger is never consulted
/// (a record there could only be a stale earlier snapshot).
fn result_from_req_err(
    err: ReqErr,
    phase: TransferPhase,
    version: Option<&str>,
    stderr_tail: String,
) -> ImportResult {
    let not_started = |stderr_tail: String| TransferFailure::CodexNotFound {
        message: "codex could not be started".to_string(),
        stderr_tail,
    };
    let failure = match err {
        ReqErr::Cancelled => return ImportResult::Cancelled,
        ReqErr::TimedOut => TransferFailure::Timeout {
            phase,
            message: format!(
                "timed out in {} after {}s",
                phase.as_str(),
                IMPORT_DEADLINE.as_secs()
            ),
            stderr_tail,
        },
        ReqErr::Rpc { code: -32601, .. } if phase == TransferPhase::Import => {
            TransferFailure::CodexTooOld {
                version: version.map(str::to_string),
                message: UPGRADE_MSG.to_string(),
                stderr_tail,
            }
        }
        err @ ReqErr::Rpc { .. } => TransferFailure::ProtocolError {
            message: err.to_string(),
            stderr_tail,
        },
        // A write into a child that already died (Windows: `cmd.exe` exits as soon
        // as it fails to find `codex`) is the same story as an early stdout EOF.
        ReqErr::Transport(_) if looks_like_codex_not_found(&stderr_tail) => {
            not_started(stderr_tail)
        }
        ReqErr::Transport(message) => TransferFailure::ProtocolError {
            message,
            stderr_tail,
        },
        ReqErr::Closed if looks_like_codex_not_found(&stderr_tail) => not_started(stderr_tail),
        ReqErr::Closed => TransferFailure::ProtocolError {
            message: format!("codex app-server exited during {}", phase.as_str()),
            stderr_tail,
        },
    };
    ImportResult::Failed(failure)
}

/// The `codex_home_from` trail value.
fn codex_home_from(source: CodexHomeSource) -> &'static str {
    match source {
        CodexHomeSource::Initialize => "initialize",
        CodexHomeSource::EnvVar => "env_var",
        CodexHomeSource::Default => "default",
    }
}

/// Sorted key names of a JSON object (empty for anything else). The trail records
/// only key names, never values that could contain transcript text.
fn sorted_keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    keys
}

// ---------------------------------------------------------------------------
// Completion judgement
// ---------------------------------------------------------------------------

/// What a completion notification says about the import.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionVerdict {
    Thread(String),
    /// A classified failure; never consults the ledger.
    Failed {
        kind: FailureKind,
        message: String,
    },
    /// No answer in the notification: look the thread up by path in the ledger.
    LedgerFallback {
        noop: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    SessionNotDetected,
    ContinuedIn,
    Deferred,
    Other,
}

impl FailureKind {
    fn as_str(self) -> &'static str {
        match self {
            FailureKind::SessionNotDetected => "session_not_detected",
            FailureKind::ContinuedIn => "continued_in",
            FailureKind::Deferred => "deferred",
            FailureKind::Other => "other",
        }
    }

    fn into_failure(self, message: String, stderr_tail: String) -> TransferFailure {
        match self {
            FailureKind::SessionNotDetected => TransferFailure::SessionNotDetected {
                message,
                stderr_tail,
            },
            FailureKind::ContinuedIn => TransferFailure::Skipped {
                reason: SkipReason::ContinuedIn,
                message,
                stderr_tail,
            },
            FailureKind::Deferred => TransferFailure::Skipped {
                reason: SkipReason::Deferred,
                message,
                stderr_tail,
            },
            FailureKind::Other => TransferFailure::Other {
                message,
                stderr_tail,
            },
        }
    }
}

impl CompletionVerdict {
    /// The `verdict` trail value.
    fn label(&self) -> String {
        match self {
            CompletionVerdict::Thread(_) => "thread".to_string(),
            CompletionVerdict::Failed { kind, .. } => format!("failed:{}", kind.as_str()),
            CompletionVerdict::LedgerFallback { .. } => "ledger_fallback".to_string(),
        }
    }
}

// Marker words, each matched as a substring of a normalized string. The deferred
// and no-op lists are guesses at upstream wording; a miss degrades to `Other` or
// to the ledger fallback, never to a wrong thread.
const SESSION_NOT_DETECTED: &[&str] = &["session_not_detected"];
const CONTINUED_IN: &[&str] = &["continued_in"];
const DEFERRED: &[&str] = &["deferred", "diverged", "thread_open", "thread_is_open"];
const NOOP: &[&str] = &["no_op", "noop", "unchanged", "already_imported"];

/// camelCase → snake (an `_` between a lowercase letter and a following uppercase
/// one), lowercased, every non-alphanumeric character → `_`. So `continued-in`,
/// `continuedIn` and `CONTINUED_IN` all become `continued_in`.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut prev_lower = false;
    for c in s.chars() {
        if prev_lower && c.is_uppercase() {
            out.push('_');
        }
        prev_lower = c.is_lowercase();
        for l in c.to_lowercase() {
            out.push(if l.is_alphanumeric() { l } else { '_' });
        }
    }
    out
}

fn contains_any(hay: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| hay.contains(m))
}

/// The `itemTypeResults` entry for sessions; else the only entry; else none.
fn select_item(params: &Value) -> Option<&Value> {
    let items = params.get("itemTypeResults")?.as_array()?;
    items
        .iter()
        .find(|i| i.get("itemType").and_then(Value::as_str) == Some("SESSIONS"))
        .or_else(|| {
            if items.len() == 1 {
                items.first()
            } else {
                None
            }
        })
}

/// Judge a completion notification's params. Pure; every field is optional.
fn judge_completion(params: &Value) -> CompletionVerdict {
    let Some(item) = select_item(params) else {
        return CompletionVerdict::LedgerFallback { noop: false };
    };
    let target = item
        .get("successes")
        .and_then(Value::as_array)
        .and_then(|s| {
            s.iter().rev().find_map(|e| {
                e.get("target")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
            })
        });
    if let Some(target) = target {
        return CompletionVerdict::Thread(target.to_string());
    }
    let failures = item
        .get("failures")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !failures.is_empty() {
        return judge_failures(failures);
    }
    let blob = normalize(&item.to_string());
    let generic = |kind| CompletionVerdict::Failed {
        kind,
        message: "import failed".to_string(),
    };
    if contains_any(&blob, CONTINUED_IN) {
        generic(FailureKind::ContinuedIn)
    } else if contains_any(&blob, DEFERRED) {
        generic(FailureKind::Deferred)
    } else {
        CompletionVerdict::LedgerFallback {
            noop: contains_any(&blob, NOOP),
        }
    }
}

/// Classify a non-empty `failures[]`: the structured fields first, then the
/// messages, each by kind priority over all failures.
fn judge_failures(failures: &[Value]) -> CompletionVerdict {
    const KINDS: [(&[&str], FailureKind); 3] = [
        (SESSION_NOT_DETECTED, FailureKind::SessionNotDetected),
        (CONTINUED_IN, FailureKind::ContinuedIn),
        (DEFERRED, FailureKind::Deferred),
    ];
    fn field<'v>(f: &'v Value, key: &str) -> &'v str {
        f.get(key).and_then(Value::as_str).unwrap_or("")
    }
    let structured: Vec<String> = failures
        .iter()
        .map(|f| {
            normalize(&format!(
                "{} {} {}",
                field(f, "failureStage"),
                field(f, "errorType"),
                field(f, "subErrorType")
            ))
        })
        .collect();
    let messages: Vec<String> = failures
        .iter()
        .map(|f| normalize(field(f, "message")))
        .collect();
    let pick = |texts: &[String]| {
        KINDS.iter().find_map(|(markers, kind)| {
            texts
                .iter()
                .position(|t| contains_any(t, markers))
                .map(|i| (*kind, i))
        })
    };
    let (kind, idx) = pick(&structured)
        .or_else(|| pick(&messages))
        .unwrap_or((FailureKind::Other, 0));
    let f = &failures[idx];
    let (stage, error_type) = (field(f, "failureStage"), field(f, "errorType"));
    let message = if !field(f, "message").is_empty() {
        field(f, "message").to_string()
    } else if !stage.is_empty() || !error_type.is_empty() {
        format!("{stage}/{error_type}")
    } else {
        "import failed".to_string()
    };
    CompletionVerdict::Failed { kind, message }
}

// ---------------------------------------------------------------------------
// Spawn, teardown hand-off and persistence plan
// ---------------------------------------------------------------------------

/// Preflight, spawn, then [`import_over`]. Tears the server down inline when it
/// was aborted (cancel, timeout), otherwise on a detached thread after the result
/// is ready, so a graceful teardown never delays the outcome. Blocking; runs
/// inside `spawn_blocking`.
fn run_claude_import(
    env: &CodexEnv,
    cwd: &str,
    jsonl: &Path,
    cancel: Arc<AtomicBool>,
    progress: &ProgressFn<'_>,
    trail: &TrailFn<'_>,
) -> ImportResult {
    let deadline = Instant::now() + IMPORT_DEADLINE;
    run_claude_import_with(
        env,
        cwd,
        jsonl,
        cancel,
        progress,
        trail,
        ImportKnobs {
            home: dirs::home_dir(),
            deadline,
            ledger_window: LEDGER_WINDOW,
            ledger_poll: LEDGER_POLL,
        },
    )
}

/// The machine- and time-dependent inputs of [`run_claude_import`], separate so
/// tests pass a temp home and short windows.
struct ImportKnobs {
    home: Option<PathBuf>,
    deadline: Instant,
    ledger_window: Duration,
    ledger_poll: Duration,
}

fn run_claude_import_with(
    env: &CodexEnv,
    cwd: &str,
    jsonl: &Path,
    cancel: Arc<AtomicBool>,
    progress: &ProgressFn<'_>,
    trail: &TrailFn<'_>,
    knobs: ImportKnobs,
) -> ImportResult {
    progress(ProgressPhase::Preflight, None);
    if !jsonl_under_home_projects(jsonl, knobs.home.as_deref()) {
        return ImportResult::Failed(TransferFailure::SessionNotDetected {
            message:
                "the Claude transcript is outside ~/.claude/projects (CLAUDE_CONFIG_DIR is set?)"
                    .to_string(),
            stderr_tail: String::new(),
        });
    }
    progress(ProgressPhase::StartingCodex, None);
    let mut server = match AppServer::spawn(env, cwd, cancel, knobs.deadline) {
        Ok(server) => server,
        Err(e) => return e.into_result(),
    };
    let ctx = ImportCtx {
        cwd,
        jsonl,
        env,
        ledger_window: knobs.ledger_window,
        ledger_poll: knobs.ledger_poll,
        progress,
        trail,
    };
    let result = import_over(&mut server, &ctx);
    release_server(server);
    result
}

/// Drop an aborted server inline (immediate kill); hand any other to a detached
/// thread that closes stdin, waits out the grace period and then kills. If that
/// thread cannot be started the closure, and the server with it, is dropped
/// inline: correct, just slower.
fn release_server<W: Write + Send + 'static>(server: AppServer<W>) {
    if server.aborted {
        drop(server);
        return;
    }
    let _ = std::thread::Builder::new()
        .name("codex-teardown".into())
        .spawn(move || drop(server));
}

/// Codex only recognises transcripts under `$HOME/.claude/projects` (it ignores
/// `CLAUDE_CONFIG_DIR`). False when either side cannot be canonicalized or there is
/// no home.
fn jsonl_under_home_projects(jsonl: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    let (Ok(file), Ok(projects)) = (
        std::fs::canonicalize(jsonl),
        std::fs::canonicalize(home.join(".claude").join("projects")),
    ) else {
        return false;
    };
    file.starts_with(projects)
}

/// What to persist for an import result: the worktree becomes Codex; the thread id
/// is set on success and cleared on failure; a cancel persists nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PersistPlan {
    cli: CliKind,
    codex_thread_id: FieldIntent<String>,
    claude_session_id: FieldIntent<String>,
}

/// Whether a failure leaves the worktree on Claude instead of switching it to a
/// fresh Codex: when Codex cannot be found, a fresh Codex could not start either.
fn keeps_claude(failure: &TransferFailure) -> bool {
    matches!(failure, TransferFailure::CodexNotFound { .. })
}

/// Pure. `claude_session_id` is the selected transcript's id, or `None` (then the
/// persisted one is kept). `Ok(None)` = cancelled; `Err` = nothing is persisted and
/// the worktree stays on Claude (the error is the user-facing reason).
fn persist_plan(
    result: &ImportResult,
    claude_session_id: Option<String>,
) -> Result<Option<PersistPlan>, String> {
    let codex_thread_id = match result {
        ImportResult::Imported { thread_id } => FieldIntent::Set(thread_id.clone()),
        ImportResult::Failed(failure) if keeps_claude(failure) => return Err(failure.user_copy()),
        ImportResult::Failed(_) => FieldIntent::Clear,
        ImportResult::Cancelled => return Ok(None),
    };
    Ok(Some(PersistPlan {
        cli: CliKind::Codex,
        codex_thread_id,
        claude_session_id: claude_session_id.map_or(FieldIntent::Keep, FieldIntent::Set),
    }))
}

/// The `externalAgentConfig/import` migration literal (`codex.mjs:681-699`),
/// reproduced verbatim. The field ORDER matters (serde serializes struct fields in
/// declaration order): outer `cwd: null`, inner `details.sessions[0].cwd` = the
/// worktree cwd, and the five empty sibling arrays in the exact order
/// `plugins, sessions, mcpServers, hooks, subagents, commands`.
fn migration_payload(source_path: &str, cwd: &str) -> Migration {
    let basename = Path::new(source_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Migration {
        migration_items: vec![MigrationItem {
            item_type: "SESSIONS",
            description: format!("Transfer Claude session {basename}"),
            cwd: Value::Null,
            details: MigrationDetails {
                plugins: vec![],
                sessions: vec![SessionEntry {
                    path: source_path.to_string(),
                    cwd: cwd.to_string(),
                    title: Value::Null,
                }],
                mcp_servers: vec![],
                hooks: vec![],
                subagents: vec![],
                commands: vec![],
            },
        }],
    }
}

#[derive(Serialize)]
struct Migration {
    #[serde(rename = "migrationItems")]
    migration_items: Vec<MigrationItem>,
}

#[derive(Serialize)]
struct MigrationItem {
    #[serde(rename = "itemType")]
    item_type: &'static str,
    description: String,
    /// Always `null` — only the inner session entry carries the real cwd.
    cwd: Value,
    details: MigrationDetails,
}

#[derive(Serialize)]
struct MigrationDetails {
    plugins: Vec<Value>,
    sessions: Vec<SessionEntry>,
    #[serde(rename = "mcpServers")]
    mcp_servers: Vec<Value>,
    hooks: Vec<Value>,
    subagents: Vec<Value>,
    commands: Vec<Value>,
}

#[derive(Serialize)]
struct SessionEntry {
    path: String,
    cwd: String,
    /// Always `null`.
    title: Value,
}

// ---------------------------------------------------------------------------
// Ledger fallback (path-only)
// ---------------------------------------------------------------------------

/// The `source_path` forms a ledger record for `jsonl` may carry: the raw string
/// we sent, the `fs::canonicalize` form (Windows keeps the `\\?\` prefix, which is
/// what Codex writes), and that form with the prefix stripped. Deduped, in that
/// order; a canonicalize error just omits the last two.
fn ledger_source_paths(jsonl: &Path) -> Vec<String> {
    let mut out = vec![jsonl.to_string_lossy().into_owned()];
    if let Ok(canonical) = std::fs::canonicalize(jsonl) {
        let canonical = canonical.to_string_lossy().into_owned();
        let stripped = keitora_tasks::strip_extended_prefix_str(&canonical);
        out.push(canonical);
        out.push(stripped);
    }
    let mut deduped: Vec<String> = Vec::with_capacity(out.len());
    for p in out {
        if !deduped.contains(&p) {
            deduped.push(p);
        }
    }
    deduped
}

/// One ledger record for our transcript, reduced to what tells two records apart:
/// a record written by this import differs from every record that predates it in
/// at least one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LedgerMark {
    thread_id: String,
    sha: Option<String>,
    /// The raw JSON of `imported_at` (its type is not pinned).
    imported_at: Option<String>,
}

/// Pure: parse the ledger (non-strict — unknown keys and per-record fields are
/// tolerated) and return, in ledger order, every record whose `source_path` is any
/// of `source_paths` and that carries a thread id. Unparseable text has no records.
fn ledger_marks_for_paths(text: &str, source_paths: &[String]) -> Vec<LedgerMark> {
    let Ok(ledger) = serde_json::from_str::<Ledger>(text) else {
        return Vec::new();
    };
    ledger
        .records
        .into_iter()
        .filter(|r| {
            r.source_path
                .as_ref()
                .is_some_and(|p| source_paths.contains(p))
        })
        .filter_map(|r| {
            Some(LedgerMark {
                thread_id: r.imported_thread_id?,
                sha: r.content_sha256,
                imported_at: r.imported_at.map(|v| v.to_string()),
            })
        })
        .collect()
}

/// Pure: the thread id of the LAST record for any of `source_paths`. Content
/// hashes are not consulted: the live transcript keeps growing while Claude runs.
#[cfg(test)]
fn ledger_thread_for_paths(text: &str, source_paths: &[String]) -> Option<String> {
    ledger_marks_for_paths(text, source_paths)
        .pop()
        .map(|m| m.thread_id)
}

/// Pure: which thread the current ledger records answer with. A record that is not
/// in `snapshot` (taken before the import request) was written by this import and
/// wins, the last such first. A record that predates the import is accepted only
/// when `accept_existing` (Codex said it already has this source).
fn ledger_pick(
    current: &[LedgerMark],
    snapshot: &[LedgerMark],
    accept_existing: bool,
) -> Option<String> {
    if let Some(fresh) = current.iter().rev().find(|m| !snapshot.contains(m)) {
        return Some(fresh.thread_id.clone());
    }
    if accept_existing {
        return current.last().map(|m| m.thread_id.clone());
    }
    None
}

/// The ledger records for `source_paths` right now (none when the file is missing
/// or unreadable). Taken before the import request so the fallback can tell a
/// record this import wrote from one an earlier import left behind.
fn ledger_snapshot(codex_home: &Path, source_paths: &[String]) -> Vec<LedgerMark> {
    std::fs::read_to_string(codex_home.join(LEDGER_FILE))
        .map(|text| ledger_marks_for_paths(&text, source_paths))
        .unwrap_or_default()
}

#[derive(Debug, PartialEq, Eq)]
enum LedgerOutcome {
    Found(String),
    NotFound,
    Cancelled,
}

/// Read `<codex_home>/external_agent_session_imports.json` at least once, then
/// every `poll` until `window` has elapsed, for a late flush. A missing file is
/// just "no records yet". `cancel` is checked before every read. Which record
/// counts is [`ledger_pick`]'s rule over `snapshot` and `accept_existing`.
fn ledger_lookup(
    codex_home: &Path,
    source_paths: &[String],
    snapshot: &[LedgerMark],
    accept_existing: bool,
    window: Duration,
    poll: Duration,
    cancel: &AtomicBool,
) -> LedgerOutcome {
    let path = codex_home.join(LEDGER_FILE);
    let until = Instant::now() + window;
    // The (len, mtime) of the last ledger read: an unchanged file is not re-read.
    let mut last_read: Option<(u64, Option<SystemTime>)> = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return LedgerOutcome::Cancelled;
        }
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()));
        if stamp.is_some() && stamp != last_read {
            last_read = stamp;
            if let Some(id) = std::fs::read_to_string(&path).ok().and_then(|text| {
                ledger_pick(
                    &ledger_marks_for_paths(&text, source_paths),
                    snapshot,
                    accept_existing,
                )
            }) {
                return LedgerOutcome::Found(id);
            }
        }
        let now = Instant::now();
        if now >= until {
            return LedgerOutcome::NotFound;
        }
        std::thread::sleep(poll.min(until - now));
    }
}

/// The ledger's top-level shape. `detected_connector_records` (and any other key)
/// is tolerated and ignored — serde skips unknown fields by default.
#[derive(Deserialize)]
struct Ledger {
    #[serde(default)]
    records: Vec<LedgerRecord>,
}

/// One ledger record. Only the fields we match or compare on are declared; the
/// rest (`connector_names`, `title`, …) are tolerated as unknown fields.
#[derive(Deserialize)]
struct LedgerRecord {
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    imported_thread_id: Option<String>,
    #[serde(default)]
    content_sha256: Option<String>,
    #[serde(default)]
    imported_at: Option<Value>,
}

// ---------------------------------------------------------------------------
// Live-session lookup + confirm preview
// ---------------------------------------------------------------------------

/// Claude's projects dir: `$CLAUDE_CONFIG_DIR/projects` when that is set and
/// non-empty (Claude relocates its whole config dir there), else
/// `~/.claude/projects`.
pub(crate) fn claude_projects_dir() -> Result<PathBuf, String> {
    claude_projects_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), dirs::home_dir())
}

/// Pure core of [`claude_projects_dir`] over the looked-up env value and home dir.
fn claude_projects_dir_from(
    config_dir: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(dir) = config_dir.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir).join("projects"));
    }
    home.map(|h| h.join(".claude").join("projects"))
        .ok_or_else(|| "could not resolve home directory for ~/.claude/projects".to_string())
}

/// Whether Claude has a transcript `<projects>/<encoded>/<id>.jsonl` for session
/// `id`. Pure over the filesystem; the caller validates `id` first.
pub(crate) fn claude_session_file_exists(projects: &Path, encoded: &str, id: &str) -> bool {
    projects.join(encoded).join(format!("{id}.jsonl")).is_file()
}

/// Whether Claude has any transcript (`*.jsonl`) under `<projects>/<encoded>/`, i.e.
/// whether `claude --continue` has something to continue.
pub(crate) fn claude_project_has_sessions(projects: &Path, encoded: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(projects.join(encoded)) else {
        return false;
    };
    entries.flatten().any(|e| {
        let path = e.path();
        path.extension().and_then(|x| x.to_str()) == Some("jsonl") && path.is_file()
    })
}

/// Encode a (already-normalized, prefix-stripped) cwd string to Claude's
/// `<encoded-cwd>` project-dir name: EACH non-alphanumeric character → a single
/// `-`. Verified against the real dir on this machine:
/// `D:\repos\proj-worktrees\beta` → `D--repos-proj-worktrees-beta` (the `:` and
/// the `\` each map to their own `-`, giving the leading `--`). Pure + testable.
fn encode_project_dir(normalized: &str) -> String {
    normalized
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The normalization pipeline for the `<encoded-cwd>` directory-name lookup:
/// canonicalize → strip `\\?\` (via the crate's canonical helper) → encode. Split
/// out so the encoding is testable without a real filesystem. Errors when `worktree_cwd`
/// does not exist (it cannot be canonicalized).
pub(crate) fn encoded_cwd_dir_name(worktree_cwd: &Path) -> Result<String, String> {
    let canonical = std::fs::canonicalize(worktree_cwd)
        .map_err(|e| format!("canonicalize {}: {e}", worktree_cwd.display()))?;
    // Strip the `\\?\` prefix ONLY here (the cwd-encoding path) — never for the
    // ledger realpath above.
    let stripped = keitora_tasks::strip_extended_prefix_str(&canonical.to_string_lossy());
    Ok(encode_project_dir(&stripped))
}

/// The worktree's Claude project dir, `<claude projects>/<encoded-cwd>/`: where its
/// transcripts live and the only dir a transfer imports from.
fn claude_project_dir(worktree_cwd: &Path) -> Result<PathBuf, String> {
    Ok(claude_projects_dir()?.join(encoded_cwd_dir_name(worktree_cwd)?))
}

/// The confirmed transcript for this worktree, validated: an existing `.jsonl`
/// regular file directly inside its Claude project dir.
fn confirmed_claude_jsonl(worktree_cwd: &Path, jsonl: &Path) -> Result<PathBuf, String> {
    validate_jsonl_in_dir(&claude_project_dir(worktree_cwd)?, jsonl)
}

/// Pure over the filesystem: `jsonl` is accepted only when it is an existing
/// `.jsonl` regular file whose canonical parent is `dir`'s canonical form. Returns
/// the path as given (the form the preview showed and the import sends).
fn validate_jsonl_in_dir(dir: &Path, jsonl: &Path) -> Result<PathBuf, String> {
    let reject = |why: &str| {
        Err(format!(
            "the confirmed transcript {} {why}",
            jsonl.display()
        ))
    };
    if jsonl.extension().and_then(|x| x.to_str()) != Some("jsonl") {
        return reject("is not a .jsonl file");
    }
    let Ok(meta) = std::fs::metadata(jsonl) else {
        return reject("does not exist");
    };
    if !meta.is_file() {
        return reject("is not a file");
    }
    let (Ok(file), Ok(dir)) = (std::fs::canonicalize(jsonl), std::fs::canonicalize(dir)) else {
        return reject("is not in this worktree's Claude sessions");
    };
    if file.parent() != Some(dir.as_path()) {
        return reject("is not in this worktree's Claude sessions");
    }
    Ok(jsonl.to_path_buf())
}

/// Find the worktree's live Claude JSONL: the newest non-`agent-*` `.jsonl` under
/// `<claude projects>/<encoded-cwd>/` (see [`pick_jsonl`]), with a containment
/// guard (the resolved path must be a real `.jsonl` under the projects dir).
fn find_live_claude_jsonl(worktree_cwd: &Path) -> Result<PathBuf, String> {
    let projects = claude_projects_dir()?;
    let dir_name = encoded_cwd_dir_name(worktree_cwd)?;
    let dir = projects.join(&dir_name);

    if let Err(e) = std::fs::read_dir(&dir) {
        return Err(format!(
            "no Claude sessions found for this worktree at {}: {e}",
            dir.display()
        ));
    }
    let path = pick_jsonl(&dir)
        .ok_or_else(|| format!("no .jsonl session files under {}", dir.display()))?;

    // Containment guard: the canonicalized pick must live under the canonicalized
    // projects dir and still be a `.jsonl`.
    let canon_path = std::fs::canonicalize(&path)
        .map_err(|e| format!("canonicalize session {}: {e}", path.display()))?;
    let canon_projects = std::fs::canonicalize(&projects)
        .map_err(|e| format!("canonicalize {}: {e}", projects.display()))?;
    if !canon_path.starts_with(&canon_projects)
        || canon_path.extension().and_then(|x| x.to_str()) != Some("jsonl")
    {
        return Err(format!(
            "selected session path escapes ~/.claude/projects: {}",
            canon_path.display()
        ));
    }
    Ok(path)
}

/// The newest `*.jsonl` regular file in `dir` by mtime, skipping sub-agent
/// transcripts (`agent-*.jsonl`). The persisted Claude session id is deliberately
/// not preferred: only a transfer writes it, so a later `--resume`, `/clear` or new
/// session makes it stale. Pure over the filesystem.
fn pick_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let is_agent = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("agent-"));
        if is_agent {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            newest = Some((mtime, path));
        }
    }
    newest.map(|(_, p)| p)
}

/// The `import` trail entry for the transcript choice: whether a persisted Claude
/// session id exists and whether it names a different file than the one picked,
/// so a wrong pick is diagnosable without the id ever changing the choice. Pure.
fn selection_trail(picked: &Path, persisted: Option<&str>) -> Value {
    let picked_stem = picked.file_stem().and_then(|s| s.to_str());
    json!({
        "selection": "newest",
        "persisted_session_id_present": persisted.is_some(),
        "persisted_differs": persisted.is_some_and(|p| Some(p) != picked_stem),
    })
}

/// Extract the session id from Claude's `<session-uuid>.jsonl` filename.
/// Empty ids must not be persisted because bare `--resume` opens an interactive picker.
fn session_id_from_jsonl(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.trim();
    keitora_tasks::is_safe_claude_session_id(stem).then(|| stem.to_string())
}

/// Build the [`ConfirmPreview`] for a resolved JSONL path.
fn build_preview(path: &Path) -> Result<ConfirmPreview, String> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let mtime = meta
        .modified()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339())
        .unwrap_or_default();
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(ConfirmPreview {
        path: path.to_string_lossy().into_owned(),
        filename,
        mtime,
        first_user_line: first_user_line(&contents),
    })
}

/// Extract the first *real* user line from Claude's JSONL: the first record with
/// `"type":"user"` and NOT `"isMeta":true` (synthetic caveats / slash-command
/// echoes are wrapped in `isMeta` records). `message.content` may be a bare string
/// or an array of content blocks (use the first block's `text`). Truncated to ~200
/// chars for display. Pure + testable.
fn first_user_line(jsonl: &str) -> String {
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(blocks)) => blocks
                .first()
                .and_then(|b| b.get("text").and_then(Value::as_str))
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        };
        if !text.trim().is_empty() {
            return truncate_chars(text.trim(), 200);
        }
    }
    String::new()
}

/// Truncate to at most `max` characters (char-boundary safe), appending `…`.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Codex → Claude: the blocking summary core
// ---------------------------------------------------------------------------

/// Process-global sequence for handoff scratch-file names. The pid alone collides
/// across two concurrent Codex→Claude transfers (same process ⇒ same pid), so two
/// worktrees would share `handoff-out-{pid}.json` and race — one worktree reading
/// the other's summary, or a mid-read delete. Pairing the pid with a monotonic
/// counter makes each transfer's schema/out pair unique.
static HANDOFF_SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Build the `codex exec resume …` argv for the handoff summary. `resume` selects
/// the resume target — `Some(id)` → `exec resume <id> …` (a persisted thread);
/// `None` → `exec resume --last …` (a native Codex worktree with no imported
/// thread; `--last` resolves to this cwd's most-recent rollout once the live
/// session is released). Both are a single token in the `SESSION_ID` slot of
/// `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]`. Pure + testable.
fn summary_args<'a>(
    resume: Option<&'a str>,
    prompt: &'a str,
    schema: &'a str,
    out: &'a str,
) -> [&'a str; 8] {
    [
        "exec",
        "resume",
        resume.unwrap_or("--last"),
        prompt,
        "--output-schema",
        schema,
        "-o",
        out,
    ]
}

/// The summary `codex exec resume …` command: the resolved executable with the
/// resolved `PATH` / `CODEX_HOME` (see [`apply_codex_env`]), run in `cwd`. Stdio is
/// left to the caller. Pure + testable.
fn summary_command(
    env: &CodexEnv,
    cwd: &str,
    resume: Option<&str>,
    schema: &str,
    out: &str,
) -> Command {
    let args = summary_args(resume, HANDOFF_PROMPT, schema, out);
    let mut cmd = codex_command(&env.exe, &args);
    apply_codex_env(&mut cmd, env);
    cmd.current_dir(cwd);
    cmd
}

/// Run `codex exec resume {<thread_id> | --last} "<prompt>" --output-schema <file>
/// -o <out>` and parse the structured `<out>` into a [`HandoffSummary`]. Blocking.
fn run_codex_handoff_summary(
    env: &CodexEnv,
    cwd: &str,
    resume: Option<&str>,
) -> Result<HandoffSummary, String> {
    let dir = transfer_scratch_dir()?;
    // Unique PER TRANSFER (pid + monotonic counter) so concurrent transfers never
    // collide on these scratch paths.
    let token = format!(
        "{}-{}",
        std::process::id(),
        HANDOFF_SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let schema_file = dir.join(format!("handoff-schema-{token}.json"));
    let out_file = dir.join(format!("handoff-out-{token}.json"));

    // Run inside a closure so the scratch files are cleaned up on EVERY exit path
    // (spawn failure, timeout, parse error), not just the success path.
    let run = || -> Result<HandoffSummary, String> {
        std::fs::write(&schema_file, HANDOFF_SCHEMA)
            .map_err(|e| format!("write schema {}: {e}", schema_file.display()))?;

        let schema_s = schema_file.to_string_lossy().into_owned();
        let out_s = out_file.to_string_lossy().into_owned();
        let mut cmd = summary_command(env, cwd, resume, &schema_s, &out_s);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_command(&mut cmd);

        let (ok, _stdout, stderr) = run_bounded(cmd, SUMMARY_TIMEOUT)?;
        if !ok {
            return Err(format!(
                "codex exec resume failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            ));
        }

        // `-o` writes the model's final JSON directly (no envelope/event unwrapping).
        let raw = std::fs::read_to_string(&out_file)
            .map_err(|e| format!("read summary output {}: {e}", out_file.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse handoff summary JSON: {e}"))
    };

    let result = run();
    let _ = std::fs::remove_file(&schema_file);
    let _ = std::fs::remove_file(&out_file);
    result
}

/// The scratch dir for transfer artifacts (schema/out/handoff files).
fn transfer_scratch_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("keitora-transfer");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Deterministic path where a Codex → Claude handoff summary is written for
/// [`inject_handoff_summary`] to read and inject. Keyed by project and worktree.
pub(crate) fn handoff_summary_path(project: &str, worktree: &str) -> Result<PathBuf, String> {
    let safe = |s: &str| -> String { encode_project_dir(s) };
    Ok(transfer_scratch_dir()?.join(format!(
        "{}__{}__handoff.json",
        safe(project),
        safe(worktree)
    )))
}

/// Write the handoff summary to [`handoff_summary_path`].
fn write_handoff_summary(
    project: &str,
    worktree: &str,
    summary: &HandoffSummary,
) -> Result<(), String> {
    let path = handoff_summary_path(project, worktree)?;
    let text = serde_json::to_string_pretty(summary).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("write handoff {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Persist provider state for one worktree of the already-resolved `project`.
/// Each id follows its intent (`Keep` leaves it, `Clear` removes it, `Set`
/// writes it); `cli` is always written.
fn persist_worktree_cli(
    project: &keitora_tasks::ProjectConfig,
    worktree: &str,
    cli: CliKind,
    codex_thread_id: FieldIntent<String>,
    claude_session_id: FieldIntent<String>,
) -> Result<(), String> {
    // A load-modify-save of the registry holds the writer lock throughout.
    let _w = keitora_tasks::UserConfig::write_lock();
    let mut user = keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?;
    let current = project.worktree_entries();
    let matched = user
        .set_worktree_provider_state_by_root(
            &project.root,
            worktree,
            cli,
            codex_thread_id,
            claude_session_id,
            &current,
        )
        .map_err(|e| e.to_string())?;
    if !matched {
        return Err(format!(
            "project {} is not registered; cannot persist transfer state",
            project.project.name
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Minimal JSON-RPC-2.0-over-stdio client for `codex app-server`
// ---------------------------------------------------------------------------

/// A parsed inbound line. Classified purely so the request/response/notification
/// demux is unit-testable.
#[derive(Debug, PartialEq)]
enum Frame {
    /// A response to one of our requests (integer `id`, no `method`).
    Response {
        id: i64,
        result: Option<Value>,
        error: Option<RpcErrData>,
    },
    /// A notification (has `method`, no `id` or a `null` one).
    Notification { method: String, params: Value },
    /// A server-initiated request (has both `id` and `method`) — we reply `-32601`.
    /// The id is kept verbatim: servers may use integers or strings.
    ServerRequest { id: Value, method: String },
    /// Unrecognized shape.
    Junk,
}

#[derive(Debug, Clone, PartialEq)]
struct RpcErrData {
    code: i64,
    message: String,
}

/// A request failure: an RPC error object (carries `code`, e.g. `-32601`), a
/// transport failure, the server going away, the shared deadline passing, or the
/// user cancelling.
#[derive(Debug)]
enum ReqErr {
    Rpc { code: i64, message: String },
    Transport(String),
    Closed,
    TimedOut,
    Cancelled,
}

impl std::fmt::Display for ReqErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReqErr::Rpc { code, message } => {
                write!(f, "codex app-server error {code}: {message}")
            }
            ReqErr::Transport(m) => write!(f, "{m}"),
            ReqErr::Closed => write!(f, "codex app-server closed unexpectedly"),
            ReqErr::TimedOut => write!(f, "timed out waiting for codex app-server"),
            ReqErr::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// Classify one inbound JSON message. Pure.
///
/// A `null` id counts as absent. A response must carry an integer id (we only
/// ever send integer ids), so a string-id response is junk; a server request may
/// carry either.
fn classify(v: &Value) -> Frame {
    let method = v.get("method").and_then(Value::as_str);
    let id = v.get("id").filter(|i| !i.is_null());
    match (id, method) {
        (Some(id), Some(m)) => Frame::ServerRequest {
            id: id.clone(),
            method: m.to_string(),
        },
        (Some(id), None) => {
            let Some(id) = id.as_i64() else {
                return Frame::Junk;
            };
            let error = v.get("error").map(|e| RpcErrData {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
            Frame::Response {
                id,
                result: v.get("result").cloned(),
                error,
            }
        }
        (None, Some(m)) => Frame::Notification {
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, None) => Frame::Junk,
    }
}

/// A bounded, newest-wins buffer of the app-server's stderr lines, shared between
/// the pump thread and the client. Codex's real failure reason usually lands here,
/// so it is captured while the process is alive rather than read after it exits.
#[derive(Clone)]
struct StderrRing {
    inner: Arc<Mutex<RingInner>>,
    /// Set by the pump on EOF or a read error.
    done: Arc<AtomicBool>,
}

#[derive(Default)]
struct RingInner {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrRing {
    fn new() -> Self {
        StderrRing {
            inner: Arc::new(Mutex::new(RingInner::default())),
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RingInner> {
        // Stderr text must never panic the import: recover a poisoned lock.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Append one line (cut to the per-line cap), then drop the oldest lines until
    /// both the line and byte caps hold. The newest lines always survive.
    fn push(&self, line: &str) {
        let line = truncate_at_char_boundary(line, STDERR_LINE_MAX_BYTES).to_string();
        let mut g = self.lock();
        g.bytes += line.len();
        g.lines.push_back(line);
        while g.lines.len() > STDERR_RING_MAX_LINES || g.bytes > STDERR_RING_MAX_BYTES {
            match g.lines.pop_front() {
                Some(old) => g.bytes -= old.len(),
                None => break,
            }
        }
    }

    fn mark_done(&self) {
        self.done.store(true, Ordering::Release);
    }

    /// The buffered lines joined by `\n`; `""` when empty. Never waits.
    fn tail(&self) -> String {
        let g = self.lock();
        g.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait up to `wait` for the pump to hit EOF, then [`tail`](Self::tail). Used
    /// only where the process is known or suspected dead, so a crashing Codex's
    /// last words are not lost to a race with the pump.
    fn tail_settled(&self, wait: Duration) -> String {
        let until = Instant::now() + wait;
        while !self.done.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.tail()
    }
}

/// Drain `stderr` into `ring` line by line until EOF or a read error, then mark
/// the ring done. Reads bytes, so invalid UTF-8 is decoded lossily instead of
/// ending the loop. Blocking; run on its own thread.
fn pump_stderr<E: Read>(stderr: E, ring: StderrRing) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let line = text.trim_end_matches(['\r', '\n']);
                if !line.trim().is_empty() {
                    ring.push(line);
                }
            }
        }
    }
    ring.mark_done();
}

/// Drain the app-server's stdout: newline-delimited JSON → classified frames →
/// channel. The sender drops on EOF, which the client sees as `Closed`.
fn pump_stdout<R: Read>(stdout: R, tx: Sender<Frame>) {
    let mut reader = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let line = text.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if tx.send(classify(&v)).is_err() {
                    break;
                }
            }
        }
    }
}

/// What `initialize` told us about the server.
#[derive(Debug, Default, PartialEq)]
struct HandshakeInfo {
    /// `codexHome`, kept only when it is a non-empty absolute path.
    codex_home: Option<PathBuf>,
    /// `userAgent`, when it is a string.
    user_agent: Option<String>,
}

impl HandshakeInfo {
    fn from_initialize(result: &Value) -> Self {
        let codex_home = result
            .get("codexHome")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let user_agent = result
            .get("userAgent")
            .and_then(Value::as_str)
            .map(str::to_string);
        HandshakeInfo {
            codex_home,
            user_agent,
        }
    }
}

/// Called once from `Drop` with the grace period to wait for a clean exit before
/// killing. A real child wraps the process; the test fake records the call.
type Teardown = Box<dyn FnOnce(Duration) + Send>;

/// The `codex app-server` client: newline-delimited JSON over a writer, integer
/// request ids, stdout and stderr pump threads, and a buffer for notifications
/// that arrive while awaiting a response. Generic over the writer so tests can
/// drive it with an in-process fake; every read honours the cancel flag and the
/// one shared deadline.
struct AppServer<W: Write> {
    /// `None` after close.
    stdin: Option<W>,
    rx: Receiver<Frame>,
    stderr: StderrRing,
    next_id: i64,
    /// Notifications seen while awaiting a response, scanned by `wait_notification`.
    notif_buf: VecDeque<(String, Value)>,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    /// Set by `abort`: teardown then kills without a grace period.
    aborted: bool,
    teardown: Option<Teardown>,
}

impl<W: Write> AppServer<W> {
    /// Build a client over arbitrary streams. The reader and stderr streams are
    /// consumed by their pump threads here (neither is joined).
    fn from_streams<R, E>(
        reader: R,
        stderr: E,
        writer: W,
        cancel: Arc<AtomicBool>,
        deadline: Instant,
        teardown: Option<Teardown>,
    ) -> Self
    where
        R: Read + Send + 'static,
        E: Read + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel::<Frame>();
        std::thread::spawn(move || pump_stdout(reader, tx));
        let ring = StderrRing::new();
        {
            let ring = ring.clone();
            std::thread::spawn(move || pump_stderr(stderr, ring));
        }
        AppServer {
            stdin: Some(writer),
            rx,
            stderr: ring,
            next_id: 1,
            notif_buf: VecDeque::new(),
            cancel,
            deadline,
            aborted: false,
            teardown,
        }
    }

    /// Mark the server for an immediate kill (no grace) when it is dropped. Used on
    /// cancel and timeout, where waiting for a clean exit is pointless.
    fn abort(&mut self) {
        self.aborted = true;
    }

    /// Handshake: `initialize` request (clientInfo names Keitora; `capabilities`
    /// reproduced verbatim incl. `optOutNotificationMethods`) then the mandatory
    /// `initialized` notification.
    fn handshake(&mut self) -> Result<HandshakeInfo, ReqErr> {
        let init = json!({
            "clientInfo": { "title": "Keitora", "name": "Keitora", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": {
                "experimentalApi": false,
                "requestAttestation": false,
                "optOutNotificationMethods": [
                    "item/agentMessage/delta",
                    "item/reasoning/summaryTextDelta",
                    "item/reasoning/summaryPartAdded",
                    "item/reasoning/textDelta"
                ]
            }
        });
        let result = self.request("initialize", init)?;
        let info = HandshakeInfo::from_initialize(&result);
        self.send(&json!({ "method": "initialized", "params": {} }))
            .map_err(ReqErr::Transport)?;
        Ok(info)
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let line = format!(
            "{}\n",
            serde_json::to_string(msg).map_err(|e| e.to_string())?
        );
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "codex app-server stdin is closed".to_string())?;
        stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write codex app-server stdin: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("flush codex app-server stdin: {e}"))
    }

    /// The one receive loop: waits in short slices so a cancel is noticed within
    /// [`POLL_SLICE`], and gives up at the shared deadline.
    fn next_frame(&mut self) -> Result<Frame, ReqErr> {
        loop {
            if self.cancel.load(Ordering::Acquire) {
                return Err(ReqErr::Cancelled);
            }
            let now = Instant::now();
            if now >= self.deadline {
                return Err(ReqErr::TimedOut);
            }
            let slice = (self.deadline - now).min(POLL_SLICE);
            match self.rx.recv_timeout(slice) {
                Ok(f) => return Ok(f),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return Err(ReqErr::Closed),
            }
        }
    }

    /// Send a request and await its matching response, buffering notifications and
    /// replying `-32601` to any server-initiated request meanwhile.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, ReqErr> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "id": id, "method": method, "params": params }))
            .map_err(ReqErr::Transport)?;
        loop {
            match self.next_frame()? {
                Frame::Response {
                    id: rid,
                    result,
                    error,
                } if rid == id => {
                    return match error {
                        Some(err) => Err(ReqErr::Rpc {
                            code: err.code,
                            message: err.message,
                        }),
                        None => Ok(result.unwrap_or(Value::Null)),
                    };
                }
                Frame::Notification { method: m, params } => self.notif_buf.push_back((m, params)),
                Frame::ServerRequest { id: sid, .. } => self.reject_server_request(&sid),
                Frame::Response { .. } | Frame::Junk => {}
            }
        }
    }

    /// Await the first `method` notification whose params satisfy `matches`. The
    /// buffer of notifications seen during earlier requests is scanned first, so a
    /// notification that beat its request's response is not missed. Any other
    /// notification read meanwhile goes to `on_other` and is dropped; server
    /// requests get `-32601`.
    fn wait_notification(
        &mut self,
        method: &str,
        mut matches: impl FnMut(&Value) -> bool,
        mut on_other: impl FnMut(&str, &Value),
    ) -> Result<Value, ReqErr> {
        if let Some(pos) = self
            .notif_buf
            .iter()
            .position(|(m, p)| m == method && matches(p))
        {
            if let Some((_, params)) = self.notif_buf.remove(pos) {
                return Ok(params);
            }
        }
        loop {
            match self.next_frame()? {
                Frame::Notification { method: m, params } => {
                    if m == method && matches(&params) {
                        return Ok(params);
                    }
                    on_other(&m, &params);
                }
                Frame::ServerRequest { id: sid, .. } => self.reject_server_request(&sid),
                Frame::Response { .. } | Frame::Junk => {}
            }
        }
    }

    /// Reply `-32601` to a server-initiated request (mirrors the plugin's
    /// `handleServerRequest`), echoing its id verbatim. Best-effort — a write
    /// failure here is non-fatal.
    fn reject_server_request(&mut self, id: &Value) {
        let _ = self.send(&json!({
            "id": id,
            "error": { "code": -32601, "message": "Unsupported server request" }
        }));
    }

    /// The captured stderr lines joined by `\n`; `""` when empty. Never waits.
    fn stderr_tail(&self) -> String {
        self.stderr.tail()
    }

    /// Like [`stderr_tail`](Self::stderr_tail), but first waits up to 200 ms for the
    /// stderr pump to reach EOF. For paths where the process has likely died.
    fn stderr_tail_settled(&self) -> String {
        self.stderr.tail_settled(Duration::from_millis(200))
    }
}

/// Why [`AppServer::spawn`] could not start the child.
#[derive(Debug)]
enum SpawnError {
    /// The executable does not exist (`io::ErrorKind::NotFound`).
    NotFound(String),
    Other(String),
}

impl SpawnError {
    fn into_result(self) -> ImportResult {
        ImportResult::Failed(match self {
            SpawnError::NotFound(message) => TransferFailure::CodexNotFound {
                message,
                stderr_tail: String::new(),
            },
            SpawnError::Other(message) => TransferFailure::ProtocolError {
                message,
                stderr_tail: String::new(),
            },
        })
    }
}

impl AppServer<ChildStdin> {
    /// Spawn `codex app-server` in `cwd` with an explicit environment. Windows goes
    /// through `cmd.exe` (see [`codex_command`]), so the direct child is the shell
    /// and teardown kills the whole tree; on unix the child leads its own process
    /// group and teardown kills the group, so a wrapper's grandchild dies too.
    fn spawn(
        env: &CodexEnv,
        cwd: &str,
        cancel: Arc<AtomicBool>,
        deadline: Instant,
    ) -> Result<Self, SpawnError> {
        let mut cmd = codex_command(&env.exe, &["app-server"]);
        cmd.current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_codex_env(&mut cmd, env);
        configure_command(&mut cmd);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }

        let mut child = cmd.spawn().map_err(|e| {
            let message = format!("failed to spawn {} app-server: {e}", env.exe);
            if e.kind() == std::io::ErrorKind::NotFound {
                SpawnError::NotFound(message)
            } else {
                SpawnError::Other(message)
            }
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            teardown_child(child, Duration::ZERO);
            return Err(SpawnError::Other(
                "codex app-server pipes unavailable".to_string(),
            ));
        };

        let teardown: Teardown = Box::new(move |grace| teardown_child(child, grace));
        Ok(AppServer::from_streams(
            stdout,
            stderr,
            stdin,
            cancel,
            deadline,
            Some(teardown),
        ))
    }
}

/// `PATH` when resolution produced one; `CODEX_HOME` only when the user set it
/// explicitly (otherwise Codex applies its own default and the pane agrees). Set on
/// the `Command`, so on Windows `cmd.exe` inherits them and the short-path program
/// resolves under them.
fn apply_codex_env(cmd: &mut Command, env: &CodexEnv) {
    if let Some(path) = &env.path {
        cmd.env("PATH", path);
    }
    if env.codex_home_source == CodexHomeSource::EnvVar {
        cmd.env("CODEX_HOME", &env.codex_home);
    }
}

/// Give `child` up to `grace` to exit on its own (stdin is already closed), then
/// force-kill it. Windows: the direct child is `cmd.exe`, so a bare kill would
/// orphan the real codex grandchild — kill the whole tree instead. Unix: kill the
/// child's process group even when the child itself already exited, so a lingering
/// grandchild dies too; then a direct kill as a backstop.
fn teardown_child(mut child: Child, grace: Duration) {
    let until = Instant::now() + grace;
    let exited = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => break false,
        }
    };
    #[cfg(windows)]
    if !exited {
        kill_process_tree_windows(child.id());
    }
    #[cfg(unix)]
    {
        let _ = exited;
        crate::backend::platform::kill_group_kill(child.id());
        let _ = child.kill();
    }
    let _ = child.wait();
}

impl<W: Write> Drop for AppServer<W> {
    fn drop(&mut self) {
        // Closing stdin makes app-server see EOF and exit on its own.
        drop(self.stdin.take());
        if let Some(t) = self.teardown.take() {
            t(if self.aborted {
                Duration::ZERO
            } else {
                TEARDOWN_GRACE
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Process spawning helpers
// ---------------------------------------------------------------------------

/// Build a `codex …` command. On win32 the `codex` shim is a `.cmd`, so route
/// through `cmd.exe` (the reference plugin's `shell: true`; also keeps `cmd.exe` the
/// direct child so the teardown tree-kill targets it) using the 8.3 short path so a
/// space in the path can't split the launch line. The program and each arg are passed
/// as SEPARATE `Command` args — Rust applies correct Windows argv quoting, which
/// survives cmd.exe's parsing and the shim's `%*` forwarding. Hand-joining a `/C`
/// string and quoting it ourselves mangled multi-word args (the handoff prompt got
/// split into `Summarize` / `this` / … and Codex rejected the extra positionals).
/// On unix invoke the resolved binary directly.
#[cfg(windows)]
pub(crate) fn codex_command(codex: &str, args: &[&str]) -> Command {
    let short = keitora_tasks::short_path(codex);
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(short).args(args);
    cmd
}

#[cfg(not(windows))]
pub(crate) fn codex_command(codex: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(codex);
    cmd.args(args);
    cmd
}

/// Kill an entire process tree on Windows (`taskkill /PID <pid> /T /F`).
#[cfg(windows)]
fn kill_process_tree_windows(pid: u32) {
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
    configure_command(&mut cmd);
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Run a prepared command to completion under a wall-clock deadline, draining both
/// pipes on their own threads. Returns `(status_ok, stdout, stderr)`. Mirrors
/// `dispatch.rs::run_claude_bounded`.
fn run_bounded(mut cmd: Command, timeout: Duration) -> Result<(bool, Vec<u8>, Vec<u8>), String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn codex: {e}"))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| format!("wait on codex: {e}"))?
        {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    // On Windows the codex summary runs through `cmd.exe /C` (see
                    // `codex_command`), so `child` is the shell and the real `codex`
                    // is a grandchild — a bare `child.kill()` would kill `cmd.exe` and
                    // ORPHAN the codex tree. Tree-kill the whole `cmd.exe → codex …`
                    // tree with the same `taskkill /T /F` the `AppServer` teardown
                    // uses; on unix the direct child IS codex, so a plain kill (the
                    // child is a group leader) suffices. Reap either way.
                    #[cfg(windows)]
                    kill_process_tree_windows(child.id());
                    #[cfg(not(windows))]
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err(format!("codex timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok((status.success(), stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive (no wildcard) so a new failure variant fails to compile here
    /// until its copy row is added below.
    fn expected_user_copy(f: &TransferFailure) -> &'static str {
        match f {
            TransferFailure::CodexNotFound { .. } => "Codex could not be found",
            TransferFailure::CodexTooOld { .. } => {
                "This version of Codex cannot import Claude sessions"
            }
            TransferFailure::SessionNotDetected { .. } => {
                "Codex did not recognise the Claude session"
            }
            TransferFailure::Skipped {
                reason: SkipReason::NoOp,
                ..
            } => "Codex found nothing new to import",
            TransferFailure::Skipped {
                reason: SkipReason::Deferred,
                ..
            } => "Codex postponed the import because the session is still in use",
            TransferFailure::Skipped {
                reason: SkipReason::ContinuedIn,
                ..
            } => "This Claude conversation was continued in another session",
            TransferFailure::Timeout {
                phase: TransferPhase::Initialize,
                ..
            } => "Codex did not start in time",
            TransferFailure::Timeout {
                phase: TransferPhase::Import,
                ..
            } => "Codex did not accept the import in time",
            TransferFailure::Timeout {
                phase: TransferPhase::Completion,
                ..
            } => "Codex did not finish the import in time",
            TransferFailure::ProtocolError { .. } => "Codex returned an unexpected response",
            TransferFailure::Other { .. } => "The import did not complete",
        }
    }

    #[test]
    fn transfer_failure_user_copy_is_exact_for_every_variant() {
        let m = || "m".to_string();
        let t = String::new;
        let all = vec![
            TransferFailure::CodexNotFound {
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::CodexTooOld {
                version: Some("0.1.0".into()),
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::SessionNotDetected {
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Skipped {
                reason: SkipReason::NoOp,
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Skipped {
                reason: SkipReason::Deferred,
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Skipped {
                reason: SkipReason::ContinuedIn,
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Timeout {
                phase: TransferPhase::Initialize,
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Timeout {
                phase: TransferPhase::Import,
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Timeout {
                phase: TransferPhase::Completion,
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::ProtocolError {
                message: m(),
                stderr_tail: t(),
            },
            TransferFailure::Other {
                message: m(),
                stderr_tail: t(),
            },
        ];
        assert_eq!(all.len(), 11, "one row per copy-table entry");
        for f in &all {
            let copy = f.user_copy();
            assert_eq!(copy, expected_user_copy(f), "copy for {f:?}");
            assert!(
                !copy.ends_with('.'),
                "copy must not end with a period: {copy}"
            );
        }
        let distinct: std::collections::HashSet<String> =
            all.iter().map(TransferFailure::user_copy).collect();
        assert_eq!(distinct.len(), all.len(), "every row has its own copy");
    }

    #[test]
    fn transfer_outcome_wire_shape() {
        let v = |o: &TransferOutcome| serde_json::to_value(o).unwrap();
        assert_eq!(
            v(&TransferOutcome::Imported {
                thread_id: "t1".into()
            }),
            json!({"kind":"imported","threadId":"t1"})
        );
        assert_eq!(v(&TransferOutcome::Done), json!({"kind":"done"}));
        assert_eq!(v(&TransferOutcome::Cancelled), json!({"kind":"cancelled"}));
        assert_eq!(
            v(&TransferOutcome::Fresh {
                reason: TransferFailure::Skipped {
                    reason: SkipReason::ContinuedIn,
                    message: "m".into(),
                    stderr_tail: String::new(),
                },
                reason_text: "x".into(),
            }),
            json!({
                "kind":"fresh",
                "reason":{"kind":"skipped","reason":"continued_in","message":"m","stderrTail":""},
                "reasonText":"x"
            })
        );

        let too_old = serde_json::to_value(TransferFailure::CodexTooOld {
            version: None,
            message: "m".into(),
            stderr_tail: "e".into(),
        })
        .unwrap();
        assert_eq!(
            too_old,
            json!({"kind":"codex_too_old","version":null,"message":"m","stderrTail":"e"})
        );

        let timeout = serde_json::to_value(TransferFailure::Timeout {
            phase: TransferPhase::Completion,
            message: "m".into(),
            stderr_tail: String::new(),
        })
        .unwrap();
        assert_eq!(
            timeout,
            json!({"kind":"timeout","phase":"completion","message":"m","stderrTail":""})
        );

        let progress = serde_json::to_value(TransferProgress {
            project: "proj-a".into(),
            worktree: "alpha".into(),
            phase: ProgressPhase::AwaitingCompletion,
            message: None,
            elapsed_ms: 5,
            deadline_ms: 150_000,
        })
        .unwrap();
        assert_eq!(
            progress,
            json!({
                "project":"proj-a","worktree":"alpha","phase":"awaiting_completion",
                "message":null,"elapsedMs":5,"deadlineMs":150_000
            })
        );
    }

    #[test]
    fn a_transfer_holds_its_session_out_of_recovery_until_it_returns() {
        use crate::backend::herdr::recovery::machine::Event;
        let (handle, rx) = crate::backend::herdr::recovery::recording_handle();
        let key = SessionKey {
            project: "p".into(),
            worktree: "w".into(),
        };
        let seen_inside =
            tauri::async_runtime::block_on(with_recovery_lock(Some(handle), &key, async {
                rx.try_iter().collect::<Vec<_>>()
            }));
        assert_eq!(seen_inside, vec![Event::TransferLock(key.clone(), true)]);
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![Event::TransferLock(key.clone(), false)]
        );
        // Without recovery the transfer just runs.
        assert_eq!(
            tauri::async_runtime::block_on(with_recovery_lock(None, &key, async { 7 })),
            7
        );
    }

    #[test]
    fn a_terminated_session_is_refused_before_anything_changes() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(refuse_terminated(&fake, &key), Ok(()));
        fake.insert_terminated(key.clone());
        assert_eq!(
            refuse_terminated(&fake, &key),
            Err("session is terminated; launch it before transferring".to_string())
        );
        // Another worktree, or the same name in another project, is unaffected.
        let other = SessionKey {
            project: "other".into(),
            worktree: "alpha".into(),
        };
        assert_eq!(refuse_terminated(&fake, &other), Ok(()));
        // The check only reads: no session call was made, nothing was killed.
        assert!(fake.calls().is_empty());
    }

    // --- Ledger parse vs. a captured (scrubbed) sample ----------------------

    /// A redacted capture of `external_agent_session_imports.json`: the real
    /// top-level `{records, detected_connector_records}` shape, records carrying
    /// extra fields beyond what we consume (plus an intentionally-unknown
    /// `experimental_flag` to prove non-strict deserialization), and TWO records
    /// with the same source_path (different thread ids) to prove last-wins.
    /// All paths/ids are synthetic.
    const LEDGER_SAMPLE: &str = r#"{
      "records": [
        {
          "source_path": "\\\\?\\C:\\Users\\dev\\.claude\\projects\\D--repos-demo\\aaaaaaaa-1111.jsonl",
          "content_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
          "imported_thread_id": "01900000-0000-7000-8000-000000000001",
          "imported_at": 1787500000,
          "source_modified_at": 1787460000000000000,
          "connector_names": [],
          "title": "First import",
          "experimental_flag": true
        },
        {
          "source_path": "\\\\?\\C:\\Users\\dev\\.claude\\projects\\D--repos-demo\\bbbbbbbb-2222.jsonl",
          "content_sha256": "2222222222222222222222222222222222222222222222222222222222222222",
          "imported_thread_id": "01900000-0000-7000-8000-000000000002",
          "imported_at": 1787500500,
          "source_modified_at": 1787460500000000000,
          "connector_names": ["codex"],
          "title": "Other worktree"
        },
        {
          "source_path": "\\\\?\\C:\\Users\\dev\\.claude\\projects\\D--repos-demo\\aaaaaaaa-1111.jsonl",
          "content_sha256": "1111111111111111111111111111111111111111111111111111111111111111",
          "imported_thread_id": "01900000-0000-7000-8000-000000000003",
          "imported_at": 1787501000,
          "source_modified_at": 1787461000000000000,
          "connector_names": [],
          "title": "Re-import (newer)"
        }
      ],
      "detected_connector_records": [
        { "connector_name": "codex", "detected_at": 1787499000 }
      ]
    }"#;

    const SAMPLE_SOURCE: &str =
        r"\\?\C:\Users\dev\.claude\projects\D--repos-demo\aaaaaaaa-1111.jsonl";

    fn paths(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    fn ledger_text(records: Value) -> String {
        json!({ "records": records, "detected_connector_records": [] }).to_string()
    }

    #[test]
    fn ledger_thread_for_paths_takes_last_and_tolerates_extra_fields() {
        // Two records match the path; the LAST (thread ...0003) wins.
        let got = ledger_thread_for_paths(LEDGER_SAMPLE, &paths(&[SAMPLE_SOURCE]));
        assert_eq!(got.as_deref(), Some("01900000-0000-7000-8000-000000000003"));
    }

    #[test]
    fn ledger_thread_for_paths_matches_any_candidate_path() {
        let raw = r"C:\Users\x\.claude\projects\enc\s-1.jsonl";
        let canonical = r"\\?\C:\Users\x\.claude\projects\enc\s-1.jsonl";
        let candidates = paths(&[raw, canonical, raw]);
        for recorded in [raw, canonical] {
            let text = ledger_text(json!([
                { "source_path": recorded, "imported_thread_id": "thread-a" }
            ]));
            assert_eq!(
                ledger_thread_for_paths(&text, &candidates).as_deref(),
                Some("thread-a"),
                "recorded as {recorded}"
            );
        }
    }

    #[test]
    fn ledger_thread_for_paths_ignores_records_without_a_thread_id() {
        let text = ledger_text(json!([
            { "source_path": "/home/u/s.jsonl", "imported_thread_id": "thread-a" },
            { "source_path": "/home/u/s.jsonl" },
            { "source_path": "/home/u/s.jsonl", "imported_thread_id": null }
        ]));
        assert_eq!(
            ledger_thread_for_paths(&text, &paths(&["/home/u/s.jsonl"])).as_deref(),
            Some("thread-a")
        );
    }

    #[test]
    fn ledger_thread_for_paths_ignores_other_paths() {
        assert_eq!(
            ledger_thread_for_paths(LEDGER_SAMPLE, &paths(&[r"\\?\C:\nope.jsonl"])),
            None
        );
        assert_eq!(ledger_thread_for_paths(LEDGER_SAMPLE, &[]), None);
    }

    #[test]
    fn ledger_thread_for_paths_tolerates_missing_records_key() {
        // A ledger with only detected_connector_records must not error out.
        let text = r#"{ "detected_connector_records": [] }"#;
        assert_eq!(
            ledger_thread_for_paths(text, &paths(&[SAMPLE_SOURCE])),
            None
        );
        assert_eq!(
            ledger_thread_for_paths("not json", &paths(&[SAMPLE_SOURCE])),
            None
        );
    }

    #[test]
    fn ledger_thread_for_paths_needs_no_sha() {
        // No hash, or a hash of different content: the path alone decides.
        let text = ledger_text(json!([
            { "source_path": "/home/u/s.jsonl", "imported_thread_id": "thread-a" },
            { "source_path": "/home/u/s.jsonl", "content_sha256": "ffff", "imported_thread_id": "thread-b" }
        ]));
        assert_eq!(
            ledger_thread_for_paths(&text, &paths(&["/home/u/s.jsonl"])).as_deref(),
            Some("thread-b")
        );
    }

    #[test]
    fn ledger_source_paths_orders_raw_then_canonical_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("s-1.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        let got = ledger_source_paths(&file);
        let raw = file.to_string_lossy().into_owned();
        let canonical = std::fs::canonicalize(&file)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(got[0], raw, "the exact string we sent comes first");
        assert!(got.contains(&canonical));
        let mut unique = got.clone();
        unique.dedup();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), got.len(), "no duplicates: {got:?}");
        #[cfg(windows)]
        {
            // The stripped form often equals the raw path, so it may be deduped away.
            let stripped = keitora_tasks::strip_extended_prefix_str(&canonical);
            assert!(
                got[1].starts_with(r"\\?\"),
                "canonical keeps the prefix: {got:?}"
            );
            assert!(got.contains(&stripped), "{got:?}");
            assert!(!stripped.starts_with(r"\\?\"), "stripped form drops it");
        }
        // A file that cannot be canonicalized yields just the raw path.
        let missing = tmp.path().join("missing.jsonl");
        assert_eq!(
            ledger_source_paths(&missing),
            vec![missing.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn ledger_lookup_returns_not_found_after_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let got = ledger_lookup(
            tmp.path(),
            &paths(&["/home/u/s.jsonl"]),
            &[],
            false,
            Duration::from_millis(100),
            Duration::from_millis(20),
            &cancel,
        );
        assert_eq!(got, LedgerOutcome::NotFound);
        assert!(start.elapsed() >= Duration::from_millis(100));
        // A record for another path does not count either.
        std::fs::write(
            tmp.path().join(LEDGER_FILE),
            ledger_text(
                json!([{ "source_path": "/home/u/other.jsonl", "imported_thread_id": "t" }]),
            ),
        )
        .unwrap();
        let got = ledger_lookup(
            tmp.path(),
            &paths(&["/home/u/s.jsonl"]),
            &[],
            false,
            Duration::from_millis(40),
            Duration::from_millis(20),
            &cancel,
        );
        assert_eq!(got, LedgerOutcome::NotFound);
    }

    #[test]
    fn ledger_lookup_rereads_the_ledger_once_it_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let ledger = tmp.path().join(LEDGER_FILE);
        std::fs::write(
            &ledger,
            ledger_text(
                json!([{ "source_path": "/home/u/other.jsonl", "imported_thread_id": "t0" }]),
            ),
        )
        .unwrap();
        let writer = {
            let ledger = ledger.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                std::fs::write(
                    &ledger,
                    ledger_text(json!([
                        { "source_path": "/home/u/other.jsonl", "imported_thread_id": "t0" },
                        { "source_path": "/home/u/s.jsonl", "imported_thread_id": "t1" },
                    ])),
                )
                .unwrap();
            })
        };
        let cancel = AtomicBool::new(false);
        let got = ledger_lookup(
            tmp.path(),
            &paths(&["/home/u/s.jsonl"]),
            &[],
            false,
            Duration::from_secs(5),
            Duration::from_millis(20),
            &cancel,
        );
        writer.join().unwrap();
        assert_eq!(got, LedgerOutcome::Found("t1".into()));
    }

    #[test]
    fn ledger_lookup_stops_on_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let got = ledger_lookup(
            tmp.path(),
            &paths(&["/home/u/s.jsonl"]),
            &[],
            false,
            Duration::from_secs(10),
            Duration::from_millis(20),
            &cancel,
        );
        setter.join().unwrap();
        assert_eq!(got, LedgerOutcome::Cancelled);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    // --- Migration payload serializes to the exact expected JSON -------------

    #[test]
    fn migration_payload_serializes_verbatim() {
        let payload = migration_payload("/home/dev/.claude/projects/x/foo.jsonl", "/work/tree");
        let got = serde_json::to_string(&payload).unwrap();
        // Exact key order (declaration order): itemType, description, cwd, details
        // { plugins, sessions, mcpServers, hooks, subagents, commands }; inner
        // session { path, cwd, title }; outer cwd null, inner session cwd real.
        let expected = concat!(
            r#"{"migrationItems":[{"itemType":"SESSIONS","#,
            r#""description":"Transfer Claude session foo.jsonl","#,
            r#""cwd":null,"#,
            r#""details":{"plugins":[],"#,
            r#""sessions":[{"path":"/home/dev/.claude/projects/x/foo.jsonl","cwd":"/work/tree","title":null}],"#,
            r#""mcpServers":[],"hooks":[],"subagents":[],"commands":[]}}]}"#
        );
        assert_eq!(got, expected);
    }

    // --- Encoded-cwd + the `\\?\`-vs-ledger asymmetry -----------------------

    #[test]
    fn encode_project_dir_matches_verified_example() {
        // Verified real dir under ~/.claude/projects: each non-alnum char → one `-`
        // (so `:` and `\` give the leading `--`).
        assert_eq!(
            encode_project_dir(r"D:\repos\proj-worktrees\beta"),
            "D--repos-proj-worktrees-beta"
        );
    }

    #[test]
    fn encode_project_dir_unix_path() {
        assert_eq!(
            encode_project_dir("/home/dev/repos/proj-wt"),
            "-home-dev-repos-proj-wt"
        );
    }

    #[test]
    fn encoded_cwd_dir_name_canonicalizes_real_dirs_and_errors_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let stripped = keitora_tasks::strip_extended_prefix_str(&canonical.to_string_lossy());
        assert_eq!(
            encoded_cwd_dir_name(tmp.path()).unwrap(),
            encode_project_dir(&stripped)
        );
        assert!(!encoded_cwd_dir_name(tmp.path()).unwrap().contains('?'));
        assert!(encoded_cwd_dir_name(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn claude_projects_dir_from_prefers_non_empty_config_dir() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            claude_projects_dir_from(Some("/cfg/claude".into()), home.clone()).unwrap(),
            PathBuf::from("/cfg/claude").join("projects")
        );
        assert_eq!(
            claude_projects_dir_from(Some("".into()), home.clone()).unwrap(),
            PathBuf::from("/home/u").join(".claude").join("projects")
        );
        assert_eq!(
            claude_projects_dir_from(None, home).unwrap(),
            PathBuf::from("/home/u").join(".claude").join("projects")
        );
        assert!(claude_projects_dir_from(None, None).is_err());
    }

    /// Serialises the tests that set the process-global `CLAUDE_CONFIG_DIR`.
    fn claude_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn claude_projects_dir_honours_claude_config_dir() {
        let _guard = claude_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("CLAUDE_CONFIG_DIR");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
        let got = claude_projects_dir();
        match old {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        assert_eq!(got.unwrap(), tmp.path().join("projects"));
    }

    #[test]
    fn claude_session_file_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        assert!(
            !claude_project_has_sessions(projects, "D--wt"),
            "missing dir"
        );
        std::fs::create_dir_all(projects.join("D--wt")).unwrap();
        assert!(!claude_project_has_sessions(projects, "D--wt"), "empty dir");
        std::fs::write(projects.join("D--wt").join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(projects.join("D--wt").join("dir.jsonl")).unwrap();
        assert!(
            !claude_project_has_sessions(projects, "D--wt"),
            "only files ending in .jsonl count"
        );
        std::fs::write(projects.join("D--wt").join("abc-1.jsonl"), "{}").unwrap();
        assert!(claude_project_has_sessions(projects, "D--wt"));
        assert!(claude_session_file_exists(projects, "D--wt", "abc-1"));
        assert!(!claude_session_file_exists(projects, "D--wt", "abc-2"));
        assert!(!claude_session_file_exists(projects, "D--other", "abc-1"));
        assert!(!claude_session_file_exists(projects, "D--wt", "dir"));
    }

    /// The two normalizations must NOT be conflated: the ledger realpath keeps the
    /// `\\?\` prefix, while the live-session-lookup cwd encoding strips it. A
    /// regression here silently fails every ledger match on Windows.
    #[cfg(windows)]
    #[test]
    fn ledger_realpath_retains_prefix_while_cwd_encoding_strips_it() {
        let canonical = r"\\?\D:\repos\proj-worktrees\beta";

        // Ledger side: strip is NOT applied — the prefix is retained verbatim, so a
        // record whose source_path carries the prefix matches.
        let ledger_source = canonical; // used as-is for ledger matching
        assert!(
            ledger_source.starts_with(r"\\?\"),
            "ledger realpath must retain the \\\\?\\ prefix"
        );

        // Lookup side: strip THEN encode → the encoded dir name has no prefix.
        let stripped = keitora_tasks::strip_extended_prefix_str(canonical);
        assert!(
            !stripped.starts_with(r"\\?\"),
            "cwd encoding must strip the \\\\?\\ prefix"
        );
        assert_eq!(
            encode_project_dir(&stripped),
            "D--repos-proj-worktrees-beta"
        );
    }

    // --- Retry-on-active-writer predicate (Codex → Claude release race) -----

    #[test]
    fn is_active_writer_error_matches_all_markers_case_insensitively() {
        // The real failure string from a not-yet-released interactive writer.
        assert!(is_active_writer_error(
            "codex exec resume failed: thread-store conflict: thread abc already has an active writer … thread/resume: thread abc already has an active writer (code -32600)"
        ));
        // Each marker in isolation, and case-insensitively.
        assert!(is_active_writer_error("ACTIVE WRITER"));
        assert!(is_active_writer_error("error code -32600"));
        assert!(is_active_writer_error("Thread-Store Conflict"));
    }

    // --- Codex→Claude summary argv: by-id vs. --last ------------------------

    #[test]
    fn summary_command_uses_the_resolved_codex_env() {
        let env_of = |source: CodexHomeSource| CodexEnv {
            exe: "/opt/codex/bin/codex".into(),
            exe_source: crate::codex_env::ExeSource::Configured,
            path: Some("/opt/codex/bin:/usr/bin".into()),
            codex_home: PathBuf::from("/home/u/codex-home"),
            codex_home_source: source,
            version: None,
        };
        let envs = |cmd: &Command| -> Vec<(String, Option<String>)> {
            cmd.get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect()
        };
        let cmd = summary_command(
            &env_of(CodexHomeSource::EnvVar),
            "/home/u/wt",
            Some("thread-a"),
            "s.json",
            "o.json",
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        #[cfg(windows)]
        {
            assert_eq!(cmd.get_program(), "cmd");
            assert_eq!(args[0], "/C");
        }
        #[cfg(not(windows))]
        assert_eq!(cmd.get_program(), "/opt/codex/bin/codex");
        assert!(args.ends_with(&[
            "exec".into(),
            "resume".into(),
            "thread-a".into(),
            HANDOFF_PROMPT.into(),
            "--output-schema".into(),
            "s.json".into(),
            "-o".into(),
            "o.json".into(),
        ]));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/home/u/wt")));
        let got = envs(&cmd);
        assert!(got.contains(&("PATH".into(), Some("/opt/codex/bin:/usr/bin".into()))));
        assert!(got.contains(&("CODEX_HOME".into(), Some("/home/u/codex-home".into()))));

        // A defaulted home is left to Codex itself.
        let cmd = summary_command(
            &env_of(CodexHomeSource::Default),
            "/home/u/wt",
            None,
            "s.json",
            "o.json",
        );
        assert!(!envs(&cmd).iter().any(|(k, _)| k == "CODEX_HOME"));
    }

    #[test]
    fn summary_args_by_id_shape() {
        // A persisted thread id lands in the SESSION_ID slot.
        assert_eq!(
            summary_args(Some("0199-abc"), "PROMPT", "/tmp/s.json", "/tmp/o.json"),
            [
                "exec",
                "resume",
                "0199-abc",
                "PROMPT",
                "--output-schema",
                "/tmp/s.json",
                "-o",
                "/tmp/o.json",
            ]
        );
    }

    #[test]
    fn summary_args_last_shape_for_native_codex_worktree() {
        // No persisted thread id → `--last` (native Codex worktree handoff).
        assert_eq!(
            summary_args(None, "PROMPT", "/tmp/s.json", "/tmp/o.json"),
            [
                "exec",
                "resume",
                "--last",
                "PROMPT",
                "--output-schema",
                "/tmp/s.json",
                "-o",
                "/tmp/o.json",
            ]
        );
    }

    #[test]
    fn is_active_writer_error_rejects_unrelated_failures() {
        // A genuine, non-retryable error must surface, not spin the retry loop.
        assert!(!is_active_writer_error(
            "codex exec resume failed: no such thread"
        ));
        assert!(!is_active_writer_error("codex timed out after 180s"));
        assert!(!is_active_writer_error("parse handoff summary JSON: eof"));
        // A different RPC code must NOT be treated as the writer conflict.
        assert!(!is_active_writer_error("app-server error -32601"));
    }

    // --- JSON-RPC framing classification round-trip -------------------------

    #[test]
    fn classify_response_success() {
        let v: Value = serde_json::from_str(r#"{"id":7,"result":{"ok":true}}"#).unwrap();
        assert_eq!(
            classify(&v),
            Frame::Response {
                id: 7,
                result: Some(json!({"ok": true})),
                error: None,
            }
        );
    }

    #[test]
    fn classify_response_error_carries_code() {
        let v: Value = serde_json::from_str(
            r#"{"id":9,"error":{"code":-32601,"message":"Method not found"}}"#,
        )
        .unwrap();
        assert_eq!(
            classify(&v),
            Frame::Response {
                id: 9,
                result: None,
                error: Some(RpcErrData {
                    code: -32601,
                    message: "Method not found".to_string(),
                }),
            }
        );
    }

    #[test]
    fn classify_notification_and_server_request() {
        let notif: Value = serde_json::from_str(
            r#"{"method":"externalAgentConfig/import/completed","params":{"x":1}}"#,
        )
        .unwrap();
        assert_eq!(
            classify(&notif),
            Frame::Notification {
                method: "externalAgentConfig/import/completed".to_string(),
                params: json!({"x": 1}),
            }
        );

        let server_req: Value =
            serde_json::from_str(r#"{"id":3,"method":"some/serverCall","params":{}}"#).unwrap();
        assert_eq!(
            classify(&server_req),
            Frame::ServerRequest {
                id: json!(3),
                method: "some/serverCall".to_string(),
            }
        );

        let junk: Value = serde_json::from_str(r#"{"hello":"world"}"#).unwrap();
        assert_eq!(classify(&junk), Frame::Junk);
    }

    #[test]
    fn classify_string_id_server_request() {
        let v = json!({"id": "srv-1", "method": "item/approval", "params": {}});
        assert_eq!(
            classify(&v),
            Frame::ServerRequest {
                id: json!("srv-1"),
                method: "item/approval".to_string(),
            }
        );
    }

    #[test]
    fn classify_response_with_string_id_is_junk() {
        let v = json!({"id": "7", "result": {"ok": true}});
        assert_eq!(classify(&v), Frame::Junk);
    }

    #[test]
    fn classify_null_id_is_a_notification() {
        let v = json!({"id": null, "method": "some/event", "params": {"a": 1}});
        assert_eq!(
            classify(&v),
            Frame::Notification {
                method: "some/event".to_string(),
                params: json!({"a": 1}),
            }
        );
        // Missing params default to null; a bare null id with no method is junk.
        assert_eq!(
            classify(&json!({"id": null, "method": "x"})),
            Frame::Notification {
                method: "x".to_string(),
                params: Value::Null,
            }
        );
        assert_eq!(classify(&json!({"id": null})), Frame::Junk);
    }

    // --- Stderr ring --------------------------------------------------------

    #[test]
    fn stderr_ring_keeps_newest_lines() {
        let ring = StderrRing::new();
        for i in 1..=250 {
            ring.push(&format!("line {i}"));
        }
        let tail = ring.tail();
        let lines: Vec<&str> = tail.split('\n').collect();
        assert_eq!(lines.len(), STDERR_RING_MAX_LINES);
        assert_eq!(lines[0], "line 51");
        assert_eq!(*lines.last().unwrap(), "line 250");
    }

    #[test]
    fn stderr_ring_caps_total_bytes() {
        let ring = StderrRing::new();
        for i in 0..40 {
            let mut line = format!("{i:04}-");
            line.push_str(&"x".repeat(1024 - line.len()));
            ring.push(&line);
        }
        let g = ring.lock();
        assert!(g.bytes <= STDERR_RING_MAX_BYTES, "bytes = {}", g.bytes);
        assert_eq!(g.bytes, g.lines.iter().map(String::len).sum::<usize>());
        assert!(g.lines.back().unwrap().starts_with("0039-"), "newest kept");
        assert!(
            !g.lines.iter().any(|l| l.starts_with("0000-")),
            "oldest dropped"
        );
    }

    #[test]
    fn stderr_ring_truncates_a_long_line() {
        let ring = StderrRing::new();
        // 3-byte chars, so a naive byte cut at the cap would split one.
        let long = "\u{20AC}".repeat(5 * 1024 / 3 + 1);
        assert!(long.len() >= 5 * 1024);
        ring.push(&long);
        let tail = ring.tail();
        assert!(tail.len() <= STDERR_LINE_MAX_BYTES, "len = {}", tail.len());
        assert!(tail.len() > STDERR_LINE_MAX_BYTES - 3);
        assert!(tail.chars().all(|c| c == '\u{20AC}'));
        assert!(long.starts_with(&tail));
    }

    #[test]
    fn stderr_pump_survives_invalid_utf8() {
        let ring = StderrRing::new();
        pump_stderr(
            std::io::Cursor::new(b"ok\n\xff\xfe\nnext\r\n\n".to_vec()),
            ring.clone(),
        );
        assert!(ring.done.load(Ordering::Acquire), "EOF marks the ring done");
        let tail = ring.tail();
        let lines: Vec<&str> = tail.split('\n').collect();
        assert_eq!(lines.first(), Some(&"ok"));
        assert_eq!(lines.last(), Some(&"next"), "CRLF trimmed, empty skipped");
        assert_eq!(lines.len(), 3, "the invalid line is kept lossily: {tail:?}");
    }

    // --- Stream-generic app-server client over the in-process fake ----------

    use fake_app_server::{ChannelWriter, FakeHandle, FakeServer, Reply, Script};

    /// Build a client over a fake, with a fresh cancel flag.
    fn client_over(
        f: FakeServer,
        deadline: Duration,
    ) -> (AppServer<ChannelWriter>, FakeHandle, Arc<AtomicBool>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = f.handle;
        let teardown = Some(handle.teardown());
        let server = AppServer::from_streams(
            f.reader,
            f.stderr,
            f.writer,
            cancel.clone(),
            Instant::now() + deadline,
            teardown,
        );
        (server, handle, cancel)
    }

    /// A thin import over the generic client: handshake, one import request, then
    /// the completion carrying our `importId`. Aborts on timeout or cancel, like
    /// the real import. Returns the first success target, or the failing phase.
    fn thin_import<W: Write>(server: &mut AppServer<W>) -> Result<String, (TransferPhase, ReqErr)> {
        fn fail<W: Write>(
            server: &mut AppServer<W>,
            phase: TransferPhase,
            err: ReqErr,
        ) -> (TransferPhase, ReqErr) {
            if matches!(err, ReqErr::TimedOut | ReqErr::Cancelled) {
                server.abort();
            }
            (phase, err)
        }
        if let Err(e) = server.handshake() {
            return Err(fail(server, TransferPhase::Initialize, e));
        }
        let resp = match server.request("externalAgentConfig/import", json!({})) {
            Ok(v) => v,
            Err(e) => return Err(fail(server, TransferPhase::Import, e)),
        };
        let import_id = resp.get("importId").cloned();
        let done = match server.wait_notification(
            EXTERNAL_AGENT_IMPORT_COMPLETED,
            |p| p.get("importId") == import_id.as_ref(),
            |_, _| {},
        ) {
            Ok(v) => v,
            Err(e) => return Err(fail(server, TransferPhase::Completion, e)),
        };
        Ok(done["itemTypeResults"][0]["successes"][0]["target"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    fn assert_times_out_in(script: Script, phase: TransferPhase) {
        let (mut server, handle, _cancel) =
            client_over(FakeServer::scripted(script), Duration::from_millis(300));
        let start = Instant::now();
        let res = thin_import(&mut server);
        let elapsed = start.elapsed();
        match res {
            Err((p, ReqErr::TimedOut)) => assert_eq!(p, phase),
            other => panic!("expected a timeout in {phase:?}, got {other:?}"),
        }
        assert!(elapsed < Duration::from_millis(1500), "took {elapsed:?}");
        drop(server);
        assert_eq!(
            handle.teardown_grace(),
            Some(Duration::ZERO),
            "a timeout kills immediately"
        );
    }

    #[test]
    fn thin_harness_deadline_in_initialize_is_timeout_initialize() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.initialize = Reply::Silent;
        assert_times_out_in(script, TransferPhase::Initialize);
    }

    #[test]
    fn thin_harness_deadline_in_import_is_timeout_import() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.import = Reply::Silent;
        assert_times_out_in(script, TransferPhase::Import);
    }

    #[test]
    fn thin_harness_deadline_in_completion_is_timeout_completion() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.after_import_response.clear();
        assert_times_out_in(script, TransferPhase::Completion);
    }

    #[test]
    fn thin_harness_completion_before_response_and_for_other_ids() {
        let mut script = Script::default_success("imp-1", "thread-a");
        let completed = script.after_import_response.remove(0);
        script.before_import_response = vec![
            (
                EXTERNAL_AGENT_IMPORT_COMPLETED.to_string(),
                json!({
                    "importId": "imp-other",
                    "itemTypeResults": [{
                        "itemType": "SESSIONS",
                        "successes": [{"target": "thread-other"}],
                        "failures": []
                    }]
                }),
            ),
            completed,
        ];
        let (mut server, _handle, _cancel) =
            client_over(FakeServer::scripted(script), Duration::from_secs(10));
        assert_eq!(thin_import(&mut server).unwrap(), "thread-a");
    }

    #[test]
    fn thin_harness_string_id_server_request_gets_a_32601_reply() {
        let f = FakeServer::new(|msg, out| {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => out.respond(&id, json!({})),
                Some("externalAgentConfig/import") => {
                    out.server_request(json!("srv-1"), "item/approval");
                    out.respond(&id, json!({"importId": "imp-1"}));
                    out.notify(
                        EXTERNAL_AGENT_IMPORT_COMPLETED,
                        json!({
                            "importId": "imp-1",
                            "itemTypeResults": [{
                                "itemType": "SESSIONS",
                                "successes": [{"target": "thread-a"}],
                                "failures": []
                            }]
                        }),
                    );
                }
                _ => {}
            }
        });
        let (mut server, handle, _cancel) = client_over(f, Duration::from_secs(10));
        assert_eq!(thin_import(&mut server).unwrap(), "thread-a");
        // The fake records lines on its own thread; wait for the reply to land.
        let until = Instant::now() + Duration::from_secs(2);
        while !handle
            .received()
            .iter()
            .any(|v| v.get("id") == Some(&json!("srv-1")))
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let received = handle.received();
        let methods: Vec<&str> = received
            .iter()
            .filter_map(|v| v.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            methods,
            ["initialize", "initialized", "externalAgentConfig/import"]
        );
        let reply = received
            .iter()
            .find(|v| v.get("id") == Some(&json!("srv-1")))
            .expect("a reply to the string-id server request");
        assert_eq!(reply["error"]["code"], json!(-32601));
        assert!(reply.get("method").is_none());
        drop(server);
        assert_eq!(handle.teardown_grace(), Some(TEARDOWN_GRACE));
    }

    #[test]
    fn thin_harness_cancel_while_waiting_returns_cancelled_and_tears_down() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.after_import_response.clear();
        let (mut server, handle, cancel) =
            client_over(FakeServer::scripted(script), Duration::from_secs(10));
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let res = thin_import(&mut server);
        let elapsed = start.elapsed();
        setter.join().unwrap();
        assert!(
            matches!(res, Err((TransferPhase::Completion, ReqErr::Cancelled))),
            "got {res:?}"
        );
        assert!(elapsed < Duration::from_secs(1), "took {elapsed:?}");
        drop(server);
        assert_eq!(handle.teardown_grace(), Some(Duration::ZERO));
    }

    #[test]
    fn thin_harness_rpc_error_keeps_code_and_live_stderr() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.import = Reply::Error(-32000, "boom");
        script.stderr_at_start = Some("boom: bad auth".to_string());
        let (mut server, _handle, _cancel) =
            client_over(FakeServer::scripted(script), Duration::from_secs(10));
        match thin_import(&mut server) {
            Err((TransferPhase::Import, ReqErr::Rpc { code, message })) => {
                assert_eq!(code, -32000);
                assert_eq!(message, "boom");
            }
            other => panic!("expected an rpc error on import, got {other:?}"),
        }
        // The fake is still open: the tail is captured while it is alive.
        let until = Instant::now() + Duration::from_secs(2);
        while server.stderr_tail().is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.stderr_tail(), "boom: bad auth");
    }

    #[test]
    fn closed_stdout_is_closed_and_the_settled_tail_has_the_last_words() {
        let mut server = AppServer::from_streams(
            std::io::Cursor::new(Vec::new()),
            std::io::Cursor::new(b"crash: x\n".to_vec()),
            std::io::sink(),
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_secs(5),
            None,
        );
        assert!(matches!(server.handshake(), Err(ReqErr::Closed)));
        assert_eq!(server.stderr_tail_settled(), "crash: x");
    }

    // --- Upstream shapes ------------------------------------------------------
    //
    // Every upstream shape the import relies on lives in one fixture, so a field
    // rename upstream is a one-line, reviewable fixture edit.

    const INITIALIZE_RESULT: &str =
        r#"{"codexHome":"/home/u/.codex","userAgent":"codex-fake/0.149.0"}"#;
    const IMPORT_RESPONSE: &str = r#"{"importId":"imp-1"}"#;
    const PROGRESS_PARAMS: &str = r#"{"importId":"imp-1","message":"step 1"}"#;
    const COMPLETION_SUCCESS: &str = r#"{"importId":"imp-1","itemTypeResults":[
        {"itemType":"SESSIONS","successes":[{"target":"thread-a"}],"failures":[]}]}"#;
    const COMPLETION_SESSION_NOT_DETECTED: &str = r#"{"importId":"imp-1","itemTypeResults":[
        {"itemType":"SESSIONS","successes":[],"failures":[
            {"failureStage":"detect","errorType":"session_not_detected","subErrorType":"","message":"m"}]}]}"#;
    const COMPLETION_CONTINUED_IN: &str = r#"{"importId":"imp-1","itemTypeResults":[
        {"itemType":"SESSIONS","successes":[],"failures":[
            {"failureStage":"import","errorType":"continued-in","subErrorType":"","message":"continued elsewhere"}]}]}"#;
    const COMPLETION_DEFERRED: &str = r#"{"importId":"imp-1","itemTypeResults":[
        {"itemType":"SESSIONS","successes":[],"failures":[
            {"failureStage":"import","errorType":"deferred","subErrorType":"thread_open","message":"the thread is open"}]}]}"#;
    const COMPLETION_NOOP: &str = r#"{"importId":"imp-1","itemTypeResults":[
        {"itemType":"SESSIONS","successes":[],"failures":[],"status":"noop"}]}"#;
    const COMPLETION_EMPTY: &str = r#"{"importId":"imp-1","itemTypeResults":[
        {"itemType":"SESSIONS","successes":[],"failures":[]}]}"#;

    fn fx(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn upstream_shape_fixtures_pin_field_names() {
        let is_str = |v: &Value, ptr: &str| {
            assert!(
                v.pointer(ptr).and_then(Value::as_str).is_some(),
                "{ptr} must be a string in {v}"
            )
        };
        let is_arr = |v: &Value, ptr: &str| {
            assert!(
                v.pointer(ptr).and_then(Value::as_array).is_some(),
                "{ptr} must be an array in {v}"
            )
        };
        // The import response carries the import id.
        is_str(&fx(IMPORT_RESPONSE), "/importId");
        // The completion carries the import id and per-item results; the session
        // item's success target is the thread id.
        let success = fx(COMPLETION_SUCCESS);
        is_str(&success, "/importId");
        is_arr(&success, "/itemTypeResults");
        assert_eq!(
            success.pointer("/itemTypeResults/0/itemType"),
            Some(&json!("SESSIONS"))
        );
        is_str(&success, "/itemTypeResults/0/successes/0/target");
        // Failures carry four string fields.
        let failed = fx(COMPLETION_SESSION_NOT_DETECTED);
        for key in ["failureStage", "errorType", "subErrorType", "message"] {
            is_str(&failed, &format!("/itemTypeResults/0/failures/0/{key}"));
        }
        for text in [
            COMPLETION_SUCCESS,
            COMPLETION_SESSION_NOT_DETECTED,
            COMPLETION_CONTINUED_IN,
            COMPLETION_DEFERRED,
            COMPLETION_NOOP,
            COMPLETION_EMPTY,
        ] {
            let v = fx(text);
            is_str(&v, "/importId");
            is_str(&v, "/itemTypeResults/0/itemType");
            is_arr(&v, "/itemTypeResults/0/successes");
            is_arr(&v, "/itemTypeResults/0/failures");
        }
        // `initialize` reports the Codex home and user agent.
        let init = fx(INITIALIZE_RESULT);
        is_str(&init, "/codexHome");
        is_str(&init, "/userAgent");
        assert!(HandshakeInfo::from_initialize(&init).codex_home.is_some() || cfg!(windows));
        // Progress notifications: the method and an optional message.
        assert_eq!(
            EXTERNAL_AGENT_IMPORT_PROGRESS,
            "externalAgentConfig/import/progress"
        );
        is_str(&fx(PROGRESS_PARAMS), "/message");
    }

    // --- Completion judgement -------------------------------------------------

    fn failed(kind: FailureKind, message: &str) -> CompletionVerdict {
        CompletionVerdict::Failed {
            kind,
            message: message.to_string(),
        }
    }

    fn sessions(successes: Value, failures: Value) -> Value {
        json!({ "itemTypeResults": [{ "itemType": "SESSIONS", "successes": successes, "failures": failures }] })
    }

    fn failure(stage: &str, error_type: &str, sub: &str, message: Option<&str>) -> Value {
        let mut f = json!({ "failureStage": stage, "errorType": error_type, "subErrorType": sub });
        if let Some(m) = message {
            f["message"] = json!(m);
        }
        f
    }

    #[test]
    fn judge_completion_table() {
        use CompletionVerdict::{LedgerFallback, Thread};
        use FailureKind::*;
        let rows: Vec<(&str, Value, CompletionVerdict)> = vec![
            ("success", fx(COMPLETION_SUCCESS), Thread("thread-a".into())),
            (
                "last non-empty success wins",
                sessions(
                    json!([{ "target": "thread-1" }, { "target": "thread-2" }, { "target": "" }]),
                    json!([]),
                ),
                Thread("thread-2".into()),
            ),
            (
                "non-string target",
                sessions(json!([{ "target": 7 }]), json!([])),
                LedgerFallback { noop: false },
            ),
            (
                "session_not_detected in errorType",
                fx(COMPLETION_SESSION_NOT_DETECTED),
                failed(SessionNotDetected, "m"),
            ),
            (
                "continuedIn in subErrorType",
                sessions(
                    json!([]),
                    json!([failure("import", "skipped", "continuedIn", Some("c"))]),
                ),
                failed(ContinuedIn, "c"),
            ),
            (
                "kebab continued-in",
                fx(COMPLETION_CONTINUED_IN),
                failed(ContinuedIn, "continued elsewhere"),
            ),
            (
                "thread_open",
                fx(COMPLETION_DEFERRED),
                failed(Deferred, "the thread is open"),
            ),
            (
                "marker only in the message",
                sessions(
                    json!([]),
                    json!([failure(
                        "import",
                        "skipped",
                        "",
                        Some("session was continued-in elsewhere")
                    )]),
                ),
                failed(ContinuedIn, "session was continued-in elsewhere"),
            ),
            (
                "priority across failures",
                sessions(
                    json!([]),
                    json!([
                        failure("import", "io", "", Some("first")),
                        failure("detect", "session_not_detected", "", Some("second"))
                    ]),
                ),
                failed(SessionNotDetected, "second"),
            ),
            (
                "unknown failure with a message",
                sessions(json!([]), json!([failure("import", "io", "", Some("m"))])),
                failed(Other, "m"),
            ),
            (
                "unknown failure without a message",
                sessions(json!([]), json!([failure("import", "io", "", None)])),
                failed(Other, "import/io"),
            ),
            (
                "no-op marker",
                fx(COMPLETION_NOOP),
                LedgerFallback { noop: true },
            ),
            (
                "empty item",
                fx(COMPLETION_EMPTY),
                LedgerFallback { noop: false },
            ),
            (
                "no itemTypeResults",
                json!({ "importId": "imp-1" }),
                LedgerFallback { noop: false },
            ),
            (
                "only the SESSIONS item counts",
                json!({ "itemTypeResults": [
                    { "itemType": "PLUGINS", "successes": [], "failures": [] },
                    { "itemType": "SESSIONS", "successes": [{ "target": "thread-a" }], "failures": [] }
                ]}),
                Thread("thread-a".into()),
            ),
            (
                "markers in a different item are ignored",
                json!({ "itemTypeResults": [
                    { "itemType": "SESSIONS", "successes": [], "failures": [] },
                    { "itemType": "PLUGINS", "status": "continued_in deferred noop" }
                ]}),
                LedgerFallback { noop: false },
            ),
            (
                "two items, none SESSIONS",
                json!({ "itemTypeResults": [
                    { "itemType": "A", "successes": [{ "target": "x" }] },
                    { "itemType": "B" }
                ]}),
                LedgerFallback { noop: false },
            ),
            (
                "continued-in marker without failures",
                json!({ "itemTypeResults": [
                    { "itemType": "SESSIONS", "successes": [], "failures": [], "note": "continuedIn" }
                ]}),
                failed(ContinuedIn, "import failed"),
            ),
        ];
        for (name, params, want) in rows {
            assert_eq!(judge_completion(&params), want, "row: {name}");
        }
    }

    #[test]
    fn normalize_handles_kebab_camel_and_screaming_case() {
        for s in [
            "continued-in",
            "continuedIn",
            "CONTINUED_IN",
            "Continued In",
        ] {
            assert_eq!(normalize(s), "continued_in", "{s}");
        }
        assert_eq!(normalize("sessionNotDetected"), "session_not_detected");
        assert_eq!(normalize("thread.is.open"), "thread_is_open");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn result_from_req_err_maps_every_row() {
        use TransferPhase::*;
        let tail = || "tail".to_string();
        let map = |e: ReqErr, p: TransferPhase, t: &str| {
            result_from_req_err(e, p, Some("0.1.0"), t.to_string())
        };
        // Cancelled, in any phase.
        for p in [Initialize, Import, Completion] {
            assert_eq!(map(ReqErr::Cancelled, p, "x"), ImportResult::Cancelled);
        }
        // Timed out names the phase.
        for p in [Initialize, Import, Completion] {
            assert_eq!(
                map(ReqErr::TimedOut, p, "tail"),
                ImportResult::Failed(TransferFailure::Timeout {
                    phase: p,
                    message: format!("timed out in {} after 150s", p.as_str()),
                    stderr_tail: tail(),
                })
            );
        }
        // -32601 on the import method: too old, with the upgrade message.
        assert_eq!(
            map(
                ReqErr::Rpc {
                    code: -32601,
                    message: "Method not found".into()
                },
                Import,
                "tail"
            ),
            ImportResult::Failed(TransferFailure::CodexTooOld {
                version: Some("0.1.0".into()),
                message: UPGRADE_MSG.into(),
                stderr_tail: tail(),
            })
        );
        // -32601 on initialize is a protocol error, not "too old".
        assert_eq!(
            map(
                ReqErr::Rpc {
                    code: -32601,
                    message: "Method not found".into()
                },
                Initialize,
                "tail"
            ),
            ImportResult::Failed(TransferFailure::ProtocolError {
                message: "codex app-server error -32601: Method not found".into(),
                stderr_tail: tail(),
            })
        );
        // Any other RPC error keeps its code and message.
        let other = map(
            ReqErr::Rpc {
                code: -32000,
                message: "boom".into(),
            },
            Import,
            "tail",
        );
        assert_eq!(
            other,
            ImportResult::Failed(TransferFailure::ProtocolError {
                message: "codex app-server error -32000: boom".into(),
                stderr_tail: tail(),
            })
        );
        // Transport errors carry their own text.
        assert_eq!(
            map(ReqErr::Transport("closed".into()), Import, "tail"),
            ImportResult::Failed(TransferFailure::ProtocolError {
                message: "closed".into(),
                stderr_tail: tail(),
            })
        );
        // Closed: the not-found wording means codex never started.
        for t in [
            "'codex' is not recognized as an internal or external command,",
            "sh: 1: codex: command not found",
            "exec: codex: No such file or directory",
            "The system cannot find the file specified.",
        ] {
            assert_eq!(
                map(ReqErr::Closed, Initialize, t),
                ImportResult::Failed(TransferFailure::CodexNotFound {
                    message: "codex could not be started".into(),
                    stderr_tail: t.into(),
                }),
                "{t}"
            );
            // A write into the dead child reads the same way.
            assert!(matches!(
                map(ReqErr::Transport("broken pipe".into()), Initialize, t),
                ImportResult::Failed(TransferFailure::CodexNotFound { .. })
            ));
        }
        assert_eq!(
            map(ReqErr::Closed, Completion, "panic: unrelated"),
            ImportResult::Failed(TransferFailure::ProtocolError {
                message: "codex app-server exited during completion".into(),
                stderr_tail: "panic: unrelated".into(),
            })
        );
    }

    #[test]
    fn persist_plan_maps_each_import_result() {
        let imported = ImportResult::Imported {
            thread_id: "thread-a".into(),
        };
        assert_eq!(
            persist_plan(&imported, Some("sess-1".into())),
            Ok(Some(PersistPlan {
                cli: CliKind::Codex,
                codex_thread_id: FieldIntent::Set("thread-a".into()),
                claude_session_id: FieldIntent::Set("sess-1".into()),
            }))
        );
        let failed = ImportResult::Failed(TransferFailure::Other {
            message: "m".into(),
            stderr_tail: String::new(),
        });
        assert_eq!(
            persist_plan(&failed, None),
            Ok(Some(PersistPlan {
                cli: CliKind::Codex,
                codex_thread_id: FieldIntent::Clear,
                claude_session_id: FieldIntent::Keep,
            }))
        );
        assert_eq!(
            persist_plan(&ImportResult::Cancelled, Some("sess-1".into())),
            Ok(None)
        );
    }

    #[test]
    fn persist_plan_codex_not_found_keeps_claude() {
        let not_found = TransferFailure::CodexNotFound {
            message: "codex could not be started".into(),
            stderr_tail: String::new(),
        };
        assert_eq!(
            persist_plan(
                &ImportResult::Failed(not_found.clone()),
                Some("sess-1".into())
            ),
            Err(not_found.user_copy()),
            "no cli=codex is persisted"
        );
        assert_eq!(
            outcome_trail(&ImportResult::Failed(not_found))["outcome"],
            json!("kept_claude")
        );
        // Every other failure still switches to a fresh Codex.
        for failure in [
            TransferFailure::SessionNotDetected {
                message: String::new(),
                stderr_tail: String::new(),
            },
            TransferFailure::ProtocolError {
                message: String::new(),
                stderr_tail: String::new(),
            },
            TransferFailure::CodexTooOld {
                version: None,
                message: String::new(),
                stderr_tail: String::new(),
            },
        ] {
            let plan = persist_plan(&ImportResult::Failed(failure.clone()), None);
            assert!(
                matches!(&plan, Ok(Some(p)) if p.cli == CliKind::Codex && p.codex_thread_id == FieldIntent::Clear),
                "{failure:?} -> {plan:?}"
            );
        }
    }

    // --- JSONL selection ------------------------------------------------------

    fn touch(path: &Path, secs: u64) {
        std::fs::write(path, "{}\n").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn pick_jsonl_picks_the_newest_by_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("old.jsonl"), 1_000_000);
        touch(&tmp.path().join("new.jsonl"), 3_000_000);
        touch(&tmp.path().join("mid.jsonl"), 2_000_000);
        assert_eq!(pick_jsonl(tmp.path()), Some(tmp.path().join("new.jsonl")));
    }

    #[test]
    fn pick_jsonl_skips_agent_prefixed_files() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("sess-1.jsonl"), 1_000_000);
        touch(&tmp.path().join("agent-1234.jsonl"), 9_000_000);
        assert_eq!(
            pick_jsonl(tmp.path()),
            Some(tmp.path().join("sess-1.jsonl"))
        );
    }

    #[test]
    fn pick_jsonl_returns_none_when_only_agent_files_exist() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("agent-1.jsonl"), 1_000_000);
        touch(&tmp.path().join("agent-2.jsonl"), 2_000_000);
        assert_eq!(pick_jsonl(tmp.path()), None);
        assert_eq!(pick_jsonl(&tmp.path().join("missing")), None);
    }

    #[test]
    fn validate_jsonl_in_dir_accepts_only_a_transcript_directly_inside() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("projects").join("enc");
        let other = tmp.path().join("projects").join("other");
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let ok = dir.join("sess-1.jsonl");
        std::fs::write(&ok, "{}\n").unwrap();
        assert_eq!(validate_jsonl_in_dir(&dir, &ok), Ok(ok.clone()));

        let elsewhere = other.join("sess-2.jsonl");
        std::fs::write(&elsewhere, "{}\n").unwrap();
        let nested = dir.join("nested").join("sess-3.jsonl");
        std::fs::write(&nested, "{}\n").unwrap();
        let not_jsonl = dir.join("notes.txt");
        std::fs::write(&not_jsonl, "x").unwrap();
        let a_dir = dir.join("dir.jsonl");
        std::fs::create_dir(&a_dir).unwrap();
        let dotted = dir.join("..").join("other").join("sess-2.jsonl");
        for bad in [
            elsewhere,
            nested,
            not_jsonl,
            a_dir,
            dotted,
            dir.join("missing.jsonl"),
        ] {
            assert!(
                validate_jsonl_in_dir(&dir, &bad).is_err(),
                "{} must be refused",
                bad.display()
            );
        }
    }

    #[test]
    fn pick_jsonl_ignores_non_jsonl_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("sess-1.jsonl"), 1_000_000);
        // Both newer than the transcript (created now).
        std::fs::write(tmp.path().join("notes.txt"), "x").unwrap();
        std::fs::create_dir(tmp.path().join("dir.jsonl")).unwrap();
        assert_eq!(
            pick_jsonl(tmp.path()),
            Some(tmp.path().join("sess-1.jsonl"))
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(pick_jsonl(empty.path()), None);
    }

    #[test]
    fn selection_trail_records_persisted_vs_picked() {
        let picked = Path::new("/home/u/.claude/projects/enc/sess-1.jsonl");
        assert_eq!(
            selection_trail(picked, Some("sess-1")),
            json!({"selection":"newest","persisted_session_id_present":true,"persisted_differs":false})
        );
        assert_eq!(
            selection_trail(picked, Some("sess-0")),
            json!({"selection":"newest","persisted_session_id_present":true,"persisted_differs":true})
        );
        assert_eq!(
            selection_trail(picked, None),
            json!({"selection":"newest","persisted_session_id_present":false,"persisted_differs":false})
        );
        // The persisted id never influences the pick: an older file named after it
        // still loses to the newest one.
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("sess-0.jsonl"), 1_000_000);
        touch(&tmp.path().join("sess-1.jsonl"), 2_000_000);
        let pick = pick_jsonl(tmp.path()).unwrap();
        assert_eq!(pick, tmp.path().join("sess-1.jsonl"));
        assert_eq!(
            selection_trail(&pick, Some("sess-0"))["persisted_differs"],
            json!(true)
        );
    }

    #[test]
    fn jsonl_under_home_projects_accepts_a_file_under_home_claude_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude").join("projects").join("enc");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sess-1.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        assert!(jsonl_under_home_projects(&file, Some(tmp.path())));
    }

    #[test]
    fn jsonl_under_home_projects_rejects_a_file_under_another_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
        let other = tmp.path().join("cfg").join("projects").join("enc");
        std::fs::create_dir_all(&other).unwrap();
        let file = other.join("sess-1.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        assert!(!jsonl_under_home_projects(&file, Some(&home)));
    }

    #[test]
    fn jsonl_under_home_projects_rejects_when_home_is_none_or_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("sess-1.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        assert!(!jsonl_under_home_projects(&file, None));
        // No `.claude/projects` under this home at all.
        assert!(!jsonl_under_home_projects(&file, Some(tmp.path())));
        assert!(!jsonl_under_home_projects(
            &tmp.path().join("missing.jsonl"),
            Some(&tmp.path().join("nowhere"))
        ));
    }

    // --- Protocol-driven import over the fake ---------------------------------

    /// Records every progress and trail call the import makes.
    #[derive(Default)]
    struct Recorder {
        progress: Mutex<Vec<(ProgressPhase, Option<String>, Instant)>>,
        trail: Mutex<Vec<(TrailStage, Value)>>,
    }

    impl Recorder {
        fn phases(&self) -> Vec<ProgressPhase> {
            let mut out: Vec<ProgressPhase> = Vec::new();
            for (p, _, _) in self.progress.lock().unwrap().iter() {
                if out.last() != Some(p) {
                    out.push(*p);
                }
            }
            out
        }

        fn saw(&self, phase: ProgressPhase) -> bool {
            self.phases().contains(&phase)
        }

        fn at(&self, phase: ProgressPhase) -> Option<Instant> {
            self.progress
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _, _)| *p == phase)
                .map(|(_, _, t)| *t)
        }

        fn trail_has(&self, stage: TrailStage, key: &str, value: Value) -> bool {
            self.trail
                .lock()
                .unwrap()
                .iter()
                .any(|(s, d)| *s == stage && d.get(key) == Some(&value))
        }
    }

    /// A temp Codex home (A) and a transcript, plus the env pointing at them.
    struct ImportEnv {
        tmp: tempfile::TempDir,
        home: PathBuf,
        jsonl: PathBuf,
        env: CodexEnv,
    }

    fn import_env() -> ImportEnv {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-a");
        std::fs::create_dir_all(&home).unwrap();
        let dir = tmp.path().join("projects").join("enc");
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("sess-1.jsonl");
        std::fs::write(&jsonl, "{}\n").unwrap();
        let env = CodexEnv {
            exe: "codex".into(),
            exe_source: crate::codex_env::ExeSource::BareName,
            path: None,
            codex_home: home.clone(),
            codex_home_source: CodexHomeSource::Default,
            version: Some("0.149.0".into()),
        };
        ImportEnv {
            tmp,
            home,
            jsonl,
            env,
        }
    }

    impl ImportEnv {
        fn raw_path(&self) -> String {
            self.jsonl.to_string_lossy().into_owned()
        }

        /// Make `script` record `thread` for `source_path` in `home`'s ledger when
        /// the import request arrives (as Codex does), replacing the ledger.
        fn ledger_on_import(script: &mut Script, home: &Path, source_path: &str, thread: &str) {
            let (home, source_path, thread) = (
                home.to_path_buf(),
                source_path.to_string(),
                thread.to_string(),
            );
            script.on_import = Some(Box::new(move || {
                Self::write_ledger_in(&home, &source_path, &thread);
            }));
        }

        fn write_ledger_in(home: &Path, source_path: &str, thread: &str) {
            std::fs::write(
                home.join(LEDGER_FILE),
                ledger_text(json!([{
                    "source_path": source_path,
                    "content_sha256": "0000",
                    "imported_thread_id": thread
                }])),
            )
            .unwrap();
        }

        fn write_ledger(&self, thread: &str) {
            Self::write_ledger_in(&self.home, &self.raw_path(), thread);
        }
    }

    struct Opts {
        deadline: Duration,
        window: Duration,
        cancel_after: Option<Duration>,
    }

    impl Default for Opts {
        fn default() -> Self {
            Opts {
                deadline: Duration::from_secs(10),
                window: Duration::from_millis(500),
                cancel_after: None,
            }
        }
    }

    struct Run {
        result: ImportResult,
        handle: FakeHandle,
        elapsed: Duration,
        finished: Instant,
    }

    /// Drive `import_over` against `f`, then drop the server (so the handle has
    /// seen the teardown).
    fn import_with(f: FakeServer, ie: &ImportEnv, rec: &Recorder, o: Opts) -> Run {
        let (mut server, handle, cancel) = client_over(f, o.deadline);
        let setter = o.cancel_after.map(|d| {
            let flag = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(d);
                flag.store(true, Ordering::Release);
            })
        });
        let progress = |p: ProgressPhase, m: Option<String>| {
            rec.progress.lock().unwrap().push((p, m, Instant::now()))
        };
        let trail = |s: TrailStage, d: Value| rec.trail.lock().unwrap().push((s, d));
        let ctx = ImportCtx {
            cwd: "/home/u/wt",
            jsonl: &ie.jsonl,
            env: &ie.env,
            ledger_window: o.window,
            ledger_poll: Duration::from_millis(20),
            progress: &progress,
            trail: &trail,
        };
        let start = Instant::now();
        let result = import_over(&mut server, &ctx);
        let finished = Instant::now();
        drop(server);
        if let Some(s) = setter {
            s.join().unwrap();
        }
        Run {
            result,
            handle,
            elapsed: finished - start,
            finished,
        }
    }

    fn run_script(script: Script, ie: &ImportEnv, rec: &Recorder) -> Run {
        import_with(FakeServer::scripted(script), ie, rec, Opts::default())
    }

    /// The default script with the completion replaced by `completion`.
    fn script_with(completion: &str) -> Script {
        let mut s = Script::default_success("imp-1", "thread-a");
        s.after_import_response =
            vec![(EXTERNAL_AGENT_IMPORT_COMPLETED.to_string(), fx(completion))];
        s
    }

    fn imported(id: &str) -> ImportResult {
        ImportResult::Imported {
            thread_id: id.to_string(),
        }
    }

    fn methods(handle: &FakeHandle) -> Vec<String> {
        handle
            .received()
            .iter()
            .filter_map(|v| v.get("method").and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    #[test]
    fn import_over_success_with_target_returns_imported() {
        let ie = import_env();
        let rec = Recorder::default();
        let run = run_script(Script::default_success("imp-1", "thread-a"), &ie, &rec);
        assert_eq!(run.result, imported("thread-a"));
        assert_eq!(
            methods(&run.handle),
            ["initialize", "initialized", "externalAgentConfig/import"]
        );
        assert!(
            !ie.home.join(LEDGER_FILE).exists(),
            "the ledger is never needed"
        );
        assert!(!rec.saw(ProgressPhase::ReadingLedger));
        assert!(rec.trail_has(TrailStage::Classify, "verdict", json!("thread")));
        assert!(rec.trail_has(TrailStage::Import, "import_id_present", json!(true)));
        assert!(rec.trail_has(TrailStage::Import, "successes", json!(1)));
        assert_eq!(
            run.handle.teardown_grace(),
            Some(TEARDOWN_GRACE),
            "graceful"
        );
    }

    #[test]
    fn import_over_matches_completion_that_arrives_before_response() {
        let ie = import_env();
        let mut script = Script::default_success("imp-1", "thread-a");
        script.before_import_response = std::mem::take(&mut script.after_import_response);
        let run = run_script(script, &ie, &Recorder::default());
        assert_eq!(run.result, imported("thread-a"));
    }

    #[test]
    fn import_over_ignores_completion_for_a_different_import_id() {
        let ie = import_env();
        let mut script = Script::default_success("imp-1", "thread-a");
        script.after_import_response.insert(
            0,
            (
                EXTERNAL_AGENT_IMPORT_COMPLETED.to_string(),
                json!({
                    "importId": "imp-other",
                    "itemTypeResults": [{ "itemType": "SESSIONS", "successes": [{ "target": "thread-other" }], "failures": [] }]
                }),
            ),
        );
        let run = run_script(script, &ie, &Recorder::default());
        assert_eq!(run.result, imported("thread-a"));
    }

    #[test]
    fn import_over_session_not_detected_failure_is_classified() {
        let ie = import_env();
        ie.write_ledger("thread-stale");
        let rec = Recorder::default();
        let run = run_script(script_with(COMPLETION_SESSION_NOT_DETECTED), &ie, &rec);
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::SessionNotDetected { message, .. }) if message == "m"),
            "{:?}",
            run.result
        );
        assert!(
            !rec.saw(ProgressPhase::ReadingLedger),
            "the ledger is not read"
        );
        assert!(rec.trail_has(
            TrailStage::Classify,
            "verdict",
            json!("failed:session_not_detected")
        ));
    }

    #[test]
    fn import_over_continued_in_failure_is_skipped() {
        let ie = import_env();
        let run = run_script(
            script_with(COMPLETION_CONTINUED_IN),
            &ie,
            &Recorder::default(),
        );
        assert!(
            matches!(
                run.result,
                ImportResult::Failed(TransferFailure::Skipped {
                    reason: SkipReason::ContinuedIn,
                    ..
                })
            ),
            "{:?}",
            run.result
        );
    }

    #[test]
    fn import_over_deferred_failure_does_not_consult_the_ledger() {
        let ie = import_env();
        ie.write_ledger("thread-stale");
        let rec = Recorder::default();
        let run = run_script(script_with(COMPLETION_DEFERRED), &ie, &rec);
        assert!(
            matches!(
                run.result,
                ImportResult::Failed(TransferFailure::Skipped {
                    reason: SkipReason::Deferred,
                    ..
                })
            ),
            "{:?}",
            run.result
        );
        assert!(!rec.saw(ProgressPhase::ReadingLedger));
    }

    #[test]
    fn import_over_empty_successes_fall_back_to_ledger_by_path() {
        let ie = import_env();
        let mut script = script_with(COMPLETION_EMPTY);
        ImportEnv::ledger_on_import(&mut script, &ie.home, &ie.raw_path(), "thread-ledger");
        let rec = Recorder::default();
        let run = run_script(script, &ie, &rec);
        assert_eq!(run.result, imported("thread-ledger"));
        assert!(rec.saw(ProgressPhase::ReadingLedger));
        assert!(rec.trail_has(TrailStage::Classify, "verdict", json!("ledger_fallback")));
    }

    /// The ledger record an earlier import of this transcript left behind.
    fn old_ledger_record(ie: &ImportEnv) -> Value {
        json!({
            "source_path": ie.raw_path(),
            "content_sha256": "0000",
            "imported_thread_id": "thread-old",
            "imported_at": 1787500000
        })
    }

    #[test]
    fn import_over_stale_ledger_record_is_not_this_imports_thread() {
        let ie = import_env();
        std::fs::write(
            ie.home.join(LEDGER_FILE),
            ledger_text(json!([old_ledger_record(&ie)])),
        )
        .unwrap();
        let run = run_script(script_with(COMPLETION_EMPTY), &ie, &Recorder::default());
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::Other { message, .. }) if message.contains("without recording a thread")),
            "the pre-existing thread must not be reused: {:?}",
            run.result
        );
    }

    #[test]
    fn import_over_late_new_ledger_record_wins_over_the_old_one() {
        let ie = import_env();
        let ledger = ie.home.join(LEDGER_FILE);
        let old = old_ledger_record(&ie);
        std::fs::write(&ledger, ledger_text(json!([old.clone()]))).unwrap();
        let raw = ie.raw_path();
        let mut script = script_with(COMPLETION_EMPTY);
        script.on_import = Some(Box::new(move || {
            let (ledger, old, raw) = (ledger.clone(), old.clone(), raw.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let new = json!({
                    "source_path": raw,
                    "content_sha256": "1111",
                    "imported_thread_id": "thread-new",
                    "imported_at": 1787600000
                });
                std::fs::write(&ledger, ledger_text(json!([old, new]))).unwrap();
            });
        }));
        let run = import_with(
            FakeServer::scripted(script),
            &ie,
            &Recorder::default(),
            Opts {
                window: Duration::from_secs(1),
                ..Opts::default()
            },
        );
        assert_eq!(run.result, imported("thread-new"));
    }

    #[test]
    fn import_over_noop_accepts_the_pre_existing_ledger_record() {
        let ie = import_env();
        std::fs::write(
            ie.home.join(LEDGER_FILE),
            ledger_text(json!([old_ledger_record(&ie)])),
        )
        .unwrap();
        let run = run_script(script_with(COMPLETION_NOOP), &ie, &Recorder::default());
        assert_eq!(run.result, imported("thread-old"));
    }

    #[test]
    fn ledger_pick_prefers_records_written_after_the_snapshot() {
        let mark = |t: &str, sha: &str, at: &str| LedgerMark {
            thread_id: t.into(),
            sha: Some(sha.into()),
            imported_at: Some(at.into()),
        };
        let old = mark("thread-old", "0", "1");
        let snapshot = vec![old.clone()];
        // Nothing new: only an accepted no-op reuses the old record.
        assert_eq!(ledger_pick(&snapshot, &snapshot, false), None);
        assert_eq!(
            ledger_pick(&snapshot, &snapshot, true).as_deref(),
            Some("thread-old")
        );
        assert_eq!(ledger_pick(&[], &snapshot, true), None);
        // A new thread, a new hash or a new timestamp each mark a fresh record.
        for fresh in [
            mark("thread-new", "0", "1"),
            mark("thread-old", "9", "1"),
            mark("thread-old", "0", "2"),
        ] {
            let current = vec![fresh.clone(), old.clone()];
            assert_eq!(
                ledger_pick(&current, &snapshot, false),
                Some(fresh.thread_id.clone()),
                "{fresh:?}"
            );
        }
        // No snapshot (no ledger before the import): the last record wins.
        let current = vec![old.clone(), mark("thread-b", "1", "3")];
        assert_eq!(
            ledger_pick(&current, &[], false).as_deref(),
            Some("thread-b")
        );
    }

    #[test]
    fn import_over_noop_marker_without_ledger_record_is_skipped_noop() {
        let ie = import_env();
        let rec = Recorder::default();
        let run = run_script(script_with(COMPLETION_NOOP), &ie, &rec);
        assert!(
            matches!(
                run.result,
                ImportResult::Failed(TransferFailure::Skipped {
                    reason: SkipReason::NoOp,
                    ..
                })
            ),
            "{:?}",
            run.result
        );
        assert!(rec.trail_has(TrailStage::Classify, "noop", json!(true)));
    }

    #[test]
    fn import_over_empty_completion_without_ledger_record_is_other() {
        let ie = import_env();
        let run = run_script(script_with(COMPLETION_EMPTY), &ie, &Recorder::default());
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::Other { message, .. }) if message.contains("without recording a thread")),
            "{:?}",
            run.result
        );
    }

    #[test]
    fn import_over_legacy_response_without_import_id_uses_ledger() {
        for completion in [json!({}), json!({ "importId": "imp-unrelated" })] {
            let ie = import_env();
            // Recorded under the canonical form while we sent the raw path.
            let canonical = std::fs::canonicalize(&ie.jsonl)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mut script = Script::default_success("imp-1", "thread-a");
            ImportEnv::ledger_on_import(&mut script, &ie.home, &canonical, "thread-legacy");
            script.import = Reply::Result(json!({}));
            script.after_import_response = vec![(
                EXTERNAL_AGENT_IMPORT_COMPLETED.to_string(),
                completion.clone(),
            )];
            let rec = Recorder::default();
            let run = run_script(script, &ie, &rec);
            assert_eq!(
                run.result,
                imported("thread-legacy"),
                "completion {completion}"
            );
            assert!(rec.trail_has(TrailStage::Import, "import_id_present", json!(false)));
        }
    }

    #[test]
    fn import_over_late_ledger_flush_is_found() {
        let ie = import_env();
        let home = ie.home.clone();
        let raw = ie.raw_path();
        let mut script = script_with(COMPLETION_EMPTY);
        script.on_import = Some(Box::new(move || {
            let (home, raw) = (home.clone(), raw.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                ImportEnv::write_ledger_in(&home, &raw, "thread-late");
            });
        }));
        let rec = Recorder::default();
        let run = import_with(
            FakeServer::scripted(script),
            &ie,
            &rec,
            Opts {
                window: Duration::from_secs(1),
                ..Opts::default()
            },
        );
        assert_eq!(run.result, imported("thread-late"));
        let lookup = run.finished - rec.at(ProgressPhase::ReadingLedger).unwrap();
        assert!(
            lookup >= Duration::from_millis(100),
            "lookup took {lookup:?}"
        );
    }

    fn assert_import_times_out_in(script: Script, phase: TransferPhase) {
        let ie = import_env();
        let run = import_with(
            FakeServer::scripted(script),
            &ie,
            &Recorder::default(),
            Opts {
                deadline: Duration::from_millis(300),
                ..Opts::default()
            },
        );
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::Timeout { phase: p, .. }) if *p == phase),
            "expected a timeout in {phase:?}, got {:?}",
            run.result
        );
        assert!(
            run.elapsed < Duration::from_millis(1500),
            "took {:?}",
            run.elapsed
        );
        assert_eq!(
            run.handle.teardown_grace(),
            Some(Duration::ZERO),
            "a timeout kills immediately"
        );
    }

    #[test]
    fn import_over_deadline_in_initialize_is_timeout_initialize() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.initialize = Reply::Silent;
        assert_import_times_out_in(script, TransferPhase::Initialize);
    }

    #[test]
    fn import_over_deadline_in_import_is_timeout_import() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.import = Reply::Silent;
        assert_import_times_out_in(script, TransferPhase::Import);
    }

    #[test]
    fn import_over_deadline_in_completion_is_timeout_completion() {
        let mut script = Script::default_success("imp-1", "thread-a");
        script.after_import_response.clear();
        assert_import_times_out_in(script, TransferPhase::Completion);
    }

    #[test]
    fn import_over_stderr_written_while_alive_is_in_the_failure_tail() {
        let ie = import_env();
        let f = FakeServer::new(|msg, out| {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    out.stderr("boom: bad auth");
                    out.respond(&id, json!({}));
                }
                Some("externalAgentConfig/import") => {
                    // Let the stderr pump catch up before the failure is judged.
                    std::thread::sleep(Duration::from_millis(100));
                    out.respond_error(&id, -32000, "denied");
                }
                _ => {}
            }
        });
        let run = import_with(f, &ie, &Recorder::default(), Opts::default());
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::ProtocolError { stderr_tail, .. }) if stderr_tail.contains("boom: bad auth")),
            "{:?}",
            run.result
        );
    }

    #[test]
    fn import_over_string_id_server_request_gets_a_32601_reply() {
        let ie = import_env();
        let f = FakeServer::new(|msg, out| {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => out.respond(&id, json!({})),
                Some("externalAgentConfig/import") => {
                    out.server_request(json!("srv-1"), "item/approval");
                    out.respond(&id, fx(IMPORT_RESPONSE));
                    out.notify(EXTERNAL_AGENT_IMPORT_COMPLETED, fx(COMPLETION_SUCCESS));
                }
                _ => {}
            }
        });
        let run = import_with(f, &ie, &Recorder::default(), Opts::default());
        assert_eq!(run.result, imported("thread-a"));
        let until = Instant::now() + Duration::from_secs(2);
        let reply = loop {
            let found = run
                .handle
                .received()
                .into_iter()
                .find(|v| v.get("id") == Some(&json!("srv-1")));
            match found {
                Some(v) => break v,
                None if Instant::now() < until => std::thread::sleep(Duration::from_millis(5)),
                None => panic!("no reply to the string-id server request"),
            }
        };
        assert_eq!(reply["error"]["code"], json!(-32601));
    }

    #[test]
    fn import_over_appended_jsonl_mid_import_still_imports() {
        let ie = import_env();
        let jsonl = ie.jsonl.clone();
        let mut script = Script::default_success("imp-1", "thread-a");
        script.on_import = Some(Box::new(move || {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&jsonl)
                .unwrap();
            writeln!(
                f,
                r#"{{"type":"user","message":{{"content":"still typing"}}}}"#
            )
            .unwrap();
        }));
        let run = run_script(script, &ie, &Recorder::default());
        assert_eq!(run.result, imported("thread-a"));
        assert!(std::fs::read_to_string(&ie.jsonl)
            .unwrap()
            .contains("still typing"));
    }

    #[test]
    fn import_over_cancel_while_waiting_returns_cancelled_and_tears_down() {
        let ie = import_env();
        let mut script = Script::default_success("imp-1", "thread-a");
        script.after_import_response.clear();
        let rec = Recorder::default();
        let run = import_with(
            FakeServer::scripted(script),
            &ie,
            &rec,
            Opts {
                cancel_after: Some(Duration::from_millis(100)),
                ..Opts::default()
            },
        );
        assert_eq!(run.result, ImportResult::Cancelled);
        assert!(
            run.elapsed < Duration::from_secs(1),
            "took {:?}",
            run.elapsed
        );
        assert!(rec.saw(ProgressPhase::Cancelling));
        assert_eq!(run.handle.teardown_grace(), Some(Duration::ZERO));
    }

    #[test]
    fn import_over_cancel_during_ledger_poll_returns_cancelled() {
        let ie = import_env();
        let rec = Recorder::default();
        let run = import_with(
            FakeServer::scripted(script_with(COMPLETION_EMPTY)),
            &ie,
            &rec,
            Opts {
                window: Duration::from_secs(10),
                cancel_after: Some(Duration::from_millis(100)),
                ..Opts::default()
            },
        );
        assert_eq!(run.result, ImportResult::Cancelled);
        assert!(
            run.elapsed < Duration::from_millis(1500),
            "took {:?}",
            run.elapsed
        );
        assert!(rec.saw(ProgressPhase::ReadingLedger));
        assert!(rec.saw(ProgressPhase::Cancelling));
        assert_eq!(run.handle.teardown_grace(), Some(Duration::ZERO));
    }

    #[test]
    fn import_over_import_method_not_found_is_codex_too_old() {
        let ie = import_env();
        ie.write_ledger("thread-stale");
        let mut script = Script::default_success("imp-1", "thread-a");
        script.import = Reply::Error(-32601, "Method not found");
        let rec = Recorder::default();
        let run = run_script(script, &ie, &rec);
        assert_eq!(
            run.result,
            ImportResult::Failed(TransferFailure::CodexTooOld {
                version: Some("0.149.0".into()),
                message: UPGRADE_MSG.into(),
                stderr_tail: String::new(),
            })
        );
        assert!(!rec.saw(ProgressPhase::ReadingLedger));
    }

    #[test]
    fn import_over_other_rpc_error_is_protocol_error() {
        let ie = import_env();
        let mut script = Script::default_success("imp-1", "thread-a");
        script.import = Reply::Error(-32000, "boom");
        let run = run_script(script, &ie, &Recorder::default());
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::ProtocolError { message, .. }) if message.contains("-32000") && message.contains("boom")),
            "{:?}",
            run.result
        );
    }

    #[test]
    fn import_over_stdout_eof_with_not_recognized_stderr_is_codex_not_found() {
        let exits_with = |text: &'static str| {
            FakeServer::new(move |msg, out| {
                if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                    out.stderr(text);
                    out.close();
                }
            })
        };
        let ie = import_env();
        let text = "'codex' is not recognized as an internal or external command,";
        let run = import_with(exits_with(text), &ie, &Recorder::default(), Opts::default());
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::CodexNotFound { stderr_tail, .. }) if stderr_tail.contains(text)),
            "{:?}",
            run.result
        );
        let run = import_with(
            exits_with("panic: something else"),
            &ie,
            &Recorder::default(),
            Opts::default(),
        );
        assert!(
            matches!(&run.result, ImportResult::Failed(TransferFailure::ProtocolError { stderr_tail, .. }) if stderr_tail.contains("something else")),
            "{:?}",
            run.result
        );
    }

    #[test]
    fn import_over_initialize_codex_home_is_used_for_the_ledger() {
        // An absolute `codexHome` from `initialize` redirects the ledger lookup.
        let ie = import_env();
        let home_b = ie.tmp.path().join("codex-b");
        std::fs::create_dir_all(&home_b).unwrap();
        let mut script = script_with(COMPLETION_EMPTY);
        ImportEnv::ledger_on_import(&mut script, &home_b, &ie.raw_path(), "thread-b");
        script.initialize =
            Reply::Result(json!({ "codexHome": home_b, "userAgent": "codex-fake" }));
        let rec = Recorder::default();
        let run = run_script(script, &ie, &rec);
        assert_eq!(run.result, imported("thread-b"));
        assert!(rec.trail_has(TrailStage::Import, "codex_home_from", json!("initialize")));
        assert!(rec.trail_has(TrailStage::Import, "user_agent_present", json!(true)));

        // A relative one is ignored: the pre-spawn home is used.
        let ie = import_env();
        let mut script = script_with(COMPLETION_EMPTY);
        ImportEnv::ledger_on_import(&mut script, &ie.home, &ie.raw_path(), "thread-a-home");
        script.initialize = Reply::Result(json!({ "codexHome": "relative/home" }));
        let rec = Recorder::default();
        let run = run_script(script, &ie, &rec);
        assert_eq!(run.result, imported("thread-a-home"));
        assert!(rec.trail_has(TrailStage::Import, "codex_home_from", json!("default")));
        assert!(rec.trail_has(TrailStage::Import, "user_agent_present", json!(false)));
    }

    #[test]
    fn import_over_forwards_progress_notifications() {
        let ie = import_env();
        let mut script = Script::default_success("imp-1", "thread-a");
        script.after_import_response.insert(
            0,
            (
                EXTERNAL_AGENT_IMPORT_PROGRESS.to_string(),
                fx(PROGRESS_PARAMS),
            ),
        );
        let rec = Recorder::default();
        let run = run_script(script, &ie, &rec);
        assert_eq!(run.result, imported("thread-a"));
        assert_eq!(
            rec.phases(),
            [
                ProgressPhase::Initializing,
                ProgressPhase::Importing,
                ProgressPhase::AwaitingCompletion
            ]
        );
        assert!(rec
            .progress
            .lock()
            .unwrap()
            .iter()
            .any(|(p, m, _)| *p == ProgressPhase::AwaitingCompletion
                && m.as_deref() == Some("step 1")));
    }

    // --- Real spawn of a fake `codex` script ----------------------------------
    //
    // These spawn only a script the test wrote into a temp dir, never Codex, and
    // kill only by the pids they spawned.

    /// A `CodexEnv` for the fake script in `dir`: an explicit `CODEX_HOME` and the
    /// temp dir prepended to `PATH`.
    fn smoke_env(dir: &Path, exe: &Path) -> CodexEnv {
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let base = std::env::var("PATH").unwrap_or_default();
        CodexEnv {
            exe: exe.to_string_lossy().into_owned(),
            exe_source: crate::codex_env::ExeSource::Configured,
            path: Some(format!("{}{sep}{base}", dir.display())),
            codex_home: home,
            codex_home_source: CodexHomeSource::EnvVar,
            version: None,
        }
    }

    /// The three protocol lines the fake script answers with.
    fn smoke_lines(home: &Path) -> (String, String, String) {
        let init =
            json!({ "id": 1, "result": { "codexHome": home, "userAgent": "codex-smoke/0.0" } });
        let resp = json!({ "id": 2, "result": { "importId": "imp-1" } });
        let done = json!({
            "method": EXTERNAL_AGENT_IMPORT_COMPLETED,
            "params": {
                "importId": "imp-1",
                "itemTypeResults": [{ "itemType": "SESSIONS", "successes": [{ "target": "thread-smoke" }], "failures": [] }]
            }
        });
        (init.to_string(), resp.to_string(), done.to_string())
    }

    /// Spawn the fake, import over it, and check the result, the child's
    /// environment and a quick teardown.
    fn assert_smoke_import(dir: &Path, env: &CodexEnv) {
        let jsonl = dir.join("sess-1.jsonl");
        std::fs::write(&jsonl, "{}\n").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let mut server = AppServer::spawn(
            env,
            &cwd,
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_secs(60),
        )
        .expect("spawn the fake codex");
        let noop_progress = |_: ProgressPhase, _: Option<String>| {};
        let noop_trail = |_: TrailStage, _: Value| {};
        let ctx = ImportCtx {
            cwd: &cwd,
            jsonl: &jsonl,
            env,
            ledger_window: Duration::from_millis(100),
            ledger_poll: Duration::from_millis(20),
            progress: &noop_progress,
            trail: &noop_trail,
        };
        let result = import_over(&mut server, &ctx);
        assert_eq!(
            result,
            imported("thread-smoke"),
            "tail: {}",
            server.stderr_tail()
        );
        let want_home = format!("CODEX_HOME={}", env.codex_home.display());
        let want_path = format!("PATH={}", dir.display());
        let until = Instant::now() + Duration::from_secs(5);
        while !(server.stderr_tail().contains(&want_home)
            && server.stderr_tail().contains(&want_path))
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let tail = server.stderr_tail();
        assert!(tail.contains(&want_home), "tail: {tail}");
        assert!(tail.contains(&want_path), "tail: {tail}");
        let start = Instant::now();
        drop(server);
        assert!(
            start.elapsed() < TEARDOWN_GRACE,
            "the fake exits on stdin EOF; teardown took {:?}",
            start.elapsed()
        );
    }

    #[cfg(windows)]
    #[test]
    fn real_spawn_smoke_windows() {
        let has_powershell = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !has_powershell {
            eprintln!("skipping real_spawn_smoke_windows: powershell is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let dir = PathBuf::from(keitora_tasks::strip_extended_prefix_str(
            &dir.to_string_lossy(),
        ));
        let cmd_path = dir.join("fake_codex.cmd");
        let env = smoke_env(&dir, &cmd_path);
        let (init, resp, done) = smoke_lines(&env.codex_home);
        std::fs::write(
            &cmd_path,
            "@echo off\r\npowershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0fake_codex.ps1\" %*\r\n",
        )
        .unwrap();
        let ps1 = r#"$out = [Console]::OpenStandardOutput()
$err = [Console]::OpenStandardError()
function Emit($stream, [string]$text) {
  $bytes = [System.Text.Encoding]::UTF8.GetBytes($text + "`n")
  $stream.Write($bytes, 0, $bytes.Length)
  $stream.Flush()
}
Emit $err ("env CODEX_HOME=" + $env:CODEX_HOME)
Emit $err ("env PATH=" + $env:PATH)
while ($true) {
  $line = [Console]::In.ReadLine()
  if ($null -eq $line) { break }
  if ($line.Contains('"method":"initialize"')) { Emit $out '<INIT>' }
  elseif ($line.Contains('"method":"externalAgentConfig/import"')) { Emit $out '<RESP>'; Emit $out '<DONE>' }
}
"#
        .replace("<INIT>", &init)
        .replace("<RESP>", &resp)
        .replace("<DONE>", &done);
        std::fs::write(dir.join("fake_codex.ps1"), ps1).unwrap();
        assert_smoke_import(&dir, &env);
    }

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A `/bin/sh` fake that answers the protocol, then runs `after_eof`.
    #[cfg(unix)]
    fn protocol_script(home: &Path, prelude: &str, after_eof: &str) -> String {
        let (init, resp, done) = smoke_lines(home);
        format!(
            "#!/bin/sh\n{prelude}\nwhile IFS= read -r line; do\n  case \"$line\" in\n    *'\"method\":\"initialize\"'*) printf '%s\\n' '{init}' ;;\n    *'\"method\":\"externalAgentConfig/import\"'*) printf '%s\\n' '{resp}' '{done}' ;;\n  esac\ndone\n{after_eof}\n"
        )
    }

    /// Read a pid the script wrote to `path`, waiting up to 5 s for it.
    #[cfg(unix)]
    fn read_pid(path: &Path) -> u32 {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
            {
                return pid;
            }
            assert!(Instant::now() < until, "no pid in {}", path.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait up to `within` for `pid` to be gone.
    #[cfg(unix)]
    fn gone_within(pid: u32, within: Duration) -> bool {
        let until = Instant::now() + within;
        while crate::backend::platform::process_is_alive(pid) {
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    #[cfg(unix)]
    #[test]
    fn real_spawn_smoke_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let exe = dir.join("fake_codex");
        let env = smoke_env(&dir, &exe);
        write_script(
            &exe,
            &protocol_script(
                &env.codex_home,
                "echo \"env CODEX_HOME=$CODEX_HOME\" >&2\necho \"env PATH=$PATH\" >&2",
                "exit 0",
            ),
        );
        assert_smoke_import(&dir, &env);
    }

    #[cfg(unix)]
    #[test]
    fn real_spawn_normal_finish_returns_before_teardown_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let exe = dir.join("fake_codex");
        let env = smoke_env(&dir, &exe);
        let pid_file = dir.join("pid");
        // Answers the protocol, then ignores stdin EOF.
        write_script(
            &exe,
            &protocol_script(
                &env.codex_home,
                &format!("echo $$ > '{}'", pid_file.display()),
                "sleep 60",
            ),
        );
        let home = dir.join("user-home");
        let projects = home.join(".claude").join("projects").join("enc");
        std::fs::create_dir_all(&projects).unwrap();
        let jsonl = projects.join("sess-1.jsonl");
        std::fs::write(&jsonl, "{}\n").unwrap();
        let start = Instant::now();
        let result = run_claude_import_with(
            &env,
            &dir.to_string_lossy(),
            &jsonl,
            Arc::new(AtomicBool::new(false)),
            &|_, _| {},
            &|_, _| {},
            ImportKnobs {
                home: Some(home),
                deadline: Instant::now() + Duration::from_secs(30),
                ledger_window: Duration::from_millis(100),
                ledger_poll: Duration::from_millis(20),
            },
        );
        let elapsed = start.elapsed();
        assert_eq!(result, imported("thread-smoke"));
        assert!(
            elapsed < Duration::from_secs(2),
            "the grace period runs after the result; took {elapsed:?}"
        );
        let pid = read_pid(&pid_file);
        assert!(
            gone_within(pid, Duration::from_secs(8)),
            "the detached teardown kills the child after the grace period"
        );
    }

    #[cfg(unix)]
    #[test]
    fn real_spawn_teardown_kills_a_hung_child_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let exe = dir.join("fake_codex");
        let env = smoke_env(&dir, &exe);
        let pid_file = dir.join("pid");
        let child_file = dir.join("grandchild");
        write_script(
            &exe,
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nsleep 60 &\necho $! > '{}'\nsleep 60\n",
                pid_file.display(),
                child_file.display()
            ),
        );
        let server = AppServer::spawn(
            &env,
            &dir.to_string_lossy(),
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_secs(30),
        )
        .expect("spawn the fake codex");
        let pid = read_pid(&pid_file);
        let grandchild = read_pid(&child_file);
        let mut server = server;
        server.abort();
        let start = Instant::now();
        drop(server);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "took {:?}",
            start.elapsed()
        );
        assert!(
            gone_within(pid, Duration::from_secs(2)),
            "child {pid} survived"
        );
        assert!(
            gone_within(grandchild, Duration::from_secs(2)),
            "grandchild {grandchild} survived the group kill"
        );
    }

    // --- First-user-line extraction -----------------------------------------

    #[test]
    fn first_user_line_skips_meta_and_reads_string_or_blocks() {
        let jsonl = concat!(
            r#"{"type":"user","isMeta":true,"message":{"content":"<synthetic caveat>"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"the real first thing I typed"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second"}]}}"#,
        );
        assert_eq!(first_user_line(jsonl), "the real first thing I typed");
    }

    #[test]
    fn first_user_line_reads_content_blocks_when_first_real_user_is_array() {
        let jsonl =
            r#"{"type":"user","message":{"content":[{"type":"text","text":"block text"}]}}"#;
        assert_eq!(first_user_line(jsonl), "block text");
    }

    #[test]
    fn session_id_from_jsonl_extracts_uuid_stem() {
        let p = Path::new("/home/me/.claude/projects/enc/1b4f-abc-uuid.jsonl");
        assert_eq!(session_id_from_jsonl(p).as_deref(), Some("1b4f-abc-uuid"));
    }

    #[test]
    fn session_id_from_jsonl_none_when_no_file_name() {
        assert_eq!(session_id_from_jsonl(Path::new("/")), None);
    }

    #[test]
    fn session_id_from_jsonl_rejects_unsafe_stem() {
        assert_eq!(session_id_from_jsonl(Path::new("session id.jsonl")), None);
        assert_eq!(session_id_from_jsonl(Path::new("--continue.jsonl")), None);
        assert_eq!(
            session_id_from_jsonl(Path::new("session;whoami.jsonl")),
            None
        );
    }

    #[test]
    fn truncate_chars_appends_ellipsis() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
    }

    // --- Handoff-summary flattening -------------------------------------------

    #[test]
    fn format_handoff_prompt_is_a_single_line() {
        let summary = HandoffSummary {
            goal: "Ship the transfer menu".to_string(),
            key_decisions: vec![
                "Use write_line".to_string(),
                "No new dialog lib".to_string(),
            ],
            next_steps: vec!["Wire tsc check".to_string()],
            open_files: vec!["Rail.tsx".to_string(), "App.tsx".to_string()],
            notes: String::new(),
        };
        let prompt = format_handoff_prompt(&summary);
        assert!(!prompt.contains('\n'));
        assert!(!prompt.contains('\r'));
        assert!(prompt.contains("Ship the transfer menu"));
        assert!(prompt.contains("Use write_line; No new dialog lib"));
        assert!(prompt.contains("Rail.tsx; App.tsx"));
        // Empty notes are omitted entirely, not rendered as "Notes: .".
        assert!(!prompt.contains("Notes:"));
    }

    #[test]
    fn format_handoff_prompt_scrubs_embedded_newlines_and_includes_notes() {
        let summary = HandoffSummary {
            goal: "Line one\nLine two".to_string(),
            key_decisions: vec![],
            next_steps: vec![],
            open_files: vec![],
            notes: "Careful:\r\nedge case".to_string(),
        };
        let prompt = format_handoff_prompt(&summary);
        assert!(!prompt.contains('\n'));
        assert!(!prompt.contains('\r'));
        assert!(prompt.contains("Line one Line two"));
        assert!(prompt.contains("Careful: edge case"));
        assert!(prompt.contains("Key decisions: none."));
    }

    // --- release_codex_session via FakeBackend --------------

    use crate::backend::fake::{Call, FakeBackend};
    use crate::backend::{PtyBackendError, SessionSummary};

    fn release_key() -> SessionKey {
        SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        }
    }

    fn fast_release(client: &dyn PtyBackend, key: &SessionKey) -> Result<(), String> {
        release_codex_session_with(
            client,
            key,
            Duration::from_millis(150),
            Duration::from_millis(10),
        )
    }

    fn kill_calls(fake: &FakeBackend) -> Vec<(SessionKey, u64)> {
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Kill { key, gen } => Some((key, gen)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn release_codex_session_no_session_is_ok_no_kill() {
        // Default FakeBackend serves an empty session list -> nothing live for the
        // key -> Ok with no kill attempted.
        let fake = FakeBackend::new();
        let key = release_key();
        assert!(fast_release(&fake, &key).is_ok());
        assert!(kill_calls(&fake).is_empty(), "no session -> no kill");
    }

    #[test]
    fn release_codex_session_waits_for_exit_and_kills_with_gen() {
        let fake = FakeBackend::new();
        let key = release_key();
        fake.push_list_sessions(Ok(vec![SessionSummary {
            key: key.clone(),
            gen: 42,
            state_hint: None,
        }]));
        fake.insert_live(key.clone());
        // is_live reads true once, then the key drops out of the live-set.
        fake.flip_live_after(2, key.clone());
        fake.push_kill(Ok(()));

        assert!(fast_release(&fake, &key).is_ok());
        // Kill used the generation `list_sessions` reported.
        assert_eq!(kill_calls(&fake), vec![(key, 42)]);
    }

    #[test]
    fn release_codex_session_times_out_when_never_exits() {
        let fake = FakeBackend::new();
        let key = release_key();
        fake.push_list_sessions(Ok(vec![SessionSummary {
            key: key.clone(),
            gen: 7,
            state_hint: None,
        }]));
        fake.insert_live(key.clone()); // never flips -> is_live stays true
        fake.push_kill(Ok(()));

        let err = fast_release(&fake, &key).unwrap_err();
        assert!(
            err.contains("timed out waiting"),
            "expected a release timeout, got: {err}"
        );
    }

    #[test]
    fn release_codex_session_errors_when_list_sessions_fails() {
        let fake = FakeBackend::new();
        let key = release_key();
        fake.push_list_sessions(Err(PtyBackendError::Other("boom".to_string())));

        let err = fast_release(&fake, &key).unwrap_err();
        assert!(
            err.contains("list daemon sessions before Codex handoff"),
            "expected a list-sessions error, got: {err}"
        );
        assert!(kill_calls(&fake).is_empty(), "no kill on a list failure");
    }

    // --- Project scoping, gate placement and the commit step ------------------

    /// A registry with `proj-a` (active) and `proj-b` under registry keys that
    /// differ from their resolved names (the dir basenames).
    fn two_projects(root: &Path) -> keitora_tasks::UserConfig {
        let a = root.join("proj-a");
        let b = root.join("proj-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut user = keitora_tasks::UserConfig::default();
        user.register("key-one", a);
        user.register("key-two", b);
        user.set_active("key-one");
        user
    }

    #[test]
    fn resolve_project_finds_non_active_project() {
        let tmp = tempfile::tempdir().unwrap();
        let user = two_projects(tmp.path());
        let config = resolve_project(&user, "proj-b").unwrap();
        assert_eq!(config.project.name, "proj-b");
        assert!(config.root.ends_with("proj-b"));
    }

    #[test]
    fn resolve_project_unknown_errors_without_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let user = two_projects(tmp.path());
        assert_eq!(
            resolve_project(&user, "nope").map(|c| c.project.name),
            Err("project `nope` not found".to_string())
        );
        // Registry keys are not names.
        assert!(resolve_project(&user, "key-two").is_err());
    }

    #[test]
    fn transfer_code_never_resolves_the_active_project() {
        let source = include_str!("transfer.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap()
            .split("#[cfg(test)]\r\nmod tests")
            .next()
            .unwrap();
        assert!(
            production.len() < source.len(),
            "test module marker not found"
        );
        assert!(!production.contains(concat!("resolve", "_active")));
    }

    #[test]
    fn persist_under_gate_holds_gate_only_during_persist() {
        let gate = Arc::new(DispatchGate::default());
        assert!(gate.0.try_lock().is_ok());
        let inner = Arc::clone(&gate);
        let held = tauri::async_runtime::block_on(persist_under_gate(&gate, move || {
            inner.0.try_lock().is_err()
        }))
        .unwrap();
        assert!(held, "the gate is held while persisting");
        assert!(gate.0.try_lock().is_ok(), "and released afterwards");
    }

    #[test]
    fn gate_is_free_during_the_long_phase() {
        let gate = DispatchGate::default();
        // The import (or summarize) runs as an ordinary blocking step outside
        // `persist_under_gate`: a concurrent dispatch never waits on it.
        let import = std::thread::spawn(|| std::thread::sleep(Duration::from_millis(200)));
        while !import.is_finished() {
            assert!(gate.0.try_lock().is_ok());
            std::thread::sleep(Duration::from_millis(20));
        }
        import.join().unwrap();
        let persisted = tauri::async_runtime::block_on(persist_under_gate(&gate, || 1)).unwrap();
        assert_eq!(persisted, 1);
        assert!(gate.0.try_lock().is_ok());
    }

    #[test]
    fn commit_then_persist_cancelled_does_not_persist() {
        let r = TransferRegistry::new();
        {
            let _guard = r.begin("proj-a", "alpha").unwrap();
            assert!(r.request_cancel("proj-a", "alpha"));
            let mut ran = false;
            let out = commit_then_persist(&r, "proj-a", "alpha", CommitCancel::Honour, || {
                ran = true;
                Ok(())
            });
            assert!(matches!(out, Committed::Cancelled));
            assert!(!ran, "persist never runs after a cancel");
        }
        assert!(
            !r.is_transferring("proj-a", "alpha"),
            "guard drop released it"
        );
    }

    #[test]
    fn commit_then_persist_failure_settles_the_marker() {
        let r = TransferRegistry::new();
        {
            let _guard = r.begin("proj-a", "alpha").unwrap();
            let out: Committed<()> =
                commit_then_persist(&r, "proj-a", "alpha", CommitCancel::Honour, || {
                    Err("disk full".to_string())
                });
            assert!(matches!(out, Committed::Done(Err(ref e)) if e == "disk full"));
            assert!(!r.is_transferring("proj-a", "alpha"));
        }
        assert!(!r.is_transferring("proj-a", "alpha"));
        assert!(r.begin("proj-a", "alpha").is_ok());
    }

    #[test]
    fn commit_then_persist_success_keeps_the_marker_until_settle() {
        let r = TransferRegistry::new();
        {
            let _guard = r.begin("proj-a", "alpha").unwrap();
            let out = commit_then_persist(&r, "proj-a", "alpha", CommitCancel::Honour, || Ok(7));
            assert!(matches!(out, Committed::Done(Ok(7))));
        }
        assert!(r.is_transferring("proj-a", "alpha"), "held until settled");
        assert!(
            !r.request_cancel("proj-a", "alpha"),
            "cancel is a no-op now"
        );
        r.settle("proj-a", "alpha");
        assert!(!r.is_transferring("proj-a", "alpha"));
    }

    #[test]
    fn commit_then_persist_ignore_runs_persist_even_if_cancelled() {
        let r = TransferRegistry::new();
        let _guard = r.begin("proj-a", "alpha").unwrap();
        assert!(r.request_cancel("proj-a", "alpha"));
        let mut ran = false;
        let out = commit_then_persist(&r, "proj-a", "alpha", CommitCancel::Ignore, || {
            ran = true;
            Ok(())
        });
        assert!(matches!(out, Committed::Done(Ok(()))));
        assert!(ran);
    }

    #[test]
    fn handoff_summary_path_is_per_project() {
        let a = handoff_summary_path("proj-a", "alpha").unwrap();
        let b = handoff_summary_path("proj-b", "alpha").unwrap();
        assert_ne!(a, b);
        assert_eq!(a, handoff_summary_path("proj-a", "alpha").unwrap());
    }

    // --- Outcome construction ---------------------------------------------------

    #[test]
    fn outcome_for_maps_each_import_result() {
        assert_eq!(
            outcome_for(ImportResult::Imported {
                thread_id: "thread-a".into()
            }),
            TransferOutcome::Imported {
                thread_id: "thread-a".into()
            }
        );
        let failure = TransferFailure::Skipped {
            reason: SkipReason::ContinuedIn,
            message: "m".into(),
            stderr_tail: String::new(),
        };
        assert_eq!(
            outcome_for(ImportResult::Failed(failure.clone())),
            TransferOutcome::Fresh {
                reason_text: failure.user_copy(),
                reason: failure,
            }
        );
        assert_eq!(
            outcome_for(ImportResult::Cancelled),
            TransferOutcome::Cancelled
        );
    }

    #[test]
    fn changes_worktrees_for_every_persisted_outcome_only() {
        let fresh = TransferOutcome::Fresh {
            reason: TransferFailure::Other {
                message: String::new(),
                stderr_tail: String::new(),
            },
            reason_text: "x".into(),
        };
        assert!(changes_worktrees(&Ok(TransferOutcome::Imported {
            thread_id: "thread-a".into()
        })));
        assert!(changes_worktrees(&Ok(fresh)));
        assert!(changes_worktrees(&Ok(TransferOutcome::Done)));
        assert!(!changes_worktrees(&Ok(TransferOutcome::Cancelled)));
        assert!(!changes_worktrees(&Err("nope".into())));
    }

    #[test]
    fn resume_record_follows_the_thread_intent() {
        let imported = persist_plan(
            &ImportResult::Imported {
                thread_id: "thread-a".into(),
            },
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            resume_record_for(&imported),
            Some(ResumeRecord::Codex {
                thread_id: "thread-a".into()
            })
        );
        let failed = persist_plan(
            &ImportResult::Failed(TransferFailure::Other {
                message: String::new(),
                stderr_tail: String::new(),
            }),
            Some("sess-1".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(resume_record_for(&failed), Some(ResumeRecord::Cleared));
        let keep = PersistPlan {
            cli: CliKind::Codex,
            codex_thread_id: FieldIntent::Keep,
            claude_session_id: FieldIntent::Keep,
        };
        assert_eq!(resume_record_for(&keep), None);
    }

    #[test]
    fn outcome_trail_carries_the_failure_details() {
        let failed = ImportResult::Failed(TransferFailure::Timeout {
            phase: TransferPhase::Completion,
            message: "no completion".into(),
            stderr_tail: "last words".into(),
        });
        assert_eq!(
            outcome_trail(&failed),
            json!({
                "outcome": "fresh",
                "failure": "timeout",
                "message": "no completion",
                "stderr_tail": "last words",
            })
        );
        assert_eq!(
            outcome_trail(&ImportResult::Imported {
                thread_id: "thread-a".into()
            }),
            json!({ "outcome": "imported" })
        );
        assert_eq!(
            outcome_trail(&ImportResult::Cancelled),
            json!({ "outcome": "cancelled" })
        );
    }

    #[test]
    fn progress_payload_counts_from_begin_against_the_import_deadline() {
        let p = progress_payload("proj-a", "alpha", ProgressPhase::Persisting, None, 1234);
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({
                "project": "proj-a",
                "worktree": "alpha",
                "phase": "persisting",
                "message": null,
                "elapsedMs": 1234,
                "deadlineMs": 150_000,
            })
        );
    }

    #[test]
    fn env_trail_records_sources_not_secrets() {
        let env = CodexEnv {
            exe: "/home/u/.local/bin/codex".into(),
            exe_source: ExeSource::Discovered,
            path: Some("/home/u/.local/bin:/usr/bin".into()),
            codex_home: PathBuf::from("/home/u/.codex"),
            codex_home_source: CodexHomeSource::Default,
            version: Some("0.166.0".into()),
        };
        assert_eq!(
            env_trail(&env),
            json!({
                "exe": "/home/u/.local/bin/codex",
                "exe_source": "discovered",
                "version": "0.166.0",
                "codex_home_source": "default",
                "path_set": true,
            })
        );
    }

    // --- Persisting provider state (isolated user config) ----------------------

    /// Serializes the tests that point `KEITORA_CONFIG_HOME` at a temp dir.
    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points `KEITORA_CONFIG_HOME` at `dir` for the guard's lifetime and
    /// restores the previous value on drop (also on a failed assertion).
    struct ConfigHome(Option<std::ffi::OsString>);

    impl ConfigHome {
        fn set(dir: &Path) -> Self {
            let prev = std::env::var_os("KEITORA_CONFIG_HOME");
            std::env::set_var("KEITORA_CONFIG_HOME", dir);
            ConfigHome(prev)
        }
    }

    impl Drop for ConfigHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(prev) => std::env::set_var("KEITORA_CONFIG_HOME", prev),
                None => std::env::remove_var("KEITORA_CONFIG_HOME"),
            }
        }
    }

    fn entry<'a>(
        user: &'a keitora_tasks::UserConfig,
        key: &str,
        worktree: &str,
    ) -> Option<&'a keitora_tasks::WorktreeEntry> {
        user.projects[key]
            .worktrees
            .as_ref()?
            .iter()
            .find(|e| e.name == worktree)
    }

    #[test]
    fn persist_worktree_cli_resolves_by_project_not_active() {
        let _lock = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let _home = ConfigHome::set(&tmp.path().join("config"));
        two_projects(tmp.path()).save().unwrap();

        let user = keitora_tasks::UserConfig::load().unwrap();
        let config_b = resolve_project(&user, "proj-b").unwrap();
        persist_worktree_cli(
            &config_b,
            "alpha",
            CliKind::Codex,
            FieldIntent::Set("thread-a".into()),
            FieldIntent::Keep,
        )
        .unwrap();

        let after = keitora_tasks::UserConfig::load().unwrap();
        let alpha = entry(&after, "key-two", "alpha").expect("proj-b entry written");
        assert_eq!(alpha.cli, Some(CliKind::Codex));
        assert_eq!(alpha.codex_thread_id.as_deref(), Some("thread-a"));
        assert_eq!(alpha.claude_session_id, None);
        assert!(
            entry(&after, "key-one", "alpha").is_none(),
            "the active project is untouched"
        );

        // A failed import clears the thread and keeps the Claude session.
        persist_worktree_cli(
            &config_b,
            "alpha",
            CliKind::Codex,
            FieldIntent::Clear,
            FieldIntent::Set("sess-1".into()),
        )
        .unwrap();
        let after = keitora_tasks::UserConfig::load().unwrap();
        let alpha = entry(&after, "key-two", "alpha").unwrap();
        assert_eq!(alpha.codex_thread_id, None);
        assert_eq!(alpha.claude_session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn persist_worktree_cli_unregistered_project_errors_without_writing() {
        let _lock = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let config_home = tmp.path().join("config");
        let _home = ConfigHome::set(&config_home);
        two_projects(tmp.path()).save().unwrap();
        let file = config_home.join("keitora").join("keitora.toml");
        let before = std::fs::read(&file).unwrap();

        // A project that resolves but is not in the saved registry.
        let other = tmp.path().join("other-proj");
        std::fs::create_dir_all(&other).unwrap();
        let mut unsaved = keitora_tasks::UserConfig::default();
        unsaved.register("other-proj", other);
        let config = resolve_project(&unsaved, "other-proj").unwrap();

        let err = persist_worktree_cli(
            &config,
            "alpha",
            CliKind::Codex,
            FieldIntent::Set("thread-a".into()),
            FieldIntent::Keep,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "project other-proj is not registered; cannot persist transfer state"
        );
        assert_eq!(std::fs::read(&file).unwrap(), before, "nothing was written");
    }
}
