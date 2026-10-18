use async_trait::async_trait;
use std::time::Duration;

use crate::dispatch::already_dispatched;
use crate::model::{
    DepNode, DispatchLogEntry, ListFilter, LogEntry, Task, TaskUpdate, TaskWithDeps,
};

// ---- Support types -------------------------------------------------------

/// Returned by `set_done`. Replaces the bare `(Task, Vec<i64>)` tuple.
#[derive(Debug)]
pub struct DoneOutcome {
    /// The task after its status was set to `'done'`.
    pub task: Task,
    /// IDs of tasks that became unblocked (all their deps are now done).
    /// Observational / best-effort — see §9 for planned split.
    pub unblocked: Vec<i64>,
}

/// One entry returned by `dispatched_reservations`. Replaces `(i64, String)`.
/// `Serialize` so the Tauri command can return it directly (a JSON object
/// `{task_id, worktree}` — NOT the old positional `[id, "wt"]` array).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Reservation {
    /// The task that has been dispatched and is still in `open` + assigned state.
    pub task_id: i64,
    /// The worktree the task was dispatched to.
    pub worktree: String,
}

/// Outcome of a [`TaskBackend::claim`] call.
///
/// Replaces the bare `Task` return so eventually-consistent backends can signal that a
/// claim write was submitted but exclusivity is *not* yet guaranteed.
///
/// * `Claimed` — `Strong` backends: a fresh CAS-confirmed claim took effect.
/// * `AlreadyYours` — `Strong` backends only: an idempotent re-claim by the same actor
///   (the task was already assigned to `actor`).
/// * `ClaimedUnverified` — `Eventual` backends: the claim write was submitted but
///   exclusivity is not guaranteed. May be returned on **both** the first-claim path
///   AND the re-claim path (an `Eventual` backend cannot cheaply distinguish them).
#[derive(Debug)]
pub enum ClaimOutcome {
    /// `Strong`: a fresh CAS-confirmed claim.
    Claimed(Task),
    /// `Strong`: idempotent re-claim by the same actor.
    AlreadyYours(Task),
    /// `Eventual`: write submitted, exclusivity NOT guaranteed.
    ClaimedUnverified(Task),
}

impl ClaimOutcome {
    /// The claimed task, regardless of which outcome variant was produced.
    pub fn task(self) -> Task {
        match self {
            ClaimOutcome::Claimed(t)
            | ClaimOutcome::AlreadyYours(t)
            | ClaimOutcome::ClaimedUnverified(t) => t,
        }
    }
}

/// Returned by [`TaskBackend::dispatch_reserve`].
///
/// Mirrors the `ClaimOutcome` / `DoneOutcome` precedent: a coarse intent method returns an
/// outcome enum rather than a bare value so the caller needs no follow-up read.
///
/// The `Reserved` variant carries a full `Task` while `AlreadyDispatched` is a unit, so the
/// enum is intentionally lopsided (`clippy::large_enum_variant`). Boxing the `Task` would
/// change the documented `Reserved(Task)` API for an outcome that is always short-lived and
/// moved once; the size asymmetry is harmless here.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ReserveOutcome {
    /// Idempotency guard fired: the task is already dispatched-and-reserved to this
    /// worktree (status == "open", assigned_to == worktree, latest dispatch decision
    /// into this worktree == "dispatched"). NO reserve write was performed.
    /// The caller must treat this as a no-op (do NOT inject). The pre-read Task is
    /// deliberately not carried — `dispatch_apply`'s early-return path reads zero
    /// fields from it.
    AlreadyDispatched,
    /// A fresh reserve write committed (`assigned_to = worktree`, `status` stays "open").
    /// Injection should proceed. Carries the full post-assign Task — `id`/`title`/
    /// `description` are exactly what `render_steps` consumes.
    Reserved(Task),
}

/// A single board read: the task list plus the active dispatch reservations the
/// board renders as `sent ✓` chips. The two are always fetched together by the UI.
///
/// `Serialize` is required: a Tauri command that returns `BoardSnapshot`
/// across the IPC boundary. Both `TaskWithDeps` and `Reservation` already derive
/// `Serialize`, so adding it here costs nothing and avoids a compilation error there.
#[derive(Debug, serde::Serialize)]
pub struct BoardSnapshot {
    pub tasks: Vec<TaskWithDeps>,
    pub reservations: Vec<Reservation>,
}

/// Whether the backend can offer linearizable conditional mutations.
///
/// `Strong` — every conditional mutation (`claim`, `assign`, `set_done`, `add_dep`) is an
///   atomic CAS; a `Conflict` error unambiguously means the precondition was false at the
///   time of the write. `SqliteBackend` returns this.
///
/// `Eventual` — the backend is eventually consistent (e.g. GitHub Projects); `claim` is
///   best-effort / advisory. The strong-consistency tier of the contract test-suite is
///   skipped for `Eventual` backends.
pub enum Consistency {
    Strong,
    Eventual,
}

// ---- Error ---------------------------------------------------------------

/// Backend-agnostic, retry-aware error for the `TaskBackend` trait surface.
///
/// `LocalStore` keeps the existing concrete `crate::Error`; only the trait uses this type.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// No task with the given id. Permanent.
    #[error("task #{0} not found")]
    NotFound(i64),

    /// A CAS precondition failed (already claimed, blocked, terminal, etc.). Never
    /// retryable — re-executing would lose the same race.
    #[error("{0}")]
    Conflict(String),

    /// Bad/missing configuration (wrong db_path, unknown backend kind). Permanent.
    #[error("{0}")]
    Config(String),

    /// Credentials missing or rejected (wrong scope, expired/invalid token). Permanent
    /// until credentials are fixed. `remediation`, when present, is a short, user-facing
    /// hint for how to fix it (e.g. "run `gh auth refresh -s project`").
    #[error("{message}")]
    Unauthorized {
        message: String,
        remediation: Option<String>,
    },

    /// A runtime prerequisite is missing — distinct from `Config` (bad toml) and
    /// `Unauthorized` (creds present but rejected). E.g. the `gh` CLI is not installed
    /// or the user has not authenticated at all. Permanent until the prerequisite is met.
    #[error("{0}")]
    Unavailable(String),

    /// Server-imposed rate limit. Retryable; honour `retry_after` if present.
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },

    /// Connection or query timed out. Retryable.
    #[error("timeout")]
    Timeout,

    /// Transient server error (5xx, replica lag, conn reset). Retryable.
    #[error("{0}")]
    Transient(String),

    /// Non-retryable server or protocol error (4xx, malformed response,
    /// non-busy rusqlite error).
    #[error("{0}")]
    Permanent(String),

    /// JSON (de)serialization failure. Permanent.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

impl BackendError {
    /// `true` for `RateLimited`, `Timeout`, `Transient` — the caller may retry.
    /// `Conflict` is **never** retryable (re-loses the same CAS race). `Unauthorized`
    /// and `Unavailable` are **never** retryable — they require human intervention
    /// (fix creds / install the prerequisite), not a retry.
    pub fn is_retryable(&self) -> bool {
        match self {
            BackendError::RateLimited { .. }
            | BackendError::Timeout
            | BackendError::Transient(_) => true,
            BackendError::NotFound(_)
            | BackendError::Conflict(_)
            | BackendError::Config(_)
            | BackendError::Unauthorized { .. }
            | BackendError::Unavailable(_)
            | BackendError::Permanent(_)
            | BackendError::Serde(_) => false,
        }
    }

    /// How long to wait before retrying, if the backend specified one.
    /// Only meaningful when `is_retryable()` is true.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            BackendError::RateLimited { retry_after } => *retry_after,
            BackendError::NotFound(_)
            | BackendError::Conflict(_)
            | BackendError::Config(_)
            | BackendError::Unauthorized { .. }
            | BackendError::Unavailable(_)
            | BackendError::Timeout
            | BackendError::Transient(_)
            | BackendError::Permanent(_)
            | BackendError::Serde(_) => None,
        }
    }
}

/// Convenience alias used by `TaskBackend` methods.
pub type BackendResult<T> = Result<T, BackendError>;

/// The `Conflict` a `delete` returns when other tasks still depend on `id`.
/// Shared by both backends so the wording can't drift between them.
pub(crate) fn dependents_conflict(id: i64, dependents: &[i64]) -> BackendError {
    BackendError::Conflict(format!("task {id} is depended on by {dependents:?}"))
}

// ---- Trait ---------------------------------------------------------------

#[async_trait]
pub trait TaskBackend: Send + Sync {
    // ======================================================================
    // Meta
    // ======================================================================

    /// Whether this backend offers linearizable conditional mutations (`Strong`)
    /// or eventual consistency (`Eventual`). Drives which contract-suite tiers run.
    /// **Synchronous** — no I/O; returns a static compile-time value per impl.
    fn consistency(&self) -> Consistency;

    /// An opaque, compare-only change cursor. Callers test equality only; the format
    /// is backend-private and **must not be parsed or split on `:`.
    ///
    /// `SqliteBackend` returns `"{max(task_log.id)}:{count(tasks)}:{max(dispatch_log.id)}"`.
    /// A future GitHub backend may return an ETag or `updatedAt` timestamp.
    /// Advances on every mutation (including `dispatch_log_add`); stable between mutations.
    async fn revision(&self) -> BackendResult<String>;

    // ======================================================================
    // Reads (required)
    // ======================================================================

    /// Return the task plus its dependency edges, or `None` if it does not exist.
    /// `blocked_by` carries only IDs of unfinished (non-done) deps.
    async fn get(&self, id: i64) -> BackendResult<Option<TaskWithDeps>>;

    /// Return all tasks matching `filter`, ordered by priority then id.
    /// Mirrors `Store::list` (`lib.rs:218`); `filter.archive_days` defaults to 30.
    async fn list(&self, filter: &ListFilter) -> BackendResult<Vec<TaskWithDeps>>;

    /// Full-text search across title and description, excluding tasks past the archive
    /// window. Results are ordered newest-first (by id DESC). Mirrors `Store::find`
    /// (`lib.rs:265`).
    async fn find(&self, term: &str, archive_days: i64) -> BackendResult<Vec<Task>>;

    /// The most recent `limit` activity-log entries for `id`, newest first.
    /// Mirrors `Store::logs` (`lib.rs:326`).
    async fn logs(&self, id: i64, limit: i64) -> BackendResult<Vec<LogEntry>>;

    /// Transitive dependency tree rooted at `id`, up to 10 levels deep, ordered by
    /// (depth ASC, id ASC). Mirrors `Store::deps_tree` (`lib.rs:300`).
    async fn deps_tree(&self, id: i64) -> BackendResult<Vec<DepNode>>;

    // ======================================================================
    // Reads (provided defaults — overridable)
    // ======================================================================

    /// Return the task or `BackendError::NotFound(id)`. Derived from `get`.
    /// `SqliteBackend` may override with a direct indexed query; the default is
    /// correct for all backends.
    async fn require(&self, id: i64) -> BackendResult<Task> {
        self.get(id)
            .await?
            .map(|twd| twd.task)
            .ok_or(BackendError::NotFound(id))
    }

    /// IDs of unfinished (non-done) tasks blocking `id`. Derived from `get`'s
    /// `TaskWithDeps.blocked_by` field. `SqliteBackend` overrides with a targeted
    /// indexed query (`lib.rs:279`) for speed; other backends may inherit the default.
    async fn blocked_by(&self, id: i64) -> BackendResult<Vec<i64>> {
        Ok(self
            .get(id)
            .await?
            .map(|twd| twd.blocked_by)
            .unwrap_or_default())
    }

    /// IDs of tasks that depend on `id` (i.e. tasks for which `id` is a blocker).
    /// Derived from `get`'s `TaskWithDeps.blocks` field.
    async fn blocks(&self, id: i64) -> BackendResult<Vec<i64>> {
        Ok(self
            .get(id)
            .await?
            .map(|twd| twd.blocks)
            .unwrap_or_default())
    }

    /// Read everything the board paints in one logical call. Provided default sequences
    /// the two existing primitives — correct for every backend. Backends that can serve
    /// both from one underlying fetch (e.g. GitHub from a single `issue_list_all`)
    /// override to eliminate the redundant round-trip(s).
    async fn board_snapshot(&self, filter: &ListFilter) -> BackendResult<BoardSnapshot> {
        Ok(BoardSnapshot {
            tasks: self.list(filter).await?,
            reservations: self.dispatched_reservations().await?,
        })
    }

    // ======================================================================
    // Mutations — task lifecycle
    // ======================================================================

    /// Create a new task. `actor` is recorded as `created_by` and in the activity log.
    ///
    /// `idempotency_key`, when `Some`, makes the create retry-safe: a backend that can
    /// dedupe (e.g. GitHub via find-before-create) will not duplicate the task if the
    /// same key is replayed after a lost ack. `SqliteBackend` ignores the key (its
    /// auto-increment id makes a server-side dedupe key meaningless); a retried create
    /// without a key still duplicates the task.
    ///
    /// `priority` must be one of `"low"`, `"normal"`, `"high"`, `"urgent"`.
    async fn create(
        &self,
        actor: &str,
        title: &str,
        description: &str,
        priority: &str,
        tags: &[String],
        idempotency_key: Option<&str>,
    ) -> BackendResult<Task>;

    /// Atomically claim `id` for `actor`. Sets `assigned_to = actor` and `status =
    /// 'active'` in a single conditional write.
    ///
    /// Idempotent: already claimed by the same `actor` → re-affirm success (no error).
    ///
    /// Returns a [`ClaimOutcome`]:
    ///   - `Claimed` — `Strong`: a fresh CAS-confirmed claim.
    ///   - `AlreadyYours` — `Strong` only: an idempotent re-claim by the same actor.
    ///   - `ClaimedUnverified` — `Eventual`: the write was submitted but exclusivity is
    ///     not guaranteed. May be returned on **both** the first-claim AND the re-claim
    ///     path (an `Eventual` backend cannot cheaply distinguish them). An `Eventual`
    ///     `Conflict` is confirmed-on-reread (not stale), so `is_retryable() == false`
    ///     stays honest.
    ///
    /// Returns `Conflict` if:
    ///   - the task is already assigned to a *different* actor, **or**
    ///   - the task has unfinished deps (`blocked_by` is non-empty), **or**
    ///   - the task is in a terminal state (`'done'` or `'cancelled'`).
    /// Returns `NotFound` if no task with `id` exists.
    async fn claim(&self, id: i64, actor: &str) -> BackendResult<ClaimOutcome>;

    /// Release `actor`'s claim on `id`. Sets `assigned_to = NULL` and `status =
    /// 'open'`. Logs the `unclaimed` action attributed to `actor`.
    ///
    /// Idempotent: already unassigned → success.
    ///
    /// Returns `Conflict` if `id` is currently assigned to a *different* actor.
    /// Returns `NotFound` if no task with `id` exists.
    async fn unclaim(&self, id: i64, actor: &str) -> BackendResult<()>;

    /// Mark `id` done. `actor` must be the current assignee.
    ///
    /// Idempotent: already `'done'` → **success, not `Conflict`** (safe to retry).
    ///
    /// Returns `DoneOutcome { task, unblocked }` where `unblocked` is the (best-effort,
    /// observational) list of task IDs that became unblocked by this completion.
    ///
    /// Returns `Conflict` if `id` is not assigned to `actor`.
    /// Returns `NotFound` if no task with `id` exists.
    async fn set_done(&self, id: i64, actor: &str) -> BackendResult<DoneOutcome>;

    /// IDs of tasks that became newly unblocked because `id` is now done: tasks that
    /// depend on `id`, are still `'open'`, and whose every *other* dependency is also
    /// done. Idempotent and side-effect-free — callers may invoke it after a
    /// done-write to (re)compute `DoneOutcome.unblocked`.
    ///
    /// The default impl derives the answer from `blocks` + per-task dep status reads,
    /// which is correct for every backend. `SqliteBackend` overrides it with a single
    /// indexed query (the same one `set_done` uses).
    async fn newly_unblocked_by(&self, id: i64) -> BackendResult<Vec<i64>> {
        let mut unblocked = Vec::new();
        for dependent in self.blocks(id).await? {
            let twd = match self.get(dependent).await? {
                Some(twd) => twd,
                None => continue,
            };
            // Only tasks still `'open'` can become unblocked by this completion.
            if twd.task.status != "open" {
                continue;
            }
            // `blocked_by` carries only unfinished deps; once `id` is done it must not
            // appear there, so an empty `blocked_by` means every dep is satisfied.
            if twd.blocked_by.is_empty() {
                unblocked.push(dependent);
            }
        }
        Ok(unblocked)
    }

    /// Set `status = 'cancelled'`. Logs the action attributed to `actor`. Idempotent.
    ///
    /// Returns `NotFound` if no task with `id` exists.
    async fn cancel(&self, id: i64, actor: &str) -> BackendResult<()>;

    /// Permanently remove `id` from the board. On a strong backend this is a hard row
    /// delete (CASCADE removes its dep edges + log rows); on an eventual backend it may
    /// be a tombstone (close + hide) — either way `id` is observably gone: a subsequent
    /// `get(id)` returns `Ok(None)` and mutations on it return `NotFound`.
    ///
    /// Returns `Conflict` if other tasks depend on `id` (i.e. `blocks(id)` is non-empty),
    /// naming the dependent ids so the caller can resolve them first.
    /// Returns `NotFound` if no task with `id` exists (so a repeated delete → `NotFound`).
    ///
    /// Takes no `actor`: delete keeps no audit record (D6) — the row and its log
    /// rows are gone — so there is nothing to attribute.
    async fn delete(&self, id: i64) -> BackendResult<()>;

    /// Set `status = 'backlog'` and clear `assigned_to`. Logs the action attributed
    /// to `actor`. Idempotent.
    ///
    /// Returns `NotFound` if no task with `id` exists.
    async fn backlog(&self, id: i64, actor: &str) -> BackendResult<()>;

    /// Apply a partial update patch. At least one field in `upd` must be `Some`;
    /// an all-`None` patch returns `Conflict("nothing to update")`.
    ///
    /// Logs the updated field names (attributed to `actor`) as the `detail` of the
    /// `'updated'` log entry.
    ///
    /// Returns `NotFound` if no task with `id` exists.
    async fn update(&self, id: i64, actor: &str, upd: &TaskUpdate) -> BackendResult<()>;

    // ======================================================================
    // Mutations — dependency graph
    // ======================================================================

    /// Add a dependency edge: `task_id` depends on `depends_on`.
    ///
    /// Idempotent: duplicate edge → `Conflict("already exists")` is treated as success
    /// by callers (see §9 idempotency table).
    ///
    /// Returns `Conflict` if:
    ///   - `task_id == depends_on` (self-dep), **or**
    ///   - adding the edge would create a cycle.
    ///
    /// Logs the addition on `task_id`, attributed to `actor`.
    async fn add_dep(&self, task_id: i64, depends_on: i64, actor: &str) -> BackendResult<()>;

    /// Remove the dependency edge `task_id → depends_on`. No-ops if the edge did not
    /// exist. Logs the removal on `task_id`, attributed to `actor`.
    async fn remove_dep(&self, task_id: i64, depends_on: i64, actor: &str) -> BackendResult<()>;

    // ======================================================================
    // Dispatcher primitives — task-state mutations
    // ======================================================================

    /// Reserve `id` for `worktree` without activating it (`status` stays `'open'`).
    /// The target worktree transitions `open → active` itself via `claim` when it
    /// actually starts work.
    ///
    /// `actor` is the dispatcher identity (e.g. `"keitora"`) — recorded in the activity
    /// log as "assigned to @{worktree}".
    ///
    /// Returns `Conflict` if:
    ///   - the task is already assigned to a *different* worktree, **or**
    ///   - the task has unfinished deps, **or**
    ///   - `status` is not `'open'` (active/backlog/done/cancelled all rejected).
    /// Returns `NotFound` if no task with `id` exists.
    async fn assign(&self, id: i64, actor: &str, worktree: &str) -> BackendResult<Task>;

    /// Reserve `id` for `worktree`, applying the dispatcher idempotency guard in one
    /// intent-level call. Returns [`ReserveOutcome`].
    ///
    /// Provided default = today's app-layer sequence verbatim, preserving every
    /// guarantee `dispatch_apply` relies on:
    ///   1. `require(id)` — read status + current assignee.
    ///   2. `dispatch_log_latest_for(id, worktree)` — read the latest decision.
    ///   3. guard: if already dispatched-and-reserved here → `AlreadyDispatched` (no write).
    ///   4. else `assign(id, dispatcher, worktree)` → `Reserved(task)`.
    ///
    /// **never-`Err`-after-reserve:** an `Err` is possible only BEFORE step 4's write.
    /// Once `assign` succeeds the method returns `Ok(Reserved(task))`.
    ///
    /// Backends that can batch (e.g. GitHub) override this to fold the precheck-read +
    /// reserve into a single request. Strong backends keep CAS semantics; `Conflict`
    /// (blocked / terminal / assigned elsewhere) bubbles up from the reserve as today.
    async fn dispatch_reserve(
        &self,
        id: i64,
        dispatcher: &str,
        worktree: &str,
    ) -> BackendResult<ReserveOutcome> {
        let existing = self.require(id).await?;
        let latest = self.dispatch_log_latest_for(id, worktree).await?;
        if already_dispatched(
            &existing.status,
            existing.assigned_to.as_deref(),
            worktree,
            latest.as_ref().map(|e| e.decision.as_str()),
        ) {
            return Ok(ReserveOutcome::AlreadyDispatched);
        }
        let task = self.assign(id, dispatcher, worktree).await?;
        Ok(ReserveOutcome::Reserved(task))
    }

    /// Admin force-release: return any task to `open` + `assigned_to = NULL` regardless
    /// of current owner. Distinct from `unclaim` (which is owner-only).
    ///
    /// Returns `Conflict` if the task is terminal (`'done'`/`'cancelled'`) or in
    /// `'backlog'` (releasing must not silently un-backlog a deliberately shelved task).
    /// Returns `NotFound` if no task with `id` exists.
    ///
    /// Logs the `released` action attributed to `actor`.
    async fn release(&self, id: i64, actor: &str) -> BackendResult<()>;

    // ======================================================================
    // Dispatcher primitives — dispatch log
    // ======================================================================

    /// Append a dispatcher decision to the audit trail.
    /// `decision` is one of `'dispatched'`, `'queued'`, `'released'`. (`'suggested'`
    /// is a legacy value written by a since-removed feature; old rows may carry it.)
    async fn dispatch_log_add(
        &self,
        task_id: i64,
        worktree: &str,
        decision: &str,
        rationale: &str,
    ) -> BackendResult<()>;

    /// The most recent `limit` dispatch-log entries across all tasks, newest first.
    async fn dispatch_log_list(&self, limit: i64) -> BackendResult<Vec<DispatchLogEntry>>;

    /// All dispatch-log entries for `task_id`, newest first.
    async fn dispatch_log_for(&self, task_id: i64) -> BackendResult<Vec<DispatchLogEntry>>;

    /// The most recent dispatch-log entry for the (`task_id`, `worktree`) pair, or
    /// `None` if this pair was never logged.
    async fn dispatch_log_latest_for(
        &self,
        task_id: i64,
        worktree: &str,
    ) -> BackendResult<Option<DispatchLogEntry>>;

    /// Every (task_id, worktree) that is currently *dispatched and still reserved*:
    /// the task is `'open'`, assigned to a worktree, and the latest dispatch decision
    /// into **that** worktree is `'dispatched'`. Drives the board's `sent ✓` chip.
    async fn dispatched_reservations(&self) -> BackendResult<Vec<Reservation>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time object-safety check.
    // If TaskBackend is not object-safe, this function will not compile.
    fn _assert_object_safe(_: &dyn TaskBackend) {}

    #[test]
    fn backend_error_retryability() {
        assert!(BackendError::RateLimited { retry_after: None }.is_retryable());
        assert!(BackendError::Timeout.is_retryable());
        assert!(BackendError::Transient("boom".into()).is_retryable());
        assert!(!BackendError::NotFound(1).is_retryable());
        assert!(!BackendError::Conflict("race".into()).is_retryable());
        assert!(!BackendError::Permanent("bad".into()).is_retryable());
        assert!(!BackendError::Unauthorized {
            message: "nope".into(),
            remediation: None
        }
        .is_retryable());
        assert!(!BackendError::Unavailable("gh missing".into()).is_retryable());
    }

    #[test]
    fn retry_after_only_rate_limited() {
        use std::time::Duration;
        let d = Duration::from_secs(5);
        assert_eq!(
            BackendError::RateLimited {
                retry_after: Some(d)
            }
            .retry_after(),
            Some(d)
        );
        assert_eq!(BackendError::Timeout.retry_after(), None);
    }
}
