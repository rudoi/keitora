//! `GitHubBackend` — a [`TaskBackend`] implementation over GitHub Issues +
//! Projects v2, with a machine-local SQLite [`Sidecar`] for the things GitHub
//! cannot model (dispatch log + a local revision counter).
//!
//! # Architecture — the typed `GhApi` seam
//!
//! Raw `gh` output is a byte stream; the universal contract suite
//! ([`crate::backend_tests`]) performs *stateful* sequences (create → claim →
//! set_done → re-read → assert), which a canned byte-returning fake cannot
//! satisfy. So the backend does **not** talk to [`GhTransport`] directly.
//! Instead it depends on a typed, in-memory-friendly seam:
//!
//! ```text
//!   GitHubBackend<A: GhApi>
//!        |
//!        v
//!   RetryingApi<A>  (uniform retry/backoff over the seam, via the Clock)
//!        |
//!        v
//!   trait GhApi  (typed issue/label/status/project ops)
//!     |                         |
//!     v                         v
//!   GhCliApi (production,     FakeGitHub (#[cfg(test)],
//!     single-shot over          a stateful in-memory model
//!     GhTransport+classify)     with fault injection)
//! ```
//!
//! Retry is layered once, in `RetryingApi`, so the same policy governs both the
//! production and the fake seam (the contract suite asserts retry via the fake's
//! fault injection). `GhCliApi` is a thin single-shot wrapper: the raw
//! [`GhTransport`] stays the low-level boundary *under* it, and
//! [`classify`](crate::github::transport::classify) /
//! [`retry`](crate::github::retry::retry) remain unit-tested.
//! This keeps the contract tests network-free and deterministic.
//!
//! # Data-model mapping
//!
//! - **task id** = issue number.
//! - **status** ↔ the project "Status" column via [`StatusMap`] (convention +
//!   `[tasks.github.status_map]` override). `cancelled` is not a column → close
//!   the issue as *not planned*; `done` → "Done" column + close as *completed*.
//! - **assignment** — a `keitora:<worktree>` label (claim/assign/release).
//! - **tags** → labels (minus reserved `keitora:*` / `priority:*`).
//! - **priority** → a `priority:<p>` label.
//! - **deps + Keitora actor + branch/pr** — an HTML-comment metadata block in the
//!   issue body (parsed on read, rewritten on update). The human body is
//!   preserved verbatim around the block.

use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::backend::{
    BackendError, BackendResult, BoardSnapshot, ClaimOutcome, Consistency, DoneOutcome,
    Reservation, ReserveOutcome, TaskBackend,
};
use crate::dispatch::already_dispatched;
use crate::github::retry::{Clock, RetryPolicy};
use crate::github::sidecar::Sidecar;
use crate::model::{
    DepNode, DispatchLogEntry, ListFilter, LogEntry, Task, TaskUpdate, TaskWithDeps,
};

mod api;
mod metadata;

#[cfg(test)]
mod fake;

pub(crate) use api::{GhApi, GhCliApi, IssueState, RawIssue, RetryingApi};
pub(crate) use metadata::{render_body, Metadata};

use crate::config::GitHubTaskConfig;
use crate::github::transport::GhCli;

// ---------------------------------------------------------------------------
// Status mapping & project reference
// ---------------------------------------------------------------------------

/// The reserved Keitora statuses we map to GitHub project columns.
pub(crate) const KEITORA_STATUSES: [&str; 3] = ["active", "open", "backlog"];

/// Tombstone marker (D5). GitHub Issues can't be deleted via the API, so `delete`
/// closes the issue and applies this label; every read path then hides any issue
/// carrying it so the issue is observably gone (`get → None`, mutations → NotFound).
/// The issue physically remains on GitHub (closed + labeled), human-recoverable by
/// removing the label.
pub(crate) const DELETED_LABEL: &str = "keitora:deleted";

/// `true` if `issue` is tombstoned (carries [`DELETED_LABEL`]). Centralized so every
/// read path (`get`/`list`/`require_issue`/`find`) hides tombstones uniformly.
fn is_tombstoned(issue: &RawIssue) -> bool {
    issue.labels.iter().any(|l| l == DELETED_LABEL)
}

/// Default convention: Keitora status name → GitHub project column name.
fn default_column_for(status: &str) -> &'static str {
    match status {
        "active" => "In progress",
        "backlog" => "Backlog",
        // `done` parks the card in "Done" before the issue is closed (close reason
        // is what `derive_status` actually reads back; the column keeps the board
        // visually correct). Override via `[tasks.github.status_map]`.
        "done" => "Done",
        // `open` ("ready to be picked up") maps to "Ready" so it is distinguishable
        // from the deliberately-shelved "Backlog" column. A board that uses a single
        // "Backlog" column for both can override via `[tasks.github.status_map]`.
        // The catch-all also covers `cancelled` (closed not-planned; its column is
        // never written by `cancel()`, so the value is cosmetic).
        _ => "Ready",
    }
}

/// Keitora-status ↔ project-column-name mapping (convention + config override).
#[derive(Debug, Clone)]
pub(crate) struct StatusMap {
    /// keitora-status → column display name.
    to_column: BTreeMap<String, String>,
}

impl StatusMap {
    /// Build from the convention, applying any `[tasks.github.status_map]`
    /// overrides (keitora-status → column-name).
    pub(crate) fn from_overrides(overrides: &BTreeMap<String, String>) -> Self {
        let mut to_column = BTreeMap::new();
        for status in KEITORA_STATUSES {
            to_column.insert(status.to_string(), default_column_for(status).to_string());
        }
        for (k, v) in overrides {
            to_column.insert(k.clone(), v.clone());
        }
        Self { to_column }
    }

    /// The project column display name for a keitora status (`active`/`open`/`backlog`).
    pub(crate) fn column(&self, status: &str) -> &str {
        self.to_column
            .get(status)
            .map(String::as_str)
            .unwrap_or_else(|| default_column_for(status))
    }

    /// The distinct project column display names that `set_status_field` may be
    /// asked to write — every `active`/`open`/`backlog` target plus the column
    /// `set_status(id, "done")` resolves to (`done` falls through `column()` to its
    /// override or the convention default). These must exist on the board; used at
    /// resolution time to validate it and to build the option-id lookup.
    pub(crate) fn required_columns(&self) -> Vec<String> {
        let mut cols: Vec<String> = Vec::new();
        for status in KEITORA_STATUSES
            .iter()
            .copied()
            .chain(std::iter::once("done"))
        {
            let c = self.column(status);
            if !cols.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                cols.push(c.to_string());
            }
        }
        cols
    }
}

impl Default for StatusMap {
    fn default() -> Self {
        Self::from_overrides(&BTreeMap::new())
    }
}

/// Resolved project coordinates, cached for the backend's lifetime.
#[derive(Debug, Clone)]
pub(crate) struct ProjectRef {
    pub owner: String,
    pub repo: String,
    /// The configured Projects v2 board number (`0` when unset → `GhCliApi`
    /// resolves the linked project at first use). The live project-item ops
    /// address the board through the resolved ProjectV2 / field / option ids cached
    /// inside `GhCliApi`, so this field is carried for reference only.
    #[allow(dead_code)]
    pub project_number: u64,
}

// ---------------------------------------------------------------------------
// Label conventions
// ---------------------------------------------------------------------------

const PRIORITY_PREFIX: &str = "priority:";
const KEITORA_PREFIX: &str = "keitora:";

fn priority_label(priority: &str) -> String {
    format!("{PRIORITY_PREFIX}{priority}")
}

fn worktree_label(worktree: &str) -> String {
    format!("{KEITORA_PREFIX}{worktree}")
}

/// `true` for labels Keitora manages internally (not surfaced as user tags).
fn is_reserved_label(label: &str) -> bool {
    label.starts_with(PRIORITY_PREFIX) || label.starts_with(KEITORA_PREFIX)
}

/// Extract the worktree name from the first `keitora:<worktree>` label, if any.
fn assignee_from_labels(labels: &[String]) -> Option<String> {
    labels
        .iter()
        .find_map(|l| l.strip_prefix(KEITORA_PREFIX).map(str::to_string))
}

/// Extract the priority from the first `priority:<p>` label (defaults to `normal`).
fn priority_from_labels(labels: &[String]) -> String {
    labels
        .iter()
        .find_map(|l| l.strip_prefix(PRIORITY_PREFIX).map(str::to_string))
        .unwrap_or_else(|| "normal".to_string())
}

/// Tags = labels that are not Keitora-reserved.
fn tags_from_labels(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter(|l| !is_reserved_label(l))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// GitHubBackend
// ---------------------------------------------------------------------------

/// A [`TaskBackend`] over GitHub, generic over the [`GhApi`] seam.
///
/// Production uses [`GhCliApi`]; the contract tests use `FakeGitHub`.
pub(crate) struct GitHubBackend<A: GhApi> {
    /// The typed GitHub seam, wrapped in a uniform retry decorator. Every op the
    /// backend issues is retried per the policy via the injectable clock.
    api: RetryingApi<A>,
    project: ProjectRef,
    status_map: StatusMap,
    sidecar: Sidecar,
}

impl GitHubBackend<GhCliApi> {
    /// Construct the production GitHub backend from `[tasks.github]` config.
    ///
    /// Resolves owner/repo (config first, else the `origin` git remote in `cwd`),
    /// the project number, and the status map. Preserves `BackendError` kinds:
    /// missing owner/repo/project → [`BackendError::Config`]; the `gh`/auth
    /// preflight surfaces `Unavailable`/`Unauthorized` lazily on first use.
    pub(crate) fn open(
        gh: &GitHubTaskConfig,
        sidecar_path: &std::path::Path,
        cwd: &std::path::Path,
    ) -> BackendResult<Self> {
        let (owner, repo) = match (gh.owner.clone(), gh.repo.clone()) {
            (Some(o), Some(r)) => (o, r),
            _ => api::owner_repo_from_git_remote(cwd)?,
        };
        // `project_number` is optional: when absent, `GhCliApi` resolves the repo's
        // linked project deterministically on first use (exact-title > stripped-
        // suffix > first). `ProjectRef.project_number` mirrors the configured value
        // (0 sentinel when unset) — the live ops address the board through the
        // resolved ids cached inside `GhCliApi`, not this field.
        let project = ProjectRef {
            owner,
            repo,
            project_number: gh.project_number.unwrap_or(0),
        };
        let status_map = StatusMap::from_overrides(&gh.status_map);
        let sidecar = Sidecar::open(sidecar_path)?;
        let retry = RetryPolicy::default();
        let clock: Arc<dyn Clock> = Arc::new(crate::github::retry::TokioClock);
        // Resolution config (project number + status map) is carried into the api so
        // it can lazily resolve the board's ProjectV2 / Status field / option ids.
        let api = GhCliApi::with_config(
            Box::new(GhCli::new()),
            gh.project_number,
            status_map.clone(),
        );
        Ok(Self::new(api, retry, clock, project, status_map, sidecar))
    }
}

impl<A: GhApi> GitHubBackend<A> {
    /// Construct from a resolved [`GhApi`], retry policy, clock, project, status
    /// map and sidecar. The api is wrapped in a [`RetryingApi`] so every op is
    /// retried uniformly.
    pub(crate) fn new(
        api: A,
        retry: RetryPolicy,
        clock: Arc<dyn Clock>,
        project: ProjectRef,
        status_map: StatusMap,
        sidecar: Sidecar,
    ) -> Self {
        Self {
            api: RetryingApi::new(api, retry, clock),
            project,
            status_map,
            sidecar,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn project(&self) -> &ProjectRef {
        &self.project
    }

    /// Convert a raw issue into a [`Task`], parsing the metadata block.
    fn issue_to_task(&self, issue: &RawIssue) -> Task {
        let meta = Metadata::parse(&issue.body);
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(issue, assignee.as_deref());
        Task {
            id: issue.number,
            title: issue.title.clone(),
            description: meta.human_body(&issue.body),
            status,
            priority: priority_from_labels(&issue.labels),
            assigned_to: assignee,
            created_by: meta.created_by.clone(),
            tags: tags_from_labels(&issue.labels),
            branch: meta.branch.clone(),
            pr_url: meta.pr_url.clone(),
            created_at: issue.created_at.clone(),
            updated_at: issue.updated_at.clone(),
        }
    }

    /// Reconstruct the Keitora status from issue open/closed state + status column +
    /// the `keitora:<worktree>` label.
    fn derive_status(&self, issue: &RawIssue, assignee: Option<&str>) -> String {
        match issue.state {
            IssueState::Closed { not_planned: true } => "cancelled".to_string(),
            IssueState::Closed { not_planned: false } => "done".to_string(),
            IssueState::Open => {
                let col = issue.status_column.as_deref().unwrap_or("");
                if col.eq_ignore_ascii_case(self.status_map.column("backlog"))
                    && !col.eq_ignore_ascii_case(self.status_map.column("open"))
                {
                    return "backlog".to_string();
                }
                // "In progress" (active) only when a worktree holds the claim.
                if assignee.is_some() && col.eq_ignore_ascii_case(self.status_map.column("active"))
                {
                    return "active".to_string();
                }
                "open".to_string()
            }
        }
    }

    async fn get_issue(&self, id: i64) -> BackendResult<Option<RawIssue>> {
        self.api.issue_get(&self.project, id).await
    }

    async fn require_issue(&self, id: i64) -> BackendResult<RawIssue> {
        // A tombstoned issue is treated as absent so every mutation that routes
        // through `require_issue` rejects it with `NotFound`.
        self.get_issue(id)
            .await?
            .filter(|issue| !is_tombstoned(issue))
            .ok_or(BackendError::NotFound(id))
    }

    /// All unfinished (non-done) dep ids of `id`, reading dep markers + each dep's
    /// state. `blocked_by` carries only deps that are not yet done.
    async fn blocked_by_ids(&self, issue: &RawIssue) -> BackendResult<Vec<i64>> {
        let meta = Metadata::parse(&issue.body);
        let mut out = Vec::new();
        for dep in meta.blocked_by {
            match self.get_issue(dep).await? {
                Some(dep_issue) => {
                    if !matches!(dep_issue.state, IssueState::Closed { not_planned: false }) {
                        out.push(dep);
                    }
                }
                None => out.push(dep), // missing dep is conservatively "unfinished"
            }
        }
        Ok(out)
    }

    /// Build a [`TaskWithDeps`] from an **already-fetched** set of every board
    /// issue — no network. `list`/`get` use this so resolving deps for N tasks
    /// costs one board fetch, not N+1 (a per-task `issue_list_all` re-fetch turns
    /// `list` into O(N²) `gh` calls and hangs on a real board).
    ///
    /// `blocked_by`: this issue's dep markers, minus any dep already done
    /// (closed-completed) or tombstoned; a dep missing from the set is
    /// conservatively unfinished.
    /// `blocks`: non-tombstoned issues in the set whose markers name this issue.
    fn with_deps_in_set(&self, issue: &RawIssue, all: &[RawIssue]) -> TaskWithDeps {
        let meta = Metadata::parse(&issue.body);
        let mut blocked_by = Vec::new();
        for dep in meta.blocked_by {
            match all.iter().find(|i| i.number == dep) {
                // Dep is done → no longer blocking.
                Some(d) if matches!(d.state, IssueState::Closed { not_planned: false }) => {}
                // Dep is tombstoned (deleted) → drop the dangling edge.
                Some(d) if is_tombstoned(d) => {}
                // Unfinished, or missing from the board (conservatively unfinished).
                _ => blocked_by.push(dep),
            }
        }
        let mut blocks: Vec<i64> = all
            .iter()
            // A tombstoned dependent is gone — it must not appear in `blocks`.
            .filter(|i| !is_tombstoned(i))
            .filter(|i| Metadata::parse(&i.body).blocked_by.contains(&issue.number))
            .map(|i| i.number)
            .collect();
        blocks.sort_unstable();
        TaskWithDeps {
            task: self.issue_to_task(issue),
            blocked_by,
            blocks,
        }
    }

    /// Shape the board `TaskWithDeps` list from an **already-fetched** issue set,
    /// applying the `ListFilter` and the stable priority/id ordering. Shared by
    /// `list` and `board_snapshot` so the two can never drift (the `board_snapshot`
    /// equivalence contract test guards this) — no network.
    fn shape_list(&self, all: &[RawIssue], filter: &ListFilter) -> Vec<TaskWithDeps> {
        let mut out = Vec::new();
        for issue in all {
            // Tombstoned issues are hidden from every view.
            if is_tombstoned(issue) {
                continue;
            }
            let twd = self.with_deps_in_set(issue, all);
            let task = &twd.task;
            if !filter.include_backlog && task.status == "backlog" {
                continue;
            }
            if !filter.include_archived && matches!(task.status.as_str(), "done" | "cancelled") {
                // Archive window: hide terminal tasks older than archive_days.
                if older_than_days(&task.updated_at, filter.archive_days) {
                    continue;
                }
            }
            if let Some(who) = &filter.assigned_to {
                if task.assigned_to.as_deref() != Some(who.as_str()) {
                    continue;
                }
            }
            out.push(twd);
        }
        out.sort_by(|a, b| {
            priority_rank(&a.task.priority)
                .cmp(&priority_rank(&b.task.priority))
                .then(a.task.id.cmp(&b.task.id))
        });
        out
    }

    /// Whether a dispatched-reservation candidate is still live given the current
    /// `RawIssue`: open + still carrying this worktree's assignee label. Shared by
    /// `dispatched_reservations` (per-issue fetch) and `board_snapshot` (from the
    /// combined fetch) so the predicate stays identical in both — no network.
    fn reservation_is_live(&self, issue: &RawIssue, worktree: &str) -> bool {
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(issue, assignee.as_deref());
        status == "open" && assignee.as_deref() == Some(worktree)
    }

    /// Set the project status column for an issue, honouring the keitora→column map.
    async fn set_status(&self, id: i64, keitora_status: &str) -> BackendResult<()> {
        let column = self.status_map.column(keitora_status).to_string();
        self.api.set_status_field(&self.project, id, &column).await
    }

    /// Detect a cycle: would adding `task_id → depends_on` create one? (i.e. is
    /// `task_id` reachable from `depends_on` through dep markers?)
    async fn creates_cycle(&self, task_id: i64, depends_on: i64) -> BackendResult<bool> {
        // Walk the dep graph from `depends_on`; if we reach `task_id`, it's a cycle.
        let all = self.api.issue_list_all(&self.project).await?;
        let mut deps_of: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for issue in &all {
            let meta = Metadata::parse(&issue.body);
            deps_of.insert(issue.number, meta.blocked_by);
        }
        let mut stack = vec![depends_on];
        let mut seen = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if node == task_id {
                return Ok(true);
            }
            if !seen.insert(node) {
                continue;
            }
            if let Some(children) = deps_of.get(&node) {
                stack.extend(children.iter().copied());
            }
        }
        Ok(false)
    }
}

#[async_trait]
impl<A: GhApi> TaskBackend for GitHubBackend<A> {
    fn consistency(&self) -> Consistency {
        Consistency::Eventual
    }

    async fn revision(&self) -> BackendResult<String> {
        let cursor = self.api.project_cursor(&self.project).await?;
        let counter = self.sidecar.revision_counter()?;
        Ok(format!("{cursor}:{counter}"))
    }

    // ---- reads -----------------------------------------------------------

    async fn get(&self, id: i64) -> BackendResult<Option<TaskWithDeps>> {
        // Fetch the whole board once: deps (`blocks`) need the full set anyway,
        // and this keeps `get` to a single round-trip.
        let all = self.api.issue_list_all(&self.project).await?;
        Ok(all
            .iter()
            .find(|i| i.number == id)
            // A tombstoned issue reads as gone: `delete → get == None`.
            .filter(|issue| !is_tombstoned(issue))
            .map(|issue| self.with_deps_in_set(issue, &all)))
    }

    async fn list(&self, filter: &ListFilter) -> BackendResult<Vec<TaskWithDeps>> {
        let all = self.api.issue_list_all(&self.project).await?;
        Ok(self.shape_list(&all, filter))
    }

    async fn find(&self, term: &str, archive_days: i64) -> BackendResult<Vec<Task>> {
        let all = self.api.issue_list_all(&self.project).await?;
        let needle = term.to_lowercase();
        let mut tasks: Vec<Task> = all
            .into_iter()
            // Tombstoned issues never appear in search results.
            .filter(|i| !is_tombstoned(i))
            .map(|i| self.issue_to_task(&i))
            .filter(|t| {
                t.title.to_lowercase().contains(&needle)
                    || t.description.to_lowercase().contains(&needle)
            })
            .filter(|t| {
                !(matches!(t.status.as_str(), "done" | "cancelled")
                    && older_than_days(&t.updated_at, archive_days))
            })
            .collect();
        // Clippy 1.98 reports `unnecessary_sort_by` on this descending sort; the comparator
        // stays as written because `sort_by_key` with `Reverse` gives the same order.
        #[allow(clippy::unnecessary_sort_by)]
        tasks.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(tasks)
    }

    async fn logs(&self, id: i64, limit: i64) -> BackendResult<Vec<LogEntry>> {
        // GitHub timeline is observational; Keitora keeps its activity log inside the
        // metadata block so reads round-trip without a separate API call.
        let issue = self.require_issue(id).await?;
        let meta = Metadata::parse(&issue.body);
        let mut entries = meta.logs;
        entries.reverse(); // newest-first
        if limit >= 0 {
            entries.truncate(limit as usize);
        }
        Ok(entries)
    }

    async fn deps_tree(&self, id: i64) -> BackendResult<Vec<DepNode>> {
        let all = self.api.issue_list_all(&self.project).await?;
        let by_id: BTreeMap<i64, RawIssue> = all.into_iter().map(|i| (i.number, i)).collect();

        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        // BFS up to depth 10, ordered by (depth, id).
        let mut frontier: Vec<i64> = match by_id.get(&id) {
            Some(issue) => Metadata::parse(&issue.body).blocked_by,
            None => return Ok(out),
        };
        let mut depth = 1i64;
        while !frontier.is_empty() && depth <= 10 {
            frontier.sort_unstable();
            frontier.dedup();
            let mut next: Vec<i64> = Vec::new();
            for dep in &frontier {
                if !seen.insert(*dep) {
                    continue;
                }
                if let Some(issue) = by_id.get(dep) {
                    // A deleted dep is gone from every read path; skip it (and its
                    // subtree) so the dep tree matches get/list/find.
                    if is_tombstoned(issue) {
                        continue;
                    }
                    let task = self.issue_to_task(issue);
                    out.push(DepNode {
                        id: *dep,
                        depth,
                        title: task.title,
                        status: task.status,
                    });
                    next.extend(Metadata::parse(&issue.body).blocked_by);
                }
            }
            frontier = next;
            depth += 1;
        }
        Ok(out)
    }

    // ---- lifecycle mutations --------------------------------------------

    async fn create(
        &self,
        actor: &str,
        title: &str,
        description: &str,
        priority: &str,
        tags: &[String],
        idempotency_key: Option<&str>,
    ) -> BackendResult<Task> {
        // find-before-create dedupe (explicit key only — a caller-supplied key is
        // the contract for replay-safety across separate `create` calls).
        if let Some(key) = idempotency_key {
            if let Some(existing) = self
                .api
                .find_by_idempotency_marker(&self.project, key)
                .await?
            {
                return Ok(self.issue_to_task(&existing));
            }
        }

        let mut labels: Vec<String> = vec![priority_label(priority)];
        labels.extend(tags.iter().cloned());

        // ALWAYS embed an idempotency marker so the multi-step `issue_create`
        // (create → item-add → re-read) is safe under RetryingApi: a transient
        // failure on a *later* step re-runs the whole call, and the marker lets
        // `issue_create` find the already-created issue instead of duplicating it.
        // An explicit key is reused verbatim; otherwise synthesize a per-CALL
        // nonce ONCE here so every internal retry of this single call shares it.
        let marker = match idempotency_key {
            Some(k) => k.to_string(),
            None => format!(
                "auto-{actor}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ),
        };

        let meta = Metadata {
            created_by: Some(actor.to_string()),
            idempotency_key: Some(marker),
            logs: vec![LogEntry {
                created_at: String::new(),
                worktree: actor.to_string(),
                action: "created".to_string(),
                detail: String::new(),
            }],
            ..Default::default()
        };
        let body = render_body(description, &meta);

        let issue = self
            .api
            .issue_create(&self.project, title, &body, &labels)
            .await?;
        // New issues land in the "open" column by convention.
        self.set_status(issue.number, "open").await?;
        // Re-read so derived fields reflect the status set above.
        let fresh = self.require_issue(issue.number).await?;
        Ok(self.issue_to_task(&fresh))
    }

    async fn claim(&self, id: i64, actor: &str) -> BackendResult<ClaimOutcome> {
        let issue = self.require_issue(id).await?;
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(&issue, assignee.as_deref());

        // Terminal → Conflict (confirmed on a fresh read).
        if matches!(status.as_str(), "done" | "cancelled") {
            return Err(BackendError::Conflict(format!("task #{id} is {status}")));
        }
        // Already claimed by a different worktree → Conflict.
        if let Some(holder) = &assignee {
            if holder != actor {
                return Err(BackendError::Conflict(format!(
                    "task #{id} is already claimed by {holder}"
                )));
            }
            // Already mine (and non-terminal): re-affirm success as a true no-op —
            // no re-labelling, no status rewrite, no duplicate "claimed" log entry.
            // Matches the trait's idempotent re-claim contract + SqliteBackend's
            // AlreadyYours no-op behaviour.
            return Ok(ClaimOutcome::ClaimedUnverified(self.issue_to_task(&issue)));
        }
        // Unfinished deps → Conflict.
        let blocked = self.blocked_by_ids(&issue).await?;
        if !blocked.is_empty() {
            return Err(BackendError::Conflict(format!(
                "task #{id} is blocked by {blocked:?}"
            )));
        }

        // Add the keitora:<worktree> label, set status active, record actor.
        self.api
            .add_label(&self.project, id, &worktree_label(actor))
            .await?;
        self.set_status(id, "active").await?;
        // `issue` is still current: add_label/set_status don't touch the body.
        self.append_log(&issue, actor, "claimed", "").await?;

        let fresh = self.require_issue(id).await?;
        Ok(ClaimOutcome::ClaimedUnverified(self.issue_to_task(&fresh)))
    }

    async fn unclaim(&self, id: i64, actor: &str) -> BackendResult<()> {
        let issue = self.require_issue(id).await?;
        let assignee = assignee_from_labels(&issue.labels);
        match assignee {
            None => Ok(()), // already unassigned → idempotent success
            Some(holder) if holder == actor => {
                self.api
                    .remove_label(&self.project, id, &worktree_label(actor))
                    .await?;
                self.set_status(id, "open").await?;
                self.append_log(&issue, actor, "unclaimed", "").await?;
                Ok(())
            }
            Some(holder) => Err(BackendError::Conflict(format!(
                "task #{id} is claimed by {holder}, not {actor}"
            ))),
        }
    }

    async fn set_done(&self, id: i64, actor: &str) -> BackendResult<DoneOutcome> {
        let issue = self.require_issue(id).await?;
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(&issue, assignee.as_deref());

        // Idempotent: already done → success.
        if status == "done" {
            let unblocked = self.newly_unblocked_by(id).await?;
            return Ok(DoneOutcome {
                task: self.issue_to_task(&issue),
                unblocked,
            });
        }
        // Must be the current assignee.
        if assignee.as_deref() != Some(actor) {
            return Err(BackendError::Conflict(format!(
                "task #{id} is not assigned to {actor}"
            )));
        }

        self.set_status(id, "done").await?;
        self.api.close_issue(&self.project, id, false).await?;
        self.append_log(&issue, actor, "done", "").await?;

        let fresh = self.require_issue(id).await?;
        let unblocked = self.newly_unblocked_by(id).await?;
        Ok(DoneOutcome {
            task: self.issue_to_task(&fresh),
            unblocked,
        })
    }

    /// Dependents newly unblocked once `id` is done, computed from a **single**
    /// board fetch (the trait default does `blocks(id)` + a `get(dep)` per
    /// dependent, each a full `issue_list_all` → O(N²) `gh` calls).
    ///
    /// In-memory, treating `id` as done: a dependent qualifies when its
    /// `blocked_by` markers include `id`, it is still `open`, and *every* one of
    /// its `blocked_by` deps is now done (closed-completed, or `id` itself).
    async fn newly_unblocked_by(&self, id: i64) -> BackendResult<Vec<i64>> {
        let all = self.api.issue_list_all(&self.project).await?;
        // Quick lookup: which ids read as done (closed-completed) right now.
        let is_done = |dep: i64| -> bool {
            dep == id
                || all.iter().any(|i| {
                    i.number == dep && matches!(i.state, IssueState::Closed { not_planned: false })
                })
        };
        let mut out = Vec::new();
        for issue in &all {
            let meta = Metadata::parse(&issue.body);
            // Must depend on the just-completed task.
            if !meta.blocked_by.contains(&id) {
                continue;
            }
            // Only an `open` dependent can become newly unblocked (mirrors the
            // trait default + `with_deps_in_set`'s `derive_status`).
            let assignee = assignee_from_labels(&issue.labels);
            if self.derive_status(issue, assignee.as_deref()) != "open" {
                continue;
            }
            // Every dep satisfied (with `id` treated as done).
            if meta.blocked_by.iter().all(|dep| is_done(*dep)) {
                out.push(issue.number);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    async fn cancel(&self, id: i64, actor: &str) -> BackendResult<()> {
        // Read once to surface NotFound and to reuse for the log append (close
        // doesn't touch the body, so this read stays current).
        let issue = self.require_issue(id).await?;
        self.api.close_issue(&self.project, id, true).await?;
        self.append_log(&issue, actor, "cancelled", "").await?;
        Ok(())
    }

    async fn delete(&self, id: i64) -> BackendResult<()> {
        // One board fetch serves both the existence/tombstone check and the
        // dependents scan — avoids `require_issue`'s `issue_get` plus the `blocks`
        // default's second `issue_list_all`.
        let all = self.api.issue_list_all(&self.project).await?;
        // NotFound if absent or already tombstoned (so a repeated delete → NotFound).
        let issue = all
            .iter()
            .find(|i| i.number == id)
            .filter(|i| !is_tombstoned(i))
            .ok_or(BackendError::NotFound(id))?;

        // Dependents (the `blocks` direction): refuse + name them rather than
        // silently unblock other work (D3). Reuse `with_deps_in_set` so the
        // tombstone filtering matches every other read path; one shared message.
        let dependents = self.with_deps_in_set(issue, &all).blocks;
        if !dependents.is_empty() {
            return Err(crate::backend::dependents_conflict(id, &dependents));
        }

        // Tombstone. GitHub can't delete issues, so `DELETED_LABEL` *is* the delete:
        // every read path hides it, so `delete → get == None` holds the instant the
        // label lands. Ensure the label exists first — `add_label` errors on an
        // unknown label and a fresh repo may never have created `keitora:deleted` — then
        // apply it. If labeling fails the issue is untouched and we abort cleanly,
        // so there is never a closed-but-visible half-tombstone.
        self.api.ensure_label(&self.project, DELETED_LABEL).await?;
        self.api.add_label(&self.project, id, DELETED_LABEL).await?;

        // Closing is cosmetic (GitHub's own UI shows it not-planned); the tombstone
        // above already hides it from Keitora. Best-effort: a close failure must not
        // resurface an already-deleted task by reporting an error to the caller.
        let _ = self.api.close_issue(&self.project, id, true).await;

        // Drop the task's dispatch history from the sidecar — it has no FK to the
        // GitHub issue, so nothing else cleans it up (mirrors the SQLite purge). No
        // audit record is kept (D6).
        self.sidecar.dispatch_log_purge(id)?;
        Ok(())
    }

    async fn backlog(&self, id: i64, actor: &str) -> BackendResult<()> {
        let issue = self.require_issue(id).await?;
        if let Some(holder) = assignee_from_labels(&issue.labels) {
            self.api
                .remove_label(&self.project, id, &worktree_label(&holder))
                .await?;
        }
        // Re-open if it was closed (backlog is a live, non-terminal state).
        if !matches!(issue.state, IssueState::Open) {
            self.api.reopen_issue(&self.project, id).await?;
        }
        self.set_status(id, "backlog").await?;
        self.append_log(&issue, actor, "backlogged", "").await?;
        Ok(())
    }

    async fn update(&self, id: i64, actor: &str, upd: &TaskUpdate) -> BackendResult<()> {
        if upd.is_empty() {
            return Err(BackendError::Conflict("nothing to update".to_string()));
        }
        // Terminal transitions can't be expressed as a Status-column write
        // (`KEITORA_STATUSES` covers only active/open/backlog); silently dropping them
        // would let `update(status=done)` succeed without applying. Reject so the
        // caller routes through `set_done`/`cancel` instead.
        if let Some(status) = &upd.status {
            if matches!(status.as_str(), "done" | "cancelled") {
                return Err(BackendError::Conflict(format!(
                    "cannot set status to {status} via update; use set_done/cancel"
                )));
            }
        }
        let issue = self.require_issue(id).await?;
        let mut meta = Metadata::parse(&issue.body);
        let mut human = meta.human_body(&issue.body);

        let mut changed: Vec<&str> = Vec::new();
        let mut new_title: Option<String> = None;
        let mut label_adds: Vec<String> = Vec::new();
        let mut label_removes: Vec<String> = Vec::new();

        if let Some(title) = &upd.title {
            new_title = Some(title.clone());
            changed.push("title");
        }
        if let Some(desc) = &upd.description {
            human = desc.clone();
            changed.push("description");
        }
        if let Some(priority) = &upd.priority {
            let old = priority_from_labels(&issue.labels);
            if old != *priority {
                label_removes.push(priority_label(&old));
            }
            label_adds.push(priority_label(priority));
            changed.push("priority");
        }
        if let Some(branch) = &upd.branch {
            meta.branch = Some(branch.clone());
            changed.push("branch");
        }
        if let Some(pr) = &upd.pr_url {
            meta.pr_url = Some(pr.clone());
            changed.push("pr_url");
        }
        // `status` updates via the patch are applied through the status column.
        let status_update = upd.status.clone();

        let detail = changed.join(",");
        meta.logs.push(LogEntry {
            created_at: String::new(),
            worktree: actor.to_string(),
            action: "updated".to_string(),
            detail: detail.clone(),
        });

        let new_body = render_body(&human, &meta);
        self.api
            .issue_edit(&self.project, id, new_title.as_deref(), Some(&new_body))
            .await?;
        for l in label_removes {
            self.api.remove_label(&self.project, id, &l).await?;
        }
        for l in label_adds {
            self.api.add_label(&self.project, id, &l).await?;
        }
        if let Some(status) = status_update {
            if KEITORA_STATUSES.contains(&status.as_str()) {
                self.set_status(id, &status).await?;
            }
        }
        Ok(())
    }

    // ---- dependency graph -----------------------------------------------

    async fn add_dep(&self, task_id: i64, depends_on: i64, actor: &str) -> BackendResult<()> {
        if task_id == depends_on {
            return Err(BackendError::Conflict(
                "a task cannot depend on itself".to_string(),
            ));
        }
        let issue = self.require_issue(task_id).await?;
        // depends_on must exist too.
        self.require_issue(depends_on).await?;

        let mut meta = Metadata::parse(&issue.body);
        if meta.blocked_by.contains(&depends_on) {
            return Err(BackendError::Conflict("already exists".to_string()));
        }
        if self.creates_cycle(task_id, depends_on).await? {
            return Err(BackendError::Conflict(format!(
                "adding #{task_id} -> #{depends_on} would create a cycle"
            )));
        }

        meta.blocked_by.push(depends_on);
        meta.blocked_by.sort_unstable();
        meta.logs.push(LogEntry {
            created_at: String::new(),
            worktree: actor.to_string(),
            action: "dep_added".to_string(),
            detail: depends_on.to_string(),
        });
        let human = meta.human_body(&issue.body);
        let new_body = render_body(&human, &meta);
        self.api
            .issue_edit(&self.project, task_id, None, Some(&new_body))
            .await
    }

    async fn remove_dep(&self, task_id: i64, depends_on: i64, actor: &str) -> BackendResult<()> {
        let issue = self.require_issue(task_id).await?;
        let mut meta = Metadata::parse(&issue.body);
        if !meta.blocked_by.contains(&depends_on) {
            return Ok(()); // no-op
        }
        meta.blocked_by.retain(|d| *d != depends_on);
        meta.logs.push(LogEntry {
            created_at: String::new(),
            worktree: actor.to_string(),
            action: "dep_removed".to_string(),
            detail: depends_on.to_string(),
        });
        let human = meta.human_body(&issue.body);
        let new_body = render_body(&human, &meta);
        self.api
            .issue_edit(&self.project, task_id, None, Some(&new_body))
            .await
    }

    // ---- dispatcher primitives ------------------------------------------

    async fn assign(&self, id: i64, actor: &str, worktree: &str) -> BackendResult<Task> {
        let issue = self.require_issue(id).await?;
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(&issue, assignee.as_deref());

        // Same-worktree re-affirm → success.
        if assignee.as_deref() == Some(worktree) && status == "open" {
            return Ok(self.issue_to_task(&issue));
        }
        // Must be open + eligible.
        if status != "open" {
            return Err(BackendError::Conflict(format!(
                "task #{id} is {status}, not assignable"
            )));
        }
        if let Some(holder) = &assignee {
            if holder != worktree {
                return Err(BackendError::Conflict(format!(
                    "task #{id} is already assigned to {holder}"
                )));
            }
        }
        let blocked = self.blocked_by_ids(&issue).await?;
        if !blocked.is_empty() {
            return Err(BackendError::Conflict(format!(
                "task #{id} is blocked by {blocked:?}"
            )));
        }

        self.api
            .add_label(&self.project, id, &worktree_label(worktree))
            .await?;
        // assign leaves status open (reservation), not active.
        self.append_log(&issue, actor, "assigned", worktree).await?;
        let fresh = self.require_issue(id).await?;
        Ok(self.issue_to_task(&fresh))
    }

    async fn release(&self, id: i64, actor: &str) -> BackendResult<()> {
        let issue = self.require_issue(id).await?;
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(&issue, assignee.as_deref());
        if matches!(status.as_str(), "done" | "cancelled" | "backlog") {
            return Err(BackendError::Conflict(format!(
                "cannot release a {status} task"
            )));
        }
        if let Some(holder) = assignee {
            self.api
                .remove_label(&self.project, id, &worktree_label(&holder))
                .await?;
        }
        self.set_status(id, "open").await?;
        self.append_log(&issue, actor, "released", "").await?;
        Ok(())
    }

    // ---- dispatch log / reservations (sidecar) --------------------------

    async fn dispatch_log_add(
        &self,
        task_id: i64,
        worktree: &str,
        decision: &str,
        rationale: &str,
    ) -> BackendResult<()> {
        self.sidecar
            .dispatch_log_add(task_id, worktree, decision, rationale)
    }

    async fn dispatch_log_list(&self, limit: i64) -> BackendResult<Vec<DispatchLogEntry>> {
        self.sidecar.dispatch_log_list(limit)
    }

    async fn dispatch_log_for(&self, task_id: i64) -> BackendResult<Vec<DispatchLogEntry>> {
        self.sidecar.dispatch_log_for(task_id)
    }

    async fn dispatch_log_latest_for(
        &self,
        task_id: i64,
        worktree: &str,
    ) -> BackendResult<Option<DispatchLogEntry>> {
        self.sidecar.dispatch_log_latest_for(task_id, worktree)
    }

    async fn dispatched_reservations(&self) -> BackendResult<Vec<Reservation>> {
        // Sidecar yields the dispatch-derived candidates; cross-filter against
        // current GitHub state (open + the keitora:<worktree> label present).
        let candidates = self.sidecar.dispatched_reservations()?;
        let mut out = Vec::new();
        for c in candidates {
            let issue = match self.get_issue(c.task_id).await? {
                Some(i) => i,
                None => continue,
            };
            if self.reservation_is_live(&issue, &c.worktree) {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// Combined board read: **one** `issue_list_all` serves both the task list and the
    /// dispatched reservations. The doc-01 default sequences `list()` +
    /// `dispatched_reservations()`, and the latter makes one `get_issue` per candidate
    /// — so the default costs `1 + N` GitHub round-trips. Here the reservations are
    /// cross-filtered against the SAME fetched slice (by `task_id == issue.number`),
    /// dropping it to **1**. Same observable result as the default (equivalence test).
    async fn board_snapshot(&self, filter: &ListFilter) -> BackendResult<BoardSnapshot> {
        let all = self.api.issue_list_all(&self.project).await?;
        let tasks = self.shape_list(&all, filter);

        // Index the fetched board by issue number once, so resolving each reservation
        // candidate is O(1) instead of an O(candidates × issues) linear rescan.
        let by_number: BTreeMap<i64, &RawIssue> = all.iter().map(|i| (i.number, i)).collect();
        let candidates = self.sidecar.dispatched_reservations()?;
        let mut reservations = Vec::new();
        for c in candidates {
            // Resolve against the already-fetched board — no per-candidate get_issue.
            // A candidate whose task_id is absent from the board is dropped, exactly
            // as the `None => continue` in `dispatched_reservations`.
            let Some(&issue) = by_number.get(&c.task_id) else {
                continue;
            };
            if self.reservation_is_live(issue, &c.worktree) {
                reservations.push(c);
            }
        }

        Ok(BoardSnapshot {
            tasks,
            reservations,
        })
    }

    /// Reserve a task for a worktree in the minimum GitHub round-trips. The doc-01
    /// default calls `require(id)` → `GitHubBackend::get()` → a **full** `issue_list_all`
    /// board fetch just to precheck one issue. Here we read that single issue directly
    /// (`get_issue` → one `issue_get`), derive the idempotency guard from it + the local
    /// sidecar decision, and only then `assign()`. `ReserveOutcome` semantics are
    /// preserved exactly, and any `Err` surfaces before the reserve write
    /// (never-`Err`-after-reserve). NB: `assign()` performs its own `require_issue`
    /// (a second single-issue read); collapsing that is tracked in task #4.
    async fn dispatch_reserve(
        &self,
        id: i64,
        dispatcher: &str,
        worktree: &str,
    ) -> BackendResult<ReserveOutcome> {
        // Single-issue read instead of the default's full-board `require`. A tombstoned
        // (deleted) issue reads as absent → NotFound, matching `require`'s rejection.
        let issue = self.require_issue(id).await?;
        let assignee = assignee_from_labels(&issue.labels);
        let status = self.derive_status(&issue, assignee.as_deref());
        let latest = self.dispatch_log_latest_for(id, worktree).await?;
        if already_dispatched(
            &status,
            assignee.as_deref(),
            worktree,
            latest.as_ref().map(|e| e.decision.as_str()),
        ) {
            return Ok(ReserveOutcome::AlreadyDispatched);
        }
        // All fallible prechecks are done; `assign` is the reserve write.
        let task = self.assign(id, dispatcher, worktree).await?;
        Ok(ReserveOutcome::Reserved(task))
    }
}

impl<A: GhApi> GitHubBackend<A> {
    /// Append an entry to the in-body activity log of an **already-fetched** issue.
    ///
    /// Takes the `RawIssue` the caller already holds (label/status/close ops don't
    /// touch the body, so the caller's fresh read is still current) — saves one
    /// `issue_get` round-trip per mutation versus re-reading by id.
    async fn append_log(
        &self,
        issue: &RawIssue,
        actor: &str,
        action: &str,
        detail: &str,
    ) -> BackendResult<()> {
        let mut meta = Metadata::parse(&issue.body);
        meta.logs.push(LogEntry {
            created_at: String::new(),
            worktree: actor.to_string(),
            action: action.to_string(),
            detail: detail.to_string(),
        });
        let human = meta.human_body(&issue.body);
        let new_body = render_body(&human, &meta);
        self.api
            .issue_edit(&self.project, issue.number, None, Some(&new_body))
            .await
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn priority_rank(priority: &str) -> u8 {
    match priority {
        "urgent" => 0,
        "high" => 1,
        "normal" => 2,
        "low" => 3,
        _ => 2,
    }
}

/// `true` if `timestamp` (RFC3339 or `YYYY-MM-DD HH:MM:SS`) is older than
/// `days` days ago. Unparseable timestamps are treated as recent (not archived).
fn older_than_days(timestamp: &str, days: i64) -> bool {
    if days <= 0 {
        // archive_days <= 0 means hide all terminal tasks (any age qualifies),
        // but only when a timestamp is present.
        return !timestamp.is_empty();
    }
    let parsed = parse_epoch_secs(timestamp);
    match parsed {
        Some(ts) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            now - ts > days * 86_400
        }
        None => false,
    }
}

/// Best-effort epoch-seconds parse for common timestamp shapes. Returns `None`
/// for anything we can't interpret (caller treats that as "recent").
#[allow(clippy::manual_div_ceil)]
fn parse_epoch_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Date-only or date-time "YYYY-MM-DD[ T]HH:MM:SS".
    let date_part = s.split(['T', ' ']).next()?;
    let mut it = date_part.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    // Days since a fixed epoch via a civil-date algorithm (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400)
}

// ---------------------------------------------------------------------------
// Contract suite + Eventual-tier tests (over FakeGitHub).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod contract_tests {
    use super::*;
    use crate::backend_tests;
    use crate::github::backend::fake::FakeGitHub;
    use crate::github::retry::RetryPolicy;
    use crate::github::test_support::TestClock;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A throwaway sidecar DB path, cleaned up on drop.
    struct TempDb {
        path: PathBuf,
    }
    impl TempDb {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "keitora-gh-contract-{}-{}.db",
                std::process::id(),
                n
            ));
            let td = TempDb { path };
            td.cleanup();
            td
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

    fn project() -> ProjectRef {
        ProjectRef {
            owner: "o".into(),
            repo: "r".into(),
            project_number: 1,
        }
    }

    /// Build a `GitHubBackend<FakeGitHub>` with a fresh sidecar; returns the
    /// backend, a handle to the same fake (via `Arc`-like sharing), and the
    /// TempDb guard.
    fn build(db: &TempDb) -> GitHubBackend<FakeGitHub> {
        let sidecar = Sidecar::open(&db.path).expect("open sidecar");
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new());
        GitHubBackend::new(
            FakeGitHub::new(),
            RetryPolicy::default(),
            clock,
            project(),
            StatusMap::default(),
            sidecar,
        )
    }

    #[tokio::test]
    async fn consistency_is_eventual() {
        let db = TempDb::new();
        let b = build(&db);
        assert!(matches!(b.consistency(), Consistency::Eventual));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn universal_tier() {
        let db = TempDb::new();
        let b = build(&db);
        backend_tests::run_universal(&b).await;
    }

    #[tokio::test]
    async fn strong_tier_auto_skips() {
        // run_strong is a no-op for Eventual backends — assert it doesn't panic.
        let db = TempDb::new();
        let b = build(&db);
        backend_tests::run_strong(&b).await;
    }

    // ---- Eventual tier -----------------------------------------------------

    #[tokio::test]
    async fn test_claim_returns_claimed_unverified() {
        let db = TempDb::new();
        let b = build(&db);
        let t = b
            .create("alice", "claim", "", "normal", &[], None)
            .await
            .unwrap();

        let first = b.claim(t.id, "alice").await.unwrap();
        assert!(
            matches!(first, ClaimOutcome::ClaimedUnverified(_)),
            "first claim must be ClaimedUnverified, got {first:?}"
        );

        let again = b.claim(t.id, "alice").await.unwrap();
        assert!(
            matches!(again, ClaimOutcome::ClaimedUnverified(_)),
            "re-claim must also be ClaimedUnverified, got {again:?}"
        );
    }

    #[tokio::test]
    async fn test_create_idempotency_key_deduplicates() {
        let db = TempDb::new();
        let b = build(&db);

        let t1 = b
            .create("alice", "idem", "desc", "normal", &[], Some("key-1"))
            .await
            .unwrap();
        // Replay the same key (lost-ack sim) → same task, no duplicate.
        let t2 = b
            .create("alice", "idem", "desc", "normal", &[], Some("key-1"))
            .await
            .unwrap();
        assert_eq!(t1.id, t2.id, "same idempotency key must dedupe to one task");

        let listed = b
            .list(&ListFilter {
                include_backlog: true,
                include_archived: true,
                archive_days: 365,
                assigned_to: None,
            })
            .await
            .unwrap();
        let matches: Vec<_> = listed.iter().filter(|x| x.task.id == t1.id).collect();
        assert_eq!(matches.len(), 1, "list must not contain a duplicate");
    }

    /// Build a backend over a shared `TestClock` so a test can assert recorded
    /// sleeps. `base` is tiny so exponential backoff stays below an injected hint.
    fn build_with_clock(db: &TempDb) -> (GitHubBackend<FakeGitHub>, Arc<TestClock>) {
        let sidecar = Sidecar::open(&db.path).unwrap();
        let clock = Arc::new(TestClock::new());
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let b = GitHubBackend::new(
            FakeGitHub::new(),
            RetryPolicy {
                max_attempts: 4,
                base: Duration::from_millis(1),
                max: Duration::from_secs(60),
            },
            clock_dyn,
            project(),
            StatusMap::default(),
            sidecar,
        );
        (b, clock)
    }

    #[tokio::test]
    async fn test_transient_triggers_retry() {
        let db = TempDb::new();
        let b = build(&db);
        // The first issue_create attempt fails transiently; retry must recover.
        b.api
            .inner()
            .fail_next_mutation(BackendError::Transient("flaky".into()));
        let before = b.api.inner().attempt_count();
        let t = b
            .create("alice", "retry-transient", "", "normal", &[], None)
            .await
            .expect("transient must be retried then succeed");
        // ≥2 attempts for the create op alone (1 failed + 1 applied).
        assert!(b.api.inner().attempt_count() >= before + 2);
        assert!(b.get(t.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_timeout_triggers_retry() {
        let db = TempDb::new();
        let b = build(&db);
        b.api.inner().fail_next_mutation(BackendError::Timeout);
        let t = b
            .create("alice", "retry-timeout", "", "normal", &[], None)
            .await
            .expect("timeout must be retried then succeed");
        assert!(b.get(t.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_conflict_never_retried() {
        let db = TempDb::new();
        let b = build(&db);
        let before = b.api.inner().attempt_count();
        b.api
            .inner()
            .fail_next_mutation(BackendError::Conflict("nope".into()));
        let err = b
            .create("alice", "no-retry-conflict", "", "normal", &[], None)
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::Conflict(_)));
        // Exactly one attempt — a Conflict is never retried.
        assert_eq!(b.api.inner().attempt_count(), before + 1);
    }

    #[tokio::test]
    async fn test_unauthorized_never_retried() {
        let db = TempDb::new();
        let b = build(&db);
        let before = b.api.inner().attempt_count();
        b.api
            .inner()
            .fail_next_mutation(BackendError::Unauthorized {
                message: "scope".into(),
                remediation: None,
            });
        let err = b
            .create("alice", "no-retry-auth", "", "normal", &[], None)
            .await
            .unwrap_err();
        assert!(matches!(err, BackendError::Unauthorized { .. }));
        assert_eq!(b.api.inner().attempt_count(), before + 1);
    }

    #[tokio::test]
    async fn test_rate_limited_waits_retry_after() {
        let db = TempDb::new();
        let (b, clock) = build_with_clock(&db);
        let hint = Duration::from_secs(30);
        // The first mutation (issue_create inside `create`) is rate-limited once.
        b.api.inner().fail_next_mutation(BackendError::RateLimited {
            retry_after: Some(hint),
        });

        let t = b
            .create("alice", "rate-limited", "", "normal", &[], None)
            .await
            .expect("rate-limited create must recover via retry");
        assert!(b.get(t.id).await.unwrap().is_some());

        let sleeps = clock.sleeps();
        assert!(
            !sleeps.is_empty(),
            "a Retry-After must have produced a sleep"
        );
        assert!(
            sleeps[0] >= hint,
            "first sleep {:?} must be >= Retry-After {hint:?}",
            sleeps[0]
        );
    }

    #[tokio::test]
    async fn test_create_retry_after_create_does_not_duplicate() {
        // FIX #1: a transient failure on the step AFTER `gh issue create` re-runs
        // the whole multi-step `issue_create` under RetryingApi. The embedded
        // idempotency marker must make the retry reuse the already-created issue —
        // exactly ONE issue with the marker, no duplicate.
        let db = TempDb::new();
        let b = build(&db);
        // Issue is recorded, then the post-create step fails once (transient).
        b.api
            .inner()
            .fail_after_create_once(BackendError::Transient("post-create flaky".into()));

        let t = b
            .create("alice", "no-dup", "", "normal", &[], Some("key-dup"))
            .await
            .expect("transient post-create must be retried then succeed");

        // Exactly one issue carries the marker.
        assert_eq!(
            b.api.inner().count_with_marker("key-dup"),
            1,
            "a retried create must not duplicate the issue"
        );

        // And the board agrees: exactly one task overall, and it's the one returned.
        let listed = b
            .list(&ListFilter {
                include_backlog: true,
                include_archived: true,
                archive_days: 365,
                assigned_to: None,
            })
            .await
            .unwrap();
        let matches: Vec<_> = listed.iter().filter(|x| x.task.id == t.id).collect();
        assert_eq!(matches.len(), 1, "list must not contain a duplicate");
        assert_eq!(listed.len(), 1, "exactly one task must exist after retry");
    }

    #[tokio::test]
    async fn test_newly_unblocked_by_override_multi_dep() {
        // FIX #3: the GitHubBackend override computes unblocked dependents from a
        // single board fetch. A dependent with TWO deps is only reported once BOTH
        // are done; finishing one alone must not report it.
        let db = TempDb::new();
        let b = build(&db);
        let dependent = b
            .create("alice", "dependent", "", "normal", &[], None)
            .await
            .unwrap();
        let dep1 = b
            .create("alice", "dep1", "", "normal", &[], None)
            .await
            .unwrap();
        let dep2 = b
            .create("alice", "dep2", "", "normal", &[], None)
            .await
            .unwrap();
        b.add_dep(dependent.id, dep1.id, "tester").await.unwrap();
        b.add_dep(dependent.id, dep2.id, "tester").await.unwrap();

        // Finish dep1 only — dependent still blocked by dep2.
        b.claim(dep1.id, "alice").await.unwrap();
        b.set_done(dep1.id, "alice").await.unwrap();
        let after_one = b.newly_unblocked_by(dep1.id).await.unwrap();
        assert!(
            !after_one.contains(&dependent.id),
            "dependent must NOT be unblocked while dep2 is open; got {after_one:?}"
        );

        // Finish dep2 — now every dep is done; finishing dep2 unblocks dependent.
        b.claim(dep2.id, "alice").await.unwrap();
        let outcome = b.set_done(dep2.id, "alice").await.unwrap();
        assert!(
            outcome.unblocked.contains(&dependent.id),
            "dependent must be unblocked once both deps are done; got {:?}",
            outcome.unblocked
        );
        // And the standalone recompute agrees.
        let recomputed = b.newly_unblocked_by(dep2.id).await.unwrap();
        assert!(recomputed.contains(&dependent.id));
    }

    #[tokio::test]
    async fn test_revision_advances_on_github_mutation() {
        let db = TempDb::new();
        let b = build(&db);
        let r0 = b.revision().await.unwrap();
        b.create("alice", "rev-gh", "", "normal", &[], None)
            .await
            .unwrap();
        let r1 = b.revision().await.unwrap();
        assert_ne!(r0, r1, "GitHub mutation must advance the composed revision");
    }

    #[tokio::test]
    async fn test_revision_advances_on_sidecar_op() {
        let db = TempDb::new();
        let b = build(&db);
        let t = b
            .create("alice", "rev-side", "", "normal", &[], None)
            .await
            .unwrap();
        let r1 = b.revision().await.unwrap();
        // A sidecar-only op (no GitHub mutation) must still advance the cursor.
        b.dispatch_log_add(t.id, "feng", "suggested", "")
            .await
            .unwrap();
        let r2 = b.revision().await.unwrap();
        assert_ne!(
            r1, r2,
            "sidecar dispatch_log_add must advance the composed revision"
        );
    }

    // ---- combined board read + single-issue reserve -----------------

    fn all_filter() -> ListFilter {
        ListFilter {
            include_backlog: true,
            include_archived: true,
            archive_days: 365,
            assigned_to: None,
        }
    }

    /// Make `id` a live dispatched reservation to `worktree` (open + labelled +
    /// latest sidecar decision "dispatched"), the state a real dispatch leaves behind.
    async fn reserve_and_log(b: &GitHubBackend<FakeGitHub>, id: i64, worktree: &str) {
        b.assign(id, "dispatcher", worktree).await.unwrap();
        b.dispatch_log_add(id, worktree, "dispatched", "")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn board_snapshot_is_one_fetch_zero_gets_and_equals_default() {
        let db = TempDb::new();
        let b = build(&db);
        let f = all_filter();

        let t1 = b.create("a", "one", "", "normal", &[], None).await.unwrap();
        let t2 = b.create("a", "two", "", "high", &[], None).await.unwrap();
        let _t3 = b.create("a", "three", "", "low", &[], None).await.unwrap();
        reserve_and_log(&b, t1.id, "wtA").await;
        reserve_and_log(&b, t2.id, "wtB").await;

        // Count only the reads board_snapshot itself issues (setup did its own).
        let fake = b.api.inner();
        let (list0, get0) = (fake.list_all_count(), fake.get_count());

        let snap = b.board_snapshot(&f).await.unwrap();

        assert_eq!(
            fake.list_all_count() - list0,
            1,
            "board_snapshot must make exactly one issue_list_all"
        );
        assert_eq!(
            fake.get_count() - get0,
            0,
            "board_snapshot must make zero per-candidate issue_get"
        );

        // Equivalence contract: identical (tasks, reservations) to the doc-01 default
        // sequence of list() + dispatched_reservations().
        let tasks = b.list(&f).await.unwrap();
        let reservations = b.dispatched_reservations().await.unwrap();
        assert_eq!(snap.tasks, tasks, "tasks must equal list()");
        assert_eq!(
            snap.reservations, reservations,
            "reservations must equal dispatched_reservations()"
        );
        assert_eq!(snap.reservations.len(), 2, "both reservations are live");
    }

    #[tokio::test]
    async fn board_snapshot_drops_a_reservation_whose_issue_is_gone() {
        // A candidate whose task_id is absent from the board is dropped — same as the
        // `None => continue` in dispatched_reservations.
        let db = TempDb::new();
        let b = build(&db);
        // A dispatch-log candidate for an issue that was never created.
        b.dispatch_log_add(999, "ghost", "dispatched", "")
            .await
            .unwrap();
        let snap = b.board_snapshot(&all_filter()).await.unwrap();
        assert!(snap.reservations.is_empty(), "orphan candidate is dropped");
        assert_eq!(
            snap.reservations,
            b.dispatched_reservations().await.unwrap(),
            "still equivalent to the default on the drop path"
        );
    }

    #[tokio::test]
    async fn dispatch_reserve_already_dispatched_is_one_get_no_board_fetch() {
        let db = TempDb::new();
        let b = build(&db);
        let t = b.create("a", "res", "", "normal", &[], None).await.unwrap();
        reserve_and_log(&b, t.id, "wt1").await;

        let fake = b.api.inner();
        let (list0, get0) = (fake.list_all_count(), fake.get_count());

        let outcome = b.dispatch_reserve(t.id, "disp", "wt1").await.unwrap();

        assert!(
            matches!(outcome, ReserveOutcome::AlreadyDispatched),
            "re-reserve of a live reservation is idempotent, got {outcome:?}"
        );
        assert_eq!(
            fake.list_all_count() - list0,
            0,
            "override must NOT fall back to a full-board issue_list_all"
        );
        assert_eq!(
            fake.get_count() - get0,
            1,
            "AlreadyDispatched path is a single issue_get precheck"
        );
    }

    #[tokio::test]
    async fn dispatch_reserve_fresh_reserves_without_a_board_fetch() {
        let db = TempDb::new();
        let b = build(&db);
        let t = b
            .create("a", "fresh", "", "normal", &[], None)
            .await
            .unwrap();

        let fake = b.api.inner();
        let list0 = fake.list_all_count();

        let outcome = b.dispatch_reserve(t.id, "disp", "wt1").await.unwrap();

        match outcome {
            ReserveOutcome::Reserved(task) => {
                assert_eq!(task.id, t.id);
                assert_eq!(task.assigned_to.as_deref(), Some("wt1"));
            }
            other => panic!("fresh reserve must be Reserved, got {other:?}"),
        }
        assert_eq!(
            fake.list_all_count() - list0,
            0,
            "the reserve path eliminates the default's full-board fetch (assign's own \
             single-issue reads are tracked separately in task #4)"
        );
    }

    #[tokio::test]
    async fn dispatch_reserve_conflicts_when_assigned_elsewhere() {
        let db = TempDb::new();
        let b = build(&db);
        let t = b
            .create("a", "held", "", "normal", &[], None)
            .await
            .unwrap();
        b.assign(t.id, "disp", "other").await.unwrap();

        let err = b
            .dispatch_reserve(t.id, "disp", "wt1")
            .await
            .expect_err("reserving a task held by another worktree must conflict");
        assert!(
            matches!(err, BackendError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
    }
}
