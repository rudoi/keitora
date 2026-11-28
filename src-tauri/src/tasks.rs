// Task commands: thin Tauri wrappers over `keitora_tasks::TaskBackend` + `keitora_tasks::LocalStore`.
// The app and CLI are two clients of one SQLite file; all concurrency safety (atomic claim, dep
// gating) lives in the backend — we never fork task logic here. A backend + local store are
// opened per call (a cheap SQLite connection), matching how the CLI opens fresh per invocation,
// so we don't hold a non-Send connection in Tauri state.

use keitora_tasks::{
    BackendError, ClaimOutcome, DepNode, ListFilter, LocalStore, LogEntry, ProjectConfig,
    Reservation, Task, TaskBackend, TaskUpdate, TaskWithDeps, WorktreeSession,
};
use serde::Serialize;
use std::collections::HashMap;

use crate::engine::{ProjectViews, SharedEngine};

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

/// A project's session rows as the app reports them, plus the names left out.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Reconciled {
    /// Rows overlaid with the engine views, scoped to the configured worktrees,
    /// stamped with the project.
    pub sessions: Vec<WorktreeSession>,
    /// Worktrees with a hook row that are not configured for the project (sorted).
    /// They are not shown; they are kept for the health check that reports them.
    pub unconfigured: Vec<String>,
}

/// The project's worktree sessions with the engine's views laid over the hook rows
/// (`keitora_tasks::overlay_sessions`): the single source of truth for "idle", so the
/// dispatcher, the rail and `sessions_list` agree.
///
/// - A fresh view sets the state (an agent herdr lists without a hook row still
///   shows, e.g. one whose hooks never fired).
/// - Once the engine is ready, a row without a view reads `offline`.
/// - Before that (or with no herdr link) the rows are reported as they are,
///   marked `stale`.
///
/// Task-state reconciliation of `current_task_id` happens inside
/// `keitora_tasks::sessions_list`.
pub(crate) fn reconciled_sessions(
    views: &ProjectViews,
    project: &str,
    config: &ProjectConfig,
) -> Result<Vec<WorktreeSession>, String> {
    // Open the LocalStore from the already-resolved config so each command resolves
    // config and opens the local handle once.
    let local =
        LocalStore::open(&config.tasks.db_path.to_string_lossy()).map_err(|e| e.to_string())?;
    Ok(reconciled_sessions_in(&local, views, project, config)?.sessions)
}

/// [`reconciled_sessions`] over an already-open store (e.g. a read-only handle),
/// with the unconfigured names it left out.
pub(crate) fn reconciled_sessions_in(
    local: &LocalStore,
    views: &ProjectViews,
    project: &str,
    config: &ProjectConfig,
) -> Result<Reconciled, String> {
    let rows = local.sessions_list().map_err(|e| e.to_string())?;
    // A store without drive tables (or a read error) only costs synthesized rows
    // their `driven_by`.
    let drivers: HashMap<String, String> = local
        .drives_list()
        .map(|ds| ds.into_iter().map(|d| (d.driven, d.driver)).collect())
        .unwrap_or_default();
    // `worktree_sessions` rows carry no project column and the DB keeps a row for
    // every worktree that ever ran a session; only the configured ones are shown.
    // An empty configured set doesn't restrict.
    let configured = config.worktrees();
    let mut unconfigured: Vec<String> = if configured.is_empty() {
        Vec::new()
    } else {
        rows.iter()
            .filter(|r| !configured.contains(&r.worktree))
            .map(|r| r.worktree.clone())
            .collect()
    };
    unconfigured.sort();
    unconfigured.dedup();
    if !unconfigured.is_empty() {
        log::debug!(
            "sessions: {project} has rows for unconfigured worktrees: {}",
            unconfigured.join(", ")
        );
    }
    let mut sessions = keitora_tasks::overlay_sessions(
        rows,
        views.ready.then_some(&views.views),
        &configured,
        &drivers,
    );
    for s in &mut sessions {
        s.project = project.to_string();
    }
    Ok(Reconciled {
        sessions,
        unconfigured,
    })
}

/// The worktree this app acts as (assignee for claim/done/unclaim). Lets the UI
/// show owner-only actions (Unclaim/Done) on just the tasks it can mutate.
#[tauri::command(async)]
pub fn task_actor() -> String {
    keitora_tasks::current_worktree()
}

/// Live worktree session states of the active project: the engine's views over
/// the hook rows (see [`reconciled_sessions`]), each stamped with the project so
/// the frontend keys sessions by `project/worktree`.
#[tauri::command(async)]
pub fn sessions_list(
    engine: tauri::State<'_, SharedEngine>,
) -> Result<Vec<WorktreeSession>, String> {
    let config = keitora_tasks::resolve_active().map_err(|e| e.to_string())?;
    let project = config.project.name.clone();
    reconciled_sessions(&engine.views(&project), &project, &config)
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
}

#[tauri::command]
pub async fn board_snapshot(filter: Option<ListFilter>) -> Result<BoardSnapshotResponse, IpcError> {
    let (backend, config) = open_backend().map_err(IpcError::internal)?;
    let filter = resolve_filter(filter, &config);
    let snapshot = backend
        .board_snapshot(&filter)
        .await
        .map_err(|e| to_ipc_error(&e))?;
    Ok(BoardSnapshotResponse {
        project: config.project.name,
        tasks: snapshot.tasks,
        reservations: snapshot.reservations,
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

#[tauri::command]
pub async fn task_update(id: i64, patch: TaskUpdate) -> Result<(), IpcError> {
    let (backend, _) = open_backend().map_err(IpcError::internal)?;
    backend
        .update(id, &keitora_tasks::current_worktree(), &patch)
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
    use crate::backend::SessionKey;
    use crate::engine::model::{obs, AgentObs};
    use crate::engine::tests::{ingest_settled, FakeEvidence};
    use crate::engine::SessionEngine;
    use keitora_tasks::WorktreeEntry;

    /// Project `proj` (worktrees alpha and bravo) with its DB in a temp dir.
    fn project(dir: &std::path::Path) -> (ProjectConfig, LocalStore) {
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut config = ProjectConfig::scaffold(&root);
        config.effective_worktrees = Some(
            ["alpha", "bravo"]
                .into_iter()
                .map(WorktreeEntry::name_only)
                .collect(),
        );
        let db = dir.join("tasks.db").to_string_lossy().into_owned();
        config.tasks.db_path = db.clone().into();
        (config, LocalStore::open(&db).unwrap())
    }

    fn key(w: &str) -> SessionKey {
        SessionKey {
            project: "proj".into(),
            worktree: w.into(),
        }
    }

    fn engine() -> SharedEngine {
        SessionEngine::new_for_test(std::sync::Arc::new(FakeEvidence::default()))
    }

    #[test]
    fn a_rowless_live_agent_reads_idle_in_sessions_list() {
        let dir = tempfile::tempdir().unwrap();
        let (config, local) = project(dir.path());
        local.drive_bind("bravo", "alpha", None, false).unwrap();
        let engine = engine();
        let agy = AgentObs {
            agent: Some("agy".into()),
            ..obs("w1:p1", "idle", 2, None)
        };
        ingest_settled(&engine, vec![(key("alpha"), Some(agy))]);

        let sessions = reconciled_sessions(&engine.views("proj"), "proj", &config).unwrap();
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!((s.worktree.as_str(), s.state.as_str()), ("alpha", "idle"));
        assert_eq!(s.project, "proj");
        assert_eq!(s.agent.as_deref(), Some("present"));
        assert_eq!(
            s.driven_by.as_deref(),
            Some("bravo"),
            "from the drives table"
        );
        assert!(!s.stale);
    }

    #[test]
    fn an_offline_row_with_a_live_agent_reads_idle_and_a_shell_reads_offline() {
        let dir = tempfile::tempdir().unwrap();
        let (config, local) = project(dir.path());
        local
            .session_upsert("alpha", "offline", None, None)
            .unwrap();
        local
            .session_upsert("bravo", "idle", Some(4), Some(99))
            .unwrap();
        let engine = engine();
        let shell = AgentObs {
            agent: None,
            ..obs("w1:p2", "unknown", 0, None)
        };
        ingest_settled(
            &engine,
            vec![
                (key("alpha"), Some(obs("w1:p1", "idle", 2, None))),
                (key("bravo"), Some(shell)),
            ],
        );
        let r = reconciled_sessions_in(&local, &engine.views("proj"), "proj", &config).unwrap();
        let states: Vec<(&str, &str)> = r
            .sessions
            .iter()
            .map(|s| (s.worktree.as_str(), s.state.as_str()))
            .collect();
        assert_eq!(states, vec![("alpha", "idle"), ("bravo", "offline")]);
        assert_eq!(
            r.sessions[1].current_task_id, None,
            "offline clears the task"
        );
        assert_eq!(r.sessions[1].pid, None);
    }

    #[test]
    fn before_the_first_snapshot_rows_are_reported_stale_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let (config, local) = project(dir.path());
        local.session_upsert("alpha", "busy", None, None).unwrap();
        let r = reconciled_sessions_in(&local, &engine().views("proj"), "proj", &config).unwrap();
        assert_eq!(r.sessions.len(), 1);
        assert_eq!(r.sessions[0].state, "busy");
        assert!(r.sessions[0].stale);
    }

    #[test]
    fn rows_outside_the_configured_set_are_reported_as_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        let (config, local) = project(dir.path());
        for wt in ["alpha", "zulu", "other-wt"] {
            local.session_upsert(wt, "idle", None, None).unwrap();
        }
        let r = reconciled_sessions_in(&local, &engine().views("proj"), "proj", &config).unwrap();
        let shown: Vec<&str> = r.sessions.iter().map(|s| s.worktree.as_str()).collect();
        assert_eq!(shown, vec!["alpha"]);
        assert_eq!(
            r.unconfigured,
            vec!["other-wt".to_string(), "zulu".to_string()]
        );

        // With no configured worktrees nothing is filtered, so nothing is reported.
        let mut open = config.clone();
        open.effective_worktrees = Some(Vec::new());
        let r = reconciled_sessions_in(&local, &engine().views("proj"), "proj", &open).unwrap();
        assert!(r.unconfigured.is_empty());
        assert_eq!(r.sessions.len(), 3);
    }
}
