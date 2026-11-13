// Task commands: thin Tauri wrappers over `keitora_tasks::TaskBackend` + `keitora_tasks::LocalStore`.
// The app and CLI are two clients of one SQLite file; all concurrency safety (atomic claim, dep
// gating) lives in the backend — we never fork task logic here. A backend + local store are
// opened per call (a cheap SQLite connection), matching how the CLI opens fresh per invocation,
// so we don't hold a non-Send connection in Tauri state.

use keitora_tasks::{
    BackendError, ClaimOutcome, DepNode, DispatchLogEntry, ListFilter, LocalStore, LogEntry,
    ProjectConfig, Reservation, Task, TaskBackend, TaskUpdate, TaskWithDeps, WorktreeSession,
};
use serde::Serialize;

/// Serializable, structured error returned by the task Tauri commands. Replaces the
/// blanket `Result<_, String>` so the frontend can branch on `kind` (e.g. show a
/// "not found" vs. a retryable "rate limited" UX) and surface `remediation` /
/// `retry_after_secs` hints carried by the typed [`BackendError`].
#[derive(Serialize)]
pub struct IpcError {
    /// Machine-readable variant tag: `not_found`, `conflict`, `config`,
    /// `unauthorized`, `unavailable`, `rate_limited`, `timeout`, `transient`,
    /// `permanent`, `serde`, or `internal` (non-backend / resolution failures).
    pub kind: String,
    /// Human-readable message. Safe to display.
    pub message: String,
    /// Optional user-facing remediation hint (e.g. how to fix credentials).
    pub remediation: Option<String>,
    /// Optional retry hint, in seconds, when the error is retryable.
    pub retry_after_secs: Option<u64>,
}

/// `Display` yields the (already redaction-aware) `message`.
impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl IpcError {
    /// A non-backend internal error (config resolution, store-open failures, etc.).
    fn internal(message: impl Into<String>) -> Self {
        IpcError {
            kind: "internal".to_string(),
            message: message.into(),
            remediation: None,
            retry_after_secs: None,
        }
    }
}

/// Map a typed [`BackendError`] onto the serializable [`IpcError`], preserving the
/// variant tag plus the retry / remediation hints.
fn to_ipc_error(e: &BackendError) -> IpcError {
    let kind = match e {
        BackendError::NotFound(_) => "not_found",
        BackendError::Conflict(_) => "conflict",
        BackendError::Config(_) => "config",
        BackendError::Unauthorized { .. } => "unauthorized",
        BackendError::Unavailable(_) => "unavailable",
        BackendError::RateLimited { .. } => "rate_limited",
        BackendError::Timeout => "timeout",
        BackendError::Transient(_) => "transient",
        BackendError::Permanent(_) => "permanent",
        BackendError::Serde(_) => "serde",
    };
    let remediation = match e {
        BackendError::Unauthorized { remediation, .. } => remediation.clone(),
        _ => None,
    };
    IpcError {
        kind: kind.to_string(),
        message: e.to_string(),
        remediation,
        retry_after_secs: e.retry_after().map(|d| d.as_secs()),
    }
}

/// Open the task backend at the active project's DB path. Uses registry-first
/// resolution (`resolve_active`) since the GUI has no meaningful cwd.
pub(crate) fn open_backend() -> Result<(Box<dyn TaskBackend>, ProjectConfig), String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let backend = keitora_tasks::open_backend(&config).map_err(|e| e.to_string())?;
    Ok((backend, config))
}

/// Worktree sessions reconciled against the daemon's live-set: a session whose
/// worktree has no live daemon PTY and isn't already offline is reported
/// `offline`, with its `current_task_id`/`pid` cleared (covers crashed sessions
/// the app never received a SessionEnd for). The single source of truth for the
/// idle rule, so the dispatcher and `sessions_list` agree on what "idle" means.
///
/// Task-state reconciliation of `current_task_id` (nulling the pointer when the
/// referenced task is no longer `active` or no longer owned by this worktree) now
/// happens inside `keitora_tasks::sessions_list`. This function layers daemon-liveness
/// on top — offline → null pointer — exactly as before. The actual downgrade is
/// `crate::backend::apply_reconcile`, keyed by the daemon's `SessionKey`s under the
/// active project namespace.
pub(crate) fn reconciled_sessions(
    live: &std::collections::HashSet<crate::backend::SessionKey>,
    bg: &std::collections::HashSet<crate::backend::SessionKey>,
    project: &str,
    config: &ProjectConfig,
) -> Result<Vec<WorktreeSession>, String> {
    // Open the LocalStore from the ALREADY-RESOLVED config's db_path so each command
    // resolves config + opens the local handle once (no second `resolve_active()` +
    // Connection — FIX 6).
    let local =
        LocalStore::open(&config.tasks.db_path.to_string_lossy()).map_err(|e| e.to_string())?;
    reconciled_sessions_in(&local, live, bg, project, config)
}

/// [`reconciled_sessions`] over an already-open store (e.g. a read-only handle).
pub(crate) fn reconciled_sessions_in(
    local: &LocalStore,
    live: &std::collections::HashSet<crate::backend::SessionKey>,
    bg: &std::collections::HashSet<crate::backend::SessionKey>,
    project: &str,
    config: &ProjectConfig,
) -> Result<Vec<WorktreeSession>, String> {
    // `worktree_sessions` rows carry NO project column, and the sessions DB
    // accumulates a row for every worktree that has ever run a session across ALL
    // projects. Scope to the ACTIVE project's configured worktrees so the board
    // shows this project's worktrees only — not a cross-project superset.
    let configured: std::collections::HashSet<String> = config.worktrees().into_iter().collect();
    let all = local.sessions_list().map_err(|e| e.to_string())?;
    // Empty configured set → don't filter: a project with no configured worktrees
    // (effective_worktrees None + an empty committed list) would otherwise blank the
    // board even when live sessions exist. Falling back to the unscoped list keeps
    // those visible; the scoping only kicks in once worktrees are actually configured.
    let sessions: Vec<WorktreeSession> = if configured.is_empty() {
        all
    } else {
        all.into_iter()
            .filter(|s| configured.contains(&s.worktree))
            .collect()
    };
    Ok(crate::backend::apply_reconcile(
        &sessions, live, bg, project,
    ))
}

/// The worktree this app acts as (assignee for claim/done/unclaim). Lets the UI
/// show owner-only actions (Unclaim/Done) on just the tasks it can mutate.
#[tauri::command(async)]
pub fn task_actor() -> String {
    keitora_tasks::current_worktree()
}

/// Live worktree session states. Reconciles against PTY ownership: a worktree the
/// app holds no live PTY for is reported `offline`, regardless of its last hook
/// write (covers crashed sessions the app never received a SessionEnd for).
/// Polled every few seconds, so the busy hints come from the backend's cached
/// session list and never spawn a server query.
#[tauri::command(async)]
pub fn sessions_list(
    client: tauri::State<'_, crate::backend::SharedBackend>,
) -> Result<Vec<WorktreeSession>, String> {
    // Resolve config ONCE and reuse it for both the project name and the LocalStore
    // open inside `reconciled_sessions` (FIX 6 — no duplicate resolve).
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let project = config.project.name.clone();
    let bg = crate::backend::cached_background_set(client.inner().as_ref());
    let mut sessions = reconciled_sessions(&client.live_set_snapshot(), &bg, &project, &config)?;
    // Stamp the active project onto each row so the frontend keys sessions by
    // `project/worktree` — DB rows carry no project column.
    for s in &mut sessions {
        s.project = project.clone();
    }
    Ok(sessions)
}

/// IPC-layer wrapper: the board's tasks plus the active project they were resolved
/// from. The frontend uses `project` to drop a late `tasks_list` response from a
/// project the user has already switched away from (the stale-overwrite bug),
/// mirroring how `sessions_list` stamps `project` onto each row.
#[derive(Serialize)]
pub struct TaskBoard {
    pub project: String,
    pub tasks: Vec<TaskWithDeps>,
}

/// Current backend revision token. A cheap sentinel the frontend polls to decide
/// whether to refetch the full board — returns the same string as
/// `TaskBackend::revision()` for the active project's backend.
#[tauri::command]
pub async fn tasks_revision() -> Result<String, String> {
    let (backend, _) = open_backend()?;
    backend.revision().await.map_err(|e| e.to_string())
}

/// Resolve an optional JS-supplied filter into a concrete [`ListFilter`], applying
/// the config archive window when the caller omitted a real one. `ListFilter` derives
/// `#[serde(default)]`, so a partial/omitted filter leaves `archive_days` at 0 — which
/// would archive *every* done/cancelled task (`-0 days` == now); fall back to the
/// config window unless the caller passed a real positive value. Shared by
/// `tasks_list` and `board_snapshot` so the two can't diverge on the archive rule.
fn resolve_filter(filter: Option<ListFilter>, config: &ProjectConfig) -> ListFilter {
    let mut filter = filter.unwrap_or_default();
    if filter.archive_days <= 0 {
        filter.archive_days = config.tasks.archive_days;
    }
    filter
}

/// The board model: every non-archived, non-backlog task with its dep edges, stamped
/// with the active project. `None` filter → a sensible default using the config's
/// archive window.
#[tauri::command]
pub async fn tasks_list(filter: Option<ListFilter>) -> Result<TaskBoard, IpcError> {
    let (backend, config) = open_backend().map_err(IpcError::internal)?;
    let filter = resolve_filter(filter, &config);
    let tasks = backend.list(&filter).await.map_err(|e| to_ipc_error(&e))?;
    Ok(TaskBoard {
        project: config.project.name,
        tasks,
    })
}

/// The whole board — tasks + dispatched reservations — in a single backend call,
/// stamped with the active project (like [`tasks_list`]). Replaces the two co-fired
/// `tasks_list` + `dispatched_reservations` commands on the desktop path: the
/// `board_snapshot` trait method fetches both, and a `Strong` backend serves them
/// from one transaction while `GitHubBackend` overrides it to one `issue_list_all`.
/// The `project` stamp lets the frontend drop a late response from a project the
/// user switched away from.
#[derive(Serialize)]
pub struct BoardSnapshotResponse {
    pub project: String,
    pub tasks: Vec<TaskWithDeps>,
    pub reservations: Vec<Reservation>,
    /// Backlog tasks, kept apart from `tasks` so nothing that reads the board model
    /// sees them by accident; the board shows them in the Open lane when its backlog
    /// toggle is on.
    pub backlog: Vec<TaskWithDeps>,
    /// Pending cards whose last send to their lane failed, with the reason.
    pub failures: Vec<DispatchFailure>,
}

/// A pending card whose latest dispatch-log entry for its lane is a failed send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DispatchFailure {
    pub task_id: i64,
    pub worktree: String,
    pub reason: String,
    pub at: String,
}

/// How many recent dispatch-log entries the snapshot scans for failed sends.
const FAILURE_SCAN: i64 = 300;

/// Pure: the failed sends still standing. For each open card reserved to a lane, its
/// newest log entry for that lane (`log` is newest first) decides: `failed` is a
/// standing failure; anything later (a send, a queue, a release) clears it.
pub(crate) fn dispatch_failures(
    tasks: &[TaskWithDeps],
    log: &[DispatchLogEntry],
) -> Vec<DispatchFailure> {
    tasks
        .iter()
        .filter(|t| t.task.status == "open")
        .filter_map(|t| {
            let wt = t.task.assigned_to.as_deref()?;
            let latest = log
                .iter()
                .find(|e| e.task_id == t.task.id && e.worktree == wt)?;
            (latest.decision == "failed").then(|| DispatchFailure {
                task_id: t.task.id,
                worktree: wt.to_string(),
                reason: latest.rationale.clone(),
                at: latest.created_at.clone(),
            })
        })
        .collect()
}

#[tauri::command]
pub async fn board_snapshot(filter: Option<ListFilter>) -> Result<BoardSnapshotResponse, IpcError> {
    let (backend, config) = open_backend().map_err(IpcError::internal)?;
    let filter = resolve_filter(filter, &config);
    let snapshot = backend
        .board_snapshot(&filter)
        .await
        .map_err(|e| to_ipc_error(&e))?;
    let backlog_filter = ListFilter {
        include_backlog: true,
        ..filter.clone()
    };
    let backlog = backend
        .list(&backlog_filter)
        .await
        .map_err(|e| to_ipc_error(&e))?
        .into_iter()
        .filter(|t| t.task.status == "backlog")
        .collect();
    // Best-effort: a log read failure just shows no failures.
    let log = backend
        .dispatch_log_list(FAILURE_SCAN)
        .await
        .unwrap_or_default();
    let failures = dispatch_failures(&snapshot.tasks, &log);
    Ok(BoardSnapshotResponse {
        project: config.project.name,
        tasks: snapshot.tasks,
        reservations: snapshot.reservations,
        backlog,
        failures,
    })
}

/// Composite inspector detail: the task with deps + dependency tree + activity log.
#[derive(Serialize)]
pub struct TaskDetail {
    task: TaskWithDeps,
    deps: Vec<DepNode>,
    logs: Vec<LogEntry>,
}

#[tauri::command]
pub async fn task_get(id: i64) -> Result<Option<TaskDetail>, IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    let task = match backend.get(id).await.map_err(|e| to_ipc_error(&e))? {
        Some(t) => t,
        None => return Ok(None),
    };
    let deps = backend.deps_tree(id).await.map_err(|e| to_ipc_error(&e))?;
    let logs = backend.logs(id, 50).await.map_err(|e| to_ipc_error(&e))?;
    Ok(Some(TaskDetail { task, deps, logs }))
}

/// Result of [`task_claim`]. `outcome` lets the frontend distinguish a fresh claim
/// from an idempotent re-claim, or surface the eventually-consistent "unverified" UX
/// when a future `Eventual` backend cannot guarantee exclusivity yet.
#[derive(Serialize)]
pub struct ClaimResult {
    /// `"claimed"` | `"already_yours"` | `"unverified"`.
    outcome: String,
    task: Task,
}

/// Claim a task as the explicit `actor` (the selected worktree). The GUI passes
/// the actor from its rail focus rather than relying on the store's default,
/// which has no meaningful git context inside the `.app` bundle.
#[tauri::command]
pub async fn task_claim(id: i64, actor: String) -> Result<ClaimResult, IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    let outcome = backend
        .claim(id, &actor)
        .await
        .map_err(|e| to_ipc_error(&e))?;
    let (label, task) = match outcome {
        ClaimOutcome::Claimed(t) => ("claimed", t),
        ClaimOutcome::AlreadyYours(t) => ("already_yours", t),
        ClaimOutcome::ClaimedUnverified(t) => ("unverified", t),
    };
    Ok(ClaimResult {
        outcome: label.to_string(),
        task,
    })
}

/// `set_done` returns a `DoneOutcome`; reshape into a named struct so
/// the JS side sees `{ task, unblocked }` rather than a positional array.
#[derive(Serialize)]
pub struct DoneResult {
    task: Task,
    unblocked: Vec<i64>,
}

#[tauri::command]
pub async fn task_done(id: i64, actor: String) -> Result<DoneResult, IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    let outcome = backend
        .set_done(id, &actor)
        .await
        .map_err(|e| to_ipc_error(&e))?;
    Ok(DoneResult {
        task: outcome.task,
        unblocked: outcome.unblocked,
    })
}

#[tauri::command]
pub async fn task_unclaim(id: i64, actor: String) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .unclaim(id, &actor)
        .await
        .map_err(|e| to_ipc_error(&e))
}

#[tauri::command]
pub async fn task_delete(id: i64) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend.delete(id).await.map_err(|e| to_ipc_error(&e))
}

/// Admin force-unclaim (the app acts as `keitora`, not the owner). Returns the task
/// to `open` + unassigned regardless of who held it, and records the decision in
/// the dispatch audit trail.
#[tauri::command]
pub async fn task_release(id: i64) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    let actor = keitora_tasks::current_worktree();
    backend
        .release(id, &actor)
        .await
        .map_err(|e| to_ipc_error(&e))?;
    backend
        .dispatch_log_add(id, &actor, "released", "")
        .await
        .map_err(|e| to_ipc_error(&e))
}

/// Move a task to the backlog: hidden from the board (unless its backlog toggle is
/// on) and never dispatched. Clears any lane assignment.
#[tauri::command]
pub async fn task_backlog(id: i64) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .backlog(id, &keitora_tasks::current_worktree())
        .await
        .map_err(|e| to_ipc_error(&e))
}

/// Bring a backlog, done or cancelled task back to `open`, unassigned.
#[tauri::command]
pub async fn task_reopen(id: i64) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .reopen(id, &keitora_tasks::current_worktree())
        .await
        .map(|_| ())
        .map_err(|e| to_ipc_error(&e))
}

/// A human's Mark done from the board or the Inspector: closes the task whoever holds
/// it (the app acts as `keitora`, not the assignee). Never goes through the CLI, so the
/// agent-session refusal doesn't apply.
#[tauri::command]
pub async fn task_mark_done(id: i64) -> Result<DoneResult, IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    let outcome = backend
        .force_done(id, &keitora_tasks::current_worktree())
        .await
        .map_err(|e| to_ipc_error(&e))?;
    Ok(DoneResult {
        task: outcome.task,
        unblocked: outcome.unblocked,
    })
}

#[tauri::command]
pub async fn task_update(id: i64, patch: TaskUpdate) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .update(id, &keitora_tasks::current_worktree(), &patch)
        .await
        .map_err(|e| to_ipc_error(&e))
}

/// Make `id` blocked by `depends_on`. Rejects self-deps, cycles and duplicates.
#[tauri::command]
pub async fn task_add_dep(id: i64, depends_on: i64) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .add_dep(id, depends_on, &keitora_tasks::current_worktree())
        .await
        .map_err(|e| to_ipc_error(&e))
}

/// Drop the edge that makes `id` blocked by `depends_on`.
#[tauri::command]
pub async fn task_remove_dep(id: i64, depends_on: i64) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .remove_dep(id, depends_on, &keitora_tasks::current_worktree())
        .await
        .map_err(|e| to_ipc_error(&e))
}

#[tauri::command]
pub async fn task_new(
    title: String,
    description: String,
    priority: String,
    tags: Vec<String>,
) -> Result<Task, IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .create(
            &keitora_tasks::current_worktree(),
            &title,
            &description,
            &priority,
            &tags,
            None,
        )
        .await
        .map_err(|e| to_ipc_error(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_entry(id: i64, task_id: i64, wt: &str, decision: &str) -> DispatchLogEntry {
        DispatchLogEntry {
            id,
            task_id,
            worktree: wt.to_string(),
            decision: decision.to_string(),
            rationale: format!("why {id}"),
            created_at: format!("t{id}"),
        }
    }

    fn card(id: i64, status: &str, wt: Option<&str>) -> TaskWithDeps {
        TaskWithDeps {
            task: Task {
                id,
                title: "t".into(),
                description: String::new(),
                status: status.into(),
                priority: "normal".into(),
                assigned_to: wt.map(str::to_string),
                created_by: None,
                tags: vec![],
                branch: None,
                pr_url: None,
                created_at: String::new(),
                updated_at: String::new(),
            },
            blocked_by: vec![],
            blocks: vec![],
        }
    }

    #[test]
    fn dispatch_failures_keep_only_standing_failures_on_the_current_lane() {
        let tasks = vec![
            card(1, "open", Some("alpha")),   // failed last → standing
            card(2, "open", Some("alpha")),   // failed, then sent → cleared
            card(3, "open", Some("bravo")),   // failed on alpha, moved to bravo → not here
            card(4, "active", Some("alpha")), // claimed → not a failure
            card(5, "open", None),
        ];
        // Newest first.
        let log = vec![
            log_entry(9, 2, "alpha", "dispatched"),
            log_entry(8, 1, "alpha", "failed"),
            log_entry(7, 3, "alpha", "failed"),
            log_entry(6, 2, "alpha", "failed"),
            log_entry(5, 4, "alpha", "failed"),
        ];
        assert_eq!(
            dispatch_failures(&tasks, &log),
            vec![DispatchFailure {
                task_id: 1,
                worktree: "alpha".into(),
                reason: "why 8".into(),
                at: "t8".into(),
            }]
        );
    }
}
