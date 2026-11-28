//! Finds an agent's transcript for a finished turn and reads that turn's reply from
//! it ([`AgentTranscripts`], the engine's [`TranscriptSource`]).
//!
//! Lookups run off the status loop (the engine's spawner) and only read: a file is
//! opened, its tail read and closed. Directory scans are bounded; a Codex id's
//! rollout (or a recent miss) is cached in a bounded [`TranscriptIndex`].
//!
//! - **Claude**: `<claude projects>/<encoded worktree>/`: herdr's transcript path,
//!   else herdr's session id (`<id>.jsonl`), then the newest non-`agent-*` `.jsonl`
//!   written since the delivery (less [`FRESH_SLACK_MS`]) when herdr's file has no
//!   reply for the turn (`/clear` or a new session moved on to another file).
//! - **Codex**: `<codex home>/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl` found by
//!   herdr's thread id (the delivery's day and the day before, then the newest dated
//!   dirs, at most [`CODEX_WALK_MAX`] `read_dir` calls), else herdr's path, else the
//!   newest rollout in those same dirs (a rollout sits in its session's start date,
//!   however long ago) written since the delivery whose `session_meta.cwd` is this
//!   worktree.
//! - **Antigravity**: `<agy state>/conversations/<id>.db` for herdr's session id, read
//!   read-only (see `keitora_tasks::transcript::agy`). No fallback: a guess could be
//!   another workspace's conversation.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Days, Local, NaiveDate, Utc};
use keitora_tasks::transcript::{self, miss, TurnWindow};
use keitora_tasks::CliKind;
use serde_json::Value;

use crate::backend::SessionKey;
use crate::engine::turns::{ReplyQuery, TranscriptSource, TurnReply};

/// How much older than the delivery a fallback pick may be (mtime granularity and
/// clock skew).
pub const FRESH_SLACK_MS: i64 = 5_000;
/// Most `read_dir` calls one Codex id search makes.
pub const CODEX_WALK_MAX: usize = 400;
/// Most bytes of a rollout's first line read for its `session_meta`.
const SESSION_META_MAX: u64 = 1024 * 1024;
/// Most Codex thread ids a [`TranscriptIndex`] remembers.
pub const INDEX_MAX: usize = 64;
/// How long a [`TranscriptIndex`] remembers that a thread id has no rollout.
pub const INDEX_MISS_TTL: Duration = Duration::from_secs(10);

/// Where each agent keeps its transcripts; `None` when unknown.
#[derive(Clone, Debug, Default)]
pub struct TranscriptDirs {
    /// `<claude config>/projects`.
    pub claude_projects: Option<PathBuf>,
    /// Codex's home (`sessions/` lives under it).
    pub codex_home: Option<PathBuf>,
    /// Antigravity's per-conversation files (`<id>.db`).
    pub agy_conversations: Option<PathBuf>,
}

impl TranscriptDirs {
    /// The app's own: `CLAUDE_CONFIG_DIR` / `CODEX_HOME` when set, else under home.
    pub fn from_env() -> Self {
        let home = dirs::home_dir();
        let codex_env = std::env::var("CODEX_HOME").ok();
        TranscriptDirs {
            claude_projects: crate::transfer::claude_projects_dir().ok(),
            codex_home: crate::codex_env::codex_home_dir(codex_env.as_deref(), home.as_deref())
                .map(|(p, _)| p),
            agy_conversations: home.map(|h| keitora_tasks::agy_state_dir(&h).join("conversations")),
        }
    }
}

/// herdr's record of the agent session: an id or a transcript path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRef<'a> {
    Id(&'a str),
    Path(&'a Path),
}

impl<'a> SessionRef<'a> {
    fn of(q: &'a ReplyQuery) -> Option<Self> {
        let value = q.agent_session.as_deref().filter(|v| !v.is_empty())?;
        match q.agent_session_kind.as_deref() {
            Some("path") => Some(SessionRef::Path(Path::new(value))),
            Some("id") => Some(SessionRef::Id(value)),
            _ => None,
        }
    }

    /// The id, when it is safe to put in a file name.
    fn safe_id(self) -> Option<&'a str> {
        match self {
            SessionRef::Id(id) if keitora_tasks::is_safe_session_id(id) => Some(id),
            _ => None,
        }
    }
}

/// Codex thread id -> rollout file (dropped when the file disappears), or a recent
/// miss (kept for [`INDEX_MISS_TTL`]). Holds at most [`INDEX_MAX`] ids; the least
/// recently used goes first. Its lock is held only to read or write an entry, never
/// across a directory walk.
#[derive(Default)]
pub struct TranscriptIndex {
    codex: Mutex<IndexMap>,
}

#[derive(Default)]
struct IndexMap {
    entries: HashMap<String, IndexEntry>,
    /// Bumped on every use; an entry's `used` is its last value.
    tick: u64,
}

struct IndexEntry {
    hit: IndexHit,
    used: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum IndexHit {
    Found(PathBuf),
    Missing { until: Instant },
}

impl TranscriptIndex {
    /// The cached answer for `id`: `Some(Some(path))` a rollout, `Some(None)` a miss
    /// still within its TTL, `None` nothing usable (a stale entry is dropped).
    fn lookup(&self, id: &str, now: Instant) -> Option<Option<PathBuf>> {
        let hit = {
            let mut map = self.codex.lock().unwrap();
            map.tick += 1;
            let tick = map.tick;
            let entry = map.entries.get_mut(id)?;
            entry.used = tick;
            entry.hit.clone()
        };
        match hit {
            IndexHit::Found(p) if p.is_file() => Some(Some(p)),
            IndexHit::Missing { until } if now < until => Some(None),
            stale => {
                self.drop_if_unchanged(id, &stale);
                None
            }
        }
    }

    /// Drop `id`'s entry if it is still the `seen` one: a lookup that found it stale
    /// never removes an answer stored since.
    fn drop_if_unchanged(&self, id: &str, seen: &IndexHit) {
        let mut map = self.codex.lock().unwrap();
        if map.entries.get(id).is_some_and(|e| e.hit == *seen) {
            map.entries.remove(id);
        }
    }

    /// Remember `id`'s rollout, or that it has none, evicting the least recently
    /// used id when full.
    fn store(&self, id: &str, found: Option<PathBuf>, now: Instant) {
        let hit = match found {
            Some(p) => IndexHit::Found(p),
            None => IndexHit::Missing {
                until: now + INDEX_MISS_TTL,
            },
        };
        let mut map = self.codex.lock().unwrap();
        if !map.entries.contains_key(id) && map.entries.len() >= INDEX_MAX {
            let oldest = map
                .entries
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                map.entries.remove(&k);
            }
        }
        map.tick += 1;
        let used = map.tick;
        map.entries.insert(id.to_string(), IndexEntry { hit, used });
    }
}

/// The worktree directory of a session key.
pub type WorktreeLookup = Box<dyn Fn(&SessionKey) -> Option<PathBuf> + Send + Sync>;

/// The engine's transcript reader. See the module docs.
pub struct AgentTranscripts {
    dirs: TranscriptDirs,
    worktree: WorktreeLookup,
    index: TranscriptIndex,
}

impl AgentTranscripts {
    pub fn new(dirs: TranscriptDirs, worktree: WorktreeLookup) -> Self {
        AgentTranscripts {
            dirs,
            worktree,
            index: TranscriptIndex::default(),
        }
    }

    /// Over the app's own dirs, resolving worktrees through the project registry.
    pub fn from_env() -> Self {
        Self::new(
            TranscriptDirs::from_env(),
            Box::new(|key| {
                crate::project_stores::registry()
                    .by_name(&key.project)
                    .map(|pc| pc.worktree_path(&key.worktree))
            }),
        )
    }

    /// The transcript files for `q`'s agent, to try in order, or a [`miss`] reason.
    fn locate(&self, cli: CliKind, q: &ReplyQuery) -> Result<Vec<PathBuf>, &'static str> {
        let session = SessionRef::of(q);
        let found = match cli {
            CliKind::Antigravity => {
                let dir = self
                    .dirs
                    .agy_conversations
                    .as_deref()
                    .ok_or(miss::NOT_FOUND)?;
                locate_agy(dir, session).into_iter().collect()
            }
            CliKind::Claude => {
                let worktree = (self.worktree)(&q.key).ok_or(miss::NOT_FOUND)?;
                let projects = self
                    .dirs
                    .claude_projects
                    .as_deref()
                    .ok_or(miss::NOT_FOUND)?;
                locate_claude(projects, &worktree, session, q.delivered_at_ms)
            }
            CliKind::Codex => {
                let worktree = (self.worktree)(&q.key).ok_or(miss::NOT_FOUND)?;
                let home = self.dirs.codex_home.as_deref().ok_or(miss::NOT_FOUND)?;
                locate_codex(home, &worktree, session, q.delivered_at_ms, &self.index)
                    .into_iter()
                    .collect()
            }
        };
        if found.is_empty() {
            return Err(miss::NOT_FOUND);
        }
        Ok(found)
    }
}

impl TranscriptSource for AgentTranscripts {
    fn reply(&self, q: &ReplyQuery) -> Result<TurnReply, &'static str> {
        let cli = q
            .agent
            .as_deref()
            .and_then(CliKind::from_herdr_agent)
            .ok_or(miss::NO_PROVIDER_SUPPORT)?;
        let window = TurnWindow::new(q.delivered_at_ms, q.detected_at_ms);
        let mut first_miss = None;
        for path in self.locate(cli, q)? {
            match transcript::read_turn_reply(cli, &path, window) {
                Ok(r) => return Ok(TurnReply { text: r.text }),
                Err(why) => {
                    log::debug!(
                        "transcript: no reply for @{} in {} ({why})",
                        q.key.worktree,
                        path.display()
                    );
                    first_miss.get_or_insert(why);
                }
            }
        }
        Err(first_miss.unwrap_or(miss::NOT_FOUND))
    }
}

/// Antigravity's conversation file for herdr's session id: exactly
/// `<conversations>/<id>.db`, for a safe id only.
pub fn locate_agy(conversations: &Path, session: Option<SessionRef<'_>>) -> Option<PathBuf> {
    let id = session?.safe_id()?;
    let p = conversations.join(format!("{id}.db"));
    p.is_file().then_some(p)
}

/// Whether `path`'s mtime is at or after `delivered_at - FRESH_SLACK_MS`.
fn fresh_since(path: &Path, delivered_at: i64) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .is_ok_and(|t| fresh_mtime(t, delivered_at))
}

/// Whether `modified` is at or after `delivered_at - FRESH_SLACK_MS`.
fn fresh_mtime(modified: SystemTime, delivered_at: i64) -> bool {
    let ms = modified
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    ms >= delivered_at - FRESH_SLACK_MS
}

/// herdr's Claude transcript: its path (inside the worktree's project dir), else its
/// id's `<id>.jsonl` there.
fn claude_herdr_file(
    projects: &Path,
    encoded: &str,
    session: Option<SessionRef<'_>>,
) -> Option<PathBuf> {
    let dir = projects.join(encoded);
    let contained = |p: &Path| crate::transfer::claude_jsonl_contained(projects, p).is_ok();
    if let Some(SessionRef::Path(p)) = session {
        if let Ok(p) = crate::transfer::validate_jsonl_in_dir(&dir, p) {
            if contained(&p) {
                return Some(p);
            }
        }
    }
    let id = session.and_then(SessionRef::safe_id)?;
    let p = dir.join(format!("{id}.jsonl"));
    (crate::transfer::claude_session_file_exists(projects, encoded, id) && contained(&p))
        .then_some(p)
}

/// Whether `a` and `b` name the same file.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// The worktree's Claude transcripts, in the order to try them: herdr's (by path,
/// else id) when written since the delivery (less [`FRESH_SLACK_MS`]), then the
/// newest fresh non-`agent-*` transcript there (after `/clear` or a new session,
/// herdr's may be the previous file), then herdr's when it is older than that (it
/// can only say why there is no reply).
pub fn locate_claude(
    projects: &Path,
    worktree: &Path,
    session: Option<SessionRef<'_>>,
    delivered_at: i64,
) -> Vec<PathBuf> {
    let Ok(encoded) = crate::transfer::encoded_cwd_dir_name(worktree) else {
        return Vec::new();
    };
    let herdr = claude_herdr_file(projects, &encoded, session);
    let (fresh, stale) = match herdr {
        Some(p) if fresh_since(&p, delivered_at) => (Some(p), None),
        other => (None, other),
    };
    let newest = crate::transfer::pick_jsonl(&projects.join(&encoded)).filter(|p| {
        crate::transfer::claude_jsonl_contained(projects, p).is_ok() && fresh_since(p, delivered_at)
    });
    let mut out: Vec<PathBuf> = Vec::new();
    for p in [fresh, newest, stale].into_iter().flatten() {
        if !out.iter().any(|q| same_file(q, &p)) {
            out.push(p);
        }
    }
    out
}

/// The days whose `sessions/YYYY/MM/DD` dir may hold a rollout started around
/// `at_ms`: that day and the one before, by local and by UTC date.
fn codex_days(at_ms: i64) -> Vec<NaiveDate> {
    let Some(utc) = DateTime::<Utc>::from_timestamp_millis(at_ms) else {
        return Vec::new();
    };
    let mut days = Vec::new();
    for day in [utc.with_timezone(&Local).date_naive(), utc.date_naive()] {
        for d in [Some(day), day.checked_sub_days(Days::new(1))]
            .into_iter()
            .flatten()
        {
            if !days.contains(&d) {
                days.push(d);
            }
        }
    }
    days
}

fn day_dir(sessions: &Path, d: NaiveDate) -> PathBuf {
    sessions
        .join(format!("{:04}", d.year()))
        .join(format!("{:02}", d.month()))
        .join(format!("{:02}", d.day()))
}

fn is_rollout(name: &str) -> bool {
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// The rollout named for `id` directly inside `dir`.
fn rollout_in(dir: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{id}.jsonl");
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        let name = name.to_str()?;
        (is_rollout(name) && name.ends_with(&suffix) && e.path().is_file()).then(|| e.path())
    })
}

/// Numeric child dirs of `dir`, newest (largest) first.
fn numeric_dirs_desc(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let n: u32 = e.file_name().to_str()?.parse().ok()?;
            e.path().is_dir().then(|| (n, e.path()))
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    dirs.into_iter().map(|(_, p)| p).collect()
}

/// Visit `sessions/`'s day dirs: the given days first, then every dated dir newest
/// first, within [`CODEX_WALK_MAX`] `read_dir` calls (a visit counts as one). Stops
/// at the first visit that returns `Some`.
fn walk_codex_days<T>(
    sessions: &Path,
    days: &[NaiveDate],
    mut visit: impl FnMut(&Path) -> Option<T>,
) -> Option<T> {
    let mut budget = CODEX_WALK_MAX;
    let mut tried = Vec::new();
    for d in days {
        let dir = day_dir(sessions, *d);
        budget = budget.checked_sub(1)?;
        if let Some(t) = visit(&dir) {
            return Some(t);
        }
        tried.push(dir);
    }
    budget = budget.checked_sub(1)?;
    for year in numeric_dirs_desc(sessions) {
        budget = budget.checked_sub(1)?;
        for month in numeric_dirs_desc(&year) {
            budget = budget.checked_sub(1)?;
            for day in numeric_dirs_desc(&month) {
                if tried.contains(&day) {
                    continue;
                }
                budget = budget.checked_sub(1)?;
                if let Some(t) = visit(&day) {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// Search `sessions/` for the rollout of thread `id` (see [`walk_codex_days`]).
fn find_codex_rollout(sessions: &Path, id: &str, days: &[NaiveDate]) -> Option<PathBuf> {
    walk_codex_days(sessions, days, |dir| rollout_in(dir, id))
}

/// The `payload.cwd` of the `session_meta` that opens the rollout at `path`.
fn session_meta_cwd(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::new(f.take(SESSION_META_MAX))
        .read_until(b'\n', &mut line)
        .ok()?;
    let v: Value = serde_json::from_slice(&line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    v.pointer("/payload/cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn path_key(p: &str) -> String {
    keitora_tasks::path_key::normalize_path(p, cfg!(windows), keitora_tasks::path_key::FOLD_CASE)
}

/// The keys a rollout's recorded cwd may match for `worktree`: as given and canonical.
fn worktree_keys(worktree: &Path) -> Vec<String> {
    let mut keys = vec![path_key(&worktree.to_string_lossy())];
    if let Ok(c) = std::fs::canonicalize(worktree) {
        let k = path_key(&c.to_string_lossy());
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys
}

/// The worktree's Codex rollout: by herdr's thread id (cached), by herdr's path, else
/// the newest fresh rollout recorded for this worktree, the delivery's days searched
/// first, then older dated dirs (see [`walk_codex_days`]).
pub fn locate_codex(
    codex_home: &Path,
    worktree: &Path,
    session: Option<SessionRef<'_>>,
    delivered_at: i64,
    index: &TranscriptIndex,
) -> Option<PathBuf> {
    let sessions = codex_home.join("sessions");
    let days = codex_days(delivered_at);
    if let Some(id) = session.and_then(SessionRef::safe_id) {
        let now = Instant::now();
        let found = match index.lookup(id, now) {
            Some(cached) => cached,
            None => {
                let found = find_codex_rollout(&sessions, id, &days);
                index.store(id, found.clone(), now);
                found
            }
        };
        if found.is_some() {
            return found;
        }
    }
    if let Some(SessionRef::Path(p)) = session {
        let inside = match (std::fs::canonicalize(p), std::fs::canonicalize(&sessions)) {
            (Ok(file), Ok(root)) => file.starts_with(&root) && file.is_file(),
            _ => false,
        };
        let named = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_rollout);
        if inside && named {
            return Some(p.to_path_buf());
        }
    }
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let mut keys: Option<Vec<String>> = None;
    walk_codex_days(&sessions, &days, |dir| {
        let entries = std::fs::read_dir(dir).ok()?;
        for e in entries.flatten() {
            let path = e.path();
            let named = e.file_name().to_str().is_some_and(is_rollout);
            let Ok(meta) = e.metadata() else { continue };
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            if !named || !meta.is_file() || !fresh_mtime(mtime, delivered_at) {
                continue;
            }
            if newest.as_ref().is_none_or(|(t, _)| mtime > *t)
                && session_meta_cwd(&path).is_some_and(|cwd| {
                    let keys = keys.get_or_insert_with(|| worktree_keys(worktree));
                    keys.contains(&path_key(&cwd))
                })
            {
                newest = Some((mtime, path));
            }
        }
        None::<()>
    });
    newest.map(|(_, p)| p)
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
