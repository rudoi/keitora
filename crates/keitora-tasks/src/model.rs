//! Task data types. `tags` is stored as a JSON array in the DB and surfaced as
//! a `Vec<String>` here; everything else maps directly to the `tasks` columns.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const STATUSES: [&str; 5] = ["open", "active", "backlog", "done", "cancelled"];
pub const PRIORITIES: [&str; 4] = ["low", "normal", "high", "urgent"];
pub const SESSION_STATES: [&str; 4] = ["idle", "busy", "waiting", "offline"];

/// Live state of a worktree's Claude session, recorded by per-worktree hooks
/// (`keitora session <state>`) and surfaced by the app. Output-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSession {
    /// The project this session belongs to — the namespace the daemon keys live
    /// PTYs by project. DB rows carry no project column, so this defaults
    /// to `""` on read; `sessions_list` stamps the active project name onto each
    /// row before returning it to the frontend.
    #[serde(default)]
    pub project: String,
    pub worktree: String,
    pub state: String,
    pub current_task_id: Option<i64>,
    pub pid: Option<i64>,
    pub last_heartbeat: String,
}

/// A worktree's active (claimed) task, as the rail shows it on the worktree's lane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveTask {
    pub worktree: String,
    pub id: i64,
    pub title: String,
}

/// Filesystem status of a configured worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    /// Exists, is a directory, and is a git repo/worktree — ready to run.
    Available,
    /// Path does not exist (uninitialized worktree).
    Missing,
    /// Path exists but is not a directory.
    NotDir,
    /// Directory exists but is not a git repo/worktree.
    NotGit,
}

/// A configured worktree resolved to a path and classified. `path` serializes
/// as a plain string for the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedWorktree {
    pub name: String,
    pub path: PathBuf,
    pub status: WorktreeStatus,
    /// The resolved default CLI for this worktree: the registry entry's
    /// `cli`, else the user default, else `claude` (via `resolve_cli`). Lets the
    /// rail render the provider glyph before any session exists.
    pub cli: crate::config::CliKind,
}

/// A git worktree discovered under a project (via `git worktree list`), offered
/// to the user in the onboarding multiselect. `path` serializes as a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeCandidate {
    /// Basename of the worktree path — the name written into `[tasks].worktrees`.
    pub name: String,
    pub path: PathBuf,
    /// Checked-out branch (`refs/heads/` stripped), or `None` if detached.
    pub branch: Option<String>,
    /// Already present in the project's current `[tasks].worktrees`.
    pub already_tracked: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub assigned_to: Option<String>,
    pub created_by: Option<String>,
    pub tags: Vec<String>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A task plus its resolved dependency edges.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskWithDeps {
    #[serde(flatten)]
    pub task: Task,
    /// IDs of unfinished tasks blocking this one.
    pub blocked_by: Vec<i64>,
    /// IDs of tasks that depend on this one.
    pub blocks: Vec<i64>,
}

/// A node in a transitive dependency tree (output of `deps_tree`).
#[derive(Debug, Clone, Serialize)]
pub struct DepNode {
    pub id: i64,
    pub depth: i64,
    pub title: String,
    pub status: String,
}

/// An activity-log entry for a task.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub created_at: String,
    pub worktree: String,
    pub action: String,
    pub detail: String,
}

/// An entry in the dispatcher audit trail (`dispatch_log`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchLogEntry {
    pub id: i64,
    pub task_id: i64,
    pub worktree: String,
    /// The dispatcher's decision: `'dispatched'`, `'queued'`, or `'released'`. Legacy
    /// rows may also carry `'suggested'`, written by a since-removed feature.
    pub decision: String,
    pub rationale: String,
    pub created_at: String,
}

/// Filters for `Store::list`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListFilter {
    /// Include tasks past the archive window (done/cancelled and stale).
    pub include_archived: bool,
    /// Include `backlog`-status tasks (hidden by default).
    pub include_backlog: bool,
    /// Restrict to a single assignee (worktree name).
    pub assigned_to: Option<String>,
    /// Archive window in days (tasks older than this in a terminal state are hidden).
    pub archive_days: i64,
}

/// Mutable fields for `Store::update`. `None` means "leave unchanged".
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskUpdate {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
    pub branch: Option<String>,
    pub pr_url: Option<String>,
}

impl TaskUpdate {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.priority.is_none()
            && self.status.is_none()
            && self.branch.is_none()
            && self.pr_url.is_none()
    }
}
