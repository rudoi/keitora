//! Keitora task store — the canonical task-DB library shared by the `task` CLI and
//! (from Phase 2) the Tauri app. Backed by SQLite via rusqlite (bundled).
//!
//! The store implements a small, proven task-tracking model: atomic `claim`
//! (TOCTOU-safe), dependency gating with cycle detection, and `done` reporting
//! newly-unblocked tasks.

mod backend;
mod config;
pub mod config_edit;
mod discovery;
pub mod dispatch;
mod factory;
pub mod git;
mod github;
mod local_store;
mod migration;
mod model;
mod sqlite_backend;

#[cfg(test)]
pub(crate) mod backend_tests;

/// Process-wide lock serializing every test that mutates global state (the
/// `KEITORA_*` env vars / the user registry). Shared across the `config` and
/// `migration` test modules because both influence `user_config_path()`; using
/// two separate locks let them race. `lock()` tolerates poisoning so one failing
/// test does not cascade into the others.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub use backend::{
    BackendError, BackendResult, BoardSnapshot, ClaimOutcome, Consistency, DoneOutcome,
    Reservation, ReserveOutcome, TaskBackend,
};
pub use config::{
    cached_login_path, classify_worktree, config_path, current_worktree, discover_claude,
    discover_cli, effective_config, git_toplevel, herdr_bin_candidates, herdr_on_path,
    herdr_release_paths, herdr_well_known_paths, is_executable_file, is_safe_claude_session_id,
    keitora_on_path, login_path, reorder_entries, resolve, resolve_active, resolve_autonomy,
    resolve_claude_path, resolve_cli, resolve_keitora_path, resolve_provider_path,
    resolve_skip_permissions, short_path, stamp_from_registry, strip_extended_prefix_str,
    CliConfig, CliKind, Config, DiffBase, DispatchConfig, HerdrPtyConfig, ProjectConfig,
    ProjectMeta, ProjectRef, ProjectSettingsPatch, ProviderConfig, PtyConfig, RecoveryConfig,
    ResolveError, TaskConfig, UserConfig, WorktreeEntry,
};
// Windows-only: expose the bundled `keitora` CLI on spawned terminals' PATH.
#[cfg(unix)]
pub use config::unix_spawn_path;
#[cfg(windows)]
pub use config::{prepend_path, windows_keitora_bin_dir};
pub use config_edit::{
    check_days, clean_dispatch_steps, normalize_worktrees_dir, remove_project_from_registry,
    remove_project_from_registry_at, set_archive_days, set_dispatch_steps,
    set_dispatch_steps_codex, set_log_retention_days, set_project_claude_path,
    set_project_codex_path, set_user_claude_path, set_user_claude_path_at, set_user_codex_path,
    set_user_codex_path_at, set_worktrees_dir, write_project_settings,
};
pub use discovery::{
    create_worktree, discover_worktrees, reorder_project_worktrees, repair_project_worktree_paths,
    set_project_worktrees,
};
pub use factory::open_backend;
pub use local_store::{LocalStore, ResumeRow};
pub use migration::{find_legacy_config, migrate_legacy};
pub use model::{
    ActiveTask, DepNode, DispatchLogEntry, ListFilter, LogEntry, ResolvedWorktree, Task,
    TaskUpdate, TaskWithDeps, WorktreeCandidate, WorktreeSession, WorktreeStatus, PRIORITIES,
    SESSION_STATES, STATUSES,
};
pub use sqlite_backend::SqliteBackend;

use rusqlite::Row;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("task #{0} not found")]
    NotFound(i64),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn row_to_dispatch_log_entry(row: &Row) -> rusqlite::Result<DispatchLogEntry> {
    Ok(DispatchLogEntry {
        id: row.get(0)?,
        task_id: row.get(1)?,
        worktree: row.get(2)?,
        decision: row.get(3)?,
        rationale: row.get(4)?,
        created_at: row.get(5)?,
    })
}

pub(crate) fn row_to_task(row: &Row) -> rusqlite::Result<Task> {
    let tags_str: String = row.get("tags")?;
    let tags: Vec<String> = serde_json::from_str(&tags_str).unwrap_or_default();
    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        description: row.get("description")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        assigned_to: row.get("assigned_to")?,
        created_by: row.get("created_by")?,
        tags,
        branch: row.get("branch")?,
        pr_url: row.get("pr_url")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}
