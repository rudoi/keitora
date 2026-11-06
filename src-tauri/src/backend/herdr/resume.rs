//! `resume` — which agent conversation a worktree's session should come back in.
//!
//! herdr's integration hook records each pane's agent session id, and `pane list`
//! reports it as `agent_session`. [`ResumeStore`] keeps the newest id observed per
//! `(project, worktree)` in one global SQLite DB (`<herdr home>/resume.db`, via
//! [`keitora_tasks::LocalStore`]) and, at spawn time, [`ResumeStore::resolve`]s a
//! [`ResumeDecision`]: resume a specific conversation, continue the newest one, or
//! start fresh.
//!
//! Persistence is best-effort: every DB error is logged at `debug` and swallowed,
//! so a broken store degrades resume, never a spawn.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use keitora_tasks::{is_safe_claude_session_id, CliKind, LocalStore};

use crate::backend::SessionKey;

use super::cli::{AgentSessionKind, PaneInfo};
use super::keymap::{held_cwd, normalize_cwd, HerdrPane};

/// Decides which keys `observe` must leave alone (a key pending recovery: its
/// restored shell still carries the old session ref and must not overwrite the
/// record).
pub type SkipFilter = Arc<dyn Fn(&SessionKey) -> bool + Send + Sync>;

/// The resume store. See the module docs.
pub struct ResumeStore {
    /// `None` when no DB could be opened at all: observations are dropped and
    /// `resolve` decides from transcripts alone.
    db: Mutex<Option<LocalStore>>,
    now_ms: Box<dyn Fn() -> i64 + Send + Sync>,
    skip: Mutex<Option<SkipFilter>>,
}

/// What [`ResumeStore::resolve`] needs about the session being spawned.
pub struct ResolveCtx<'a> {
    pub cli: CliKind,
    /// The worktree path the agent runs in.
    pub cwd: &'a Path,
    /// The worktree registry's stored id (`claude_session_id` / `codex_thread_id`).
    pub registry_id: Option<&'a str>,
    /// The user asked for a fresh conversation.
    pub fresh: bool,
    /// Claude's projects dir; `None` → [`crate::transfer::claude_projects_dir`].
    pub projects_dir: Option<&'a Path>,
}

/// How to launch the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumeDecision {
    /// Resume this conversation.
    Resume { cli: CliKind, id: String },
    /// Continue the newest conversation in the worktree.
    Continue,
    /// Start a new conversation.
    Fresh,
}

fn cli_of(agent: &str) -> Option<CliKind> {
    match agent {
        "claude" => Some(CliKind::Claude),
        "codex" => Some(CliKind::Codex),
        _ => None,
    }
}

fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ResumeStore {
    /// Open (creating if needed) the store at `path`, on the system clock.
    pub fn open(path: &Path) -> Result<Self, String> {
        Self::open_with_clock(path, system_now_ms)
    }

    /// Open the store at `path` with an injected millisecond clock.
    pub fn open_with_clock(
        path: &Path,
        now_ms: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let db = LocalStore::open(&path.to_string_lossy())
            .map_err(|e| format!("open resume db {}: {e}", path.display()))?;
        Ok(Self::with_db(Some(db), Box::new(now_ms)))
    }

    /// A store that never fails to construct: the DB at `path`, else an in-memory
    /// DB (records last for this process only), else none at all. Each fallback is
    /// logged.
    pub fn open_or_degraded(path: &Path) -> Self {
        match Self::open(path) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("herdr: {e}; resume records will not survive a restart");
                let db = LocalStore::open(":memory:")
                    .map_err(|e| log::warn!("herdr: in-memory resume db failed: {e}"))
                    .ok();
                Self::with_db(db, Box::new(system_now_ms))
            }
        }
    }

    fn with_db(db: Option<LocalStore>, now_ms: Box<dyn Fn() -> i64 + Send + Sync>) -> Self {
        ResumeStore {
            db: Mutex::new(db),
            now_ms,
            skip: Mutex::new(None),
        }
    }

    /// Install the filter of keys `observe` skips (default: skip nothing).
    pub fn set_skip_filter(&self, f: SkipFilter) {
        *self.skip.lock().unwrap() = Some(f);
    }

    /// Whether the latest observation of `key` saw its agent gone from the pane
    /// (the user quit it): such a session is not owed a resume.
    pub fn agent_quit(&self, key: &SessionKey) -> bool {
        self.with_db_do("get", |db| db.resume_get(&key.project, &key.worktree))
            .flatten()
            .is_some_and(|row| !row.agent_present)
    }

    /// Persist whether the user terminated `key`. Unlike the rest of the store this
    /// is not best-effort: a terminate must never silently fail to stick, so a
    /// missing DB or a failed write is an `Err`.
    pub fn set_terminated(&self, key: &SessionKey, terminated: bool) -> Result<(), String> {
        let db = self.db.lock().unwrap();
        let db = db.as_ref().ok_or("resume store unavailable")?;
        db.set_terminated(&key.project, &key.worktree, terminated)
            .map_err(|e| format!("persist terminated flag for {key}: {e}"))
    }

    /// Whether the user terminated `key` (`false` without a DB or on a read error).
    pub fn is_terminated(&self, key: &SessionKey) -> bool {
        self.with_db_do("is_terminated", |db| {
            db.is_terminated(&key.project, &key.worktree)
        })
        .unwrap_or(false)
    }

    /// The terminated worktree names of `project` (empty on failure).
    pub fn terminated_in(&self, project: &str) -> HashSet<String> {
        self.with_db_do("terminated_in", |db| db.terminated_in(project))
            .map(|names| names.into_iter().collect())
            .unwrap_or_default()
    }

    /// Whether recovery must leave `key` down: its agent was quit, or the user
    /// terminated the session.
    pub fn skip_resume(&self, key: &SessionKey) -> bool {
        self.agent_quit(key) || self.is_terminated(key)
    }

    /// Run `f` against the DB, if there is one; errors are logged and dropped.
    fn with_db_do<T>(
        &self,
        what: &str,
        f: impl FnOnce(&LocalStore) -> keitora_tasks::Result<T>,
    ) -> Option<T> {
        let db = self.db.lock().unwrap();
        let db = db.as_ref()?;
        match f(db) {
            Ok(v) => Some(v),
            Err(e) => {
                log::debug!("herdr: resume store {what} failed: {e}");
                None
            }
        }
    }

    /// Record what herdr reports for each `(key, pane)` (see [`observation_pairs`]).
    ///
    /// A pane with a recorded session stores its id when the session is a herdr
    /// id for claude/codex (`source == "herdr:<agent>"`, `kind == id`, and a safe
    /// id for claude). A pane without one only refreshes whether the agent is still
    /// running, on an existing record.
    pub fn observe(&self, obs: &[(SessionKey, PaneInfo)]) {
        let skip = self.skip.lock().unwrap().clone();
        let now = (self.now_ms)();
        for (key, pane) in obs {
            if skip.as_ref().is_some_and(|f| f(key)) {
                continue;
            }
            let present = pane.agent.is_some();
            match &pane.agent_session {
                Some(s) => {
                    let Some(cli) = cli_of(&s.agent) else {
                        continue;
                    };
                    if s.kind != AgentSessionKind::Id || s.source != format!("herdr:{}", s.agent) {
                        continue;
                    }
                    if !is_safe_session_id(&s.value) {
                        log::debug!("herdr: ignoring unsafe session id for {key}");
                        continue;
                    }
                    self.with_db_do("observe", |db| {
                        db.resume_observe(
                            &key.project,
                            &key.worktree,
                            cli,
                            Some(&s.value),
                            &s.source,
                            present,
                            now,
                        )
                    });
                }
                None => {
                    // Only an existing record is refreshed; its cli is the one known
                    // for the key.
                    self.with_db_do("observe", |db| {
                        let Some(row) = db.resume_get(&key.project, &key.worktree)? else {
                            return Ok(());
                        };
                        db.resume_observe(
                            &key.project,
                            &key.worktree,
                            row.cli,
                            None,
                            "",
                            present,
                            now,
                        )
                    });
                }
            }
        }
    }

    /// Decide how to launch `key`'s agent. In order:
    /// 1. `fresh` → clear the record, start fresh;
    /// 2. the stored id for the same cli, if its transcript exists (claude);
    /// 3. the registry's id, with the same checks, unless the row cleared it;
    /// 4. claude: continue when the worktree has any transcript, else fresh;
    ///    codex: continue (`resume --last` starts fresh when there is nothing);
    /// 5. fresh.
    pub fn resolve(&self, key: &SessionKey, ctx: &ResolveCtx) -> ResumeDecision {
        if ctx.fresh {
            self.clear(key);
            return ResumeDecision::Fresh;
        }
        // The worktree's Claude transcript dir, if it can be located.
        let claude_dir = || -> Option<(std::path::PathBuf, String)> {
            let projects = match ctx.projects_dir {
                Some(p) => p.to_path_buf(),
                None => crate::transfer::claude_projects_dir().ok()?,
            };
            let encoded = crate::transfer::encoded_cwd_dir_name(ctx.cwd).ok()?;
            Some((projects, encoded))
        };
        let usable = |id: &str| -> bool {
            if !is_safe_session_id(id) {
                return false;
            }
            match ctx.cli {
                CliKind::Claude => claude_dir().is_some_and(|(projects, encoded)| {
                    crate::transfer::claude_session_file_exists(&projects, &encoded, id)
                }),
                // Codex rollouts live in a date tree; a pruned thread is caught by the
                // launch's early-exit fallback instead.
                CliKind::Codex => true,
            }
        };

        let stored = self
            .with_db_do("get", |db| db.resume_get(&key.project, &key.worktree))
            .flatten();
        // An id cleared for this cli (a fresh start, or a resume that failed) is
        // not accepted from the registry either.
        let mut cleared: Option<String> = None;
        if let Some(row) = stored {
            if row.cli == ctx.cli {
                if let Some(id) = row.session_id.filter(|id| usable(id)) {
                    return ResumeDecision::Resume { cli: ctx.cli, id };
                }
                cleared = row.cleared_id;
            }
        }
        if let Some(id) = ctx
            .registry_id
            .filter(|id| usable(id) && cleared.as_deref() != Some(*id))
        {
            return ResumeDecision::Resume {
                cli: ctx.cli,
                id: id.to_string(),
            };
        }
        match ctx.cli {
            CliKind::Claude => {
                let has_any = claude_dir().is_some_and(|(projects, encoded)| {
                    crate::transfer::claude_project_has_sessions(&projects, &encoded)
                });
                if has_any {
                    ResumeDecision::Continue
                } else {
                    ResumeDecision::Fresh
                }
            }
            CliKind::Codex => ResumeDecision::Continue,
        }
    }

    /// Forget `key`'s session id (a fresh start); the cleared id is never
    /// accepted again.
    pub fn clear(&self, key: &SessionKey) {
        self.with_db_do("clear", |db| db.resume_clear(&key.project, &key.worktree));
    }

    /// Overwrite `key`'s record with `session_id` for `cli`, whatever was stored
    /// (no newest-wins check, no cleared-id guard): a transfer has decided what
    /// the worktree resumes. A missing DB or a failed write is an `Err` the
    /// caller reports (it treats the write as best-effort).
    pub fn replace(&self, key: &SessionKey, cli: CliKind, session_id: &str) -> Result<(), String> {
        let now = (self.now_ms)();
        let db = self.db.lock().unwrap();
        let db = db.as_ref().ok_or("resume store unavailable")?;
        db.resume_replace(
            &key.project,
            &key.worktree,
            cli,
            session_id,
            "keitora:transfer",
            now,
        )
        .map_err(|e| format!("replace resume record for {key}: {e}"))
    }
}

/// Session ids are passed to the agent as bare arguments; accept only the
/// conservative id alphabet.
fn is_safe_session_id(id: &str) -> bool {
    is_safe_claude_session_id(id)
}

/// Pair each keymap entry with the live pane it maps to: same `pane_id` **and**
/// the same normalized cwd (a pane id can be reused for a different worktree after
/// a restart). Panes without a cwd are skipped.
pub fn observation_pairs(
    entries: &[(SessionKey, HerdrPane)],
    panes: &[PaneInfo],
) -> Vec<(SessionKey, PaneInfo)> {
    entries
        .iter()
        .filter_map(|(key, entry)| {
            let want = held_cwd(&entry.cwd);
            panes
                .iter()
                .find(|p| {
                    p.pane_id == entry.pane_id
                        && p.cwd.as_deref().is_some_and(|c| normalize_cwd(c) == want)
                })
                .map(|p| (key.clone(), p.clone()))
        })
        .collect()
}

/// The observations that may be recorded. A pane without an agent says the
/// user quit it only on the server the keymap was reconciled against
/// (`same_server`): on another one (restored, or not reconciled yet) it is a
/// bare shell whose reused id and cwd happen to match, and recording it would
/// mark a running session quit. Panes with an agent are kept either way.
pub fn trusted_observations(
    pairs: Vec<(SessionKey, PaneInfo)>,
    same_server: bool,
) -> Vec<(SessionKey, PaneInfo)> {
    if same_server {
        return pairs;
    }
    pairs
        .into_iter()
        .filter(|(_, p)| p.agent.is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::herdr::cli::AgentSession;
    use keitora_tasks::ResumeRow;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicI64, Ordering};

    fn key(w: &str) -> SessionKey {
        SessionKey {
            project: "p".into(),
            worktree: w.into(),
        }
    }

    /// A store over a temp DB with a clock that ticks 10 ms per call.
    struct Fixture {
        store: ResumeStore,
        tmp: tempfile::TempDir,
        /// A real worktree dir (the cwd) and the Claude projects dir.
        cwd: PathBuf,
        projects: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let clock = Arc::new(AtomicI64::new(0));
            let store = ResumeStore::open_with_clock(&tmp.path().join("resume.db"), move || {
                clock.fetch_add(10, Ordering::SeqCst) + 10
            })
            .unwrap();
            let cwd = tmp.path().join("wt");
            std::fs::create_dir_all(&cwd).unwrap();
            let projects = tmp.path().join("projects");
            Fixture {
                store,
                tmp,
                cwd,
                projects,
            }
        }

        fn transcript_dir(&self) -> PathBuf {
            self.projects
                .join(crate::transfer::encoded_cwd_dir_name(&self.cwd).unwrap())
        }

        fn add_transcript(&self, id: &str) {
            std::fs::create_dir_all(self.transcript_dir()).unwrap();
            std::fs::write(self.transcript_dir().join(format!("{id}.jsonl")), "{}").unwrap();
        }

        fn ctx<'a>(&'a self, cli: CliKind, registry_id: Option<&'a str>) -> ResolveCtx<'a> {
            ResolveCtx {
                cli,
                cwd: &self.cwd,
                registry_id,
                fresh: false,
                projects_dir: Some(&self.projects),
            }
        }

        fn row(&self, w: &str) -> Option<ResumeRow> {
            let db = self.store.db.lock().unwrap();
            db.as_ref().unwrap().resume_get("p", w).unwrap()
        }

        fn observe_session(&self, w: &str, agent: &str, id: &str) {
            self.store
                .observe(&[(key(w), pane_with(Some(agent), Some(session(agent, id))))]);
        }
    }

    fn session(agent: &str, id: &str) -> AgentSession {
        AgentSession {
            source: format!("herdr:{agent}"),
            agent: agent.into(),
            kind: AgentSessionKind::Id,
            value: id.into(),
        }
    }

    fn pane_with(agent: Option<&str>, s: Option<AgentSession>) -> PaneInfo {
        PaneInfo {
            workspace_id: "w1".into(),
            pane_id: "w1:p1".into(),
            terminal_id: "t".into(),
            cwd: Some("/wt".into()),
            agent: agent.map(str::to_string),
            agent_session: s,
            ..Default::default()
        }
    }

    // ---- resolve precedence ----

    #[test]
    fn resolve_store_hit_resumes() {
        let f = Fixture::new();
        f.observe_session("a", "claude", "sess-a");
        f.add_transcript("sess-a");
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Claude, Some("reg-1"))),
            ResumeDecision::Resume {
                cli: CliKind::Claude,
                id: "sess-a".into()
            }
        );
    }

    #[test]
    fn resolve_store_id_without_transcript_falls_back_to_registry_id() {
        let f = Fixture::new();
        f.observe_session("a", "claude", "sess-gone");
        f.add_transcript("reg-1");
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Claude, Some("reg-1"))),
            ResumeDecision::Resume {
                cli: CliKind::Claude,
                id: "reg-1".into()
            }
        );
    }

    #[test]
    fn resolve_skips_a_registry_id_the_row_cleared() {
        let f = Fixture::new();
        f.store
            .replace(&key("a"), CliKind::Codex, "thr-failed")
            .unwrap();
        f.store.clear(&key("a"));
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Codex, Some("thr-failed"))),
            ResumeDecision::Continue
        );
        // Another registry id is still honoured.
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Codex, Some("thr-other"))),
            ResumeDecision::Resume {
                cli: CliKind::Codex,
                id: "thr-other".into()
            }
        );
    }

    #[test]
    fn resolve_registry_without_transcript_continues_when_any_transcript_exists() {
        let f = Fixture::new();
        f.add_transcript("something-else");
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Claude, Some("reg-gone"))),
            ResumeDecision::Continue
        );
        // An unsafe registry id is never resumed, even with a matching file name.
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Claude, Some("../x"))),
            ResumeDecision::Continue
        );
    }

    #[test]
    fn resolve_empty_or_missing_transcript_dir_is_fresh() {
        let f = Fixture::new();
        assert_eq!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Fresh,
            "missing dir"
        );
        std::fs::create_dir_all(f.transcript_dir()).unwrap();
        assert_eq!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Fresh,
            "empty dir"
        );
        // A cwd that no longer exists cannot be encoded: no transcripts.
        let gone = f.tmp.path().join("gone");
        let ctx = ResolveCtx {
            cwd: &gone,
            ..f.ctx(CliKind::Claude, None)
        };
        assert_eq!(f.store.resolve(&key("a"), &ctx), ResumeDecision::Fresh);
    }

    #[test]
    fn resolve_fresh_clears_the_record() {
        let f = Fixture::new();
        f.observe_session("a", "claude", "sess-a");
        f.add_transcript("sess-a");
        let ctx = ResolveCtx {
            fresh: true,
            ..f.ctx(CliKind::Claude, None)
        };
        assert_eq!(f.store.resolve(&key("a"), &ctx), ResumeDecision::Fresh);
        let row = f.row("a").unwrap();
        assert_eq!(row.session_id, None);
        assert_eq!(row.cleared_id.as_deref(), Some("sess-a"));
        // The cleared id is never re-adopted from a stale observation.
        f.observe_session("a", "claude", "sess-a");
        assert_eq!(f.row("a").unwrap().session_id, None);
    }

    #[test]
    fn store_replace_then_resolve_returns_resume_codex() {
        let f = Fixture::new();
        f.observe_session("a", "claude", "sess-a");
        f.store.replace(&key("a"), CliKind::Codex, "thr-1").unwrap();
        let row = f.row("a").unwrap();
        assert_eq!(row.cli, CliKind::Codex);
        assert_eq!(row.session_id.as_deref(), Some("thr-1"));
        assert_eq!(row.source, "keitora:transfer");
        assert!(row.agent_present);
        assert_eq!(row.cleared_id, None);
        // The stored row outranks the registry id.
        assert_eq!(
            f.store
                .resolve(&key("a"), &f.ctx(CliKind::Codex, Some("other"))),
            ResumeDecision::Resume {
                cli: CliKind::Codex,
                id: "thr-1".into()
            }
        );
    }

    #[test]
    fn store_replace_without_a_db_is_an_error() {
        let store = ResumeStore::with_db(None, Box::new(|| 0));
        assert!(store.replace(&key("a"), CliKind::Codex, "thr-1").is_err());
    }

    #[test]
    fn store_clear_then_resolve_never_returns_the_cleared_claude_id() {
        let f = Fixture::new();
        f.observe_session("a", "claude", "sess-a");
        f.add_transcript("sess-a");
        f.store.clear(&key("a"));
        // Codex with no registry id continues (the strict launch never asks).
        assert_eq!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Codex, None)),
            ResumeDecision::Continue
        );
        // The cleared claude id is not resumed, nor re-adopted from a stale
        // observation.
        assert_ne!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Resume {
                cli: CliKind::Claude,
                id: "sess-a".into()
            }
        );
        f.observe_session("a", "claude", "sess-a");
        assert_eq!(f.row("a").unwrap().session_id, None);
    }

    #[test]
    fn resolve_ignores_a_row_for_another_cli() {
        let f = Fixture::new();
        f.observe_session("a", "codex", "thread-1");
        f.add_transcript("thread-1");
        assert_eq!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Continue,
            "the codex id is not resumed by claude"
        );
    }

    #[test]
    fn resolve_codex_store_id_resumes_without_a_file_check() {
        let f = Fixture::new();
        f.observe_session("a", "codex", "thread-1");
        assert_eq!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Codex, None)),
            ResumeDecision::Resume {
                cli: CliKind::Codex,
                id: "thread-1".into()
            }
        );
    }

    #[test]
    fn resolve_codex_without_id_continues() {
        let f = Fixture::new();
        assert_eq!(
            f.store.resolve(&key("a"), &f.ctx(CliKind::Codex, None)),
            ResumeDecision::Continue
        );
    }

    // ---- observe ----

    #[test]
    fn observe_upserts_claude_and_codex_sessions() {
        let f = Fixture::new();
        f.observe_session("a", "claude", "c-1");
        f.observe_session("b", "codex", "x-1");
        let a = f.row("a").unwrap();
        assert_eq!(
            (a.cli, a.session_id.as_deref(), a.source.as_str()),
            (CliKind::Claude, Some("c-1"), "herdr:claude")
        );
        let b = f.row("b").unwrap();
        assert_eq!(
            (b.cli, b.session_id.as_deref(), b.source.as_str()),
            (CliKind::Codex, Some("x-1"), "herdr:codex")
        );
        // Newer observation replaces the id.
        f.observe_session("a", "claude", "c-2");
        assert_eq!(f.row("a").unwrap().session_id.as_deref(), Some("c-2"));
    }

    #[test]
    fn observe_ignores_unknown_agents_unsafe_ids_and_foreign_sources() {
        let f = Fixture::new();
        f.observe_session("a", "gemini", "g-1");
        f.observe_session("b", "claude", "bad id; rm -rf");
        let mut foreign = session("claude", "c-1");
        foreign.source = "user:claude".into();
        let mut path = session("claude", "/x/c.jsonl");
        path.kind = AgentSessionKind::Path;
        f.store.observe(&[
            (key("c"), pane_with(Some("claude"), Some(foreign))),
            (key("d"), pane_with(Some("claude"), Some(path))),
        ]);
        for w in ["a", "b", "c", "d"] {
            assert_eq!(f.row(w), None, "{w} must not be recorded");
        }
    }

    #[test]
    fn observe_skips_filtered_keys() {
        let f = Fixture::new();
        f.store
            .set_skip_filter(Arc::new(|k: &SessionKey| k.worktree == "pending"));
        f.observe_session("pending", "claude", "c-1");
        f.observe_session("normal", "claude", "c-2");
        assert_eq!(f.row("pending"), None);
        assert!(f.row("normal").is_some());
    }

    #[test]
    fn agent_quit_reads_the_latest_presence() {
        let f = Fixture::new();
        assert!(!f.store.agent_quit(&key("a")), "no record: not quit");
        f.observe_session("a", "claude", "c-1");
        assert!(!f.store.agent_quit(&key("a")));
        f.store.observe(&[(key("a"), pane_with(None, None))]);
        assert!(f.store.agent_quit(&key("a")), "agent left its shell");
    }

    #[test]
    fn terminated_flag_round_trips_and_feeds_skip_resume() {
        let f = Fixture::new();
        assert!(!f.store.is_terminated(&key("a")));
        assert!(!f.store.skip_resume(&key("a")));
        f.store.set_terminated(&key("a"), true).unwrap();
        assert!(f.store.is_terminated(&key("a")));
        assert!(f.store.skip_resume(&key("a")), "terminated is skipped");
        assert_eq!(f.store.terminated_in("p"), HashSet::from(["a".to_string()]));
        assert!(f.store.terminated_in("other").is_empty());
        f.store.set_terminated(&key("a"), false).unwrap();
        assert!(!f.store.skip_resume(&key("a")));
        assert!(f.store.terminated_in("p").is_empty());
        // A quit agent is skipped without any flag.
        f.observe_session("b", "claude", "c-1");
        f.store.observe(&[(key("b"), pane_with(None, None))]);
        assert!(f.store.skip_resume(&key("b")));
    }

    #[test]
    fn set_terminated_without_a_db_is_an_error() {
        let store = ResumeStore::with_db(None, Box::new(|| 0));
        assert!(store.set_terminated(&key("a"), true).is_err());
        assert!(store.set_terminated(&key("a"), false).is_err());
        assert!(!store.is_terminated(&key("a")));
        assert!(store.terminated_in("p").is_empty());
    }

    #[test]
    fn observe_agent_present_follows_the_pane() {
        let f = Fixture::new();
        f.store
            .observe(&[(key("a"), pane_with(None, Some(session("claude", "c-1"))))]);
        assert!(
            !f.row("a").unwrap().agent_present,
            "a shell holding the session ref"
        );
        f.observe_session("a", "claude", "c-1");
        assert!(f.row("a").unwrap().agent_present);
        // No session reported: only presence of an existing record changes.
        f.store.observe(&[(key("a"), pane_with(None, None))]);
        let row = f.row("a").unwrap();
        assert!(!row.agent_present);
        assert_eq!(row.session_id.as_deref(), Some("c-1"));
        f.store
            .observe(&[(key("new"), pane_with(Some("claude"), None))]);
        assert_eq!(f.row("new"), None, "no record is created without an id");
    }

    #[test]
    fn degraded_store_still_resolves_from_transcripts() {
        let f = Fixture::new();
        let store = ResumeStore::with_db(None, Box::new(|| 0));
        store.observe(&[(
            key("a"),
            pane_with(Some("claude"), Some(session("claude", "c-1"))),
        )]);
        assert_eq!(
            store.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Fresh
        );
        f.add_transcript("c-1");
        assert_eq!(
            store.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Continue
        );
        // An unopenable path degrades instead of failing.
        let blocker = f.tmp.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let s = ResumeStore::open_or_degraded(&blocker.join("resume.db"));
        s.observe(&[(
            key("a"),
            pane_with(Some("claude"), Some(session("claude", "c-1"))),
        )]);
        assert_eq!(
            s.resolve(&key("a"), &f.ctx(CliKind::Claude, None)),
            ResumeDecision::Resume {
                cli: CliKind::Claude,
                id: "c-1".into()
            },
            "the in-memory fallback still records for this process"
        );
    }

    // ---- observation_pairs ----

    fn entry(pane_id: &str, cwd: &str) -> HerdrPane {
        HerdrPane {
            workspace_id: "w".into(),
            pane_id: pane_id.into(),
            terminal_id: "t".into(),
            gen: 1,
            cwd: cwd.into(),
            server_generation: Default::default(),
        }
    }

    fn live(pane_id: &str, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.into(),
            cwd: cwd.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn observation_pairs_match_on_pane_id_and_cwd() {
        let entries = vec![
            (key("a"), entry("w1:p1", "/repos/a")),
            (key("b"), entry("w2:p1", "/repos/b")),
            (key("c"), entry("w3:p1", "/repos/c")),
        ];
        let panes = vec![
            live("w1:p1", Some("/repos/a/")),
            // Same id as b's entry but another worktree's pane (id reused).
            live("w2:p1", Some("/repos/other")),
            // c's pane reports no cwd.
            live("w3:p1", None),
        ];
        let pairs = observation_pairs(&entries, &panes);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, key("a"));
        assert_eq!(pairs[0].1.pane_id, "w1:p1");
        assert!(observation_pairs(&entries, &[]).is_empty());
    }

    #[test]
    fn an_agentless_pane_counts_only_on_the_reconciled_server() {
        let shell = live("w1:p1", Some("/repos/a"));
        let agent = PaneInfo {
            agent: Some("claude".into()),
            ..live("w2:p1", Some("/repos/b"))
        };
        let pairs = vec![(key("a"), shell), (key("b"), agent)];
        assert_eq!(trusted_observations(pairs.clone(), true), pairs);
        let other = trusted_observations(pairs, false);
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].0, key("b"));
    }

    /// herdr reports a pane's directory with its symlinks resolved; the entry
    /// holds the worktree path as Keitora was given it.
    #[cfg(unix)]
    #[test]
    fn observation_pairs_match_a_worktree_reached_through_a_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("wt");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("ln");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let entries = vec![(key("a"), entry("w1:p1", &link.to_string_lossy()))];
        let panes = vec![live("w1:p1", Some(&real.to_string_lossy()))];
        assert_eq!(observation_pairs(&entries, &panes).len(), 1);
        let panes = vec![live("w1:p1", Some("/repos/other"))];
        assert!(observation_pairs(&entries, &panes).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn observation_pairs_match_windows_verbatim_and_case_variants() {
        let entries = vec![(key("a"), entry("w1:p1", r"D:\Repos\Proj\WT"))];
        let panes = vec![live("w1:p1", Some(r"\\?\d:\repos\proj\wt\"))];
        assert_eq!(observation_pairs(&entries, &panes).len(), 1);
        let panes = vec![live("w1:p1", Some("D:/repos/proj/wt"))];
        assert_eq!(observation_pairs(&entries, &panes).len(), 1);
        let panes = vec![live("w1:p1", Some(r"D:\repos\proj\other"))];
        assert!(observation_pairs(&entries, &panes).is_empty());
    }
}
