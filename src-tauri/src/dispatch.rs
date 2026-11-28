//! Manual board dispatch: a state-aware apply reserves the task always but
//! injects only into an idle worktree with a live PTY (`dispatch_apply`). The
//! human stays in the loop — Keitora only performs the mechanical injection, and
//! only into idle sessions.
//!
//! The same apply also serves CLI dispatch requests (`keitora task dispatch`), which
//! arrive as `dispatch_requests` rows in a registered project's `tasks.db` and are
//! executed by `run_dispatch_requests` when a tasks-DB watcher sees one. Going
//! through this one path keeps the gate, the reserve, the inject and the audit log
//! shared, so a CLI dispatch can't race a board dispatch. A request is stricter than
//! a drag: only an idle session takes it, and one that sends nothing leaves nothing
//! reserved (the board, not the request table, is the queue).
//!
//! The IPC-facing command is thin; the testable core is pure functions:
//! `dispatch_gate` (the engine's send gate, read for dispatch), `render_steps` (the
//! ritual lines),
//! `injection_outcome` (the write result → audit decision), and for requests
//! `request_refusal` (the lane rule) and `request_outcome` (result → row status).

use std::time::Duration;

use serde::Serialize;
use tauri::State;

use keitora_tasks::dispatch::{interpolate, DispatchContext};
use keitora_tasks::{CliKind, DispatchConfig, ReserveOutcome};

use crate::backend::{
    map_write_line_result, PtyBackend, PtyBackendError, SessionKey, SharedBackend, WriteLineOutcome,
};
use crate::drive::{DriveRuntime, SEND_SETTLE_TIMEOUT};
use crate::engine::gate::{self, Hold, Verdict};
use crate::engine::{ProjectViews, SessionEngine, SharedEngine};
use crate::transfer_registry::TransferRegistry;

/// Serializes `dispatch_apply` so its idempotency check, reserve, PTY inject, and
/// `dispatched` log write run as one critical section. Without it, two concurrent
/// calls for the same task could both read the log before either writes its
/// `dispatched` row, both pass the guard, and both inject (the double-inject this
/// module exists to prevent). Dispatch is human-driven and rare, so a single
/// global lock held across the (bounded) inject is cheap.
///
/// Uses `tokio::sync::Mutex` because the gate is held across `.await` points after
/// migrating `dispatch_apply` to `async` (a `std::sync::Mutex` guard is `!Send`
/// across await boundaries).
#[derive(Default)]
pub struct DispatchGate(pub(crate) tokio::sync::Mutex<()>);

/// The result of `dispatch_apply`: whether the task was actually injected
/// (`dispatched`) or merely reserved (`queued`), plus a human-readable `reason`
/// when it was deferred.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchOutcome {
    pub dispatched: bool,
    pub reason: Option<String>,
}

/// What dispatch may do with a worktree now: the engine's send gate (the one drive
/// sends use), read for dispatch. Only `Go` injects; a board drag queues on anything
/// else and a request is refused (internal; never crosses IPC).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Gate {
    /// A fresh, settled, input-ready idle agent with no send still settling.
    Go,
    /// A fresh view that is not idle (`busy`, `waiting` or `offline`).
    NotIdle(String),
    /// herdr's answer is stale or missing; names the link.
    Stale(&'static str),
    /// The agent is starting, settling, or still taking a send just typed into it.
    NotReady(Hold),
}

/// Pure: the [`Gate`] for `worktree` from the project's engine `views`. A worktree
/// outside a non-empty `configured` list has no session dispatch can see (offline).
/// `settling` says whether a send just typed into the view still holds it.
fn dispatch_gate(
    views: &ProjectViews,
    worktree: &str,
    configured: &[String],
    settling: impl FnOnce(&keitora_tasks::StatusSession) -> bool,
) -> Gate {
    if unknown_worktree_refusal(worktree, configured).is_some() {
        return Gate::NotIdle("offline".to_string());
    }
    match gate::send_gate(views, worktree, settling) {
        Verdict::Go => Gate::Go,
        Verdict::Wait(hold) if hold.is_stale() => Gate::Stale(views.link.as_str()),
        Verdict::Wait(hold) => Gate::NotReady(hold),
        Verdict::Refuse(r) => Gate::NotIdle(r.state().to_string()),
    }
}

/// Input reached `key`'s pane: hold the worktree until its turn is seen starting
/// (herdr's `state_change_seq` moves off the one in `views`), exactly as a drive send
/// does, so later drive sends and dispatches wait for it; and poll hot until the next
/// snapshot.
fn note_dispatch_delivery(
    engine: &SessionEngine,
    drive: Option<&DriveRuntime>,
    key: &SessionKey,
    views: &ProjectViews,
) {
    let baseline = views
        .views
        .get(&key.worktree)
        .and_then(|v| v.state_change_seq)
        .and_then(|s| u64::try_from(s).ok())
        .unwrap_or(0);
    if let Some(rt) = drive {
        rt.mark_sent(&key.project, &key.worktree, 0, baseline);
    }
    engine.note_delivery(key);
}

/// Pure: map the AUTHORITATIVE final `write_line` outcome (D6) of a dispatch's
/// injection sequence into the `(audit_decision, DispatchOutcome)` pair. Kept
/// separate so the never-Err-after-reserve mapping is unit-testable. `sent_any`
/// records whether at least one earlier step's text+submit reached the pane.
///
/// NB: this NEVER yields an `Err`; the task is already reserved, and a retry would
/// re-inject. A partial inject reports `dispatched` (the agent has input and may be
/// acting); a clean miss reports `queued` (safe to retry).
fn injection_outcome(
    final_outcome: WriteLineOutcome,
    sent_any: bool,
    worktree: &str,
    detail: &str,
) -> (&'static str, DispatchOutcome) {
    match final_outcome {
        WriteLineOutcome::Dispatched => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                reason: None,
            },
        ),
        // Partial: earlier lines already reached the pane (the agent may be acting).
        // Report dispatched so the caller doesn't re-dispatch; record for diagnostics.
        WriteLineOutcome::Queued | WriteLineOutcome::Error if sent_any => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                reason: Some(format!("{worktree}: partially injected ({detail})")),
            },
        ),
        // Nothing reached the pane → safe to leave reserved/queued for retry.
        WriteLineOutcome::Queued | WriteLineOutcome::Error => (
            "queued",
            DispatchOutcome {
                dispatched: false,
                reason: Some(format!(
                    "{worktree}: reserved — could not inject ({detail})"
                )),
            },
        ),
    }
}

/// Why a dispatch to `worktree` was only reserved: its session is busy, or the
/// user terminated it.
fn queued_reason(worktree: &str, terminated: bool) -> String {
    if terminated {
        format!("{worktree} is terminated — reserved")
    } else {
        format!("{worktree} busy — reserved")
    }
}

/// The detail recorded for an injection's final `write_line` result: empty on
/// success; for a missing session of a terminated worktree, how to recover.
pub(crate) fn injection_detail(result: &Result<(), PtyBackendError>, terminated: bool) -> String {
    match result {
        Ok(()) => String::new(),
        Err(PtyBackendError::NoSuchSession) if terminated => {
            "session is terminated; launch it to dispatch".to_string()
        }
        Err(e) => e.to_string(),
    }
}

/// Inject each ritual `step` as its own submitted line via the backend's atomic
/// `write_line` (text, pause `submit_delay_ms`, then `\r`). Returns the AUTHORITATIVE
/// final `write_line` result (D6) and whether at least one step's text+submit reached
/// the pane (`sent_any`). Stops at the first `Queued`/`Error` step — nothing more
/// should land. Extracted from `dispatch_apply` so the loop is testable against a
/// `FakeBackend` without a live daemon.
fn inject_steps(
    backend: &dyn PtyBackend,
    key: &SessionKey,
    steps: &[String],
    submit_delay_ms: u32,
    inter_step: Duration,
) -> (Result<(), PtyBackendError>, bool) {
    let mut result: Result<(), PtyBackendError> = Ok(());
    let mut sent_any = false;
    for (i, line) in steps.iter().enumerate() {
        result = backend.write_line(key, line, submit_delay_ms);
        match map_write_line_result(&result) {
            WriteLineOutcome::Dispatched => {
                sent_any = true; // text + submit reached the pane
            }
            // Queued / Error: stop — nothing more should land.
            WriteLineOutcome::Queued | WriteLineOutcome::Error => break,
        }
        if i + 1 < steps.len() {
            std::thread::sleep(inter_step);
        }
    }
    (result, sent_any)
}

/// Pause between a step's text and its submit byte, passed to the daemon's atomic
/// `WriteLine` RPC (which writes the text, sleeps this long, then writes the `\r`).
/// The pause matters — a contiguous `text\r` burst reads to the TUI as a paste
/// (embedded newline, no submit), so the Enter must arrive as a distinct, later
/// keystroke. Empirically tunable; raise it if the TUI still fails to submit.
pub(crate) const SUBMIT_DELAY: Duration = Duration::from_millis(60);

/// Delay between dispatch steps so the target TUI finishes processing one submitted
/// line before the next arrives. Empirically tunable.
const INTER_STEP_DELAY: Duration = Duration::from_millis(80);

/// Pure: map every control char (incl. ESC, BEL, newline, tab) to a space, THEN
/// collapse whitespace runs — so an interpolated title/description can't submit the
/// line early or smuggle an escape sequence. `split_whitespace` alone leaves
/// non-whitespace C0 controls like ESC intact, hence the explicit map first.
pub(crate) fn collapse(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pure: Keitora's built-in dispatch prompt, used when no custom `[dispatch].steps`
/// are configured. The submit byte is appended separately at injection time.
fn default_line(ctx: &DispatchContext) -> String {
    let id = ctx.task.id;
    format!(
        "Please pick up Keitora task #{id}. Run `./keitora task claim {id}` to claim it, then implement it: {title}. {desc}. When finished, run `./keitora task done {id}`.",
        title = ctx.task.title,
        desc = ctx.task.description,
    )
}

/// Pure: the ordered lines to inject for a dispatch, given the already-selected
/// per-CLI step list (see [`select_ritual_steps`]). This function itself carries
/// NO CLI-awareness — it just renders whichever step list its caller selected;
/// the per-CLI branch lives entirely in `select_ritual_steps`. Each step is
/// interpolated against `ctx`, then control-char/whitespace collapsed, and lines
/// that render empty are dropped (no blank submits). If nothing remains — no
/// steps configured, or every configured step rendered blank — falls back to the
/// built-in default line, so a dispatch ALWAYS injects at least one line (a
/// misconfigured all-blank list can't silently produce a no-op that still
/// reports success). The submit byte is appended per line at injection time, NOT
/// here.
fn render_steps(steps: &[String], ctx: &DispatchContext) -> Vec<String> {
    let rendered: Vec<String> = steps
        .iter()
        .map(|s| collapse(&interpolate(s, ctx)))
        .filter(|l| !l.is_empty())
        .collect();
    if rendered.is_empty() {
        vec![collapse(&default_line(ctx))]
    } else {
        rendered
    }
}

/// Pure: which `DispatchConfig` step list `render_steps` should render, chosen by
/// the TARGET WORKTREE's resolved [`CliKind`] — a Claude
/// worktree gets `claude_steps`, a Codex worktree gets `codex_steps`. This is the
/// ONE place the per-CLI branch lives; `render_steps` stays CLI-agnostic.
fn select_ritual_steps(cfg: &DispatchConfig, cli: CliKind) -> &[String] {
    cfg.steps_for(cli)
}

/// Who is asking for a dispatch, which decides how strict the apply is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyMode {
    /// A board drag or the card menu. The board already ran its lane rule
    /// (`decideLane`), so the apply reserves and decides inject-vs-queue: a busy
    /// session gets the card queued on its lane.
    Board,
    /// A CLI dispatch request from `tasks.db`: "send this task to that session now".
    /// Nobody has checked anything yet, so the apply does it before reserving: an
    /// unknown worktree, a terminal, blocked or elsewhere-reserved task, and any
    /// session that isn't idle (busy, waiting or offline) are refused, not queued.
    /// The board is the queue; a request either lands or leaves nothing behind.
    Request,
}

/// State-aware apply: reserve the task for `worktree` always, then inject it iff
/// the worktree is idle with a live PTY (else leave it reserved/queued). The
/// backend is authoritative on the idle decision so we never write into a busy PTY.
#[tauri::command]
pub async fn dispatch_apply(
    worktree: String,
    task_id: i64,
    client: State<'_, SharedBackend>,
    engine: State<'_, SharedEngine>,
    gate: State<'_, DispatchGate>,
    registry: State<'_, TransferRegistry>,
    drive: State<'_, DriveRuntime>,
) -> Result<DispatchOutcome, String> {
    apply(
        &worktree,
        task_id,
        ApplyMode::Board,
        None,
        client.inner(),
        engine.inner(),
        &gate,
        &registry,
        Some(drive.inner()),
    )
    .await
}

/// The body of [`dispatch_apply`], shared with the CLI request executor
/// ([`run_dispatch_requests`]) so both go through one gate, one reserve, one
/// inject and one audit log. `project` is the project to dispatch in; `None` means
/// the active one (the board's).
#[allow(clippy::too_many_arguments)] // the command's managed state, passed through
async fn apply(
    worktree: &str,
    task_id: i64,
    mode: ApplyMode,
    project: Option<keitora_tasks::ProjectConfig>,
    client: &SharedBackend,
    engine: &SessionEngine,
    gate: &DispatchGate,
    registry: &TransferRegistry,
    drive: Option<&DriveRuntime>,
) -> Result<DispatchOutcome, String> {
    let worktree = worktree.to_string();
    // Hold the dispatch gate for the whole command so the guard-check → reserve →
    // inject → log sequence is atomic vs. concurrent dispatch_apply calls.
    // `tokio::sync::Mutex::lock` is cancel-safe and does not poison, so the guard is
    // always acquired cleanly.
    let _gate = gate.0.lock().await;

    let (backend, config) = match project {
        Some(pc) => (
            keitora_tasks::open_backend(&pc).map_err(|e| e.to_string())?,
            pc,
        ),
        None => crate::tasks::open_backend()?,
    };
    // Before anything is reserved: a worktree mid-transfer is never injected into.
    if let Some(reason) = transfer_refusal(registry, &config.project.name, &worktree) {
        return Err(reason);
    }

    // Live state via the engine's send gate, the one drive sends use: only a fresh,
    // settled, input-ready idle agent that no send is still settling into is injected
    // into. The backend's `write_line` result below stays the final word on whether
    // the write landed. Read before the reserve, so nothing is reserved on a refusal.
    let project = config.project.name.clone();
    let views = engine.views(&project);
    let state = dispatch_gate(&views, &worktree, &config.worktrees(), |view| {
        drive.is_some_and(|rt| rt.send_settling(&project, &worktree, view, SEND_SETTLE_TIMEOUT))
    });

    // Request mode: refuse up front, and note whether the task was already reserved
    // here (a card queued on this lane), so a failed inject leaves it as it was.
    let mut reserved_before = false;
    if mode == ApplyMode::Request {
        let twd = backend.get(task_id).await.map_err(|e| e.to_string())?;
        if let Some(reason) = gated_request_refusal(
            twd.as_ref(),
            task_id,
            &worktree,
            &config.worktrees(),
            &state,
        ) {
            return Err(reason);
        }
        reserved_before =
            twd.is_some_and(|t| t.task.assigned_to.as_deref() == Some(worktree.as_str()));
    }

    // Idempotency guard + reserve in one intent-level call. If this task is
    // already dispatched and still reserved to this worktree, `dispatch_reserve`
    // returns `AlreadyDispatched` and performs NO write, so a re-click / re-drag /
    // keyboard re-apply no-ops. Otherwise it commits the reserve and returns the
    // post-assign `Task`. Errors (blocked / terminal / assigned elsewhere) bubble up
    // from the reserve as before — but only BEFORE the write (never-Err-after-reserve).
    let task = match backend
        .dispatch_reserve(task_id, "keitora", &worktree)
        .await
        .map_err(|e| e.to_string())?
    {
        ReserveOutcome::AlreadyDispatched => {
            return Ok(DispatchOutcome {
                dispatched: true,
                reason: Some("already dispatched".into()),
            });
        }
        ReserveOutcome::Reserved(task) => task,
    };

    // Why an inject sent nothing, for a request's refusal reason.
    let mut inject_detail = String::new();
    let mut outcome = match state {
        Gate::Go => {
            let ctx = DispatchContext {
                task: &task,
                worktree: &worktree,
            };
            // Resolve the target worktree's CLI so the injected ritual matches it:
            // worktree entry `cli` → user `default_cli` →
            // `CliKind::Claude`. The gate above already held this branch to a
            // settled, input-ready idle agent, whichever CLI it runs.
            let user = keitora_tasks::UserConfig::load().unwrap_or_default();
            let entry = config.worktree_entry(&worktree);
            let resolved_cli = keitora_tasks::resolve_cli(entry.as_ref(), &user);
            let steps = render_steps(select_ritual_steps(&config.dispatch, resolved_cli), &ctx);
            let key = SessionKey {
                project: project.clone(),
                worktree: worktree.clone(),
            };
            // Inject each step as its own submitted line via a single atomic daemon
            // `WriteLine` RPC: the daemon writes the text, pauses `SUBMIT_DELAY` ms,
            // then writes the `\r`. Folding the old two-step client-side
            // text→sleep→`\r` split into one server-side RPC removes the window where
            // a concurrent keystroke could land between a step's text and its Enter.
            // The `write_line` RESULT is AUTHORITATIVE for liveness (D6):
            //   * Ok            → Dispatched (text + `\r` reached the PTY).
            //   * Queued        → NoSuchSession / NotConnected: leave reserved, retry
            //                     later. NOT an Err (a retry would re-inject).
            //   * Error (other) → a real write failure; still NEVER an Err after
            //                     reserve — the partial/queued reporting preserves the
            //                     never-Err-after-reserve invariant.
            // The inter-step delay still runs on this thread so the TUI finishes one
            // submitted line before the next arrives — bounded and small for the
            // handful of steps a ritual holds.
            let (result, sent_any) = inject_steps(
                client.as_ref(),
                &key,
                &steps,
                SUBMIT_DELAY.as_millis() as u32,
                INTER_STEP_DELAY,
            );
            // The task is already reserved. NEVER return Err after reserving (a retry
            // would re-inject). Map the authoritative final outcome to a (never-Err)
            // result + audit decision via the pure `injection_outcome` helper.
            let terminated = client.terminated_keys(&project).contains(&worktree);
            let detail = injection_detail(&result, terminated);
            let (decision, outcome) =
                injection_outcome(map_write_line_result(&result), sent_any, &worktree, &detail);
            if outcome.dispatched {
                note_dispatch_delivery(engine, drive, &key, &views);
            }
            // Best-effort audit: a failed log must not flip us to Err (which would
            // prompt a re-inject of an already-dispatched task).
            let _ = backend
                .dispatch_log_add(task_id, &worktree, decision, &detail)
                .await;
            inject_detail = detail;
            outcome
        }
        Gate::NotIdle(_) | Gate::Stale(_) | Gate::NotReady(_) => {
            // Best-effort audit, matching the Dispatched branch: the task is already
            // reserved, so a failed log must NOT flip us to Err (a retried
            // dispatch_apply would re-reserve / re-inject). never-Err-after-reserve.
            let _ = backend
                .dispatch_log_add(task_id, &worktree, "queued", "")
                .await;
            let terminated = client.terminated_keys(&project).contains(&worktree);
            DispatchOutcome {
                dispatched: false,
                reason: Some(queued_reason(&worktree, terminated)),
            }
        }
    };

    // A request that sent nothing leaves nothing behind: undo the reserve it made
    // (a partial inject reports `dispatched` and is kept, since the agent has input).
    // Best-effort, like the audit writes; the outcome stays not-dispatched either way.
    if mode == ApplyMode::Request && !outcome.dispatched {
        if !reserved_before {
            let _ = backend.release(task_id, "keitora").await;
            let _ = backend
                .dispatch_log_add(task_id, &worktree, "released", "nothing was sent")
                .await;
        }
        outcome.reason = Some(request_miss_reason(
            &worktree,
            &inject_detail,
            reserved_before,
        ));
    }
    Ok(outcome)
}

/// Pure: why a CLI dispatch request for `task_id` onto `worktree` (whose reconciled
/// session is in `state`) must be refused before anything is reserved, or `None` to
/// go ahead. `configured` is the project's worktree list; an empty list doesn't
/// restrict, matching how `reconciled_sessions` scopes. Task-side, these are the
/// board's lane checks; session-side, only an idle session passes.
fn request_refusal(
    task: Option<&keitora_tasks::TaskWithDeps>,
    task_id: i64,
    worktree: &str,
    configured: &[String],
    state: &str,
) -> Option<String> {
    if let Some(reason) = unknown_worktree_refusal(worktree, configured) {
        return Some(reason);
    }
    let Some(task) = task else {
        return Some(format!("task #{task_id} not found"));
    };
    keitora_tasks::dispatch::task_refusal(task, worktree)
        .or_else(|| keitora_tasks::dispatch::session_refusal(state, worktree))
}

/// Pure: [`request_refusal`] against the dispatch [`Gate`]. A stale gate (herdr's
/// answer is old or missing) or an agent not ready for input is refused as such
/// rather than by its last known state; the task's own refusals still come first.
fn gated_request_refusal(
    task: Option<&keitora_tasks::TaskWithDeps>,
    task_id: i64,
    worktree: &str,
    configured: &[String],
    gate: &Gate,
) -> Option<String> {
    let state = match gate {
        Gate::NotIdle(state) => state.as_str(),
        Gate::Go | Gate::Stale(_) | Gate::NotReady(_) => "idle",
    };
    request_refusal(task, task_id, worktree, configured, state).or_else(|| match gate {
        Gate::Stale(link) => Some(format!(
            "@{worktree}: herdr status is stale ({link}); try again shortly"
        )),
        Gate::NotReady(hold) => Some(format!(
            "@{worktree} is not ready for input yet ({}); try again shortly",
            not_ready_why(*hold)
        )),
        Gate::Go | Gate::NotIdle(_) => None,
    })
}

/// Pure: a request's words for an agent the send gate holds.
fn not_ready_why(hold: Hold) -> &'static str {
    match hold {
        Hold::Starting => "starting",
        Hold::Unsettled => "settling",
        Hold::SendSettling => "a send to it is still settling",
        Hold::Link(_) | Hold::FirstSnapshot | Hold::Stale => "herdr status is stale",
    }
}

/// Pure: `Some(reason)` when `worktree` is not one of the project's `configured`
/// worktrees. An empty list doesn't restrict, matching how `reconciled_sessions`
/// scopes. Shared by dispatch and drive requests so both word it the same.
pub(crate) fn unknown_worktree_refusal(worktree: &str, configured: &[String]) -> Option<String> {
    (!configured.is_empty() && !configured.iter().any(|w| w == worktree)).then(|| {
        format!(
            "unknown worktree `{worktree}`; expected one of: {}",
            configured.join(", ")
        )
    })
}

/// Pure: the refusal reason for a request whose inject sent nothing. `detail` is the
/// write error (may be empty); `reserved_before` says the card was already queued on
/// that lane, so it stays there rather than being released.
fn request_miss_reason(worktree: &str, detail: &str, reserved_before: bool) -> String {
    let why = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    let left = if reserved_before {
        "it stays queued on the board as before"
    } else {
        "nothing is left reserved"
    };
    format!("could not send to @{worktree}{why}; {left}")
}

/// How old a pending dispatch request may be before the app refuses it rather than
/// acting on it: a request made while the app was closed shouldn't fire whenever it
/// next opens.
const REQUEST_MAX_AGE_SECS: i64 = 600;

/// Pure: the `(status, reason)` a dispatch request resolves to, from its apply
/// result. An inject (or an idempotent re-apply) is `applied`; anything else is
/// `refused`: an `Err` (nothing was reserved) or an inject that sent nothing (its
/// reserve was undone).
fn request_outcome(result: &Result<DispatchOutcome, String>) -> (&'static str, String) {
    match result {
        Ok(o) if o.dispatched => ("applied", o.reason.clone().unwrap_or_default()),
        Ok(o) => (
            "refused",
            o.reason
                .clone()
                .unwrap_or_else(|| "nothing was sent".to_string()),
        ),
        Err(e) => ("refused", e.clone()),
    }
}

/// Whether `local` holds pending dispatch or drive requests: the engine polls hot for
/// the project while so. Reads only `status`.
pub(crate) fn has_pending_requests(local: &keitora_tasks::LocalStore) -> bool {
    local.dispatch_requests_has_pending().unwrap_or(false)
        || local.drive_requests_has_pending().unwrap_or(false)
}

/// Execute `project`'s pending CLI dispatch requests (`keitora task dispatch`),
/// oldest first, each through [`apply`] in request mode, and record each outcome on
/// its row. `project` is a resolved `[project].name` from the app's registry; a
/// requested pass runs this (see `crate::pipeline`) when that project's DB holds a
/// pending row.
///
/// Safe to run concurrently with itself and with board dispatches: each request is
/// taken with a compare-and-set (`dispatch_request_begin`), so it executes once,
/// and every apply holds the [`DispatchGate`].
pub(crate) async fn run_dispatch_requests<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project: String,
) {
    use tauri::Manager;

    let (Some(client), Some(engine), Some(gate), Some(registry)) = (
        app.try_state::<SharedBackend>(),
        app.try_state::<SharedEngine>(),
        app.try_state::<DispatchGate>(),
        app.try_state::<TransferRegistry>(),
    ) else {
        return; // still starting up; the next DB event retries
    };
    let drive = app.try_state::<DriveRuntime>();
    let Some(config) = keitora_tasks::UserConfig::load()
        .ok()
        .and_then(|u| u.project_config_by_name(&project))
    else {
        log::warn!("dispatch requests: project {project} is not registered");
        return;
    };
    let db_path = config.tasks.db_path.to_string_lossy().into_owned();
    let local = match keitora_tasks::LocalStore::open(&db_path) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("dispatch requests: cannot open {db_path}: {e}");
            return;
        }
    };
    match local.dispatch_requests_expire(REQUEST_MAX_AGE_SECS) {
        Ok(0) => {}
        Ok(n) => log::info!("dispatch requests: {n} expired unprocessed in {project}"),
        Err(e) => log::warn!("dispatch requests: expiry failed: {e}"),
    }
    let pending = match local.dispatch_requests_pending() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("dispatch requests: cannot list pending: {e}");
            return;
        }
    };
    // Poll hot while the project has requests; re-read once they are served.
    engine.set_pending(&project, has_pending_requests(&local));
    for req in pending {
        match local.dispatch_request_begin(req.id) {
            Ok(true) => {}
            Ok(false) => continue, // another pass took it
            Err(e) => {
                log::warn!("dispatch request {}: cannot begin: {e}", req.id);
                continue;
            }
        }
        let result = apply(
            &req.worktree,
            req.task_id,
            ApplyMode::Request,
            Some(config.clone()),
            client.inner(),
            engine.inner(),
            &gate,
            &registry,
            drive.as_ref().map(|d| d.inner()),
        )
        .await;
        let (status, reason) = request_outcome(&result);
        log::info!(
            "dispatch request {} ({project}: task #{} → {}, by {}): {status} {reason}",
            req.id,
            req.task_id,
            req.worktree,
            req.requested_by
        );
        if let Err(e) = local.dispatch_request_resolve(req.id, status, &reason) {
            log::warn!("dispatch request {}: cannot record outcome: {e}", req.id);
        }
    }
    engine.set_pending(&project, has_pending_requests(&local));
}

/// `Some(error)` when `worktree` of `project` is mid-transfer or mid-switch;
/// dispatch must not inject into it.
pub(crate) fn transfer_refusal(
    registry: &TransferRegistry,
    project: &str,
    worktree: &str,
) -> Option<String> {
    registry.is_transferring(project, worktree).then(|| {
        format!(
            "a transfer or switch is in progress for worktree `{worktree}`; try again when it finishes"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_refusal_blocks_only_the_transferring_worktree() {
        let r = TransferRegistry::new();
        let _guard = r.begin("proj-a", "alpha").unwrap();
        let refusal = transfer_refusal(&r, "proj-a", "alpha").expect("refused");
        assert!(refusal.contains("`alpha`"));
        assert!(refusal.contains("in progress"));
        assert_eq!(transfer_refusal(&r, "proj-a", "bravo"), None);
        assert_eq!(transfer_refusal(&r, "proj-b", "alpha"), None);
    }

    #[test]
    fn transfer_refusal_clears_after_settle() {
        let r = TransferRegistry::new();
        {
            let _guard = r.begin("proj-a", "alpha").unwrap();
            assert!(r.mark_committed("proj-a", "alpha"));
        }
        assert!(transfer_refusal(&r, "proj-a", "alpha").is_some());
        r.settle("proj-a", "alpha");
        assert_eq!(transfer_refusal(&r, "proj-a", "alpha"), None);

        // The commit backstop releases a marker nobody settled.
        let base = std::time::Instant::now();
        {
            let _guard = r.begin_at("proj-a", "alpha", base).unwrap();
            assert!(r.mark_committed_at("proj-a", "alpha", base));
        }
        assert!(r.is_transferring_at("proj-a", "alpha", base + Duration::from_secs(179)));
        assert!(!r.is_transferring_at("proj-a", "alpha", base + Duration::from_secs(181)));
        assert_eq!(transfer_refusal(&r, "proj-a", "alpha"), None);
    }

    #[test]
    fn switch_refuses_while_a_transfer_is_running() {
        let r = TransferRegistry::new();
        let _guard = r.begin("proj-a", "alpha").unwrap();
        assert!(r.begin("proj-a", "alpha").is_err(), "a switch cannot begin");
        assert_eq!(
            transfer_refusal(&r, "proj-a", "alpha").as_deref(),
            Some(
                "a transfer or switch is in progress for worktree `alpha`; try again when it finishes"
            )
        );
    }

    #[test]
    fn transfer_refusal_message_is_exact() {
        let r = TransferRegistry::new();
        let _guard = r.begin("proj-a", "alpha").unwrap();
        assert_eq!(
            transfer_refusal(&r, "proj-a", "alpha").as_deref(),
            Some(
                "a transfer or switch is in progress for worktree `alpha`; try again when it finishes"
            )
        );
    }

    fn open_card(assigned_to: Option<&str>, blocked_by: Vec<i64>) -> keitora_tasks::TaskWithDeps {
        let mut task = task_fixture(5, "t", "d");
        task.status = "open".to_string();
        task.assigned_to = assigned_to.map(str::to_string);
        keitora_tasks::TaskWithDeps {
            task,
            blocked_by,
            blocks: vec![],
        }
    }

    #[test]
    fn request_refusal_follows_the_lane_rule() {
        let wts = vec!["alpha".to_string(), "bravo".to_string()];
        let card = open_card(None, vec![]);
        // Only an idle session takes a request.
        assert_eq!(request_refusal(Some(&card), 5, "alpha", &wts, "idle"), None);
        // Busy, waiting and offline are refused, not queued: the board is the queue.
        for state in ["busy", "waiting", "offline"] {
            let msg = request_refusal(Some(&card), 5, "alpha", &wts, state).expect(state);
            assert!(msg.starts_with(&format!("@alpha is {state}")), "{msg}");
        }
        // A blocked task is refused whatever the session state.
        assert_eq!(
            request_refusal(Some(&open_card(None, vec![2])), 5, "alpha", &wts, "idle"),
            Some("task #5 is blocked by #2".to_string())
        );
        // Reserved elsewhere is refused; reserved here goes ahead (sends a queued task).
        assert!(request_refusal(
            Some(&open_card(Some("bravo"), vec![])),
            5,
            "alpha",
            &wts,
            "idle"
        )
        .is_some());
        assert_eq!(
            request_refusal(
                Some(&open_card(Some("alpha"), vec![])),
                5,
                "alpha",
                &wts,
                "idle"
            ),
            None
        );
    }

    #[test]
    fn request_refusal_checks_worktree_then_task() {
        let wts = vec!["alpha".to_string()];
        let card = open_card(None, vec![]);
        assert_eq!(
            request_refusal(Some(&card), 5, "zulu", &wts, "idle"),
            Some("unknown worktree `zulu`; expected one of: alpha".to_string())
        );
        assert_eq!(
            request_refusal(None, 9, "alpha", &wts, "idle"),
            Some("task #9 not found".to_string())
        );
        // No configured worktrees: no restriction, as in `reconciled_sessions`.
        assert_eq!(request_refusal(Some(&card), 5, "zulu", &[], "idle"), None);
    }

    #[test]
    fn request_miss_reason_says_what_was_left() {
        assert_eq!(
            request_miss_reason("alpha", "no live session", false),
            "could not send to @alpha (no live session); nothing is left reserved"
        );
        assert_eq!(
            request_miss_reason("alpha", "", true),
            "could not send to @alpha; it stays queued on the board as before"
        );
    }

    #[test]
    fn request_outcome_maps_apply_results() {
        let ok = |dispatched: bool, reason: Option<&str>| {
            Ok(DispatchOutcome {
                dispatched,
                reason: reason.map(str::to_string),
            })
        };
        assert_eq!(request_outcome(&ok(true, None)), ("applied", String::new()));
        assert_eq!(
            request_outcome(&ok(true, Some("already dispatched"))),
            ("applied", "already dispatched".to_string())
        );
        // An inject that sent nothing is refused (its reserve was undone).
        assert_eq!(
            request_outcome(&ok(false, Some("alpha: could not inject (gone)"))),
            ("refused", "alpha: could not inject (gone)".to_string())
        );
        assert_eq!(
            request_outcome(&ok(false, None)),
            ("refused", "nothing was sent".to_string())
        );
        assert_eq!(
            request_outcome(&Err("task #5 is done".to_string())),
            ("refused", "task #5 is done".to_string())
        );
    }

    fn gate_of(views: &ProjectViews) -> Gate {
        dispatch_gate(views, "alpha", &[], |_| false)
    }

    /// An engine view of `worktree` in `state`: settled and ready for input when idle.
    fn view(worktree: &str, state: &str, stale: bool) -> keitora_tasks::StatusSession {
        keitora_tasks::StatusSession {
            project: "proj".to_string(),
            worktree: worktree.to_string(),
            state: state.to_string(),
            agent: if state == "offline" {
                "absent"
            } else {
                "present"
            }
            .to_string(),
            agent_kind: None,
            herdr_status: None,
            settled: true,
            stale,
            interactive_ready: None,
            pane_id: None,
            terminal_id: None,
            agent_session: None,
            state_change_seq: None,
            completion_seq: None,
            brokered: false,
            escalated_reason: None,
            last_change_at: 0,
        }
    }

    fn views_of(
        ready: bool,
        link: crate::engine::HerdrLink,
        v: Option<keitora_tasks::StatusSession>,
    ) -> ProjectViews {
        ProjectViews {
            ready,
            link,
            views: v.map(|v| (v.worktree.clone(), v)).into_iter().collect(),
        }
    }

    fn up(v: keitora_tasks::StatusSession) -> ProjectViews {
        views_of(true, crate::engine::HerdrLink::Up, Some(v))
    }

    /// A session herdr reports working reads `busy` and is never injected into, so the
    /// dispatcher never writes into a session running background work.
    #[test]
    fn idle_with_background_task_gates_dispatch() {
        assert_eq!(
            gate_of(&up(view("alpha", "busy", false))),
            Gate::NotIdle("busy".into())
        );
    }

    /// A session whose agent herdr reports `blocked` reads `waiting`: not idle.
    #[test]
    fn dispatch_refuses_a_blocked_lane() {
        assert_eq!(
            gate_of(&up(view("alpha", "waiting", false))),
            Gate::NotIdle("waiting".into())
        );
    }

    #[test]
    fn dispatch_gates_on_idle_and_offline_views() {
        assert_eq!(gate_of(&up(view("alpha", "idle", false))), Gate::Go);
        assert_eq!(
            gate_of(&up(view("alpha", "offline", false))),
            Gate::NotIdle("offline".into())
        );
        // No session at all, once the engine is ready: offline.
        assert_eq!(
            gate_of(&views_of(true, crate::engine::HerdrLink::Up, None)),
            Gate::NotIdle("offline".into())
        );
        // A worktree outside the configured list has no session dispatch can see.
        let wts = vec!["bravo".to_string()];
        assert_eq!(
            dispatch_gate(&up(view("alpha", "idle", false)), "alpha", &wts, |_| false),
            Gate::NotIdle("offline".into())
        );
    }

    /// A delivered dispatch holds its worktree like a drive send: the next drive send
    /// or dispatch waits for the turn to start, and the engine polls hot meanwhile.
    #[test]
    fn a_delivered_dispatch_holds_the_worktree_until_its_turn_starts() {
        let engine = crate::engine::SessionEngine::new_for_test(std::sync::Arc::new(
            crate::engine::tests::FakeEvidence::default(),
        ));
        let rt = DriveRuntime::default();
        let key = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };
        let mut idle = view("alpha", "idle", false);
        idle.state_change_seq = Some(7);
        let views = up(idle.clone());
        assert_eq!(engine.mode(), crate::engine::Mode::Cold);

        note_dispatch_delivery(&engine, Some(&rt), &key, &views);
        assert_eq!(engine.mode(), crate::engine::Mode::Hot);
        let settling = |v: &keitora_tasks::StatusSession| {
            rt.send_settling("proj", "alpha", v, SEND_SETTLE_TIMEOUT)
        };
        assert_eq!(
            dispatch_gate(&views, "alpha", &[], settling),
            Gate::NotReady(Hold::SendSettling),
            "the next dispatch waits"
        );
        let drive_sets = crate::drive::SessionSets {
            views: views.clone(),
            terminated: Default::default(),
            hooks: Default::default(),
            at_ms: 0,
        };
        assert!(
            matches!(
                drive_sets.send_gate("alpha", false, settling),
                crate::drive::SendGate::Wait(_)
            ),
            "a drive send waits too"
        );
        // The turn starts (state_change_seq moves): the hold ends.
        let mut moved = idle;
        moved.state_change_seq = Some(8);
        assert_eq!(dispatch_gate(&up(moved), "alpha", &[], settling), Gate::Go);
    }

    /// An agent drive would make wait (starting, not settled, not ready for input, or
    /// a send still settling into it) gets nothing injected either.
    #[test]
    fn dispatch_waits_wherever_a_drive_send_would() {
        let mut starting = view("alpha", "busy", false);
        starting.agent = "starting".into();
        assert_eq!(gate_of(&up(starting)), Gate::NotReady(Hold::Starting));
        let mut not_ready = view("alpha", "idle", false);
        not_ready.interactive_ready = Some(false);
        assert_eq!(gate_of(&up(not_ready)), Gate::NotReady(Hold::Starting));
        let mut unsettled = view("alpha", "idle", false);
        unsettled.settled = false;
        assert_eq!(gate_of(&up(unsettled)), Gate::NotReady(Hold::Unsettled));
        assert_eq!(
            dispatch_gate(&up(view("alpha", "idle", false)), "alpha", &[], |_| true),
            Gate::NotReady(Hold::SendSettling)
        );

        let wts = vec!["alpha".to_string()];
        let card = open_card(None, vec![]);
        assert_eq!(
            gated_request_refusal(
                Some(&card),
                5,
                "alpha",
                &wts,
                &Gate::NotReady(Hold::Unsettled)
            ),
            Some("@alpha is not ready for input yet (settling); try again shortly".to_string())
        );
        // The task's own refusals still come first.
        assert_eq!(
            gated_request_refusal(None, 9, "alpha", &wts, &Gate::NotReady(Hold::Starting)),
            Some("task #9 not found".to_string())
        );
    }

    #[test]
    fn dispatch_treats_a_stale_session_as_not_idle() {
        use crate::engine::HerdrLink;
        let gate = gate_of(&up(view("alpha", "idle", true)));
        assert_eq!(gate, Gate::Stale("up"), "a board drag queues");
        // Before the first snapshot even a missing session is stale.
        assert_eq!(
            gate_of(&views_of(false, HerdrLink::Up, None)),
            Gate::Stale("up")
        );
        assert_eq!(
            gate_of(&views_of(
                false,
                HerdrLink::Recovering,
                Some(view("alpha", "idle", false))
            )),
            Gate::Stale("recovering")
        );

        let wts = vec!["alpha".to_string()];
        let card = open_card(None, vec![]);
        assert_eq!(
            gated_request_refusal(Some(&card), 5, "alpha", &wts, &Gate::Stale("recovering")),
            Some("@alpha: herdr status is stale (recovering); try again shortly".to_string())
        );
        // The task's own refusals still come first.
        assert_eq!(
            gated_request_refusal(None, 9, "alpha", &wts, &gate),
            Some("task #9 not found".to_string())
        );
        // A fresh session refuses by its state as before.
        assert!(gated_request_refusal(
            Some(&card),
            5,
            "alpha",
            &wts,
            &Gate::NotIdle("busy".into())
        )
        .is_some_and(|r| r.starts_with("@alpha is busy")));
        assert_eq!(
            gated_request_refusal(Some(&card), 5, "alpha", &wts, &Gate::Go),
            None
        );
    }

    fn task_fixture(id: i64, title: &str, desc: &str) -> keitora_tasks::Task {
        keitora_tasks::Task {
            id,
            title: title.to_string(),
            description: desc.to_string(),
            status: "active".to_string(),
            priority: "high".to_string(),
            assigned_to: Some("sol".to_string()),
            created_by: None,
            tags: vec![],
            branch: None,
            pr_url: None,
            created_at: "2026-06-21".to_string(),
            updated_at: "2026-06-21".to_string(),
        }
    }

    #[test]
    fn render_steps_empty_config_uses_default_line() {
        let task = task_fixture(42, "Fix\tthe   parser", "It crashes\non  empty\n\ninput");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let steps = render_steps(&DispatchConfig::default().claude_steps, &ctx);
        assert_eq!(
            steps.len(),
            1,
            "empty config yields exactly the default line"
        );
        let line = &steps[0];
        // Embedded newlines/tabs/multi-spaces collapse to single spaces.
        assert!(
            line.contains("Fix the parser"),
            "title not collapsed: {line}"
        );
        assert!(
            line.contains("It crashes on empty input"),
            "desc not collapsed: {line}"
        );
        // Built-in claim/done guidance preserved.
        assert!(line.contains("./keitora task claim 42"));
        assert!(line.contains("./keitora task done 42"));
        // The submit byte is NOT carried in-band any more — it is written separately.
        assert!(
            !line.ends_with('\r'),
            "submit byte must not be in the line: {line:?}"
        );
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte: {line:?}"
        );
    }

    #[test]
    fn render_steps_default_line_strips_control_bytes() {
        // ESC/ANSI in the title and a BEL in the desc must both be neutralised.
        let task = task_fixture(7, "a\u{1b}[31mb", "x\x07y");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let line = &render_steps(&DispatchConfig::default().claude_steps, &ctx)[0];
        assert!(
            line.contains("a [31mb") || line.contains("a[31mb"),
            "title: {line:?}"
        );
        assert!(line.contains("x y"), "desc: {line:?}");
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte may survive: {line:?}"
        );
        assert!(!line.contains('\u{1b}'), "no ESC may survive: {line:?}");
    }

    #[test]
    fn render_steps_interpolates_configured_steps_in_order() {
        let task = task_fixture(19, "custom dispatch", "wire it up");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow pick up #{task_number}: {task_title}".to_string(),
            ],
            codex_steps: vec![],
            antigravity_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(
            steps,
            vec![
                "/clear".to_string(),
                "/park".to_string(),
                "/workflow pick up #0019: custom dispatch".to_string(),
            ]
        );
    }

    #[test]
    fn render_steps_drops_blank_steps() {
        let task = task_fixture(1, "t", "d");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string(), "   ".to_string(), "/park".to_string()],
            codex_steps: vec![],
            antigravity_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(steps, vec!["/clear".to_string(), "/park".to_string()]);
    }

    #[test]
    fn render_steps_all_blank_config_falls_back_to_default() {
        // A misconfigured all-blank `steps` must NOT yield an empty injection (which
        // dispatch_apply would report as a successful no-op) — it falls back to the
        // built-in default line so a dispatch always sends something.
        let task = task_fixture(42, "the title", "the desc");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["   ".to_string(), "\t".to_string()],
            codex_steps: vec![],
            antigravity_steps: vec![],
        };
        let steps = render_steps(&cfg.claude_steps, &ctx);
        assert_eq!(steps.len(), 1, "must fall back to one default line");
        assert!(
            steps[0].contains("./keitora task claim 42"),
            "default line: {:?}",
            steps[0]
        );
    }

    #[test]
    fn render_steps_collapses_interpolated_control_chars() {
        // A title carrying a newline/ESC, injected via a custom step, must collapse so
        // it can't submit early or smuggle an escape sequence.
        let task = task_fixture(5, "line1\nline2\u{1b}[0m", "d");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["go {task_title}".to_string()],
            codex_steps: vec![],
            antigravity_steps: vec![],
        };
        let line = &render_steps(&cfg.claude_steps, &ctx)[0];
        assert!(
            !line.chars().any(|c| c.is_control()),
            "no control byte: {line:?}"
        );
        assert!(line.starts_with("go line1 line2"), "collapsed: {line:?}");
    }

    // -----------------------------------------------------------------------
    // select_ritual_steps
    // -----------------------------------------------------------------------

    #[test]
    fn select_ritual_steps_picks_claude_steps_for_claude_cli() {
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string(), "/park".to_string()],
            codex_steps: vec!["/codex-step".to_string()],
            antigravity_steps: vec![],
        };
        assert_eq!(
            select_ritual_steps(&cfg, CliKind::Claude),
            &cfg.claude_steps[..]
        );
    }

    #[test]
    fn select_ritual_steps_picks_codex_steps_for_codex_cli() {
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec!["/codex-step".to_string(), "/codex-step-2".to_string()],
            antigravity_steps: vec![],
        };
        assert_eq!(
            select_ritual_steps(&cfg, CliKind::Codex),
            &cfg.codex_steps[..]
        );
    }

    #[test]
    fn select_ritual_steps_empty_codex_list_falls_back_to_default_via_render_steps() {
        // `select_ritual_steps` itself has no fallback logic — it is a pure field
        // selector. The fallback lives in `render_steps`, exercised here through
        // the full pipeline for a Codex worktree with no configured `codex_steps`,
        // mirroring `render_steps_all_blank_config_falls_back_to_default` but for
        // the Codex list specifically.
        let task = task_fixture(42, "the title", "the desc");
        let ctx = DispatchContext {
            task: &task,
            worktree: "sol",
        };
        let cfg = DispatchConfig {
            claude_steps: vec!["/clear".to_string()],
            codex_steps: vec![],
            antigravity_steps: vec![],
        };
        let steps = render_steps(select_ritual_steps(&cfg, CliKind::Codex), &ctx);
        assert_eq!(
            steps.len(),
            1,
            "empty codex_steps falls back to one default line"
        );
        assert!(
            steps[0].contains("./keitora task claim 42"),
            "default line: {:?}",
            steps[0]
        );
    }

    // -----------------------------------------------------------------------
    // inject_steps — the injection loop against a FakeBackend.
    // -----------------------------------------------------------------------

    fn inject_key() -> SessionKey {
        SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        }
    }

    fn write_line_calls(
        fake: &crate::backend::fake::FakeBackend,
    ) -> Vec<(SessionKey, String, u32)> {
        use crate::backend::fake::Call;
        fake.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::WriteLine {
                    key,
                    text,
                    submit_delay_ms,
                } => Some((key, text, submit_delay_ms)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn inject_steps_all_ok_reports_sent() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Ok(()));
        fake.push_write_line(Ok(()));
        let key = inject_key();
        let steps = vec!["/clear".to_string(), "/go".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        assert!(result.is_ok());
        assert!(sent_any);
        // Both steps attempted, with the right key + submit delay.
        let writes = write_line_calls(&fake);
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].0, key);
        assert_eq!(writes[0].1, "/clear");
        assert_eq!(writes[0].2, 60);
        assert_eq!(writes[1].1, "/go");
    }

    #[test]
    fn inject_steps_stops_on_no_such_session_but_sent_any() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Ok(()));
        fake.push_write_line(Err(PtyBackendError::NoSuchSession));
        let key = inject_key();
        let steps = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        // The failing step's result is authoritative and maps to Queued.
        assert!(matches!(result, Err(PtyBackendError::NoSuchSession)));
        assert_eq!(map_write_line_result(&result), WriteLineOutcome::Queued);
        assert!(sent_any, "step 1 reached the pane");
        // Stops after the failing step — step 3 never runs.
        assert_eq!(write_line_calls(&fake).len(), 2);
    }

    #[test]
    fn inject_steps_first_not_connected_sends_nothing() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        fake.push_write_line(Err(PtyBackendError::NotConnected));
        let key = inject_key();
        let steps = vec!["a".to_string(), "b".to_string()];
        let (result, sent_any) = inject_steps(&fake, &key, &steps, 60, Duration::ZERO);
        assert!(matches!(result, Err(PtyBackendError::NotConnected)));
        assert_eq!(map_write_line_result(&result), WriteLineOutcome::Queued);
        assert!(!sent_any);
        // No step runs after the first failure.
        assert_eq!(write_line_calls(&fake).len(), 1);
    }

    #[test]
    fn a_terminated_worktree_is_named_in_the_queued_reason() {
        assert_eq!(queued_reason("sayo", false), "sayo busy — reserved");
        assert_eq!(queued_reason("sayo", true), "sayo is terminated — reserved");
    }

    #[test]
    fn a_missing_session_of_a_terminated_worktree_says_so() {
        use crate::backend::fake::FakeBackend;
        let fake = FakeBackend::new();
        let key = inject_key();
        fake.insert_terminated(key.clone());
        fake.push_write_line(Err(PtyBackendError::NoSuchSession));
        let (result, sent_any) = inject_steps(&fake, &key, &["a".to_string()], 60, Duration::ZERO);
        let terminated = fake.terminated_keys(&key.project).contains(&key.worktree);
        let detail = injection_detail(&result, terminated);
        assert_eq!(detail, "session is terminated; launch it to dispatch");
        let (decision, outcome) =
            injection_outcome(map_write_line_result(&result), sent_any, "sayo", &detail);
        assert_eq!(decision, "queued");
        assert!(!outcome.dispatched);
        assert!(outcome.reason.unwrap().contains("terminated"));
        // Not terminated, or another error: the error's own text.
        let missing = Err(PtyBackendError::NoSuchSession);
        assert_eq!(injection_detail(&missing, false), "no live session");
        let down = Err(PtyBackendError::NotConnected);
        assert_eq!(injection_detail(&down, true), "herdr unreachable");
        assert_eq!(injection_detail(&Ok(()), true), "");
    }
}
