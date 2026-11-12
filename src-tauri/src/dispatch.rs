//! Board dispatch: assign a task to a worktree lane at once, and send (type) the
//! front pending card of a lane into its session only when the lane is free: the
//! session is live and idle, nothing on the lane is active, and no sent card is still
//! waiting for its claim (`keitora_tasks::dispatch::lane_hold`). The board's
//! worktree lanes are the queue: a card assigned to a lane that isn't free stays
//! *pending* there and is sent automatically when it reaches the front and the lane
//! frees up (`pump_lanes`, run from the tasks-DB watchers).
//!
//! One apply serves the board (`dispatch_apply`, and `dispatch_auto` for one-click
//! Dispatch) and CLI dispatch requests (`keitora task dispatch`), which arrive as
//! `dispatch_requests` rows in a registered project's `tasks.db` and are executed by
//! `run_dispatch_requests`. Going through this one path keeps the gate, the reserve,
//! the inject and the audit log shared, so nothing can double-send. Only worktrees in
//! the project's dispatch pool (`ProjectConfig::in_dispatch_pool`) take dispatches.
//!
//! The IPC-facing commands are thin; the testable core is pure functions:
//! `render_steps` (the ritual lines), `injection_outcome` (the write result → audit
//! decision), `board_refusal` / `request_refusal` (the up-front checks) and the lane
//! rules in `keitora_tasks::dispatch` (`lane_queue`, `lane_hold`, `pick_worktree`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Serialize;
use tauri::State;

use keitora_tasks::dispatch::{
    interpolate, lane_hold, lane_queue, pending_reason, pick_worktree, DispatchContext, LaneHold,
    PoolLane,
};
use keitora_tasks::{
    CliKind, DispatchConfig, ListFilter, ProjectConfig, ReserveOutcome, TaskBackend, TaskWithDeps,
    WorktreeSession,
};

use crate::backend::{
    background_set, map_write_line_result, PtyBackend, PtyBackendError, SessionKey, SharedBackend,
    WriteLineOutcome,
};
use crate::transfer_registry::TransferRegistry;

/// Serializes every dispatch (board, CLI request, auto-send) so its checks, reserve,
/// PTY inject and `dispatched` log write run as one critical section. Without it, two
/// concurrent calls could both see a lane free before either logs its send, and both
/// inject. Dispatch is rare, so a single global lock held across the (bounded) inject
/// is cheap. It also guards [`SendMemo`].
///
/// Uses `tokio::sync::Mutex` because the gate is held across `.await` points (a
/// `std::sync::Mutex` guard is `!Send` across await boundaries).
#[derive(Default)]
pub struct DispatchGate(pub(crate) tokio::sync::Mutex<SendMemo>);

/// What the automatic send remembers between watcher passes: per `(project,
/// worktree)`, the card whose send last failed and the session heartbeat at the time.
/// The auto-send doesn't retry that card until the session reports again (a new
/// heartbeat), so a failing pane can't spin the watcher (each attempt writes the
/// dispatch log, which wakes the watcher again). A manual retry ignores it.
#[derive(Default)]
pub struct SendMemo {
    failed: HashMap<(String, String), (i64, String)>,
}

/// The result of a dispatch, for the board and for CLI requests.
#[derive(Debug, Clone, Serialize)]
pub struct DispatchOutcome {
    /// The prompt reached the session (a fresh or partial send, or a card that was
    /// already sent there).
    pub dispatched: bool,
    /// `sent`, `already_sent`, `pending` (assigned, waiting on its lane) or `failed`
    /// (assigned, but the send errored; it stays pending).
    pub state: &'static str,
    /// The lane the task is on (the picked one, for an auto-pick).
    pub worktree: String,
    /// Why it is pending or failed, or a partial-send note.
    pub reason: Option<String>,
}

/// Pure: map the AUTHORITATIVE final `write_line` outcome (D6) of a dispatch's
/// injection sequence into the `(audit_decision, DispatchOutcome)` pair. Kept
/// separate so the never-Err-after-reserve mapping is unit-testable. `sent_any`
/// records whether at least one earlier step's text+submit reached the pane.
///
/// NB: this NEVER yields an `Err`; the task is already reserved, and a retry would
/// re-inject. A partial inject reports `dispatched` (the agent has input and may be
/// acting); a clean miss reports `failed` (the card stays pending;
/// safe to retry).
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
                state: "sent",
                worktree: worktree.to_string(),
                reason: None,
            },
        ),
        // Partial: earlier lines already reached the pane (the agent may be acting).
        // Report dispatched so the caller doesn't re-dispatch; record for diagnostics.
        WriteLineOutcome::Queued | WriteLineOutcome::Error if sent_any => (
            "dispatched",
            DispatchOutcome {
                dispatched: true,
                state: "sent",
                worktree: worktree.to_string(),
                reason: Some(format!("{worktree}: partially injected ({detail})")),
            },
        ),
        // Nothing reached the pane → the card stays pending on its lane; safe to retry.
        WriteLineOutcome::Queued | WriteLineOutcome::Error => (
            "failed",
            DispatchOutcome {
                dispatched: false,
                state: "failed",
                worktree: worktree.to_string(),
                reason: Some(format!(
                    "could not send to @{worktree} ({detail}); it stays pending"
                )),
            },
        ),
    }
}

/// The detail recorded for an injection's final `write_line` result: empty on
/// success; for a missing session of a terminated worktree, how to recover.
fn injection_detail(result: &Result<(), PtyBackendError>, terminated: bool) -> String {
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
const SUBMIT_DELAY: Duration = Duration::from_millis(60);

/// Delay between dispatch steps so the target TUI finishes processing one submitted
/// line before the next arrives. Empirically tunable.
const INTER_STEP_DELAY: Duration = Duration::from_millis(80);

/// Pure: map every control char (incl. ESC, BEL, newline, tab) to a space, THEN
/// collapse whitespace runs — so an interpolated title/description can't submit the
/// line early or smuggle an escape sequence. `split_whitespace` alone leaves
/// non-whitespace C0 controls like ESC intact, hence the explicit map first.
fn collapse(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Pure: Keitora's built-in dispatch prompt, used when no custom `[dispatch].steps`
/// are configured. The agent ends in `review`, not `done`: the task keeps its lane
/// until a human has reviewed the work and marks it done. The submit byte is appended
/// separately at injection time.
fn default_line(ctx: &DispatchContext) -> String {
    let id = ctx.task.id;
    format!(
        "Please pick up Keitora task #{id}. Run `./keitora task claim {id}` to claim it, then implement it: {title}. {desc}. When your work is ready for review, run `./keitora task review {id}` and stop. Don't run `task done`; a human does that after review.",
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
    match cli {
        CliKind::Claude => &cfg.claude_steps,
        CliKind::Codex => &cfg.codex_steps,
    }
}

/// Every lane input the rules read, loaded once per apply or pump under the gate.
struct Lanes {
    tasks: Vec<TaskWithDeps>,
    /// Ids of cards already typed into their lane's session and not yet claimed.
    sent: HashSet<i64>,
    sessions: Vec<WorktreeSession>,
}

impl Lanes {
    async fn load(
        backend: &dyn TaskBackend,
        config: &ProjectConfig,
        client: &SharedBackend,
    ) -> Result<Self, String> {
        // Live state via the shared reconciled view, so the idle rule matches
        // `sessions_list`. The AUTHORITATIVE liveness check is still the daemon's
        // `write_line` result when a send happens (D6).
        let live = client.live_set_snapshot();
        let bg = background_set(client.as_ref()); // on-demand, exact; empty on RPC error
        let sessions = crate::tasks::reconciled_sessions(&live, &bg, &config.project.name, config)?;
        let filter = ListFilter {
            archive_days: config.tasks.archive_days,
            ..ListFilter::default()
        };
        let tasks = backend.list(&filter).await.map_err(|e| e.to_string())?;
        let sent = backend
            .dispatched_reservations()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| r.task_id)
            .collect();
        Ok(Lanes {
            tasks,
            sent,
            sessions,
        })
    }

    /// `(state, last_heartbeat)` of `worktree`'s session; `offline` when it has none.
    fn session(&self, worktree: &str) -> (String, String) {
        self.sessions
            .iter()
            .find(|s| s.worktree == worktree)
            .map(|s| (s.state.clone(), s.last_heartbeat.clone()))
            .unwrap_or_else(|| ("offline".to_string(), String::new()))
    }

    fn hold(&self, worktree: &str) -> Option<LaneHold> {
        lane_hold(&self.tasks, worktree, &self.session(worktree).0, &self.sent)
    }

    /// The lane's pending card ids, in send order.
    fn queue(&self, worktree: &str) -> Vec<i64> {
        lane_queue(&self.tasks, worktree, &self.sent)
            .iter()
            .map(|t| t.task.id)
            .collect()
    }

    /// Record a fresh reserve so the rules see it without a re-read.
    fn upsert(&mut self, twd: TaskWithDeps) {
        match self.tasks.iter_mut().find(|t| t.task.id == twd.task.id) {
            Some(t) => *t = twd,
            None => self.tasks.push(twd),
        }
    }
}

/// What a lane did when asked to send its front pending card.
#[derive(Debug, Clone, PartialEq)]
enum FrontSend {
    /// The lane isn't free, has nothing pending, or (auto-send) its front card failed
    /// and the session hasn't reported since.
    Nothing,
    /// The card's prompt reached the session (`reason`: a partial-send note).
    Sent {
        task_id: i64,
        reason: Option<String>,
    },
    /// The send errored and nothing reached the pane; the card stays pending.
    Failed { task_id: i64, reason: String },
}

/// Pure: whether the auto-send should skip `task_id` on a lane because its last send
/// failed and the session (now at `heartbeat`) hasn't reported since.
fn skip_failed(memo: &SendMemo, key: &(String, String), task_id: i64, heartbeat: &str) -> bool {
    memo.failed
        .get(key)
        .is_some_and(|(id, hb)| *id == task_id && hb == heartbeat)
}

/// Send `worktree`'s front pending card if the lane is free. `auto` is the
/// watcher-driven send, which skips a card whose last send failed until the session
/// reports again ([`SendMemo`]); a manual dispatch always tries. Logs the attempt.
#[allow(clippy::too_many_arguments)]
async fn send_front(
    backend: &dyn TaskBackend,
    config: &ProjectConfig,
    lanes: &mut Lanes,
    worktree: &str,
    client: &SharedBackend,
    memo: &mut SendMemo,
    user: &keitora_tasks::UserConfig,
    auto: bool,
) -> FrontSend {
    if lanes.hold(worktree).is_some() {
        return FrontSend::Nothing;
    }
    let Some(&task_id) = lanes.queue(worktree).first() else {
        return FrontSend::Nothing;
    };
    let project = config.project.name.clone();
    let memo_key = (project.clone(), worktree.to_string());
    let heartbeat = lanes.session(worktree).1;
    if auto && skip_failed(memo, &memo_key, task_id, &heartbeat) {
        return FrontSend::Nothing;
    }
    let Some(task) = lanes
        .tasks
        .iter()
        .find(|t| t.task.id == task_id)
        .map(|t| t.task.clone())
    else {
        return FrontSend::Nothing;
    };

    let ctx = DispatchContext {
        task: &task,
        worktree,
    };
    // Resolve the target worktree's CLI so the injected ritual matches it: worktree
    // entry `cli` → user `default_cli` → `CliKind::Claude`. Only reached for a live,
    // idle session (the lane is free), which for Codex needs its own liveness signal
    // (`.codex/hooks.json`).
    let entry = config
        .worktree_entries()
        .into_iter()
        .find(|e| e.name == worktree);
    let resolved_cli = keitora_tasks::resolve_cli(entry.as_ref(), user);
    let steps = render_steps(select_ritual_steps(&config.dispatch, resolved_cli), &ctx);
    let key = SessionKey {
        project: project.clone(),
        worktree: worktree.to_string(),
    };
    // Each step goes out as one atomic daemon `WriteLine` (text, pause, `\r`). The
    // result is AUTHORITATIVE for liveness (D6); see `injection_outcome`.
    let (result, sent_any) = inject_steps(
        client.as_ref(),
        &key,
        &steps,
        SUBMIT_DELAY.as_millis() as u32,
        INTER_STEP_DELAY,
    );
    let terminated = client.terminated_keys(&project).contains(worktree);
    let detail = injection_detail(&result, terminated);
    let (decision, outcome) =
        injection_outcome(map_write_line_result(&result), sent_any, worktree, &detail);
    // Best-effort audit: the task is reserved either way, so a failed log write must
    // not turn into an error (a retry would re-inject).
    let _ = backend
        .dispatch_log_add(task_id, worktree, decision, &detail)
        .await;
    if outcome.dispatched {
        lanes.sent.insert(task_id);
        memo.failed.remove(&memo_key);
        FrontSend::Sent {
            task_id,
            reason: outcome.reason,
        }
    } else {
        log::warn!("dispatch: #{task_id} → {project}/{worktree} failed: {detail}");
        memo.failed.insert(memo_key, (task_id, heartbeat));
        FrontSend::Failed {
            task_id,
            reason: outcome.reason.unwrap_or_default(),
        }
    }
}

/// Who is asking for a dispatch, which decides the up-front task checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyMode {
    /// A board drag, the card's Dispatch button or menu: a pending (unsent) card may
    /// be moved here from another lane.
    Board,
    /// A CLI dispatch request from `tasks.db`: the worktree name is checked, and a
    /// card reserved to another lane is refused (release it first).
    Request,
}

/// Assign `task_id` to `worktree` (in the board's view, drop it on that lane) and
/// send the lane's front card if the lane is free. The card is assigned and shown
/// pending at once whatever the session state; refusals are an `Err` with nothing
/// changed.
#[tauri::command]
pub async fn dispatch_apply(
    worktree: String,
    task_id: i64,
    client: State<'_, SharedBackend>,
    gate: State<'_, DispatchGate>,
    registry: State<'_, TransferRegistry>,
) -> Result<DispatchOutcome, String> {
    apply(
        Some(&worktree),
        task_id,
        ApplyMode::Board,
        None,
        client.inner(),
        &gate,
        &registry,
    )
    .await
}

/// One-click Dispatch: auto-pick a worktree from the pool ([`pick_worktree`]) and
/// apply as [`dispatch_apply`] does.
#[tauri::command]
pub async fn dispatch_auto(
    task_id: i64,
    client: State<'_, SharedBackend>,
    gate: State<'_, DispatchGate>,
    registry: State<'_, TransferRegistry>,
) -> Result<DispatchOutcome, String> {
    apply(
        None,
        task_id,
        ApplyMode::Board,
        None,
        client.inner(),
        &gate,
        &registry,
    )
    .await
}

/// The body of [`dispatch_apply`] / [`dispatch_auto`], shared with the CLI request
/// executor ([`run_dispatch_requests`]). `worktree: None` (or empty) auto-picks.
/// `project` is the project to dispatch in; `None` means the active one (the
/// board's).
async fn apply(
    worktree: Option<&str>,
    task_id: i64,
    mode: ApplyMode,
    project: Option<ProjectConfig>,
    client: &SharedBackend,
    gate: &DispatchGate,
    registry: &TransferRegistry,
) -> Result<DispatchOutcome, String> {
    // Hold the gate for the whole apply so checks → reserve → send → log are atomic
    // vs. every other dispatch. Cancel-safe and non-poisoning.
    let mut memo = gate.0.lock().await;

    let (backend, config) = match project {
        Some(pc) => (
            keitora_tasks::open_backend(&pc).map_err(|e| e.to_string())?,
            pc,
        ),
        None => crate::tasks::open_backend()?,
    };
    let backend = backend.as_ref();
    let mut lanes = Lanes::load(backend, &config, client).await?;
    let project = config.project.name.clone();

    let worktree = match worktree.filter(|w| !w.is_empty()) {
        Some(w) => w.to_string(),
        None => auto_pick(backend, &config, &lanes, registry, task_id).await?,
    };
    // Before anything is reserved: a worktree mid-transfer is never injected into.
    if let Some(reason) = transfer_refusal(registry, &project, &worktree) {
        return Err(reason);
    }
    if !config.in_dispatch_pool(&worktree) {
        return Err(pool_refusal(&worktree));
    }

    let twd = backend.get(task_id).await.map_err(|e| e.to_string())?;
    let refusal = match mode {
        ApplyMode::Request => {
            request_refusal(twd.as_ref(), task_id, &worktree, &config.worktrees())
        }
        ApplyMode::Board => board_refusal(twd.as_ref(), task_id, &worktree, &lanes.sent),
    };
    if let Some(reason) = refusal {
        return Err(reason);
    }
    let twd = twd.ok_or_else(|| format!("task #{task_id} not found"))?;

    // A pending card dragged from another lane moves: release it there first.
    if let Some(from) = twd.task.assigned_to.as_deref().filter(|a| *a != worktree) {
        backend
            .release(task_id, "keitora")
            .await
            .map_err(|e| e.to_string())?;
        let _ = backend
            .dispatch_log_add(task_id, from, "released", &format!("moved to @{worktree}"))
            .await;
    }

    // Idempotency guard + reserve in one call. A card already sent to this lane (and
    // not yet claimed) is `AlreadyDispatched` with NO write, so a re-drop no-ops.
    // Refusals (blocked / terminal / assigned elsewhere) bubble up from the reserve,
    // BEFORE any write.
    match backend
        .dispatch_reserve(task_id, "keitora", &worktree)
        .await
        .map_err(|e| e.to_string())?
    {
        ReserveOutcome::AlreadyDispatched => {
            return Ok(DispatchOutcome {
                dispatched: true,
                state: "already_sent",
                reason: Some(format!(
                    "already sent to @{worktree}; waiting for it to claim the task"
                )),
                worktree,
            });
        }
        ReserveOutcome::Reserved(task) => lanes.upsert(TaskWithDeps { task, ..twd }),
    }

    // From here on NEVER return Err: the task is reserved, and a retry would re-send.
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    let front = send_front(
        backend, &config, &mut lanes, &worktree, client, &mut memo, &user, false,
    )
    .await;
    let outcome = match front {
        FrontSend::Sent {
            task_id: id,
            reason,
        } if id == task_id => DispatchOutcome {
            dispatched: true,
            state: "sent",
            worktree,
            reason,
        },
        FrontSend::Failed {
            task_id: id,
            reason,
        } if id == task_id => DispatchOutcome {
            dispatched: false,
            state: "failed",
            worktree,
            reason: Some(reason),
        },
        other => {
            let queue = lanes.queue(&worktree);
            let ahead = queue
                .iter()
                .position(|&id| id == task_id)
                .unwrap_or(queue.len());
            let hold = lanes.hold(&worktree);
            let mut reason = pending_reason(&worktree, hold.as_ref(), ahead);
            if hold == Some(LaneHold::Offline)
                && client.terminated_keys(&project).contains(&worktree)
            {
                reason.push_str(" (its session is terminated; launch it to dispatch)");
            }
            let _ = backend
                .dispatch_log_add(task_id, &worktree, "queued", &reason)
                .await;
            pending_outcome(worktree, reason, &other)
        }
    };
    Ok(outcome)
}

/// Pure: the outcome for a card left pending on `worktree`. If the lane's front card
/// (ahead of it) failed to send just now, that is reported as a failure too.
fn pending_outcome(worktree: String, reason: String, front: &FrontSend) -> DispatchOutcome {
    match front {
        FrontSend::Failed {
            task_id,
            reason: why,
        } => DispatchOutcome {
            dispatched: false,
            state: "failed",
            worktree,
            reason: Some(format!("{reason}; sending #{task_id} failed: {why}")),
        },
        _ => DispatchOutcome {
            dispatched: false,
            state: "pending",
            worktree,
            reason: Some(reason),
        },
    }
}

/// Auto-pick a pool worktree for `task_id` ([`pick_worktree`]). A card already pending
/// on a pool lane stays there (Dispatch on it retries that lane). Worktrees mid-
/// transfer are skipped.
async fn auto_pick(
    backend: &dyn TaskBackend,
    config: &ProjectConfig,
    lanes: &Lanes,
    registry: &TransferRegistry,
    task_id: i64,
) -> Result<String, String> {
    let project = &config.project.name;
    let pool: Vec<String> = config
        .dispatch_pool()
        .into_iter()
        .filter(|w| transfer_refusal(registry, project, w).is_none())
        .collect();
    if pool.is_empty() {
        return Err(
            "no worktree is in the dispatch pool; add one from its worktree menu".to_string(),
        );
    }
    if let Some(here) = lanes
        .tasks
        .iter()
        .find(|t| t.task.id == task_id && t.task.status == "open")
        .and_then(|t| t.task.assigned_to.clone())
        .filter(|a| pool.contains(a))
    {
        return Ok(here);
    }
    let log = backend.dispatch_log_list(500).await.unwrap_or_default();
    let candidates: Vec<PoolLane> = pool
        .iter()
        .map(|w| PoolLane {
            worktree: w.clone(),
            free: lanes.hold(w).is_none(),
            queue_len: lanes.queue(w).len(),
            last_dispatched: log
                .iter()
                .filter(|e| e.worktree == *w && e.decision == "dispatched")
                .map(|e| e.created_at.clone())
                .max(),
        })
        .collect();
    pick_worktree(&candidates)
        .map(|l| l.worktree.clone())
        .ok_or_else(|| "no worktree is in the dispatch pool".to_string())
}

/// The refusal for a worktree outside the dispatch pool.
fn pool_refusal(worktree: &str) -> String {
    format!(
        "@{worktree} is not in the dispatch pool, so it takes no tasks; add it from its worktree menu"
    )
}

/// Pure: why a board dispatch of `task_id` onto `worktree` must be refused before
/// anything is reserved, or `None` to go ahead. As [`request_refusal`]'s task checks,
/// except that a card pending (assigned, not yet sent) on another lane may move.
fn board_refusal(
    task: Option<&TaskWithDeps>,
    task_id: i64,
    worktree: &str,
    sent: &HashSet<i64>,
) -> Option<String> {
    let Some(task) = task else {
        return Some(format!("task #{task_id} not found"));
    };
    let t = &task.task;
    match t.assigned_to.as_deref() {
        Some(other) if other != worktree && t.status == "open" => {
            if sent.contains(&task_id) {
                return Some(format!(
                    "task #{task_id} was already sent to @{other}; release it first"
                ));
            }
            // Movable: judge the rest as if unassigned.
            let mut probe = task.clone();
            probe.task.assigned_to = None;
            keitora_tasks::dispatch::task_refusal(&probe, worktree)
        }
        _ => keitora_tasks::dispatch::task_refusal(task, worktree),
    }
}

/// Pure: why a CLI dispatch request for `task_id` onto `worktree` must be refused
/// before anything is reserved, or `None` to go ahead. `configured` is the project's
/// worktree list; an empty list doesn't restrict, matching how `reconciled_sessions`
/// scopes. The session state is not checked: like a board drop, a request assigns the
/// card at once and it waits on the lane as pending.
fn request_refusal(
    task: Option<&TaskWithDeps>,
    task_id: i64,
    worktree: &str,
    configured: &[String],
) -> Option<String> {
    if !configured.is_empty() && !configured.iter().any(|w| w == worktree) {
        return Some(format!(
            "unknown worktree `{worktree}`; expected one of: {}",
            configured.join(", ")
        ));
    }
    let Some(task) = task else {
        return Some(format!("task #{task_id} not found"));
    };
    keitora_tasks::dispatch::task_refusal(task, worktree)
}

/// How old a pending dispatch request may be before the app refuses it rather than
/// acting on it: a request made while the app was closed shouldn't fire whenever it
/// next opens.
const REQUEST_MAX_AGE_SECS: i64 = 600;

/// Pure: the `(status, reason)` a dispatch request resolves to, from its apply
/// result. Any `Ok` is `applied` (the task is on the lane: sent, pending, or pending
/// after a failed send, said in the reason); an `Err` is `refused` (nothing changed).
fn request_outcome(result: &Result<DispatchOutcome, String>) -> (&'static str, String) {
    match result {
        Ok(o) => ("applied", o.reason.clone().unwrap_or_default()),
        Err(e) => ("refused", e.clone()),
    }
}

/// Called from the tasks-DB watchers on each debounced write of registered project
/// `project`'s DB (`local`): start [`run_dispatch_requests`] if CLI dispatch requests
/// are pending, else [`pump_lanes`] if any card is reserved to a lane. Both checks
/// are cheap indexed reads.
pub(crate) fn spawn_pending_requests<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    local: &keitora_tasks::LocalStore,
    project: &str,
) {
    let has_pending = local
        .dispatch_requests_pending()
        .is_ok_and(|p| !p.is_empty());
    if has_pending {
        // Each request's apply sends what it can; the writes it makes wake the
        // watcher again, which then pumps the rest.
        tauri::async_runtime::spawn(run_dispatch_requests(app.clone(), project.to_string()));
    } else if local.has_reserved_tasks() {
        tauri::async_runtime::spawn(pump_lanes(app.clone(), project.to_string()));
    }
}

/// The automatic send: for each pool worktree of registered project `project`, send
/// the lane's front pending card if the lane is free. This is what makes a pending
/// card go out when its session turns idle, its active task finishes, or the card
/// ahead of it is claimed.
pub(crate) async fn pump_lanes<R: tauri::Runtime>(app: tauri::AppHandle<R>, project: String) {
    use tauri::Manager;

    let (Some(client), Some(gate), Some(registry)) = (
        app.try_state::<SharedBackend>(),
        app.try_state::<DispatchGate>(),
        app.try_state::<TransferRegistry>(),
    ) else {
        return; // still starting up; the next DB event retries
    };
    let Some(config) = keitora_tasks::UserConfig::load()
        .ok()
        .and_then(|u| u.project_config_by_name(&project))
    else {
        return;
    };
    let mut memo = gate.0.lock().await;
    let backend = match keitora_tasks::open_backend(&config) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("auto-send: cannot open {project}'s tasks: {e}");
            return;
        }
    };
    let mut lanes = match Lanes::load(backend.as_ref(), &config, client.inner()).await {
        Ok(l) => l,
        Err(e) => {
            log::warn!("auto-send: cannot read {project}'s lanes: {e}");
            return;
        }
    };
    let user = keitora_tasks::UserConfig::load().unwrap_or_default();
    for worktree in config.dispatch_pool() {
        if transfer_refusal(&registry, &project, &worktree).is_some() {
            continue;
        }
        let sent = send_front(
            backend.as_ref(),
            &config,
            &mut lanes,
            &worktree,
            client.inner(),
            &mut memo,
            &user,
            true,
        )
        .await;
        if let FrontSend::Sent { task_id, .. } = sent {
            log::info!("auto-send: #{task_id} → {project}/{worktree}");
        }
    }
}

/// How often [`pump_tick`] re-checks the lanes of every registered project.
const PUMP_TICK: Duration = Duration::from_secs(10);

/// Run [`pump_lanes`] for every registered project with a card reserved to a lane,
/// every [`PUMP_TICK`]. The tasks-DB watchers cover session hooks and task writes;
/// this covers what changes liveness without a DB write (a pane coming up, a herdr
/// agent leaving a dialog). Cheap when nothing is pending: one indexed read per
/// project.
pub(crate) async fn pump_tick<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    loop {
        tokio::time::sleep(PUMP_TICK).await;
        let Ok(user) = keitora_tasks::UserConfig::load() else {
            continue;
        };
        let projects: Vec<String> = user
            .projects
            .values()
            .filter_map(|pref| keitora_tasks::effective_config(pref).ok())
            .filter(|pc| {
                keitora_tasks::LocalStore::open(&pc.tasks.db_path.to_string_lossy())
                    .is_ok_and(|l| l.has_reserved_tasks())
            })
            .map(|pc| pc.project.name)
            .collect();
        for project in projects {
            pump_lanes(app.clone(), project).await;
        }
    }
}

/// Execute `project`'s pending CLI dispatch requests (`keitora task dispatch`),
/// oldest first, each through [`apply`] in request mode, and record each outcome on
/// its row. A request with no worktree is auto-picked, and its row is pointed at the
/// picked worktree. `project` is a resolved `[project].name` from the app's registry.
///
/// Safe to run concurrently with itself and with board dispatches: each request is
/// taken with a compare-and-set (`dispatch_request_begin`), so it executes once,
/// and every apply holds the [`DispatchGate`].
pub(crate) async fn run_dispatch_requests<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    project: String,
) {
    use tauri::Manager;

    let (Some(client), Some(gate), Some(registry)) = (
        app.try_state::<SharedBackend>(),
        app.try_state::<DispatchGate>(),
        app.try_state::<TransferRegistry>(),
    ) else {
        return; // still starting up; the next DB event retries
    };
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
            Some(&req.worktree),
            req.task_id,
            ApplyMode::Request,
            Some(config.clone()),
            client.inner(),
            &gate,
            &registry,
        )
        .await;
        let target = match &result {
            Ok(o) => o.worktree.clone(),
            Err(_) => req.worktree.clone(),
        };
        if target != req.worktree {
            let _ = local.dispatch_request_retarget(req.id, &target);
        }
        let (status, reason) = request_outcome(&result);
        log::info!(
            "dispatch request {} ({project}: task #{} → {}, by {}): {status} {reason}",
            req.id,
            req.task_id,
            if target.is_empty() { "(auto)" } else { &target },
            req.requested_by
        );
        if let Err(e) = local.dispatch_request_resolve(req.id, status, &reason) {
            log::warn!("dispatch request {}: cannot record outcome: {e}", req.id);
        }
    }
}

/// `Some(error)` when `worktree` of `project` is mid-transfer; dispatch must not
/// inject into it.
fn transfer_refusal(registry: &TransferRegistry, project: &str, worktree: &str) -> Option<String> {
    registry.is_transferring(project, worktree).then(|| {
        format!("a transfer is in progress for worktree `{worktree}`; try again when it finishes")
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
    fn transfer_refusal_message_is_exact() {
        let r = TransferRegistry::new();
        let _guard = r.begin("proj-a", "alpha").unwrap();
        assert_eq!(
            transfer_refusal(&r, "proj-a", "alpha").as_deref(),
            Some("a transfer is in progress for worktree `alpha`; try again when it finishes")
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
    fn request_refusal_checks_the_task_not_the_session() {
        let wts = vec!["alpha".to_string(), "bravo".to_string()];
        let card = open_card(None, vec![]);
        // Any session state goes ahead: the card is assigned and waits as pending.
        assert_eq!(request_refusal(Some(&card), 5, "alpha", &wts), None);
        // A blocked task is refused.
        assert_eq!(
            request_refusal(Some(&open_card(None, vec![2])), 5, "alpha", &wts),
            Some("task #5 is blocked by #2".to_string())
        );
        // Reserved elsewhere is refused; reserved here goes ahead (retries a pending one).
        assert!(
            request_refusal(Some(&open_card(Some("bravo"), vec![])), 5, "alpha", &wts).is_some()
        );
        assert_eq!(
            request_refusal(Some(&open_card(Some("alpha"), vec![])), 5, "alpha", &wts),
            None
        );
        // Backlog is refused with how to bring it back.
        let mut backlog = open_card(None, vec![]);
        backlog.task.status = "backlog".to_string();
        let msg = request_refusal(Some(&backlog), 5, "alpha", &wts).unwrap();
        assert!(msg.contains("backlog"), "{msg}");
    }

    #[test]
    fn board_refusal_lets_a_pending_card_move_but_not_a_sent_one() {
        let none = HashSet::new();
        let pending_elsewhere = open_card(Some("bravo"), vec![]);
        assert_eq!(
            board_refusal(Some(&pending_elsewhere), 5, "alpha", &none),
            None
        );
        let sent: HashSet<i64> = [5].into();
        assert_eq!(
            board_refusal(Some(&pending_elsewhere), 5, "alpha", &sent).as_deref(),
            Some("task #5 was already sent to @bravo; release it first")
        );
        // A blocked card can't move either.
        assert!(
            board_refusal(Some(&open_card(Some("bravo"), vec![3])), 5, "alpha", &none).is_some()
        );
        // Same lane: a retry.
        assert_eq!(
            board_refusal(Some(&open_card(Some("alpha"), vec![])), 5, "alpha", &none),
            None
        );
        assert_eq!(
            board_refusal(None, 5, "alpha", &none).as_deref(),
            Some("task #5 not found")
        );
        let mut active = open_card(Some("bravo"), vec![]);
        active.task.status = "active".to_string();
        assert_eq!(
            board_refusal(Some(&active), 5, "alpha", &none).as_deref(),
            Some("task #5 is already active on @bravo")
        );
    }

    #[test]
    fn pool_refusal_names_the_worktree_and_the_fix() {
        let msg = pool_refusal("fellowship");
        assert!(
            msg.starts_with("@fellowship is not in the dispatch pool"),
            "{msg}"
        );
        assert!(msg.contains("worktree menu"), "{msg}");
    }

    #[test]
    fn pending_outcome_reports_a_failed_front_card() {
        let o = pending_outcome(
            "alpha".into(),
            "pending: behind 1 card on @alpha".into(),
            &FrontSend::Nothing,
        );
        assert_eq!(o.state, "pending");
        assert!(!o.dispatched);
        let o = pending_outcome(
            "alpha".into(),
            "pending: behind 1 card on @alpha".into(),
            &FrontSend::Failed {
                task_id: 3,
                reason: "herdr unreachable".into(),
            },
        );
        assert_eq!(o.state, "failed");
        assert_eq!(
            o.reason.as_deref(),
            Some("pending: behind 1 card on @alpha; sending #3 failed: herdr unreachable")
        );
    }

    // -----------------------------------------------------------------------
    // send_front — the send-on-idle path against a real SQLite DB + FakeBackend.
    // -----------------------------------------------------------------------

    struct SendFixture {
        _dir: tempfile::TempDir,
        backend: keitora_tasks::SqliteBackend,
        config: ProjectConfig,
        fake: std::sync::Arc<crate::backend::fake::FakeBackend>,
        client: SharedBackend,
    }

    fn send_fixture() -> SendFixture {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tasks.db");
        let backend = keitora_tasks::SqliteBackend::open(&db.to_string_lossy()).unwrap();
        let mut config = ProjectConfig::scaffold(&dir.path().join("proj"));
        config.project.name = "proj".to_string();
        let fake = std::sync::Arc::new(crate::backend::fake::FakeBackend::new());
        let client: SharedBackend = fake.clone();
        SendFixture {
            _dir: dir,
            backend,
            config,
            fake,
            client,
        }
    }

    fn session(worktree: &str, state: &str, heartbeat: &str) -> WorktreeSession {
        WorktreeSession {
            project: String::new(),
            worktree: worktree.to_string(),
            state: state.to_string(),
            current_task_id: None,
            pid: None,
            last_heartbeat: heartbeat.to_string(),
        }
    }

    async fn reserve(f: &SendFixture, title: &str, priority: &str, wt: &str) -> i64 {
        let t = f
            .backend
            .create("me", title, "d", priority, &[], None)
            .await
            .unwrap();
        f.backend.assign(t.id, "keitora", wt).await.unwrap();
        t.id
    }

    async fn lanes_for(f: &SendFixture, sessions: Vec<WorktreeSession>) -> Lanes {
        let tasks = f.backend.list(&ListFilter::default()).await.unwrap();
        let sent = f
            .backend
            .dispatched_reservations()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.task_id)
            .collect();
        Lanes {
            tasks,
            sent,
            sessions,
        }
    }

    async fn front(
        f: &SendFixture,
        lanes: &mut Lanes,
        memo: &mut SendMemo,
        auto: bool,
    ) -> FrontSend {
        send_front(
            &f.backend,
            &f.config,
            lanes,
            "alpha",
            &f.client,
            memo,
            &keitora_tasks::UserConfig::default(),
            auto,
        )
        .await
    }

    #[tokio::test]
    async fn an_idle_lane_sends_its_front_card_once() {
        let f = send_fixture();
        let low = reserve(&f, "low one", "low", "alpha").await;
        let urgent = reserve(&f, "urgent one", "urgent", "alpha").await;
        let mut memo = SendMemo::default();
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t1")]).await;
        // The urgent card is the front of the lane, so it goes first.
        assert_eq!(
            front(&f, &mut lanes, &mut memo, true).await,
            FrontSend::Sent {
                task_id: urgent,
                reason: None
            }
        );
        let writes = write_line_calls(&f.fake);
        assert_eq!(writes.len(), 1);
        assert!(
            writes[0].1.contains(&format!("task #{urgent}")),
            "{:?}",
            writes[0]
        );
        // Logged as dispatched: the board shows it sent.
        let sent = f.backend.dispatched_reservations().await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].task_id, urgent);
        // The lane now waits for the claim: nothing more is typed, even on a re-read.
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t2")]).await;
        assert_eq!(lanes.hold("alpha"), Some(LaneHold::AwaitingClaim(urgent)));
        assert_eq!(
            front(&f, &mut lanes, &mut memo, true).await,
            FrontSend::Nothing
        );
        assert_eq!(write_line_calls(&f.fake).len(), 1);
        // Claimed: the agent is working, so the idle lane still holds.
        f.backend.claim(urgent, "alpha").await.unwrap();
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t3")]).await;
        assert_eq!(lanes.hold("alpha"), Some(LaneHold::Working(urgent)));
        assert_eq!(
            front(&f, &mut lanes, &mut memo, true).await,
            FrontSend::Nothing
        );
        // Done → the next card (the low one) goes out.
        f.backend.set_done(urgent, "alpha").await.unwrap();
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t4")]).await;
        assert_eq!(
            front(&f, &mut lanes, &mut memo, true).await,
            FrontSend::Sent {
                task_id: low,
                reason: None
            }
        );
    }

    #[tokio::test]
    async fn a_busy_or_offline_lane_sends_nothing() {
        let f = send_fixture();
        reserve(&f, "t", "normal", "alpha").await;
        let mut memo = SendMemo::default();
        for sessions in [vec![session("alpha", "busy", "t1")], vec![]] {
            let mut lanes = lanes_for(&f, sessions).await;
            assert_eq!(
                front(&f, &mut lanes, &mut memo, false).await,
                FrontSend::Nothing
            );
        }
        assert!(write_line_calls(&f.fake).is_empty());
    }

    #[tokio::test]
    async fn a_failed_send_stays_pending_and_auto_retries_on_the_next_report() {
        let f = send_fixture();
        let id = reserve(&f, "t", "normal", "alpha").await;
        let mut memo = SendMemo::default();
        f.fake.push_write_line(Err(PtyBackendError::NotConnected));
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t1")]).await;
        match front(&f, &mut lanes, &mut memo, true).await {
            FrontSend::Failed { task_id, reason } => {
                assert_eq!(task_id, id);
                assert!(reason.contains("herdr unreachable"), "{reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        // Logged as failed (the board shows it), and still reserved, not sent.
        let log = f.backend.dispatch_log_for(id).await.unwrap();
        assert_eq!(log[0].decision, "failed");
        assert!(f
            .backend
            .dispatched_reservations()
            .await
            .unwrap()
            .is_empty());
        // Same heartbeat: the auto-send leaves it alone (no write loop)...
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t1")]).await;
        assert_eq!(
            front(&f, &mut lanes, &mut memo, true).await,
            FrontSend::Nothing
        );
        assert_eq!(write_line_calls(&f.fake).len(), 1);
        // ...a manual retry doesn't wait...
        f.fake.push_write_line(Err(PtyBackendError::NotConnected));
        assert!(matches!(
            front(&f, &mut lanes, &mut memo, false).await,
            FrontSend::Failed { .. }
        ));
        assert_eq!(write_line_calls(&f.fake).len(), 2);
        // ...and a new session report lets the auto-send try again.
        let mut lanes = lanes_for(&f, vec![session("alpha", "idle", "t2")]).await;
        assert!(matches!(
            front(&f, &mut lanes, &mut memo, true).await,
            FrontSend::Sent { task_id, .. } if task_id == id
        ));
        assert_eq!(write_line_calls(&f.fake).len(), 3);
    }

    #[test]
    fn auto_send_skips_a_failed_card_until_the_session_reports_again() {
        let mut memo = SendMemo::default();
        let key = ("p".to_string(), "alpha".to_string());
        assert!(!skip_failed(&memo, &key, 5, "t1"));
        memo.failed.insert(key.clone(), (5, "t1".to_string()));
        assert!(skip_failed(&memo, &key, 5, "t1"), "same heartbeat: wait");
        assert!(!skip_failed(&memo, &key, 5, "t2"), "new heartbeat: retry");
        assert!(!skip_failed(&memo, &key, 6, "t1"), "another card: try it");
        let other = ("p".to_string(), "bravo".to_string());
        assert!(!skip_failed(&memo, &other, 5, "t1"));
    }

    #[test]
    fn request_refusal_checks_worktree_then_task() {
        let wts = vec!["alpha".to_string()];
        let card = open_card(None, vec![]);
        assert_eq!(
            request_refusal(Some(&card), 5, "zulu", &wts),
            Some("unknown worktree `zulu`; expected one of: alpha".to_string())
        );
        assert_eq!(
            request_refusal(None, 9, "alpha", &wts),
            Some("task #9 not found".to_string())
        );
        // No configured worktrees: no restriction, as in `reconciled_sessions`.
        assert_eq!(request_refusal(Some(&card), 5, "zulu", &[]), None);
    }

    #[test]
    fn request_outcome_maps_apply_results() {
        let ok = |dispatched: bool, state: &'static str, reason: Option<&str>| {
            Ok(DispatchOutcome {
                dispatched,
                state,
                worktree: "alpha".to_string(),
                reason: reason.map(str::to_string),
            })
        };
        assert_eq!(
            request_outcome(&ok(true, "sent", None)),
            ("applied", String::new())
        );
        // Pending (and pending after a failed send) is still applied: the card is on the lane.
        assert_eq!(
            request_outcome(&ok(false, "pending", Some("pending: @alpha is busy"))),
            ("applied", "pending: @alpha is busy".to_string())
        );
        assert_eq!(
            request_outcome(&ok(false, "failed", Some("could not send"))),
            ("applied", "could not send".to_string())
        );
        assert_eq!(
            request_outcome(&Err("task #5 is done".to_string())),
            ("refused", "task #5 is done".to_string())
        );
    }

    /// Gate test: a session that is `idle` per the DB but present in the background
    /// set reconciles to `busy`, and a busy lane holds — so the dispatcher never
    /// injects into a session running background work.
    #[test]
    fn idle_with_background_task_gates_dispatch() {
        use crate::backend::{apply_reconcile, SessionKey};
        use keitora_tasks::WorktreeSession;
        use std::collections::HashSet;

        // Build a session that the DB reports as idle.
        let session = WorktreeSession {
            project: String::new(),
            worktree: "sayo".to_string(),
            state: "idle".to_string(),
            current_task_id: Some(5),
            pid: Some(9999),
            last_heartbeat: "2026-06-22T00:00:00Z".to_string(),
        };

        let key = SessionKey {
            project: "proj".to_string(),
            worktree: "sayo".to_string(),
        };

        // Session is live (present in the daemon live-set) …
        let mut live: HashSet<SessionKey> = HashSet::new();
        live.insert(key.clone());

        // … and the daemon reports it is running a background task.
        let mut bg: HashSet<SessionKey> = HashSet::new();
        bg.insert(key.clone());

        // Stage-003 override: idle + bg in live-set → busy.
        let reconciled = apply_reconcile(&[session], &live, &bg, "proj");
        assert_eq!(
            reconciled[0].state, "busy",
            "idle+bg session must reconcile to busy"
        );

        // The lane rule: busy holds the lane, so the dispatcher never injects.
        assert_eq!(
            lane_hold(&[], "sayo", &reconciled[0].state, &HashSet::new()),
            Some(LaneHold::Busy("busy".to_string())),
            "a busy session must hold its lane even with a live PTY"
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
        // Built-in claim/review guidance: the agent stops at review, never done.
        assert!(line.contains("./keitora task claim 42"));
        assert!(line.contains("./keitora task review 42"));
        assert!(!line.contains("./keitora task done"));
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
        assert_eq!(decision, "failed");
        assert_eq!(outcome.state, "failed");
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
