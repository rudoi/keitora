//! Keitora task store — the canonical task-DB library shared by the `task` CLI and
//! (from Phase 2) the Tauri app. Backed by SQLite via rusqlite (bundled).
//!
//! The store implements a small, proven task-tracking model: atomic `claim`
//! (TOCTOU-safe), dependency gating with cycle detection, and `done` reporting
//! newly-unblocked tasks.

mod backend;
pub mod codex_trust;
mod config;
pub mod config_edit;
mod discovery;
pub mod dispatch;
mod drive;
mod drive_store;
mod factory;
pub mod git;
mod github;
mod home;
pub mod hook_log;
mod local_store;
mod migration;
mod model;
pub mod path_key;
mod sqlite_backend;
pub mod status_store;
pub mod transcript;

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
pub use codex_trust::{codex_project_trust, read_codex_project_trust, ProjectTrust};
pub use config::{
    agy_state_dir, bundled_cli, cached_login_path, classify_worktree, compare_versions_desc,
    config_path, current_worktree, current_worktree_in, discover_claude, discover_cli,
    discover_cli_cached, effective_config, git_toplevel, herdr_bin_candidates, herdr_on_path,
    herdr_release_exes_in, herdr_release_paths, herdr_release_paths_in,
    herdr_release_paths_with_home, herdr_releases_dir_with_home, herdr_well_known_paths,
    invalidate_discovery_misses, is_executable_file, is_safe_session_id, keitora_on_path,
    keitora_on_path_fresh, login_path, managed_keitora_bin_dir, mirror_file, mirror_is_stale,
    project_provider, project_provider_mut, resolve, resolve_active, resolve_autonomy,
    resolve_claude_path, resolve_cli, resolve_keitora_path, resolve_provider_path,
    resolve_skip_permissions, same_file_name, short_path, sort_versions_desc, stamp_from_registry,
    strip_extended_prefix_str, unreserved_project_name, user_config_path, user_provider,
    user_provider_mut, write_atomic, CliConfig, CliKind, Config, DiffBase, DispatchConfig,
    FieldIntent, HerdrPtyConfig, ProjectConfig, ProjectMeta, ProjectRef, ProjectSettingsPatch,
    ProviderConfig, ProviderIds, ProviderSpec, PtyConfig, RecoveryConfig, ResolveError, TaskConfig,
    UpdatesConfig, UserConfig, WorktreeEntry, CLI_COUNT, DISCOVER_NONE_TTL, RESERVED_PROJECT_NAME,
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
    set_project_codex_path, set_project_dispatch_steps, set_project_provider_path,
    set_user_claude_path, set_user_claude_path_at, set_user_codex_path, set_user_codex_path_at,
    set_user_provider_path, set_user_provider_path_at, set_worktrees_dir, write_project_settings,
    ProviderWrite,
};
pub use discovery::{
    create_worktree, discover_worktrees, repair_project_worktree_paths, set_project_worktrees,
};
pub use drive::{
    truncate_bytes, truncate_chars, Baseline, Delivery, Drive, DriveBind, DrivePrompt,
    DriveRequest, DriveTurn, NewDrivePrompt, NewDriveTurn, PromptDecision, SessionReply,
    DRIVE_CONFIRM_MAX_AGE_SECS, DRIVE_DDL, DRIVE_DEFAULT_ESCALATE_AFTER_S,
    DRIVE_ESCALATE_AFTER_MAX_S, DRIVE_ESCALATE_AFTER_MIN_S, DRIVE_HOOK_TIMEOUT_S,
    DRIVE_PERMISSION_INPUT_MAX, DRIVE_PROMPT_KINDS, DRIVE_PROMPT_STALE_SLACK_S,
    DRIVE_PROMPT_STATUSES, DRIVE_QUESTION_INPUT_MAX, DRIVE_READ_LINES_DEFAULT,
    DRIVE_READ_LINES_MAX, DRIVE_READ_RESULT_MAX, DRIVE_REQUEST_KINDS, DRIVE_REQUEST_MAX_AGE_SECS,
    DRIVE_REQUEST_OUTCOMES, DRIVE_REQUEST_STATUSES, DRIVE_RETENTION_SECS, DRIVE_SEND_TEXT_MAX,
    DRIVE_STALL_NO_ACTIVITY_S, DRIVE_SUMMARY_MAX, DRIVE_TURNS_DDL, DRIVE_TURNS_KEEP_PER_WORKTREE,
    DRIVE_TURNS_RETENTION_DAYS, DRIVE_TURN_CLAUSES, DRIVE_TURN_KINDS, DRIVE_TURN_OUTCOMES,
    DRIVE_TURN_REPLY_SOURCES, DRIVE_TURN_SIGNALS, SESSION_REPLY_MAX,
};
pub use factory::open_backend;
pub use home::{keitora_home, KEITORA_HOME_ENV};
pub use hook_log::{HookLogLevel, HookLogLine};
pub use local_store::{LocalStore, ResumeRow};
pub use migration::{find_legacy_config, migrate_legacy};
pub use model::{
    ActiveTask, DepNode, DispatchLogEntry, DispatchRequest, ListFilter, LogEntry, ResolvedWorktree,
    Task, TaskUpdate, TaskWithDeps, WorktreeCandidate, WorktreeSession, WorktreeStatus,
    DISPATCH_REQUEST_OUTCOMES, DISPATCH_REQUEST_STATUSES, PRIORITIES, SESSION_STATES, STATUSES,
};
pub use sqlite_backend::SqliteBackend;
pub use status_store::{
    age_text, clock_text, freshness, now_ms, overlay_sessions, sqlite_utc_to_ms, status_db_path,
    AppProject, AppProjectTouch, Beat, EngineRead, FindingKey, FindingRow, FindingWrite, Freshness,
    LeaseOutcome, ProjectStatus, StatusReader, StatusSession, StatusWriter, WriterIdentity,
    WriterOpen, WriterRow, HERDR_LINKS, LEASE_STALE_MS, PRESENCE_TOUCH_LAG_MS, STATUS_DDL,
    STATUS_FRESH_MS, STATUS_SCHEMA_VERSION,
};

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
