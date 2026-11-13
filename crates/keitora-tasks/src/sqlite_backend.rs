//! `SqliteBackend` — the SQLite implementation of [`TaskBackend`](crate::TaskBackend).
//!
//! The task + dispatch SQL bodies are ported (near-verbatim) from `Store` in
//! [`lib.rs`](crate). They live on synchronous `*_blocking` inherent methods holding the
//! real rusqlite calls; the async `TaskBackend` impl is a set of thin wrappers that call
//! those inherents directly — **no `block_on`, no `spawn_blocking`**.
//!
//! `rusqlite::Connection` is `!Sync`, so offloading to `spawn_blocking` would require an
//! `Arc<Mutex<Connection>>` / pool with no benefit at sub-millisecond local latencies. This
//! is a deliberate, documented choice.
//!
//! ## Transitional duplication
//!
//! This stage **duplicates** the SQL bodies rather than moving them off `Store`. The prompt
//! permits either; duplication keeps `Store` (and all its current consumers — CLI, Tauri)
//! untouched and the tree green with the least risk. A later change reconciles the two
//! by making `Store` delegate to / wrap `SqliteBackend`.

use async_trait::async_trait;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, ToSql};
use std::sync::Mutex;

use crate::backend::{
    BackendError, BackendResult, ClaimOutcome, Consistency, DoneOutcome, Reservation, TaskBackend,
};
use crate::model::{
    DepNode, DispatchLogEntry, ListFilter, LogEntry, Task, TaskUpdate, TaskWithDeps,
};
use crate::{row_to_dispatch_log_entry, row_to_task};

/// Shared connection opener used by both `SqliteBackend` and, later,
/// `LocalStore`. Runs the same WAL PRAGMAs as `Store::open_actor_unchecked` (`lib.rs`).
/// Schema init is performed separately by the caller via `init_schema_blocking`.
pub(crate) fn open_conn(db_path: &str) -> rusqlite::Result<Connection> {
    let conn = if db_path == ":memory:" || db_path.starts_with("file:") {
        // SQLite special / URI forms — open verbatim.
        Connection::open(db_path)?
    } else {
        // Resolve the on-disk path: strip the Windows `\\?\` extended-length prefix
        // (SQLite's VFS mishandles it) and lexically resolve `..`, then ensure the
        // parent directory exists. `Connection::open` creates the DB *file* but NOT
        // its parent dirs, so a configured `db_path` under a not-yet-created dir
        // (e.g. a fresh `shared/`) otherwise fails with "unable to open database
        // file". Best-effort: if the dir can't be made, `open` surfaces the error.
        let path = crate::config::normalize(&crate::config::strip_extended_prefix(
            std::path::PathBuf::from(db_path),
        ));
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        Connection::open(&path)?
    };
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 3000;",
    )?;
    Ok(conn)
}

/// The `tasks` table's columns, shared by its create and by
/// [`upgrade_tasks_status_check`]'s rebuild.
const TASKS_COLUMNS: &str = "
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          title TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          status TEXT NOT NULL DEFAULT 'open'
            CHECK(status IN ('open', 'active', 'review', 'backlog', 'done', 'cancelled')),
          priority TEXT NOT NULL DEFAULT 'normal'
            CHECK(priority IN ('low', 'normal', 'high', 'urgent')),
          assigned_to TEXT DEFAULT NULL,
          created_by TEXT DEFAULT NULL,
          tags TEXT NOT NULL DEFAULT '[]',
          branch TEXT DEFAULT NULL,
          pr_url TEXT DEFAULT NULL,
          created_at TEXT NOT NULL DEFAULT (datetime('now')),
          updated_at TEXT NOT NULL DEFAULT (datetime('now'))
";

/// The columns [`upgrade_tasks_status_check`] copies.
const TASKS_COLUMN_NAMES: &str = "id, title, description, status, priority, assigned_to, \
     created_by, tags, branch, pr_url, created_at, updated_at";

/// Rebuild a `tasks` table created before the `review` status existed: its
/// `CHECK(status IN …)` rejects `review`, and SQLite can't alter a CHECK. The rows are
/// copied into a table with the current columns, which replaces the old one; the
/// indexes and the `updated_at` trigger dropped with it are recreated by the
/// `IF NOT EXISTS` DDL that follows in [`init_schema`]. Foreign keys are off for the
/// swap so dropping the old table doesn't cascade into `task_deps` / `task_log`, and
/// the AUTOINCREMENT counter is carried over so a deleted task's id is never reused.
/// Several processes open the same DB, so the check is repeated under the write lock.
fn upgrade_tasks_status_check(conn: &Connection) -> rusqlite::Result<()> {
    fn needed(conn: &Connection) -> rusqlite::Result<bool> {
        let sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'tasks'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(sql.is_some_and(|s| !s.contains("'review'")))
    }
    if !needed(conn)? {
        return Ok(());
    }
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let rebuild = || -> rusqlite::Result<()> {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        if !needed(conn)? {
            return conn.execute_batch("COMMIT;");
        }
        let seq: Option<i64> = conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'tasks'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        conn.execute_batch(&format!(
            "CREATE TABLE tasks_new ({TASKS_COLUMNS});
             INSERT INTO tasks_new ({TASKS_COLUMN_NAMES}) SELECT {TASKS_COLUMN_NAMES} FROM tasks;
             DROP TABLE tasks;
             ALTER TABLE tasks_new RENAME TO tasks;"
        ))?;
        if let Some(seq) = seq {
            conn.execute(
                "UPDATE sqlite_sequence SET seq = MAX(seq, ?1) WHERE name = 'tasks'",
                params![seq],
            )?;
        }
        conn.execute_batch("COMMIT;")
    };
    let result = rebuild();
    if result.is_err() {
        let _ = conn.execute_batch("ROLLBACK;");
    }
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    result
}

/// Create the full schema if absent. Shared by both [`SqliteBackend::open`] and
/// `LocalStore::open` so each handle sees every table when pointed at the same DB
/// file (WAL-safe concurrent opens). The DDL is `CREATE … IF NOT EXISTS`, so
/// running it from both opens is idempotent.
pub(crate) fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS tasks ({TASKS_COLUMNS});"
    ))?;
    upgrade_tasks_status_check(conn)?;
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
        CREATE INDEX IF NOT EXISTS idx_tasks_assigned ON tasks(assigned_to);
        CREATE INDEX IF NOT EXISTS idx_tasks_status_assigned ON tasks(status, assigned_to);

        CREATE TABLE IF NOT EXISTS task_deps (
          task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          PRIMARY KEY (task_id, depends_on)
        );
        CREATE INDEX IF NOT EXISTS idx_task_deps_depends_on ON task_deps(depends_on);

        CREATE TABLE IF NOT EXISTS task_log (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          worktree TEXT NOT NULL,
          action TEXT NOT NULL,
          detail TEXT NOT NULL DEFAULT '',
          created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_task_log_task_id ON task_log(task_id);

        CREATE TRIGGER IF NOT EXISTS tasks_updated_at AFTER UPDATE ON tasks
        BEGIN
          UPDATE tasks SET updated_at = datetime('now') WHERE id = NEW.id;
        END;

        CREATE TABLE IF NOT EXISTS worktree_sessions (
          worktree        TEXT PRIMARY KEY,
          state           TEXT NOT NULL DEFAULT 'idle'
                          CHECK(state IN ('idle','busy','waiting','offline')),
          current_task_id INTEGER,
          pid             INTEGER,
          last_heartbeat  TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- The agent conversation each (project, worktree) should resume. Only the
        -- global resume DB uses it; project DBs carry it empty.
        CREATE TABLE IF NOT EXISTS resume_sessions (
          project        TEXT NOT NULL,
          worktree       TEXT NOT NULL,
          cli            TEXT NOT NULL CHECK(cli IN ('claude','codex')),
          session_id     TEXT,
          source         TEXT NOT NULL DEFAULT '',
          agent_present  INTEGER NOT NULL DEFAULT 1,
          cleared_id     TEXT,
          observed_at    INTEGER NOT NULL,
          PRIMARY KEY (project, worktree)
        );

        -- Whether the user terminated a (project, worktree) session. Kept apart from
        -- resume_sessions so a flag survives resume rows being rewritten. Only the
        -- global resume DB uses it; project DBs carry it empty.
        CREATE TABLE IF NOT EXISTS session_flags (
          project    TEXT NOT NULL,
          worktree   TEXT NOT NULL,
          terminated INTEGER NOT NULL DEFAULT 0,
          updated_at TEXT NOT NULL DEFAULT (datetime('now')),
          PRIMARY KEY (project, worktree)
        );

        -- Dispatch requests written by another process (the CLI) for the running app
        -- to execute through its own dispatch path. SQLite is the only channel
        -- between the CLI and the app, so the request and its outcome live here. A
        -- short-lived mailbox, not a queue: each row is applied or refused at once.
        CREATE TABLE IF NOT EXISTS dispatch_requests (
          id           INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id      INTEGER NOT NULL,
          worktree     TEXT NOT NULL,
          requested_by TEXT NOT NULL DEFAULT '',
          status       TEXT NOT NULL DEFAULT 'pending'
                       CHECK(status IN ('pending','running','applied','refused')),
          reason       TEXT NOT NULL DEFAULT '',
          created_at   TEXT NOT NULL DEFAULT (datetime('now')),
          resolved_at  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_dispatch_requests_status ON dispatch_requests(status);
        "#,
    )?;

    // `dispatch_log` (table + index) shares one DDL source of truth with the
    // GitHub sidecar so the two schemas can never drift .
    conn.execute_batch(crate::github::sidecar::DISPATCH_LOG_DDL)?;

    Ok(())
}

/// Map an arbitrary `rusqlite::Error` to a `BackendError`, classifying SQLITE_BUSY /
/// SQLITE_LOCKED as `Transient` (retryable) and everything else as `Permanent`.
pub(crate) fn map_sqlite(e: rusqlite::Error) -> BackendError {
    if let rusqlite::Error::SqliteFailure(err, _) = &e {
        match err.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                return BackendError::Transient(e.to_string());
            }
            _ => {}
        }
    }
    BackendError::Permanent(e.to_string())
}

/// The SQLite-backed [`TaskBackend`]. Wraps a `rusqlite::Connection` behind a `Mutex`.
///
/// `rusqlite::Connection` is `!Sync`; the trait is `Send + Sync` and `#[async_trait]`
/// requires `Send` futures, so the connection is guarded by a `std::sync::Mutex`. This is
/// *not* `spawn_blocking` / `block_on` — the SQL still runs synchronously inline; the lock
/// only provides the `Sync` the trait surface needs and serializes the (sub-millisecond,
/// already-WAL-serialized) writes. The `*_blocking` inherents lock once and delegate to
/// `_locked` helpers that take the connection by reference, so reentrant calls within one
/// operation reuse the same guard (no deadlock) and keep the read-after-write diagnostics
/// atomic.
///
/// Carries no construction-time actor — every actor-attributed mutation receives an
/// explicit `actor: &str`.
pub struct SqliteBackend {
    conn: Mutex<Connection>,
}

impl SqliteBackend {
    /// Open (creating if needed) the DB at `db_path`. Runs the same WAL PRAGMAs and schema
    /// init as today's `Store::open_actor_unchecked`.
    pub fn open(db_path: &str) -> Result<Self, BackendError> {
        let conn = open_conn(db_path).map_err(|e| BackendError::Permanent(e.to_string()))?;
        init_schema(&conn).map_err(|e| BackendError::Permanent(e.to_string()))?;
        Ok(SqliteBackend {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the connection. Poisoning is mapped to a `Permanent` error rather than
    /// panicking so one failed call cannot wedge the whole backend.
    fn lock(&self) -> BackendResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| BackendError::Permanent("task backend connection mutex poisoned".into()))
    }

    /// Append a log row attributed to an explicit `actor` (worktree name).
    fn log_locked(
        conn: &Connection,
        task_id: i64,
        actor: &str,
        action: &str,
        detail: &str,
    ) -> BackendResult<()> {
        conn.execute(
            "INSERT INTO task_log (task_id, worktree, action, detail) VALUES (?1, ?2, ?3, ?4)",
            params![task_id, actor, action, detail],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    // ---- Reads ----

    fn require_locked(conn: &Connection, id: i64) -> BackendResult<Task> {
        conn.query_row(
            "SELECT * FROM tasks WHERE id = ?1",
            params![id],
            row_to_task,
        )
        .optional()
        .map_err(map_sqlite)?
        .ok_or(BackendError::NotFound(id))
    }

    pub(crate) fn require_blocking(&self, id: i64) -> BackendResult<Task> {
        let conn = self.lock()?;
        Self::require_locked(&conn, id)
    }

    fn attach_deps_locked(conn: &Connection, task: Task) -> BackendResult<TaskWithDeps> {
        let blocked_by = Self::blocked_by_locked(conn, task.id)?;
        let blocks = Self::blocks_locked(conn, task.id)?;
        Ok(TaskWithDeps {
            task,
            blocked_by,
            blocks,
        })
    }

    pub(crate) fn get_blocking(&self, id: i64) -> BackendResult<Option<TaskWithDeps>> {
        let conn = self.lock()?;
        let task = conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(map_sqlite)?;
        match task {
            Some(t) => Ok(Some(Self::attach_deps_locked(&conn, t)?)),
            None => Ok(None),
        }
    }

    pub fn list_blocking(&self, filter: &ListFilter) -> BackendResult<Vec<TaskWithDeps>> {
        let conn = self.lock()?;
        let mut sql = String::from("SELECT * FROM tasks");
        let mut conds: Vec<String> = Vec::new();
        let mut binds: Vec<String> = Vec::new();

        if !filter.include_archived {
            conds.push(format!(
                "NOT (status IN ('done','cancelled') AND updated_at < datetime('now', '-{} days'))",
                filter.archive_days
            ));
        }
        if !filter.include_backlog {
            conds.push("status != 'backlog'".to_string());
        }
        if let Some(who) = &filter.assigned_to {
            conds.push("assigned_to = ?".to_string());
            binds.push(who.clone());
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(
            " ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 \
              WHEN 'normal' THEN 2 WHEN 'low' THEN 3 END, id",
        );

        let tasks = {
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite)?;
            let bind_refs: Vec<&dyn ToSql> = binds.iter().map(|s| s as &dyn ToSql).collect();
            let tasks = stmt
                .query_map(params_from_iter(bind_refs), row_to_task)
                .map_err(map_sqlite)?
                .collect::<rusqlite::Result<Vec<Task>>>()
                .map_err(map_sqlite)?;
            tasks
        };

        tasks
            .into_iter()
            .map(|t| Self::attach_deps_locked(&conn, t))
            .collect()
    }

    pub(crate) fn find_blocking(&self, term: &str, archive_days: i64) -> BackendResult<Vec<Task>> {
        let conn = self.lock()?;
        let like = format!("%{term}%");
        let sql = format!(
            "SELECT * FROM tasks WHERE (title LIKE ?1 OR description LIKE ?1) \
             AND NOT (status IN ('done','cancelled') AND updated_at < datetime('now', '-{archive_days} days')) \
             ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql).map_err(map_sqlite)?;
        let tasks = stmt
            .query_map(params![like], row_to_task)
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<Task>>>()
            .map_err(map_sqlite)?;
        Ok(tasks)
    }

    fn blocked_by_locked(conn: &Connection, id: i64) -> BackendResult<Vec<i64>> {
        let mut stmt = conn
            .prepare(
                "SELECT d.depends_on FROM task_deps d JOIN tasks t ON d.depends_on = t.id \
                 WHERE d.task_id = ?1 AND t.status != 'done'",
            )
            .map_err(map_sqlite)?;
        let ids = stmt
            .query_map(params![id], |r| r.get::<_, i64>(0))
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<i64>>>()
            .map_err(map_sqlite)?;
        Ok(ids)
    }

    pub(crate) fn blocked_by_blocking(&self, id: i64) -> BackendResult<Vec<i64>> {
        let conn = self.lock()?;
        Self::blocked_by_locked(&conn, id)
    }

    fn blocks_locked(conn: &Connection, id: i64) -> BackendResult<Vec<i64>> {
        let mut stmt = conn
            .prepare("SELECT task_id FROM task_deps WHERE depends_on = ?1")
            .map_err(map_sqlite)?;
        let ids = stmt
            .query_map(params![id], |r| r.get::<_, i64>(0))
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<i64>>>()
            .map_err(map_sqlite)?;
        Ok(ids)
    }

    pub(crate) fn blocks_blocking(&self, id: i64) -> BackendResult<Vec<i64>> {
        let conn = self.lock()?;
        Self::blocks_locked(&conn, id)
    }

    pub(crate) fn deps_tree_blocking(&self, id: i64) -> BackendResult<Vec<DepNode>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "WITH RECURSIVE tree(id, depth) AS (
                   SELECT depends_on, 1 FROM task_deps WHERE task_id = ?1
                   UNION ALL
                   SELECT d.depends_on, t.depth + 1
                   FROM task_deps d JOIN tree t ON d.task_id = t.id
                   WHERE t.depth < 10
                 )
                 SELECT tree.id, tree.depth, tasks.title, tasks.status
                 FROM tree JOIN tasks ON tree.id = tasks.id
                 ORDER BY tree.depth, tree.id",
            )
            .map_err(map_sqlite)?;
        let nodes = stmt
            .query_map(params![id], |r| {
                Ok(DepNode {
                    id: r.get(0)?,
                    depth: r.get(1)?,
                    title: r.get(2)?,
                    status: r.get(3)?,
                })
            })
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<DepNode>>>()
            .map_err(map_sqlite)?;
        Ok(nodes)
    }

    pub(crate) fn logs_blocking(&self, id: i64, limit: i64) -> BackendResult<Vec<LogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT created_at, worktree, action, detail FROM task_log \
                 WHERE task_id = ?1 ORDER BY created_at DESC LIMIT ?2",
            )
            .map_err(map_sqlite)?;
        let logs = stmt
            .query_map(params![id, limit], |r| {
                Ok(LogEntry {
                    created_at: r.get(0)?,
                    worktree: r.get(1)?,
                    action: r.get(2)?,
                    detail: r.get(3)?,
                })
            })
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<LogEntry>>>()
            .map_err(map_sqlite)?;
        Ok(logs)
    }

    pub fn revision_blocking(&self) -> BackendResult<String> {
        let conn = self.lock()?;
        let (max_log, count, max_dispatch): (i64, i64, i64) = conn
            .query_row(
                "SELECT COALESCE((SELECT MAX(id) FROM task_log), 0), \
                        (SELECT COUNT(*) FROM tasks), \
                        COALESCE((SELECT MAX(id) FROM dispatch_log), 0)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(map_sqlite)?;
        Ok(format!("{max_log}:{count}:{max_dispatch}"))
    }

    // ---- Writes ----

    pub(crate) fn create_blocking(
        &self,
        actor: &str,
        title: &str,
        description: &str,
        priority: &str,
        tags: &[String],
        // SQLite ignores the idempotency key: its auto-increment primary key makes a
        // server-side dedupe key meaningless. Accepted for trait parity (the GitHub
        // backend uses it for find-before-create dedupe).
        _idempotency_key: Option<&str>,
    ) -> BackendResult<Task> {
        let tags_json = serde_json::to_string(tags)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO tasks (title, description, priority, created_by, tags) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![title, description, priority, actor, tags_json],
        )
        .map_err(map_sqlite)?;
        let id = conn.last_insert_rowid();
        Self::log_locked(&conn, id, actor, "created", title)?;
        Self::require_locked(&conn, id)
    }

    pub(crate) fn claim_blocking(&self, id: i64, actor: &str) -> BackendResult<ClaimOutcome> {
        let conn = self.lock()?;
        // Snapshot the pre-write ownership so a same-actor idempotent re-claim can be
        // reported as `AlreadyYours` rather than `Claimed`. Read inside the same lock
        // as the CAS to keep the decision atomic. A missing task is fine here — the
        // CAS below leaves `changed == 0` and the error branch surfaces `NotFound`.
        let already_yours = conn
            .query_row(
                "SELECT assigned_to FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(map_sqlite)?
            .flatten()
            .as_deref()
            == Some(actor);
        let changed = conn
            .execute(
                "UPDATE tasks SET assigned_to = ?1, status = 'active' \
                 WHERE id = ?2 AND (assigned_to IS NULL OR assigned_to = ?1) \
                 AND NOT EXISTS ( \
                   SELECT 1 FROM task_deps d JOIN tasks t ON d.depends_on = t.id \
                   WHERE d.task_id = ?2 AND t.status != 'done' \
                 )",
                params![actor, id],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?; // NotFound if missing
            if let Some(owner) = &task.assigned_to {
                if owner != actor {
                    return Err(BackendError::Conflict(format!(
                        "task #{id} is already assigned to @{owner}"
                    )));
                }
            }
            let blocked = Self::blocked_by_locked(&conn, id)?;
            if !blocked.is_empty() {
                let list = blocked
                    .iter()
                    .map(|b| format!("#{b}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(BackendError::Conflict(format!(
                    "task #{id} is blocked by: {list}"
                )));
            }
            if task.status == "done" || task.status == "cancelled" {
                return Err(BackendError::Conflict(format!(
                    "task #{id} is already {}",
                    task.status
                )));
            }
            return Err(BackendError::Conflict(format!(
                "could not claim task #{id}"
            )));
        }
        Self::log_locked(&conn, id, actor, "claimed", "")?;
        let task = Self::require_locked(&conn, id)?;
        // SqliteBackend is `Strong`: a fresh CAS → `Claimed`; an idempotent re-claim
        // by the same actor → `AlreadyYours`. It never returns `ClaimedUnverified`.
        if already_yours {
            Ok(ClaimOutcome::AlreadyYours(task))
        } else {
            Ok(ClaimOutcome::Claimed(task))
        }
    }

    pub(crate) fn unclaim_blocking(&self, id: i64, actor: &str) -> BackendResult<()> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET assigned_to = NULL, status = 'open' WHERE id = ?1 AND assigned_to = ?2",
                params![id, actor],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?;
            if task.assigned_to.as_deref() != Some(actor) {
                return Err(BackendError::Conflict(format!(
                    "task #{id} is not assigned to @{actor}"
                )));
            }
            return Err(BackendError::Conflict(format!(
                "could not unclaim task #{id}"
            )));
        }
        Self::log_locked(&conn, id, actor, "unclaimed", "")?;
        Ok(())
    }

    pub(crate) fn set_done_blocking(&self, id: i64, actor: &str) -> BackendResult<DoneOutcome> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET status = 'done'                  WHERE id = ?1 AND (assigned_to = ?2 OR status = 'review')",
                params![id, actor],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?;
            // Idempotency: already done (by the same actor or in terminal state) → success.
            // The trait contract (§9) requires already-done → Ok, not Conflict.
            if task.status == "done" {
                return Ok(DoneOutcome {
                    task,
                    unblocked: vec![],
                });
            }
            let owner = task.assigned_to.as_deref().unwrap_or("nobody");
            return Err(BackendError::Conflict(format!(
                "task #{id} is not assigned to @{actor} (assigned to @{owner})"
            )));
        }
        Self::log_locked(&conn, id, actor, "status_changed", "done")?;

        let unblocked = Self::newly_unblocked_by_locked(&conn, id)?;

        let task = Self::require_locked(&conn, id)?;
        Ok(DoneOutcome { task, unblocked })
    }

    pub(crate) fn set_review_blocking(&self, id: i64, actor: &str) -> BackendResult<Task> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET status = 'review'                  WHERE id = ?1 AND assigned_to = ?2 AND status = 'active'",
                params![id, actor],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?;
            if task.assigned_to.as_deref() != Some(actor) {
                let owner = task.assigned_to.as_deref().unwrap_or("nobody");
                return Err(BackendError::Conflict(format!(
                    "task #{id} is not assigned to @{actor} (assigned to @{owner})"
                )));
            }
            if task.status == "review" {
                return Ok(task);
            }
            return Err(BackendError::Conflict(format!(
                "task #{id} is {}, not active",
                task.status
            )));
        }
        Self::log_locked(&conn, id, actor, "status_changed", "review")?;
        Self::require_locked(&conn, id)
    }

    pub(crate) fn force_done_blocking(&self, id: i64, actor: &str) -> BackendResult<DoneOutcome> {
        let conn = self.lock()?;
        let task = Self::require_locked(&conn, id)?;
        match task.status.as_str() {
            "done" => {
                return Ok(DoneOutcome {
                    task,
                    unblocked: vec![],
                })
            }
            "cancelled" => {
                return Err(BackendError::Conflict(format!(
                    "task #{id} is cancelled; reopen it first"
                )))
            }
            _ => {}
        }
        conn.execute(
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
            params![id],
        )
        .map_err(map_sqlite)?;
        Self::log_locked(&conn, id, actor, "status_changed", "done")?;
        let unblocked = Self::newly_unblocked_by_locked(&conn, id)?;
        let task = Self::require_locked(&conn, id)?;
        Ok(DoneOutcome { task, unblocked })
    }

    pub(crate) fn reopen_blocking(&self, id: i64, actor: &str) -> BackendResult<Task> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET status = 'open', assigned_to = NULL \
                 WHERE id = ?1 AND status IN ('done', 'cancelled', 'backlog')",
                params![id],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?;
            if task.status == "open" && task.assigned_to.is_none() {
                return Ok(task);
            }
            let on = task
                .assigned_to
                .as_deref()
                .map(|w| format!(" on @{w}"))
                .unwrap_or_default();
            return Err(BackendError::Conflict(format!(
                "task #{id} is {}{on}; release it instead",
                task.status
            )));
        }
        Self::log_locked(&conn, id, actor, "status_changed", "open")?;
        Self::require_locked(&conn, id)
    }

    /// IDs of tasks newly unblocked by `id` becoming done: dependents that are still
    /// `'open'` and whose every *other* dep is done. The single indexed unblock query
    /// shared by `set_done_blocking` and `newly_unblocked_by_blocking`.
    fn newly_unblocked_by_locked(conn: &Connection, id: i64) -> BackendResult<Vec<i64>> {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT d.task_id FROM task_deps d \
                 JOIN tasks t ON d.task_id = t.id \
                 WHERE d.depends_on = ?1 AND t.status = 'open' \
                 AND NOT EXISTS ( \
                   SELECT 1 FROM task_deps d2 JOIN tasks t2 ON d2.depends_on = t2.id \
                   WHERE d2.task_id = d.task_id AND t2.status != 'done' AND d2.depends_on != ?1 \
                 )",
            )
            .map_err(map_sqlite)?;
        let unblocked = stmt
            .query_map(params![id], |r| r.get::<_, i64>(0))
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<i64>>>()
            .map_err(map_sqlite)?;
        Ok(unblocked)
    }

    pub(crate) fn newly_unblocked_by_blocking(&self, id: i64) -> BackendResult<Vec<i64>> {
        let conn = self.lock()?;
        Self::newly_unblocked_by_locked(&conn, id)
    }

    pub(crate) fn cancel_blocking(&self, id: i64, actor: &str) -> BackendResult<()> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET status = 'cancelled' WHERE id = ?1",
                params![id],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            return Err(BackendError::NotFound(id));
        }
        Self::log_locked(&conn, id, actor, "status_changed", "cancelled")?;
        Ok(())
    }

    pub(crate) fn delete_blocking(&self, id: i64) -> BackendResult<()> {
        let conn = self.lock()?;
        // One transaction so the exists/dependent checks and the DELETE see a
        // consistent snapshot and the row + its owned edges/logs drop atomically.
        let tx = conn.unchecked_transaction().map_err(map_sqlite)?;

        // 1. Exists check → NotFound if absent (also makes a repeated delete → NotFound).
        Self::require_locked(&tx, id)?;

        // 2. Dependents (the `blocks` direction: tasks that depend on `id`). Block + name
        //    them rather than silently unblock other work (D3).
        let dependents = Self::blocks_locked(&tx, id)?;
        if !dependents.is_empty() {
            return Err(crate::backend::dependents_conflict(id, &dependents));
        }

        // 3. Hard delete. `task_deps`/`task_log` carry ON DELETE CASCADE so they drop
        //    with the row; `dispatch_log` has no FK to `tasks` (its DDL is shared with
        //    the GitHub sidecar, which has no `tasks` table — see github::sidecar), so
        //    purge it explicitly. No audit record is kept (D6).
        tx.execute("DELETE FROM dispatch_log WHERE task_id = ?1", params![id])
            .map_err(map_sqlite)?;
        tx.execute("DELETE FROM tasks WHERE id = ?1", params![id])
            .map_err(map_sqlite)?;

        tx.commit().map_err(map_sqlite)?;
        Ok(())
    }

    pub(crate) fn backlog_blocking(&self, id: i64, actor: &str) -> BackendResult<()> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET status = 'backlog', assigned_to = NULL WHERE id = ?1",
                params![id],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            return Err(BackendError::NotFound(id));
        }
        Self::log_locked(&conn, id, actor, "status_changed", "backlog")?;
        Ok(())
    }

    pub(crate) fn update_blocking(
        &self,
        id: i64,
        actor: &str,
        upd: &TaskUpdate,
    ) -> BackendResult<()> {
        if upd.is_empty() {
            return Err(BackendError::Conflict("nothing to update".to_string()));
        }
        let conn = self.lock()?;
        Self::require_locked(&conn, id)?; // NotFound if missing

        let mut sets: Vec<&str> = Vec::new();
        let mut vals: Vec<String> = Vec::new();
        let mut fields: Vec<&str> = Vec::new();
        let push = |col: &'static str,
                    set: &'static str,
                    v: &Option<String>,
                    sets: &mut Vec<&str>,
                    vals: &mut Vec<String>,
                    fields: &mut Vec<&str>| {
            if let Some(x) = v {
                sets.push(set);
                vals.push(x.clone());
                fields.push(col);
            }
        };
        push(
            "title",
            "title = ?",
            &upd.title,
            &mut sets,
            &mut vals,
            &mut fields,
        );
        push(
            "description",
            "description = ?",
            &upd.description,
            &mut sets,
            &mut vals,
            &mut fields,
        );
        push(
            "priority",
            "priority = ?",
            &upd.priority,
            &mut sets,
            &mut vals,
            &mut fields,
        );
        push(
            "status",
            "status = ?",
            &upd.status,
            &mut sets,
            &mut vals,
            &mut fields,
        );
        push(
            "branch",
            "branch = ?",
            &upd.branch,
            &mut sets,
            &mut vals,
            &mut fields,
        );
        push(
            "pr_url",
            "pr_url = ?",
            &upd.pr_url,
            &mut sets,
            &mut vals,
            &mut fields,
        );
        let tags_json = upd.tags.as_ref().map(serde_json::to_string).transpose()?;
        push(
            "tags",
            "tags = ?",
            &tags_json,
            &mut sets,
            &mut vals,
            &mut fields,
        );

        let sql = format!("UPDATE tasks SET {} WHERE id = ?", sets.join(", "));
        let mut p: Vec<&dyn ToSql> = vals.iter().map(|s| s as &dyn ToSql).collect();
        p.push(&id);
        conn.execute(&sql, params_from_iter(p))
            .map_err(map_sqlite)?;
        Self::log_locked(&conn, id, actor, "updated", &fields.join(", "))?;
        Ok(())
    }

    pub(crate) fn add_dep_blocking(
        &self,
        task_id: i64,
        depends_on: i64,
        actor: &str,
    ) -> BackendResult<()> {
        if task_id == depends_on {
            return Err(BackendError::Conflict(
                "a task cannot depend on itself".to_string(),
            ));
        }
        let conn = self.lock()?;
        // Cycle detection: would `depends_on` reach `task_id` already?
        let cycle: Option<i64> = conn
            .query_row(
                "WITH RECURSIVE chain(id) AS (
                   SELECT ?1
                   UNION ALL
                   SELECT d.depends_on FROM task_deps d JOIN chain c ON d.task_id = c.id
                 )
                 SELECT 1 FROM chain WHERE id = ?2 LIMIT 1",
                params![depends_on, task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(map_sqlite)?;
        if cycle.is_some() {
            return Err(BackendError::Conflict(
                "adding this dependency would create a cycle".to_string(),
            ));
        }
        let res = conn.execute(
            "INSERT INTO task_deps (task_id, depends_on) VALUES (?1, ?2)",
            params![task_id, depends_on],
        );
        match res {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(BackendError::Conflict(
                    "dependency already exists".to_string(),
                ));
            }
            Err(e) => return Err(map_sqlite(e)),
        }
        Self::log_locked(
            &conn,
            task_id,
            actor,
            "updated",
            &format!("added dependency on #{depends_on}"),
        )?;
        Ok(())
    }

    pub(crate) fn remove_dep_blocking(
        &self,
        task_id: i64,
        depends_on: i64,
        actor: &str,
    ) -> BackendResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM task_deps WHERE task_id = ?1 AND depends_on = ?2",
            params![task_id, depends_on],
        )
        .map_err(map_sqlite)?;
        Self::log_locked(
            &conn,
            task_id,
            actor,
            "updated",
            &format!("removed dependency on #{depends_on}"),
        )?;
        Ok(())
    }

    // ---- Dispatcher primitives ----

    pub(crate) fn assign_blocking(
        &self,
        id: i64,
        actor: &str,
        worktree: &str,
    ) -> BackendResult<Task> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET assigned_to = ?1 \
                 WHERE id = ?2 \
                   AND (assigned_to IS NULL OR assigned_to = ?1) \
                   AND status = 'open' \
                   AND NOT EXISTS ( \
                     SELECT 1 FROM task_deps d JOIN tasks t ON d.depends_on = t.id \
                     WHERE d.task_id = ?2 AND t.status != 'done' \
                   )",
                params![worktree, id],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?; // NotFound if missing
            if let Some(owner) = &task.assigned_to {
                if owner != worktree {
                    return Err(BackendError::Conflict(format!(
                        "task #{id} is already assigned to @{owner}"
                    )));
                }
            }
            let blocked = Self::blocked_by_locked(&conn, id)?;
            if !blocked.is_empty() {
                let list = blocked
                    .iter()
                    .map(|b| format!("#{b}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(BackendError::Conflict(format!(
                    "task #{id} is blocked by: {list}"
                )));
            }
            // Anything not `open` (active/backlog/done/cancelled) can't be reserved.
            if task.status != "open" {
                return Err(BackendError::Conflict(format!(
                    "task #{id} is not open (it is {})",
                    task.status
                )));
            }
            return Err(BackendError::Conflict(format!(
                "could not assign task #{id}"
            )));
        }
        // Log as the dispatcher `actor` (the app = `keitora`), recording who dispatched; the
        // target worktree authors its own `claimed` log when it runs `./keitora task claim`.
        Self::log_locked(&conn, id, actor, "assigned", &format!("to @{worktree}"))?;
        Self::require_locked(&conn, id)
    }

    pub(crate) fn release_blocking(&self, id: i64, actor: &str) -> BackendResult<()> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE tasks SET assigned_to = NULL, status = 'open' \
                 WHERE id = ?1 AND status NOT IN ('done','cancelled','backlog')",
                params![id],
            )
            .map_err(map_sqlite)?;
        if changed == 0 {
            let task = Self::require_locked(&conn, id)?; // NotFound if missing
            return Err(BackendError::Conflict(format!(
                "task #{id} cannot be released (it is {})",
                task.status
            )));
        }
        Self::log_locked(&conn, id, actor, "released", "")?;
        Ok(())
    }

    // ---- Dispatch log ----

    pub(crate) fn dispatch_log_add_blocking(
        &self,
        task_id: i64,
        worktree: &str,
        decision: &str,
        rationale: &str,
    ) -> BackendResult<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO dispatch_log (task_id, worktree, decision, rationale) \
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, worktree, decision, rationale],
        )
        .map_err(map_sqlite)?;
        Ok(())
    }

    pub(crate) fn dispatch_log_list_blocking(
        &self,
        limit: i64,
    ) -> BackendResult<Vec<DispatchLogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, worktree, decision, rationale, created_at \
                 FROM dispatch_log ORDER BY created_at DESC, id DESC LIMIT ?1",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![limit], row_to_dispatch_log_entry)
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<DispatchLogEntry>>>()
            .map_err(map_sqlite)?;
        Ok(rows)
    }

    pub(crate) fn dispatch_log_for_blocking(
        &self,
        task_id: i64,
    ) -> BackendResult<Vec<DispatchLogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, worktree, decision, rationale, created_at \
                 FROM dispatch_log WHERE task_id = ?1 ORDER BY created_at DESC, id DESC",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![task_id], row_to_dispatch_log_entry)
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<DispatchLogEntry>>>()
            .map_err(map_sqlite)?;
        Ok(rows)
    }

    pub(crate) fn dispatch_log_latest_for_blocking(
        &self,
        task_id: i64,
        worktree: &str,
    ) -> BackendResult<Option<DispatchLogEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, task_id, worktree, decision, rationale, created_at \
                 FROM dispatch_log WHERE task_id = ?1 AND worktree = ?2 \
                 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .map_err(map_sqlite)?;
        let mut rows = stmt
            .query_map(params![task_id, worktree], row_to_dispatch_log_entry)
            .map_err(map_sqlite)?;
        rows.next().transpose().map_err(map_sqlite)
    }

    pub(crate) fn dispatched_reservations_blocking(&self) -> BackendResult<Vec<Reservation>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT t.id, t.assigned_to FROM tasks t \
                 WHERE t.status = 'open' AND t.assigned_to IS NOT NULL \
                   AND ( SELECT dl.decision FROM dispatch_log dl \
                         WHERE dl.task_id = t.id AND dl.worktree = t.assigned_to \
                         ORDER BY dl.created_at DESC, dl.id DESC LIMIT 1 ) = 'dispatched'",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Reservation {
                    task_id: r.get::<_, i64>(0)?,
                    worktree: r.get::<_, String>(1)?,
                })
            })
            .map_err(map_sqlite)?
            .collect::<rusqlite::Result<Vec<Reservation>>>()
            .map_err(map_sqlite)?;
        Ok(rows)
    }
}

#[async_trait]
impl TaskBackend for SqliteBackend {
    fn consistency(&self) -> Consistency {
        Consistency::Strong
    }

    async fn revision(&self) -> BackendResult<String> {
        self.revision_blocking()
    }

    // ---- Reads ----

    async fn get(&self, id: i64) -> BackendResult<Option<TaskWithDeps>> {
        self.get_blocking(id)
    }

    async fn list(&self, filter: &ListFilter) -> BackendResult<Vec<TaskWithDeps>> {
        self.list_blocking(filter)
    }

    async fn find(&self, term: &str, archive_days: i64) -> BackendResult<Vec<Task>> {
        self.find_blocking(term, archive_days)
    }

    async fn logs(&self, id: i64, limit: i64) -> BackendResult<Vec<LogEntry>> {
        self.logs_blocking(id, limit)
    }

    async fn deps_tree(&self, id: i64) -> BackendResult<Vec<DepNode>> {
        self.deps_tree_blocking(id)
    }

    // Override the provided default with the targeted indexed query.
    async fn require(&self, id: i64) -> BackendResult<Task> {
        self.require_blocking(id)
    }

    async fn blocked_by(&self, id: i64) -> BackendResult<Vec<i64>> {
        self.blocked_by_blocking(id)
    }

    async fn blocks(&self, id: i64) -> BackendResult<Vec<i64>> {
        self.blocks_blocking(id)
    }

    // ---- Mutations: task lifecycle ----

    async fn create(
        &self,
        actor: &str,
        title: &str,
        description: &str,
        priority: &str,
        tags: &[String],
        idempotency_key: Option<&str>,
    ) -> BackendResult<Task> {
        self.create_blocking(actor, title, description, priority, tags, idempotency_key)
    }

    async fn claim(&self, id: i64, actor: &str) -> BackendResult<ClaimOutcome> {
        self.claim_blocking(id, actor)
    }

    async fn unclaim(&self, id: i64, actor: &str) -> BackendResult<()> {
        self.unclaim_blocking(id, actor)
    }

    async fn set_done(&self, id: i64, actor: &str) -> BackendResult<DoneOutcome> {
        self.set_done_blocking(id, actor)
    }

    async fn set_review(&self, id: i64, actor: &str) -> BackendResult<Task> {
        self.set_review_blocking(id, actor)
    }

    async fn force_done(&self, id: i64, actor: &str) -> BackendResult<DoneOutcome> {
        self.force_done_blocking(id, actor)
    }

    async fn reopen(&self, id: i64, actor: &str) -> BackendResult<Task> {
        self.reopen_blocking(id, actor)
    }

    // Override the default (read-derived) impl with the targeted indexed query.
    async fn newly_unblocked_by(&self, id: i64) -> BackendResult<Vec<i64>> {
        self.newly_unblocked_by_blocking(id)
    }

    async fn cancel(&self, id: i64, actor: &str) -> BackendResult<()> {
        self.cancel_blocking(id, actor)
    }

    async fn delete(&self, id: i64) -> BackendResult<()> {
        self.delete_blocking(id)
    }

    async fn backlog(&self, id: i64, actor: &str) -> BackendResult<()> {
        self.backlog_blocking(id, actor)
    }

    async fn update(&self, id: i64, actor: &str, upd: &TaskUpdate) -> BackendResult<()> {
        self.update_blocking(id, actor, upd)
    }

    // ---- Mutations: dependency graph ----

    async fn add_dep(&self, task_id: i64, depends_on: i64, actor: &str) -> BackendResult<()> {
        self.add_dep_blocking(task_id, depends_on, actor)
    }

    async fn remove_dep(&self, task_id: i64, depends_on: i64, actor: &str) -> BackendResult<()> {
        self.remove_dep_blocking(task_id, depends_on, actor)
    }

    // ---- Dispatcher primitives ----

    async fn assign(&self, id: i64, actor: &str, worktree: &str) -> BackendResult<Task> {
        self.assign_blocking(id, actor, worktree)
    }

    async fn release(&self, id: i64, actor: &str) -> BackendResult<()> {
        self.release_blocking(id, actor)
    }

    async fn dispatch_log_add(
        &self,
        task_id: i64,
        worktree: &str,
        decision: &str,
        rationale: &str,
    ) -> BackendResult<()> {
        self.dispatch_log_add_blocking(task_id, worktree, decision, rationale)
    }

    async fn dispatch_log_list(&self, limit: i64) -> BackendResult<Vec<DispatchLogEntry>> {
        self.dispatch_log_list_blocking(limit)
    }

    async fn dispatch_log_for(&self, task_id: i64) -> BackendResult<Vec<DispatchLogEntry>> {
        self.dispatch_log_for_blocking(task_id)
    }

    async fn dispatch_log_latest_for(
        &self,
        task_id: i64,
        worktree: &str,
    ) -> BackendResult<Option<DispatchLogEntry>> {
        self.dispatch_log_latest_for_blocking(task_id, worktree)
    }

    async fn dispatched_reservations(&self) -> BackendResult<Vec<Reservation>> {
        self.dispatched_reservations_blocking()
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::backend_tests;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CTRACT_COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDb {
        pub path: PathBuf,
    }
    impl TempDb {
        fn new() -> Self {
            let n = CTRACT_COUNTER.fetch_add(1, Ordering::SeqCst);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "keitora-contract-test-{}-{}.db",
                std::process::id(),
                n
            ));
            let td = TempDb { path };
            td.cleanup();
            td
        }
        fn open(&self) -> SqliteBackend {
            SqliteBackend::open(self.path.to_str().unwrap()).unwrap()
        }
        fn cleanup(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{}", self.path.display(), suffix));
            }
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn universal_tier() {
        let db = TempDb::new();
        let b = db.open();
        backend_tests::run_universal(&b).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn strong_tier() {
        let db = TempDb::new();
        let b = db.open();
        // Sanity-check: SqliteBackend must advertise Strong before running the tier.
        assert!(
            matches!(b.consistency(), crate::Consistency::Strong),
            "SqliteBackend must return Strong consistency"
        );
        backend_tests::run_strong(&b).await;

        // The concurrent-claim test needs its own db_path (N separate handles on one file).
        let race_db = TempDb::new();
        backend_tests::test_concurrent_claim_exactly_one_wins(race_db.path.to_str().unwrap()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway on-disk DB so multiple actors (worktrees) can share it.
    /// (`:memory:` is per-connection, so it can't model two worktrees.)
    struct TempDb {
        path: PathBuf,
    }
    impl TempDb {
        fn new() -> TempDb {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "keitora-sqlbackend-test-{}-{}.db",
                std::process::id(),
                n
            ));
            let td = TempDb { path };
            td.cleanup_files();
            td
        }
        fn open(&self) -> SqliteBackend {
            SqliteBackend::open(self.path.to_str().unwrap()).unwrap()
        }
        fn cleanup_files(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{}", self.path.display(), suffix));
            }
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            self.cleanup_files();
        }
    }

    #[tokio::test]
    async fn consistency_is_strong() {
        let db = TempDb::new();
        let b = db.open();
        assert!(matches!(b.consistency(), Consistency::Strong));
    }

    #[tokio::test]
    async fn create_get_round_trip() {
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create(
                "alice",
                "hello",
                "world",
                "high",
                &["x".into(), "y".into()],
                None,
            )
            .await
            .unwrap();
        assert_eq!(task.title, "hello");
        assert_eq!(task.description, "world");
        assert_eq!(task.priority, "high");
        assert_eq!(task.status, "open");
        assert_eq!(task.created_by.as_deref(), Some("alice"));
        assert_eq!(task.tags, vec!["x".to_string(), "y".to_string()]);

        let got = b.get(task.id).await.unwrap().unwrap();
        assert_eq!(got.task.id, task.id);
        assert_eq!(got.task.created_by.as_deref(), Some("alice"));
        assert_eq!(got.task.tags, task.tags);
    }

    #[tokio::test]
    async fn claim_then_set_done_transitions() {
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create("alice", "work", "", "normal", &[], None)
            .await
            .unwrap();

        let claimed = b.claim(task.id, "alice").await.unwrap();
        assert!(
            matches!(claimed, ClaimOutcome::Claimed(_)),
            "fresh claim must be Claimed, got {claimed:?}"
        );
        let claimed = claimed.task();
        assert_eq!(claimed.status, "active");
        assert_eq!(claimed.assigned_to.as_deref(), Some("alice"));

        // Idempotent re-claim by the same actor → AlreadyYours.
        let reclaimed = b.claim(task.id, "alice").await.unwrap();
        assert!(
            matches!(reclaimed, ClaimOutcome::AlreadyYours(_)),
            "same-actor re-claim must be AlreadyYours, got {reclaimed:?}"
        );

        let outcome = b.set_done(task.id, "alice").await.unwrap();
        assert_eq!(outcome.task.status, "done");
        assert!(outcome.unblocked.is_empty());
    }

    #[tokio::test]
    async fn review_keeps_the_assignee_and_only_then_lets_anyone_finish() {
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create("alice", "work", "", "normal", &[], None)
            .await
            .unwrap();
        // Only an active task can go to review.
        assert!(matches!(
            b.set_review(task.id, "alpha").await,
            Err(BackendError::Conflict(_))
        ));
        b.claim(task.id, "alpha").await.unwrap();
        // Not the assignee: refused, and so is finishing it from elsewhere while active.
        assert!(matches!(
            b.set_review(task.id, "bravo").await,
            Err(BackendError::Conflict(_))
        ));
        assert!(matches!(
            b.set_done(task.id, "main").await,
            Err(BackendError::Conflict(_))
        ));

        let reviewed = b.set_review(task.id, "alpha").await.unwrap();
        assert_eq!(reviewed.status, "review");
        assert_eq!(reviewed.assigned_to.as_deref(), Some("alpha"));
        // Idempotent.
        assert_eq!(
            b.set_review(task.id, "alpha").await.unwrap().status,
            "review"
        );
        let logs = b.logs(task.id, 10).await.unwrap();
        assert!(logs
            .iter()
            .any(|l| l.action == "status_changed" && l.detail == "review"));

        // In review, a human anywhere can mark it done.
        let done = b.set_done(task.id, "main").await.unwrap();
        assert_eq!(done.task.status, "done");
        assert_eq!(done.task.assigned_to.as_deref(), Some("alpha"));
    }

    #[tokio::test]
    async fn reclaiming_a_task_in_review_makes_it_active_again() {
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create("alice", "work", "", "normal", &[], None)
            .await
            .unwrap();
        b.claim(task.id, "alpha").await.unwrap();
        b.set_review(task.id, "alpha").await.unwrap();
        assert!(matches!(
            b.claim(task.id, "bravo").await,
            Err(BackendError::Conflict(_))
        ));
        let again = b.claim(task.id, "alpha").await.unwrap().task();
        assert_eq!(again.status, "active");
    }

    /// A DB created before `review` existed is rebuilt on open: rows, ids, deps, logs
    /// and the id counter survive, and `review` is then accepted.
    #[tokio::test]
    async fn opening_an_old_db_upgrades_its_status_check() {
        let db = TempDb::new();
        {
            let conn = Connection::open(&db.path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   title TEXT NOT NULL,
                   description TEXT NOT NULL DEFAULT '',
                   status TEXT NOT NULL DEFAULT 'open'
                     CHECK(status IN ('open', 'active', 'backlog', 'done', 'cancelled')),
                   priority TEXT NOT NULL DEFAULT 'normal'
                     CHECK(priority IN ('low', 'normal', 'high', 'urgent')),
                   assigned_to TEXT DEFAULT NULL,
                   created_by TEXT DEFAULT NULL,
                   tags TEXT NOT NULL DEFAULT '[]',
                   branch TEXT DEFAULT NULL,
                   pr_url TEXT DEFAULT NULL,
                   created_at TEXT NOT NULL DEFAULT (datetime('now')),
                   updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 CREATE TABLE task_deps (
                   task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                   depends_on INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                   PRIMARY KEY (task_id, depends_on)
                 );
                 CREATE TABLE task_log (
                   id INTEGER PRIMARY KEY AUTOINCREMENT,
                   task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                   worktree TEXT NOT NULL,
                   action TEXT NOT NULL,
                   detail TEXT NOT NULL DEFAULT '',
                   created_at TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 INSERT INTO tasks (title, status, assigned_to, branch) VALUES
                   ('a', 'active', 'alpha', 'feat/a'), ('b', 'open', NULL, NULL),
                   ('gone', 'open', NULL, NULL);
                 DELETE FROM tasks WHERE id = 3;
                 INSERT INTO task_deps (task_id, depends_on) VALUES (2, 1);
                 INSERT INTO task_log (task_id, worktree, action) VALUES (1, 'alpha', 'claimed');",
            )
            .unwrap();
        }
        let b = db.open();
        let a = b.require(1).await.unwrap();
        assert_eq!(
            (
                a.status.as_str(),
                a.assigned_to.as_deref(),
                a.branch.as_deref()
            ),
            ("active", Some("alpha"), Some("feat/a"))
        );
        assert_eq!(b.blocked_by(2).await.unwrap(), vec![1]);
        assert_eq!(b.logs(1, 10).await.unwrap().len(), 1);
        assert_eq!(b.set_review(1, "alpha").await.unwrap().status, "review");
        // The deleted id 3 is not handed out again.
        let next = b
            .create("alice", "c", "", "normal", &[], None)
            .await
            .unwrap();
        assert_eq!(next.id, 4);
        // The rebuilt table's trigger still stamps updates, and reopening is a no-op.
        drop(b);
        let b = db.open();
        assert_eq!(b.require(1).await.unwrap().status, "review");
    }

    #[tokio::test]
    async fn claim_returns_conflict_on_wrong_actor() {
        // Two actors sharing the same on-disk DB — a Strong-tier exclusivity check.
        let db = TempDb::new();
        let a = db.open();
        let task = a
            .create("alice", "contended", "", "normal", &[], None)
            .await
            .unwrap();
        a.claim(task.id, "alice").await.unwrap();

        let b = db.open();
        let err = b.claim(task.id, "bob").await.unwrap_err();
        assert!(matches!(err, BackendError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn missing_task_is_not_found() {
        let db = TempDb::new();
        let b = db.open();
        let err = b.claim(999, "alice").await.unwrap_err();
        assert!(matches!(err, BackendError::NotFound(999)), "got {err:?}");
    }

    #[tokio::test]
    async fn dispatch_reserve_default_reserves_fresh_open_task() {
        // The provided default (sqlite inherits it): a fresh open task reserves to
        // the worktree and carries the post-assign Task.
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create("alice", "fresh", "", "normal", &[], None)
            .await
            .unwrap();

        let outcome = b
            .dispatch_reserve(task.id, "keitora", "feng")
            .await
            .unwrap();
        match outcome {
            crate::ReserveOutcome::Reserved(t) => {
                assert_eq!(t.id, task.id);
                assert_eq!(t.status, "open", "reserve leaves status open");
                assert_eq!(t.assigned_to.as_deref(), Some("feng"));
            }
            other => panic!("expected Reserved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_reserve_default_guards_already_dispatched() {
        // status=open + assigned=worktree + latest decision="dispatched" → the
        // idempotency guard fires and NO new reserve write happens.
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create("alice", "guarded", "", "normal", &[], None)
            .await
            .unwrap();
        b.assign(task.id, "keitora", "feng").await.unwrap();
        b.dispatch_log_add(task.id, "feng", "dispatched", "")
            .await
            .unwrap();

        let outcome = b
            .dispatch_reserve(task.id, "keitora", "feng")
            .await
            .unwrap();
        assert!(
            matches!(outcome, crate::ReserveOutcome::AlreadyDispatched),
            "expected AlreadyDispatched, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_reserve_default_conflict_bubbles_from_assign() {
        // A terminal (done) task is ineligible for reserve: the `Conflict` from the
        // underlying `assign` must bubble up through the default.
        let db = TempDb::new();
        let b = db.open();
        let task = b
            .create("alice", "terminal", "", "normal", &[], None)
            .await
            .unwrap();
        b.claim(task.id, "keitora").await.unwrap();
        b.set_done(task.id, "keitora").await.unwrap();

        let err = b
            .dispatch_reserve(task.id, "keitora", "feng")
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn board_snapshot_default_equals_list_plus_reservations() {
        // The provided default sequences `list` + `dispatched_reservations`; the
        // result must equal calling the two primitives directly.
        let db = TempDb::new();
        let b = db.open();
        let open = b
            .create("alice", "open-one", "", "normal", &[], None)
            .await
            .unwrap();
        // A dispatched-and-reserved task so `reservations` is non-empty.
        let reserved = b
            .create("alice", "reserved-one", "", "normal", &[], None)
            .await
            .unwrap();
        b.assign(reserved.id, "keitora", "feng").await.unwrap();
        b.dispatch_log_add(reserved.id, "feng", "dispatched", "")
            .await
            .unwrap();

        let filter = ListFilter::default();
        let snapshot = b.board_snapshot(&filter).await.unwrap();
        let tasks = b.list(&filter).await.unwrap();
        let reservations = b.dispatched_reservations().await.unwrap();

        assert_eq!(
            snapshot.tasks.iter().map(|t| t.task.id).collect::<Vec<_>>(),
            tasks.iter().map(|t| t.task.id).collect::<Vec<_>>(),
            "snapshot tasks must equal list()"
        );
        assert_eq!(
            snapshot
                .reservations
                .iter()
                .map(|r| (r.task_id, r.worktree.clone()))
                .collect::<Vec<_>>(),
            reservations
                .iter()
                .map(|r| (r.task_id, r.worktree.clone()))
                .collect::<Vec<_>>(),
            "snapshot reservations must equal dispatched_reservations()"
        );
        // Sanity: the seeded data actually exercises both halves.
        assert!(snapshot.tasks.iter().any(|t| t.task.id == open.id));
        assert!(snapshot
            .reservations
            .iter()
            .any(|r| r.task_id == reserved.id && r.worktree == "feng"));
    }
}
