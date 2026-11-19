//! `LocalStore` — the synchronous, concrete store for the always-local tables
//! (worktree sessions, resume sessions).
//!
//! These tables are never backed by a remote task backend (e.g. a future GitHub
//! backend); they stay SQLite + synchronous regardless of which `TaskBackend` a
//! project uses. `LocalStore` therefore keeps the existing concrete
//! [`crate::Error`] / [`crate::Result`] — it is **not** behind the
//! [`TaskBackend`](crate::TaskBackend) trait and does not use `BackendError`.
//!
//! The method bodies are moved verbatim from the original `Store` in
//! [`lib.rs`](crate); the transitional `Store` façade now delegates to them.

use rusqlite::{params, OptionalExtension};

use crate::config::CliKind;
use crate::model::{self, ActiveTask, DispatchRequest, WorktreeSession};
use crate::sqlite_backend::{init_schema, open_conn};
use crate::{Error, Result};

/// Sync concrete store over the always-local tables. Holds its own
/// `rusqlite::Connection`, opened via the shared [`open_conn`] helper. Safe to
/// open against the same DB file as an accompanying
/// [`SqliteBackend`](crate::SqliteBackend) (WAL-safe concurrent opens).
pub struct LocalStore {
    conn: rusqlite::Connection,
}

/// One `resume_sessions` row: the agent conversation a (project, worktree)
/// should resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRow {
    pub cli: CliKind,
    /// `None` after a fresh-start clear.
    pub session_id: Option<String>,
    /// Who reported the id, e.g. `"herdr:claude"`.
    pub source: String,
    /// Whether the latest observation saw the agent running in the pane.
    pub agent_present: bool,
    /// The id a fresh start cleared; never accepted again.
    pub cleared_id: Option<String>,
    /// Unix epoch milliseconds of the observation that last wrote the row.
    pub observed_at: i64,
}

fn cli_to_sql(cli: CliKind) -> &'static str {
    match cli {
        CliKind::Claude => "claude",
        CliKind::Codex => "codex",
    }
}

fn cli_from_sql(s: &str) -> rusqlite::Result<CliKind> {
    match s {
        "claude" => Ok(CliKind::Claude),
        "codex" => Ok(CliKind::Codex),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("unknown cli {other:?}").into(),
        )),
    }
}

impl LocalStore {
    /// Open (creating if needed) the DB at `db_path`. Runs the same WAL PRAGMAs
    /// and full schema init as [`SqliteBackend::open`](crate::SqliteBackend::open),
    /// so both handles see every table when pointed at the same file.
    pub fn open(db_path: &str) -> Result<Self> {
        let conn = open_conn(db_path)?;
        // Shared with `SqliteBackend::open` (see `sqlite_backend::init_schema`) — both
        // handles run the **full** init so each sees all tables on a shared file.
        init_schema(&conn)?;
        Ok(LocalStore { conn })
    }

    /// How long a read-only handle waits on a locked DB before giving up.
    pub const READ_ONLY_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

    /// Open an existing DB for reading only: no PRAGMA writes, no schema init, and
    /// a short [`Self::READ_ONLY_BUSY_TIMEOUT`], so a poll over many projects can't
    /// stall on one locked file. Errors when the file is missing (it is never
    /// created); reads of a table the DB lacks error too.
    pub fn open_read_only(db_path: &str) -> Result<Self> {
        let path = crate::config::normalize(&crate::config::strip_extended_prefix(
            std::path::PathBuf::from(db_path),
        ));
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Self::READ_ONLY_BUSY_TIMEOUT)?;
        Ok(LocalStore { conn })
    }

    // ---- Worktree sessions ----

    /// Upsert `worktree`'s session state. (In the original `Store` the worktree
    /// key was the construction-time `self.actor`; here it is passed explicitly so
    /// `LocalStore` carries no construction-time actor.)
    pub fn session_upsert(
        &self,
        worktree: &str,
        state: &str,
        current_task_id: Option<i64>,
        pid: Option<i64>,
    ) -> Result<()> {
        if !model::SESSION_STATES.contains(&state) {
            return Err(Error::Conflict(format!("invalid session state: {state}")));
        }
        self.conn.execute(
            "INSERT INTO worktree_sessions (worktree, state, current_task_id, pid, last_heartbeat) \
             VALUES (?1, ?2, ?3, ?4, datetime('now')) \
             ON CONFLICT(worktree) DO UPDATE SET \
               state = excluded.state, current_task_id = excluded.current_task_id, \
               pid = excluded.pid, last_heartbeat = excluded.last_heartbeat",
            params![worktree, state, current_task_id, pid],
        )?;
        Ok(())
    }

    /// Force a specific worktree offline (used by the app when its PTY exits).
    /// Clears `current_task_id` and `pid` so an offline row never advertises a
    /// stale active task.
    pub fn session_set_offline(&self, worktree: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO worktree_sessions (worktree, state, last_heartbeat) \
             VALUES (?1, 'offline', datetime('now')) \
             ON CONFLICT(worktree) DO UPDATE SET \
               state = 'offline', current_task_id = NULL, pid = NULL, last_heartbeat = datetime('now')",
            params![worktree],
        )?;
        Ok(())
    }

    /// The id of `worktree`'s active (claimed) task, if any. A cheap targeted read
    /// for the session hook — avoids loading the full task list every turn.
    pub fn active_task_id(&self, worktree: &str) -> Result<Option<i64>> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM tasks WHERE assigned_to = ?1 AND status = 'active' ORDER BY id LIMIT 1",
                params![worktree],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Every worktree's active task, by worktree name. A worktree with more than one
    /// gets its lowest id, matching [`Self::active_task_id`].
    pub fn active_tasks(&self) -> Result<Vec<ActiveTask>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.assigned_to, t.id, t.title FROM tasks t \
             WHERE t.status = 'active' AND t.assigned_to IS NOT NULL \
               AND t.id = (SELECT MIN(u.id) FROM tasks u \
                           WHERE u.status = 'active' AND u.assigned_to = t.assigned_to) \
             ORDER BY t.assigned_to",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ActiveTask {
                    worktree: r.get(0)?,
                    id: r.get(1)?,
                    title: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<ActiveTask>>>()?;
        Ok(rows)
    }

    pub fn sessions_list(&self) -> Result<Vec<WorktreeSession>> {
        // current_task_id is a hook-written cache; validated here against the
        // authoritative tasks table — the pointer is nulled unless it names a
        // task that is still status='active' AND assigned_to = this worktree.
        let mut stmt = self.conn.prepare(
            "SELECT s.worktree, s.state, \
                    CASE WHEN t.id IS NOT NULL THEN s.current_task_id END AS current_task_id, \
                    s.pid, s.last_heartbeat \
             FROM worktree_sessions s \
             LEFT JOIN tasks t \
               ON t.id = s.current_task_id \
              AND t.assigned_to = s.worktree \
              AND t.status = 'active' \
             ORDER BY s.worktree",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(WorktreeSession {
                    // No project column in the DB; stamped by `sessions_list` (the
                    // active project namespace).
                    project: String::new(),
                    worktree: r.get(0)?,
                    state: r.get(1)?,
                    current_task_id: r.get(2)?,
                    pid: r.get(3)?,
                    last_heartbeat: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<WorktreeSession>>>()?;
        Ok(rows)
    }

    /// A **state + reconciled-task** change marker (`"{worktree}={state}:{rid};…"`,
    /// sorted). Excludes `last_heartbeat` so heartbeat-only re-upserts (e.g. repeated
    /// `Stop`) don't trip `sessions://changed` — only real state transitions **or a
    /// change in the reconciled active-task pointer** (task leaving `active`, or being
    /// reassigned) do. `rid` is the reconciled task id (empty string when none).
    pub fn session_revision(&self) -> Result<String> {
        let rev: Option<String> = self.conn.query_row(
            "SELECT group_concat(worktree || '=' || state || ':' || COALESCE(rid, ''), ';') \
             FROM ( \
               SELECT s.worktree, s.state, \
                      (SELECT t.id FROM tasks t \
                        WHERE t.id = s.current_task_id \
                          AND t.assigned_to = s.worktree \
                          AND t.status = 'active') AS rid \
               FROM worktree_sessions s \
               ORDER BY s.worktree \
             )",
            [],
            |r| r.get(0),
        )?;
        Ok(rev.unwrap_or_default())
    }

    // ---- Resume sessions ----

    /// Record an observation of the session running in `(project, worktree)`.
    ///
    /// Newest wins: it applies only when `observed_at_ms` is not older than the
    /// stored observation. With `Some(session_id)` the id is stored, unless it is
    /// the row's `cleared_id` (a fresh start cleared it; it is never accepted
    /// again) — a different `cli` than the stored one resets the row instead. With
    /// `None`, only `agent_present` / `observed_at` of an existing row change; no
    /// row is created.
    #[allow(clippy::too_many_arguments)]
    pub fn resume_observe(
        &self,
        project: &str,
        worktree: &str,
        cli: CliKind,
        session_id: Option<&str>,
        source: &str,
        agent_present: bool,
        observed_at_ms: i64,
    ) -> Result<()> {
        match session_id {
            Some(id) => {
                self.conn.execute(
                    "INSERT INTO resume_sessions \
                       (project, worktree, cli, session_id, source, agent_present, cleared_id, observed_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7) \
                     ON CONFLICT(project, worktree) DO UPDATE SET \
                       session_id = excluded.session_id, \
                       source = excluded.source, \
                       agent_present = excluded.agent_present, \
                       observed_at = excluded.observed_at, \
                       cleared_id = CASE WHEN resume_sessions.cli <> excluded.cli \
                                         THEN NULL ELSE resume_sessions.cleared_id END, \
                       cli = excluded.cli \
                     WHERE excluded.observed_at >= resume_sessions.observed_at \
                       AND (resume_sessions.cli <> excluded.cli \
                            OR resume_sessions.cleared_id IS NULL \
                            OR resume_sessions.cleared_id <> excluded.session_id)",
                    params![
                        project,
                        worktree,
                        cli_to_sql(cli),
                        id,
                        source,
                        agent_present,
                        observed_at_ms
                    ],
                )?;
            }
            None => {
                self.conn.execute(
                    "UPDATE resume_sessions SET agent_present = ?3, observed_at = ?4 \
                     WHERE project = ?1 AND worktree = ?2 AND observed_at <= ?4",
                    params![project, worktree, agent_present, observed_at_ms],
                )?;
            }
        }
        Ok(())
    }

    /// Unconditionally record `session_id` as the running session of `(project, worktree)`.
    /// Unlike [`Self::resume_observe`] there is no newest-wins check and no cleared-id guard:
    /// the row is overwritten (or created) with `agent_present = true` and no `cleared_id`.
    pub fn resume_replace(
        &self,
        project: &str,
        worktree: &str,
        cli: CliKind,
        session_id: &str,
        source: &str,
        observed_at_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO resume_sessions \
               (project, worktree, cli, session_id, source, agent_present, cleared_id, observed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1, NULL, ?6) \
             ON CONFLICT(project, worktree) DO UPDATE SET \
               cli = excluded.cli, \
               session_id = excluded.session_id, \
               source = excluded.source, \
               agent_present = 1, \
               cleared_id = NULL, \
               observed_at = excluded.observed_at",
            params![
                project,
                worktree,
                cli_to_sql(cli),
                session_id,
                source,
                observed_at_ms
            ],
        )?;
        Ok(())
    }

    /// The resume row for `(project, worktree)`, if any.
    pub fn resume_get(&self, project: &str, worktree: &str) -> Result<Option<ResumeRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT cli, session_id, source, agent_present, cleared_id, observed_at \
                 FROM resume_sessions WHERE project = ?1 AND worktree = ?2",
                params![project, worktree],
                row_to_resume_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Fresh start: forget the session id and remember it as cleared so a stale
    /// observation of it is never accepted again. A row whose id is already NULL
    /// keeps its previous `cleared_id`.
    pub fn resume_clear(&self, project: &str, worktree: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE resume_sessions \
             SET cleared_id = COALESCE(session_id, cleared_id), session_id = NULL \
             WHERE project = ?1 AND worktree = ?2",
            params![project, worktree],
        )?;
        Ok(())
    }

    /// Every resume row as `(project, worktree, row)`, ordered by key.
    pub fn resume_list(&self) -> Result<Vec<(String, String, ResumeRow)>> {
        let mut stmt = self.conn.prepare(
            "SELECT project, worktree, cli, session_id, source, agent_present, cleared_id, \
                    observed_at \
             FROM resume_sessions ORDER BY project, worktree",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    ResumeRow {
                        cli: cli_from_sql(&r.get::<_, String>(2)?)?,
                        session_id: r.get(3)?,
                        source: r.get(4)?,
                        agent_present: r.get(5)?,
                        cleared_id: r.get(6)?,
                        observed_at: r.get(7)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record whether the user terminated `(project, worktree)`. Clearing a flag on a
    /// key with no row inserts a harmless `0`.
    pub fn set_terminated(&self, project: &str, worktree: &str, terminated: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_flags (project, worktree, terminated) VALUES (?1, ?2, ?3) \
             ON CONFLICT(project, worktree) DO UPDATE SET \
               terminated = excluded.terminated, updated_at = datetime('now')",
            params![project, worktree, terminated],
        )?;
        Ok(())
    }

    /// Whether `(project, worktree)` is flagged terminated; no row means `false`.
    pub fn is_terminated(&self, project: &str, worktree: &str) -> Result<bool> {
        let flag = self
            .conn
            .query_row(
                "SELECT terminated FROM session_flags WHERE project = ?1 AND worktree = ?2",
                params![project, worktree],
                |r| r.get::<_, bool>(0),
            )
            .optional()?;
        Ok(flag.unwrap_or(false))
    }

    /// Worktree names flagged terminated in `project`, ordered by name.
    pub fn terminated_in(&self, project: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT worktree FROM session_flags \
             WHERE project = ?1 AND terminated = 1 ORDER BY worktree",
        )?;
        let names = stmt
            .query_map(params![project], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(names)
    }

    // ---- Dispatch requests ----
    //
    // A short-lived mailbox from the CLI to the running app, not a queue: each row
    // asks the app to send one task to one idle session now, and is resolved
    // `applied` or `refused` within moments (or expires). The board is the queue.

    /// Ask the running app to dispatch `task_id` onto `worktree` now. Returns the new
    /// `pending` row; the app's tasks-DB watcher picks it up.
    pub fn dispatch_request_add(
        &self,
        task_id: i64,
        worktree: &str,
        requested_by: &str,
    ) -> Result<DispatchRequest> {
        self.conn.execute(
            "INSERT INTO dispatch_requests (task_id, worktree, requested_by) VALUES (?1, ?2, ?3)",
            params![task_id, worktree, requested_by],
        )?;
        let id = self.conn.last_insert_rowid();
        self.dispatch_request_get(id)?
            .ok_or_else(|| Error::Conflict(format!("dispatch request {id} vanished")))
    }

    /// One request by id, or `None`.
    pub fn dispatch_request_get(&self, id: i64) -> Result<Option<DispatchRequest>> {
        let row = self
            .conn
            .query_row(
                &format!("SELECT {DISPATCH_REQUEST_COLS} FROM dispatch_requests WHERE id = ?1"),
                params![id],
                row_to_dispatch_request,
            )
            .optional()?;
        Ok(row)
    }

    /// Every `pending` request, oldest first (the order the app executes them in).
    pub fn dispatch_requests_pending(&self) -> Result<Vec<DispatchRequest>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DISPATCH_REQUEST_COLS} FROM dispatch_requests \
             WHERE status = 'pending' ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([], row_to_dispatch_request)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The most recent `limit` requests, newest first.
    pub fn dispatch_requests_list(&self, limit: i64) -> Result<Vec<DispatchRequest>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {DISPATCH_REQUEST_COLS} FROM dispatch_requests ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit], row_to_dispatch_request)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Take ownership of a `pending` request by moving it to `running`. `true` when
    /// this call won; `false` when it was not pending (another app instance, or an
    /// earlier pass, already took it). This compare-and-set is what keeps a request
    /// from executing twice.
    pub fn dispatch_request_begin(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE dispatch_requests SET status = 'running' \
             WHERE id = ?1 AND status = 'pending'",
            params![id],
        )?;
        Ok(n == 1)
    }

    /// Record a request's outcome: `status` must be one of
    /// [`DISPATCH_REQUEST_OUTCOMES`](crate::DISPATCH_REQUEST_OUTCOMES). Stamps
    /// `resolved_at`. Resolving an already-resolved request is a `Conflict`, so an
    /// outcome is written once.
    pub fn dispatch_request_resolve(&self, id: i64, status: &str, reason: &str) -> Result<()> {
        if !model::DISPATCH_REQUEST_OUTCOMES.contains(&status) {
            return Err(Error::Conflict(format!(
                "invalid dispatch request outcome: {status}"
            )));
        }
        let n = self.conn.execute(
            "UPDATE dispatch_requests \
             SET status = ?2, reason = ?3, resolved_at = datetime('now') \
             WHERE id = ?1 AND status IN ('pending', 'running')",
            params![id, status, reason],
        )?;
        if n == 1 {
            return Ok(());
        }
        match self.dispatch_request_get(id)? {
            None => Err(Error::Conflict(format!("no dispatch request {id}"))),
            Some(r) => Err(Error::Conflict(format!(
                "dispatch request {id} is already {}",
                r.status
            ))),
        }
    }

    /// Withdraw a request the app hasn't picked up yet (the requester stopped waiting):
    /// a `pending` row becomes `refused` with `reason`. `true` when it was withdrawn;
    /// `false` when the app had already begun or resolved it, which is left alone.
    pub fn dispatch_request_cancel(&self, id: i64, reason: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE dispatch_requests \
             SET status = 'refused', reason = ?2, resolved_at = datetime('now') \
             WHERE id = ?1 AND status = 'pending'",
            params![id, reason],
        )?;
        Ok(n == 1)
    }

    /// Refuse every `pending` request older than `max_age_secs`, so an app that starts
    /// long after a request was made doesn't act on it. Returns how many expired.
    pub fn dispatch_requests_expire(&self, max_age_secs: i64) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE dispatch_requests \
             SET status = 'refused', resolved_at = datetime('now'), \
                 reason = 'expired: no running Keitora app picked it up in time' \
             WHERE status = 'pending' AND created_at < datetime('now', ?1)",
            params![format!("-{max_age_secs} seconds")],
        )?;
        Ok(n)
    }
}

/// Column list shared by every `dispatch_requests` read, in
/// [`row_to_dispatch_request`] order.
const DISPATCH_REQUEST_COLS: &str =
    "id, task_id, worktree, requested_by, status, reason, created_at, resolved_at";

fn row_to_dispatch_request(r: &rusqlite::Row) -> rusqlite::Result<DispatchRequest> {
    Ok(DispatchRequest {
        id: r.get(0)?,
        task_id: r.get(1)?,
        worktree: r.get(2)?,
        requested_by: r.get(3)?,
        status: r.get(4)?,
        reason: r.get(5)?,
        created_at: r.get(6)?,
        resolved_at: r.get(7)?,
    })
}

fn row_to_resume_row(r: &rusqlite::Row) -> rusqlite::Result<ResumeRow> {
    Ok(ResumeRow {
        cli: cli_from_sql(&r.get::<_, String>(0)?)?,
        session_id: r.get(1)?,
        source: r.get(2)?,
        agent_present: r.get(3)?,
        cleared_id: r.get(4)?,
        observed_at: r.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway on-disk DB. Sessions reconcile against the `tasks` table, so an
    /// on-disk file (not `:memory:`, which is per-connection) is used for parity
    /// with the `Store` tests.
    struct TempDb {
        path: PathBuf,
    }
    impl TempDb {
        fn new() -> TempDb {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "keitora-localstore-test-{}-{}.db",
                std::process::id(),
                n
            ));
            let td = TempDb { path };
            td.cleanup_files();
            td
        }
        fn open(&self) -> LocalStore {
            LocalStore::open(self.path.to_str().unwrap()).unwrap()
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

    #[test]
    fn open_read_only_reads_an_existing_db_without_writing() {
        let db = TempDb::new();
        db.open()
            .session_upsert("feng", "busy", None, Some(7))
            .unwrap();
        let ro = LocalStore::open_read_only(db.path.to_str().unwrap()).unwrap();
        let sessions = ro.sessions_list().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].worktree, "feng");
        assert_eq!(sessions[0].state, "busy");
        assert!(
            ro.session_upsert("feng", "idle", None, None).is_err(),
            "a read-only handle rejects writes"
        );
        let timeout: i64 = ro
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            timeout,
            LocalStore::READ_ONLY_BUSY_TIMEOUT.as_millis() as i64
        );
    }

    #[test]
    fn open_read_only_errors_on_a_missing_db_and_creates_nothing() {
        let db = TempDb::new();
        assert!(LocalStore::open_read_only(db.path.to_str().unwrap()).is_err());
        assert!(!db.path.exists(), "a read-only open never creates the file");
    }

    #[test]
    fn open_read_only_does_not_create_missing_tables() {
        let db = TempDb::new();
        rusqlite::Connection::open(&db.path)
            .unwrap()
            .execute_batch("CREATE TABLE unrelated (x INTEGER);")
            .unwrap();
        let ro = LocalStore::open_read_only(db.path.to_str().unwrap()).unwrap();
        assert!(ro.sessions_list().is_err(), "no schema init on read-only");
    }

    #[test]
    fn session_upsert_and_list() {
        let db = TempDb::new();
        let ls = db.open();
        ls.session_upsert("feng", "busy", None, Some(1234)).unwrap();
        let sessions = ls.sessions_list().unwrap();
        assert!(!sessions.is_empty());
        let s = sessions.iter().find(|s| s.worktree == "feng").unwrap();
        assert_eq!(s.state, "busy");
        assert_eq!(s.pid, Some(1234));
    }

    #[test]
    fn invalid_session_state_is_rejected() {
        let db = TempDb::new();
        let ls = db.open();
        assert!(matches!(
            ls.session_upsert("feng", "bogus", None, None).unwrap_err(),
            Error::Conflict(_)
        ));
    }

    // ---- session reconciliation (sessions read against the live `tasks` table) ----
    //
    // These tests exercise the LEFT-JOIN reconcile that nulls a session's
    // `current_task_id` pointer when the referenced task is no longer active+owned,
    // and the `session_set_offline` clearing behavior. They were ported from the
    // deleted `Store` façade tests in `lib.rs`; the task lifecycle (claim / done /
    // release / reassign) is driven through the async `SqliteBackend` (`TaskBackend`)
    // on the SAME db file as the `LocalStore` — exactly the dual-handle production
    // arrangement.

    use crate::{SqliteBackend, TaskBackend};

    /// Create + claim a task as `actor` via a `SqliteBackend` on the same path,
    /// returning its id. The `LocalStore` then sees the active task for reconcile.
    async fn seed_claimed(path: &str, actor: &str, title: &str) -> i64 {
        let b = SqliteBackend::open(path).unwrap();
        let t = b
            .create(actor, title, "", "normal", &[], None)
            .await
            .unwrap();
        b.claim(t.id, actor).await.unwrap();
        t.id
    }

    #[tokio::test]
    async fn session_set_offline_clears_task_and_inserts_missing_row() {
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        let tid = seed_claimed(&path, "wt-a", "live task").await;

        // wt-a busy with a real active task.
        ls.session_upsert("wt-a", "busy", Some(tid), Some(42))
            .unwrap();

        // Force-offline clears the stale task id + pid and sets state offline.
        ls.session_set_offline("wt-a").unwrap();
        let row = ls
            .sessions_list()
            .unwrap()
            .into_iter()
            .find(|s| s.worktree == "wt-a")
            .unwrap();
        assert_eq!(row.state, "offline");
        assert_eq!(
            row.current_task_id, None,
            "offline must not advertise a task"
        );
        assert_eq!(row.pid, None);

        // Force-offline an arbitrary worktree with no prior row → inserted.
        ls.session_set_offline("wt-b").unwrap();
        let row_b = ls
            .sessions_list()
            .unwrap()
            .into_iter()
            .find(|s| s.worktree == "wt-b")
            .unwrap();
        assert_eq!(row_b.state, "offline");
    }

    #[tokio::test]
    async fn sessions_list_nulls_pointer_after_task_done() {
        // Repro for #13: the LEFT JOIN must null the pointer once the task is done,
        // without a follow-up session_upsert (the hook-gap case).
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        let b = SqliteBackend::open(&path).unwrap();
        let t = b
            .create("wt-a", "active work", "", "normal", &[], None)
            .await
            .unwrap();
        b.claim(t.id, "wt-a").await.unwrap();
        ls.session_upsert("wt-a", "busy", Some(t.id), None).unwrap();

        // Pointer visible while active.
        assert_eq!(ls.sessions_list().unwrap()[0].current_task_id, Some(t.id));

        // Mark done — no session_upsert.
        b.set_done(t.id, "wt-a").await.unwrap();
        assert_eq!(
            ls.sessions_list().unwrap()[0].current_task_id,
            None,
            "done task must not appear as current_task_id"
        );
    }

    #[tokio::test]
    async fn sessions_list_nulls_pointer_when_task_reassigned() {
        // Pointer set for wt-a; task released and re-claimed by wt-b → wt-a's read None.
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        let b = SqliteBackend::open(&path).unwrap();
        let t = b
            .create("wt-a", "contested", "", "normal", &[], None)
            .await
            .unwrap();
        b.claim(t.id, "wt-a").await.unwrap();
        ls.session_upsert("wt-a", "busy", Some(t.id), None).unwrap();

        b.release(t.id, "wt-a").await.unwrap(); // open + assigned_to NULL
        b.claim(t.id, "wt-b").await.unwrap(); // active, owned by wt-b

        let sessions = ls.sessions_list().unwrap();
        let row_a = sessions.iter().find(|s| s.worktree == "wt-a").unwrap();
        assert_eq!(
            row_a.current_task_id, None,
            "pointer must be nulled when the task is owned by another worktree"
        );
    }

    #[tokio::test]
    async fn sessions_list_keeps_pointer_while_active() {
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        let tid = seed_claimed(&path, "wt-a", "in progress").await;
        ls.session_upsert("wt-a", "busy", Some(tid), None).unwrap();
        assert_eq!(
            ls.sessions_list().unwrap()[0].current_task_id,
            Some(tid),
            "active owned task pointer must be preserved"
        );
    }

    #[tokio::test]
    async fn active_task_id_returns_claimed_task() {
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        assert_eq!(ls.active_task_id("wt-a").unwrap(), None);
        let tid = seed_claimed(&path, "wt-a", "work").await;
        assert_eq!(ls.active_task_id("wt-a").unwrap(), Some(tid));
        assert_eq!(
            ls.active_task_id("wt-b").unwrap(),
            None,
            "other worktree has none"
        );
    }

    #[tokio::test]
    async fn active_tasks_lists_one_claimed_task_per_worktree() {
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        assert!(ls.active_tasks().unwrap().is_empty());
        let a = seed_claimed(&path, "wt-a", "first").await;
        let _later = seed_claimed(&path, "wt-a", "second").await;
        let b = seed_claimed(&path, "wt-b", "other").await;
        let backend = SqliteBackend::open(&path).unwrap();
        // An open (unclaimed) task is not active.
        backend
            .create("wt-c", "unclaimed", "", "normal", &[], None)
            .await
            .unwrap();
        // A finished one drops out.
        let done = seed_claimed(&path, "wt-d", "done").await;
        backend.set_done(done, "wt-d").await.unwrap();

        let tasks = ls.active_tasks().unwrap();
        assert_eq!(
            tasks,
            vec![
                ActiveTask {
                    worktree: "wt-a".into(),
                    id: a,
                    title: "first".into()
                },
                ActiveTask {
                    worktree: "wt-b".into(),
                    id: b,
                    title: "other".into()
                },
            ],
            "lowest id wins per worktree; open and done tasks are left out"
        );
    }

    #[tokio::test]
    async fn session_revision_changes_when_task_leaves_active() {
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        let b = SqliteBackend::open(&path).unwrap();
        let t = b
            .create("wt-a", "active", "", "normal", &[], None)
            .await
            .unwrap();
        b.claim(t.id, "wt-a").await.unwrap();
        ls.session_upsert("wt-a", "busy", Some(t.id), None).unwrap();

        let before = ls.session_revision().unwrap();
        b.set_done(t.id, "wt-a").await.unwrap();
        let after = ls.session_revision().unwrap();
        assert_ne!(
            before, after,
            "revision must change when the reconciled task pointer becomes None"
        );
    }

    #[tokio::test]
    async fn session_revision_stable_on_heartbeat_with_active_task() {
        let db = TempDb::new();
        let path = db.path.to_str().unwrap().to_string();
        let ls = db.open();
        let tid = seed_claimed(&path, "wt-a", "work").await;
        ls.session_upsert("wt-a", "busy", Some(tid), Some(99))
            .unwrap();

        let r0 = ls.session_revision().unwrap();
        // Heartbeat-only re-upsert: same state + task id, new timestamp.
        ls.session_upsert("wt-a", "busy", Some(tid), Some(99))
            .unwrap();
        assert_eq!(
            r0,
            ls.session_revision().unwrap(),
            "heartbeat-only re-upsert with an active task must not move the revision"
        );
    }

    #[test]
    fn session_revision_tracks_state_not_heartbeat() {
        let db = TempDb::new();
        let ls = db.open();
        ls.session_upsert("wt-a", "idle", None, None).unwrap();
        let r0 = ls.session_revision().unwrap();

        // Heartbeat-only re-upsert (same state) → revision stable.
        ls.session_upsert("wt-a", "idle", None, None).unwrap();
        assert_eq!(
            r0,
            ls.session_revision().unwrap(),
            "heartbeat-only must not move the revision"
        );

        // Real state change → revision moves.
        ls.session_upsert("wt-a", "busy", None, None).unwrap();
        assert_ne!(
            r0,
            ls.session_revision().unwrap(),
            "state change must move the revision"
        );
    }

    // ---- Resume sessions ----

    fn observe(ls: &LocalStore, cli: CliKind, id: Option<&str>, present: bool, t: i64) {
        ls.resume_observe("p", "w", cli, id, "herdr:claude", present, t)
            .unwrap();
    }

    fn row(ls: &LocalStore) -> Option<ResumeRow> {
        ls.resume_get("p", "w").unwrap()
    }

    #[test]
    fn resume_observe_inserts_and_newest_wins() {
        let db = TempDb::new();
        let ls = db.open();
        assert_eq!(row(&ls), None);
        observe(&ls, CliKind::Claude, Some("A"), true, 10);
        assert_eq!(
            row(&ls),
            Some(ResumeRow {
                cli: CliKind::Claude,
                session_id: Some("A".into()),
                source: "herdr:claude".into(),
                agent_present: true,
                cleared_id: None,
                observed_at: 10,
            })
        );
        observe(&ls, CliKind::Claude, Some("B"), true, 5);
        assert_eq!(
            row(&ls).unwrap().session_id.as_deref(),
            Some("A"),
            "older ignored"
        );
        observe(&ls, CliKind::Claude, Some("C"), true, 20);
        let r = row(&ls).unwrap();
        assert_eq!(r.session_id.as_deref(), Some("C"));
        assert_eq!(r.observed_at, 20);
        // Keys are independent.
        ls.resume_observe(
            "p",
            "other",
            CliKind::Codex,
            Some("X"),
            "herdr:codex",
            true,
            1,
        )
        .unwrap();
        let all = ls.resume_list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!((all[0].0.as_str(), all[0].1.as_str()), ("p", "other"));
        assert_eq!(all[0].2.cli, CliKind::Codex);
        assert_eq!(all[1].2.session_id.as_deref(), Some("C"));
    }

    #[test]
    fn resume_observe_none_session_only_updates_agent_present() {
        let db = TempDb::new();
        let ls = db.open();
        observe(&ls, CliKind::Claude, Some("A"), true, 10);
        observe(&ls, CliKind::Claude, None, false, 20);
        let r = row(&ls).unwrap();
        assert_eq!(r.session_id.as_deref(), Some("A"), "id kept");
        assert!(!r.agent_present);
        assert_eq!(r.observed_at, 20);
        // An older id-less observation does not apply.
        observe(&ls, CliKind::Claude, None, true, 15);
        assert!(!row(&ls).unwrap().agent_present);
    }

    #[test]
    fn resume_observe_none_session_does_not_create_row() {
        let db = TempDb::new();
        let ls = db.open();
        observe(&ls, CliKind::Claude, None, true, 10);
        assert_eq!(row(&ls), None);
        assert!(ls.resume_list().unwrap().is_empty());
    }

    #[test]
    fn resume_replace_overwrites_a_claude_row() {
        let db = TempDb::new();
        let ls = db.open();
        observe(&ls, CliKind::Claude, Some("A"), false, 10);
        ls.resume_replace("p", "w", CliKind::Codex, "thr-1", "keitora:transfer", 20)
            .unwrap();
        assert_eq!(
            row(&ls),
            Some(ResumeRow {
                cli: CliKind::Codex,
                session_id: Some("thr-1".into()),
                source: "keitora:transfer".into(),
                agent_present: true,
                cleared_id: None,
                observed_at: 20,
            })
        );
    }

    #[test]
    fn resume_replace_ignores_newest_wins_and_cleared_id() {
        let db = TempDb::new();
        let ls = db.open();
        observe(&ls, CliKind::Codex, Some("thr-1"), true, 100);
        ls.resume_clear("p", "w").unwrap();
        assert_eq!(row(&ls).unwrap().cleared_id.as_deref(), Some("thr-1"));
        // Older timestamp and the id equals the cleared id: still replaced.
        ls.resume_replace("p", "w", CliKind::Codex, "thr-1", "keitora:transfer", 5)
            .unwrap();
        let r = row(&ls).unwrap();
        assert_eq!(r.session_id.as_deref(), Some("thr-1"));
        assert_eq!(r.cleared_id, None);
        assert_eq!(r.observed_at, 5);
    }

    #[test]
    fn resume_replace_creates_a_missing_row() {
        let db = TempDb::new();
        let ls = db.open();
        assert_eq!(row(&ls), None);
        ls.resume_replace("p", "w", CliKind::Codex, "thr-1", "keitora:transfer", 1)
            .unwrap();
        assert_eq!(row(&ls).unwrap().session_id.as_deref(), Some("thr-1"));
    }

    #[test]
    fn resume_clear_sets_cleared_id_and_blocks_reobserve() {
        let db = TempDb::new();
        let ls = db.open();
        observe(&ls, CliKind::Claude, Some("A"), true, 10);
        ls.resume_clear("p", "w").unwrap();
        let r = row(&ls).unwrap();
        assert_eq!(r.session_id, None);
        assert_eq!(r.cleared_id.as_deref(), Some("A"));

        // A stale observation of the cleared id (even a newer one) is ignored.
        observe(&ls, CliKind::Claude, Some("A"), true, 30);
        let r = row(&ls).unwrap();
        assert_eq!(r.session_id, None);
        assert_eq!(r.observed_at, 10);

        // Clearing again keeps the cleared id when there is nothing new.
        ls.resume_clear("p", "w").unwrap();
        assert_eq!(row(&ls).unwrap().cleared_id.as_deref(), Some("A"));

        // A new conversation is accepted.
        observe(&ls, CliKind::Claude, Some("B"), true, 40);
        let r = row(&ls).unwrap();
        assert_eq!(r.session_id.as_deref(), Some("B"));
        assert_eq!(r.cleared_id.as_deref(), Some("A"));
        // Clearing on a missing row is a no-op.
        ls.resume_clear("p", "missing").unwrap();
        assert_eq!(ls.resume_get("p", "missing").unwrap(), None);
    }

    #[test]
    fn resume_observe_cli_change_resets_row() {
        let db = TempDb::new();
        let ls = db.open();
        observe(&ls, CliKind::Claude, Some("A"), true, 10);
        ls.resume_clear("p", "w").unwrap();
        // The worktree switched to codex and its id happens to equal the cleared one.
        ls.resume_observe(
            "p",
            "w",
            CliKind::Codex,
            Some("A"),
            "herdr:codex",
            false,
            20,
        )
        .unwrap();
        assert_eq!(
            row(&ls),
            Some(ResumeRow {
                cli: CliKind::Codex,
                session_id: Some("A".into()),
                source: "herdr:codex".into(),
                agent_present: false,
                cleared_id: None,
                observed_at: 20,
            })
        );
    }

    #[test]
    fn terminated_flag_set_clear_and_query() {
        let db = TempDb::new();
        let ls = db.open();
        // No resume row exists for this key; the flag is independent of it.
        assert_eq!(ls.resume_get("p", "w").unwrap(), None);
        assert!(!ls.is_terminated("p", "w").unwrap());
        ls.set_terminated("p", "w", true).unwrap();
        assert!(ls.is_terminated("p", "w").unwrap());
        assert_eq!(ls.resume_get("p", "w").unwrap(), None);
        ls.set_terminated("p", "w", false).unwrap();
        assert!(!ls.is_terminated("p", "w").unwrap());
        assert!(ls.terminated_in("p").unwrap().is_empty());
    }

    #[test]
    fn terminated_flag_clear_on_unknown_key_is_harmless() {
        let db = TempDb::new();
        let ls = db.open();
        ls.set_terminated("p", "never-seen", false).unwrap();
        assert!(!ls.is_terminated("p", "never-seen").unwrap());
        assert!(ls.terminated_in("p").unwrap().is_empty());
    }

    #[test]
    fn terminated_in_is_scoped_and_ordered_by_name() {
        let db = TempDb::new();
        let ls = db.open();
        ls.set_terminated("p", "beta", true).unwrap();
        ls.set_terminated("p", "alpha", true).unwrap();
        ls.set_terminated("p", "gamma", false).unwrap();
        ls.set_terminated("other", "delta", true).unwrap();
        assert_eq!(ls.terminated_in("p").unwrap(), vec!["alpha", "beta"]);
        assert_eq!(ls.terminated_in("other").unwrap(), vec!["delta"]);
        assert!(ls.terminated_in("none").unwrap().is_empty());
    }

    #[test]
    fn terminated_flag_repeated_set_is_idempotent() {
        let db = TempDb::new();
        let ls = db.open();
        ls.set_terminated("p", "w", true).unwrap();
        ls.set_terminated("p", "w", true).unwrap();
        assert!(ls.is_terminated("p", "w").unwrap());
        assert_eq!(ls.terminated_in("p").unwrap(), vec!["w"]);
        ls.set_terminated("p", "w", false).unwrap();
        ls.set_terminated("p", "w", false).unwrap();
        assert!(!ls.is_terminated("p", "w").unwrap());
    }

    #[test]
    fn dispatch_request_add_starts_pending() {
        let db = TempDb::new();
        let ls = db.open();
        let r = ls.dispatch_request_add(7, "alpha", "main").unwrap();
        assert_eq!(r.task_id, 7);
        assert_eq!(r.worktree, "alpha");
        assert_eq!(r.requested_by, "main");
        assert_eq!(r.status, "pending");
        assert_eq!(r.reason, "");
        assert_eq!(r.resolved_at, None);
        assert!(!r.is_resolved());
        assert_eq!(ls.dispatch_request_get(r.id).unwrap(), Some(r));
        assert_eq!(ls.dispatch_request_get(999).unwrap(), None);
    }

    #[test]
    fn dispatch_requests_pending_is_oldest_first_and_excludes_taken() {
        let db = TempDb::new();
        let ls = db.open();
        let a = ls.dispatch_request_add(1, "alpha", "me").unwrap();
        let b = ls.dispatch_request_add(2, "bravo", "me").unwrap();
        let c = ls.dispatch_request_add(3, "alpha", "me").unwrap();
        assert!(ls.dispatch_request_begin(b.id).unwrap());
        let pending: Vec<i64> = ls
            .dispatch_requests_pending()
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(pending, vec![a.id, c.id]);
        // Newest first, every status.
        let all: Vec<i64> = ls
            .dispatch_requests_list(10)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(all, vec![c.id, b.id, a.id]);
        assert_eq!(ls.dispatch_requests_list(1).unwrap().len(), 1);
    }

    #[test]
    fn dispatch_request_begin_wins_once() {
        let db = TempDb::new();
        let ls = db.open();
        // A second handle on the same file stands in for a second app instance.
        let other = db.open();
        let r = ls.dispatch_request_add(1, "alpha", "me").unwrap();
        assert!(ls.dispatch_request_begin(r.id).unwrap());
        assert!(!other.dispatch_request_begin(r.id).unwrap());
        assert_eq!(
            ls.dispatch_request_get(r.id).unwrap().unwrap().status,
            "running"
        );
        assert!(!ls.dispatch_request_begin(999).unwrap());
    }

    #[test]
    fn dispatch_request_resolve_records_outcome_once() {
        let db = TempDb::new();
        let ls = db.open();
        let r = ls.dispatch_request_add(1, "alpha", "me").unwrap();
        assert!(ls.dispatch_request_begin(r.id).unwrap());
        ls.dispatch_request_resolve(r.id, "refused", "@alpha is busy")
            .unwrap();
        let got = ls.dispatch_request_get(r.id).unwrap().unwrap();
        assert_eq!(got.status, "refused");
        assert_eq!(got.reason, "@alpha is busy");
        assert!(got.resolved_at.is_some());
        assert!(got.is_resolved());
        // Written once: a second outcome is a conflict naming the current status.
        let err = ls
            .dispatch_request_resolve(r.id, "applied", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already refused"), "{err}");
        assert_eq!(
            ls.dispatch_request_get(r.id).unwrap().unwrap().status,
            "refused"
        );
    }

    #[test]
    fn dispatch_request_resolve_rejects_non_outcomes_and_unknown_ids() {
        let db = TempDb::new();
        let ls = db.open();
        let r = ls.dispatch_request_add(1, "alpha", "me").unwrap();
        for bad in ["pending", "running", "queued", "dispatched", ""] {
            assert!(ls.dispatch_request_resolve(r.id, bad, "").is_err(), "{bad}");
        }
        assert_eq!(
            ls.dispatch_request_get(r.id).unwrap().unwrap().status,
            "pending"
        );
        // A pending request can be resolved without being begun (e.g. refused early).
        ls.dispatch_request_resolve(r.id, "refused", "task #1 not found")
            .unwrap();
        let err = ls
            .dispatch_request_resolve(999, "refused", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no dispatch request 999"), "{err}");
    }

    #[test]
    fn dispatch_request_cancel_only_withdraws_pending() {
        let db = TempDb::new();
        let ls = db.open();
        let pending = ls.dispatch_request_add(1, "alpha", "me").unwrap();
        let running = ls.dispatch_request_add(2, "alpha", "me").unwrap();
        assert!(ls.dispatch_request_begin(running.id).unwrap());
        assert!(ls
            .dispatch_request_cancel(pending.id, "requester gave up")
            .unwrap());
        let got = ls.dispatch_request_get(pending.id).unwrap().unwrap();
        assert_eq!(got.status, "refused");
        assert_eq!(got.reason, "requester gave up");
        assert!(got.resolved_at.is_some());
        // Already taken by the app, already resolved, or unknown: left alone.
        assert!(!ls.dispatch_request_cancel(running.id, "x").unwrap());
        assert!(!ls.dispatch_request_cancel(pending.id, "x").unwrap());
        assert!(!ls.dispatch_request_cancel(999, "x").unwrap());
        assert_eq!(
            ls.dispatch_request_get(running.id).unwrap().unwrap().status,
            "running"
        );
    }

    #[test]
    fn dispatch_requests_expire_refuses_only_old_pending_rows() {
        let db = TempDb::new();
        let ls = db.open();
        let old = ls.dispatch_request_add(1, "alpha", "me").unwrap();
        let old_running = ls.dispatch_request_add(2, "alpha", "me").unwrap();
        let fresh = ls.dispatch_request_add(3, "alpha", "me").unwrap();
        assert!(ls.dispatch_request_begin(old_running.id).unwrap());
        ls.conn
            .execute(
                "UPDATE dispatch_requests SET created_at = datetime('now', '-1 hour') \
                 WHERE id IN (?1, ?2)",
                params![old.id, old_running.id],
            )
            .unwrap();
        assert_eq!(ls.dispatch_requests_expire(600).unwrap(), 1);
        let got = ls.dispatch_request_get(old.id).unwrap().unwrap();
        assert_eq!(got.status, "refused");
        assert!(got.reason.starts_with("expired"), "{}", got.reason);
        // A running request belongs to whoever began it; a fresh one stays pending.
        assert_eq!(
            ls.dispatch_request_get(old_running.id)
                .unwrap()
                .unwrap()
                .status,
            "running"
        );
        assert_eq!(
            ls.dispatch_request_get(fresh.id).unwrap().unwrap().status,
            "pending"
        );
        assert_eq!(ls.dispatch_requests_expire(600).unwrap(), 0);
    }

    #[test]
    fn init_schema_adds_dispatch_requests_to_existing_db() {
        let db = TempDb::new();
        {
            // A DB from before the dispatch_requests table existed.
            let conn = rusqlite::Connection::open(&db.path).unwrap();
            conn.execute_batch(
                "CREATE TABLE worktree_sessions (
                   worktree TEXT PRIMARY KEY,
                   state TEXT NOT NULL DEFAULT 'idle',
                   current_task_id INTEGER,
                   pid INTEGER,
                   last_heartbeat TEXT NOT NULL DEFAULT (datetime('now'))
                 );",
            )
            .unwrap();
        }
        let ls = db.open();
        assert!(ls.dispatch_requests_pending().unwrap().is_empty());
        ls.dispatch_request_add(1, "alpha", "me").unwrap();
        assert_eq!(ls.dispatch_requests_pending().unwrap().len(), 1);
    }

    #[test]
    fn init_schema_adds_resume_table_to_existing_db() {
        let db = TempDb::new();
        {
            // A DB from before the resume table existed.
            let conn = rusqlite::Connection::open(&db.path).unwrap();
            conn.execute_batch(
                "CREATE TABLE worktree_sessions (
                   worktree TEXT PRIMARY KEY,
                   state TEXT NOT NULL DEFAULT 'idle',
                   current_task_id INTEGER,
                   pid INTEGER,
                   last_heartbeat TEXT NOT NULL DEFAULT (datetime('now'))
                 );
                 INSERT INTO worktree_sessions (worktree, state) VALUES ('old', 'idle');",
            )
            .unwrap();
        }
        let ls = db.open();
        let has_table: i64 = ls
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'resume_sessions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_table, 1);
        assert!(ls.sessions_list().is_ok(), "existing data still readable");
        observe(&ls, CliKind::Claude, Some("A"), true, 1);
        assert_eq!(row(&ls).unwrap().session_id.as_deref(), Some("A"));
    }
}
