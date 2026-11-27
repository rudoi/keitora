//! `activity` — per-worktree activity for the rail: each worktree's `/workflow` stage,
//! current git branch, and Unreal Build Tool progress, for every project, continuously.
//!
//! Every signal producer is pure / on-disk and testable without a live herdr:
//! - [`workflow`] — tolerant reader for a worktree's `WORKFLOW_STATE.json`.
//! - [`branch`] — a worktree's current git branch, read from `.git` with no subprocess.
//! - [`progress`] — a log-driven Unreal Build Tool build-progress engine.
//!
//! [`worktree_activity`] fuses them: it enumerates **all** projects' worktrees (cached), and
//! for each returns the workflow stage, branch, any attributed UBT build, and a `working`
//! flag (build running, or a herdr pane in that worktree is busy). It never counts processes
//! or agents — Claude Code's internal Task subagents are OS-invisible, so a count would be
//! false precision.

pub mod branch;
pub mod progress;
pub mod workflow;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::State;

use crate::backend::herdr::keymap::normalize_either_separator;
use crate::backend::{SessionSummary, SharedBackend, HINT_BUSY, HINT_WAITING};
use progress::BuildProgress;
use workflow::{WorkflowSlot, WorkflowState};

/// Lock an activity mutex, recovering it if a previous holder panicked. Everything behind
/// these locks is a cache that later ticks revalidate, so a panic costs one tick instead of
/// poisoning the lock and failing every later poll, which leaves the rail blank until restart.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Re-enumerate the all-projects worktree set at most this often (each enumeration re-reads
/// the user config + every project's `keitora.toml`).
const WORKTREES_TTL: Duration = Duration::from_secs(5);

/// One worktree's current activity, as surfaced to the rail.
#[derive(Serialize, Clone, Debug)]
pub struct WorktreeActivity {
    /// Resolved project name (`[project].name`, the session namespace) — not the
    /// registry key `set_active_project` takes; the two can differ.
    pub project: String,
    /// Worktree name within the project.
    pub worktree: String,
    /// Default CLI for the lane's provider glyph (`"claude"` / `"codex"`).
    pub cli: keitora_tasks::CliKind,
    /// Current git branch, if resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The worktree's `/workflow` state, if it has a `WORKFLOW_STATE.json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<WorkflowState>,
    /// An attributed Unreal Build Tool build in progress (or just finished), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildProgress>,
    /// The worktree is doing something now: a build is running, or a herdr pane in it is
    /// busy. (Internal Claude Task subagents are OS-invisible and never counted.)
    pub working: bool,
    /// The driver worktree when this worktree is driven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driven_by: Option<String>,
    /// Worktrees this worktree drives (names, sorted); empty when it drives none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub drives: Vec<String>,
    /// Pending, undecided brokered prompts for this worktree (0 when not driven).
    pub pending_prompts: u32,
    /// This worktree's drive is paused.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub drive_paused: bool,
}

/// One project's drive state for a tick: its bindings and live pending-prompt counts.
#[derive(Debug, Default, Clone)]
struct ProjectDrives {
    drives: Vec<keitora_tasks::Drive>,
    pending: HashMap<String, usize>,
}

/// A worktree's drive fields on [`WorktreeActivity`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DriveFields {
    driven_by: Option<String>,
    drives: Vec<String>,
    pending_prompts: u32,
    drive_paused: bool,
}

/// Read one project's drive state through a read-only handle. `None` when the DB
/// can't be opened or read (missing, locked); a DB without the drive tables reads
/// as no drives.
#[cfg(test)]
fn read_project_drives(db_path: &Path) -> Option<ProjectDrives> {
    let local = keitora_tasks::LocalStore::open_read_only(&db_path.to_string_lossy()).ok()?;
    read_drives_from(&local)
}

fn read_drives_from(local: &keitora_tasks::LocalStore) -> Option<ProjectDrives> {
    Some(ProjectDrives {
        drives: local.drives_list().ok()?,
        pending: local.drive_pending_prompt_counts().ok()?,
    })
}

/// A project's long-lived read-only handle with its last read.
struct CachedDrives {
    store: keitora_tasks::LocalStore,
    /// `PRAGMA data_version` observed just before `info` was read.
    version: i64,
    info: ProjectDrives,
}

/// Per-DB read-only handles held across ticks. While a DB's `data_version` is
/// unchanged the drive bindings are reused; pending-prompt counts are also
/// time-sensitive (stale prompts drop out), so they are re-read while any are live.
#[derive(Default)]
struct DriveCache {
    entries: HashMap<PathBuf, CachedDrives>,
    /// Full (drives + pending) reads performed; lets tests observe the skip.
    full_reads: usize,
}

impl DriveCache {
    /// Drop handles for DBs no longer in `keep`.
    fn retain_paths(&mut self, keep: &HashSet<&PathBuf>) {
        self.entries.retain(|p, _| keep.contains(p));
    }

    fn read(&mut self, db_path: &Path) -> Option<ProjectDrives> {
        if !db_path.exists() {
            self.entries.remove(db_path);
            return None;
        }
        if let Some(entry) = self.entries.get_mut(db_path) {
            match entry.store.data_version() {
                Ok(v) if v == entry.version => {
                    if entry.info.pending.is_empty() {
                        return Some(entry.info.clone());
                    }
                    if let Ok(pending) = entry.store.drive_pending_prompt_counts() {
                        entry.info.pending = pending;
                        return Some(entry.info.clone());
                    }
                }
                Ok(v) => {
                    self.full_reads += 1;
                    if let Some(info) = read_drives_from(&entry.store) {
                        entry.version = v;
                        entry.info = info.clone();
                        return Some(info);
                    }
                }
                Err(_) => {}
            }
            self.entries.remove(db_path);
            return None;
        }
        let store = keitora_tasks::LocalStore::open_read_only(&db_path.to_string_lossy()).ok()?;
        let version = store.data_version().ok()?;
        self.full_reads += 1;
        let info = read_drives_from(&store)?;
        self.entries.insert(
            db_path.to_path_buf(),
            CachedDrives {
                store,
                version,
                info: info.clone(),
            },
        );
        Some(info)
    }
}

/// Pure: `worktree`'s drive fields within its project's drive state.
fn drive_fields(info: Option<&ProjectDrives>, worktree: &str) -> DriveFields {
    let Some(info) = info else {
        return DriveFields::default();
    };
    let own = info.drives.iter().find(|d| d.driven == worktree);
    let mut drives: Vec<String> = info
        .drives
        .iter()
        .filter(|d| d.driver == worktree)
        .map(|d| d.driven.clone())
        .collect();
    drives.sort();
    DriveFields {
        driven_by: own.map(|d| d.driver.clone()),
        drives,
        pending_prompts: info.pending.get(worktree).copied().unwrap_or(0) as u32,
        drive_paused: own.is_some_and(|d| d.paused),
    }
}

/// One enumerated worktree (resolved from the registry).
#[derive(Clone)]
struct WtRef {
    project: String,
    worktree: String,
    path: PathBuf,
    cli: keitora_tasks::CliKind,
    /// The project's task DB (drive state lives there).
    db_path: PathBuf,
}

/// A worktree with its per-tick branch + workflow reads, before UBT/busy are merged in.
struct Prepared {
    wt: WtRef,
    branch: Option<String>,
    workflow: Option<WorkflowState>,
}

/// Managed state: the workflow-slot + branch caches (one lock), the enumerated-worktree TTL
/// cache (its own lock, so the config I/O never blocks the slot reads), and the single UBT
/// engine (its own interior lock).
#[derive(Default)]
pub struct ActivityState {
    inner: Mutex<Inner>,
    worktrees: Mutex<Option<(Vec<WtRef>, Instant)>>,
    drives: Mutex<DriveCache>,
    ubt: progress::UbtEngine,
}

#[derive(Default)]
struct Inner {
    /// Workflow reader slots, keyed by worktree path.
    workflow_slots: HashMap<PathBuf, WorkflowSlot>,
    branches: branch::BranchCache,
}

impl ActivityState {
    /// All projects' available worktrees, re-enumerated at most once per [`WORKTREES_TTL`].
    /// The enumeration (config I/O) runs with NO lock held, so a slow refresh never blocks a
    /// concurrent tick's slot reads.
    fn worktrees(&self) -> Vec<WtRef> {
        if let Some((cached, at)) = &*lock(&self.worktrees) {
            if at.elapsed() < WORKTREES_TTL {
                return cached.clone();
            }
        }
        let fresh = enumerate_worktrees();
        *lock(&self.worktrees) = Some((fresh.clone(), Instant::now()));
        fresh
    }

    /// The registered worktrees as perf's path-inference roots.
    pub(crate) fn worktree_roots(&self) -> Vec<crate::perf::attribute::WorktreeRoot> {
        self.worktrees()
            .into_iter()
            .map(|w| crate::perf::attribute::WorktreeRoot {
                project: w.project,
                worktree: w.worktree,
                path: w.path.to_string_lossy().into_owned(),
            })
            .collect()
    }
}

/// Enumerate every registered project's available worktrees. Best-effort: a project whose
/// config fails to resolve is skipped, not fatal.
fn enumerate_worktrees() -> Vec<WtRef> {
    let Ok(cfg) = keitora_tasks::UserConfig::load() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pref in cfg.projects.values() {
        let Ok(pc) = keitora_tasks::effective_config(pref) else {
            continue;
        };
        // Key by the project's resolved name (`[project].name`), NOT the registry map key:
        // the active-project UI (`current_project` → `activeProjectName`) and herdr session
        // keys both use the resolved name, so the lane lookup + busy-join must match it.
        let project = pc.project.name.clone();
        let db_path = pc.tasks.db_path.clone();
        let Ok(wts) = keitora_tasks::Config::from_project(pc).resolved_worktrees() else {
            continue;
        };
        for w in wts {
            if w.status != keitora_tasks::WorktreeStatus::Available {
                continue;
            }
            out.push(WtRef {
                project: project.clone(),
                worktree: w.name,
                path: w.path,
                cli: w.cli,
                db_path: db_path.clone(),
            });
        }
    }
    out
}

/// Composite keys of the sessions that light the working dot: a busy agent, or one
/// blocked on the user (a waiting lane keeps its dot; its colour comes from the state).
fn working_keys(sessions: Vec<SessionSummary>) -> HashSet<String> {
    sessions
        .into_iter()
        .filter(|s| {
            matches!(
                s.state_hint.as_deref(),
                Some(HINT_BUSY) | Some(HINT_WAITING)
            )
        })
        .map(|s| composite(&s.key.project, &s.key.worktree))
        .collect()
}

/// Composite `"project/worktree"` key — unique across projects (worktree names can collide).
fn composite(project: &str, worktree: &str) -> String {
    format!("{project}/{worktree}")
}

impl ActivityState {
    /// Compute activity for every project's worktree this tick.
    pub fn compute(&self, client: &SharedBackend) -> Vec<WorktreeActivity> {
        let now_ms = workflow::now_ms();

        // Enumerate worktrees with NO lock held (config I/O), then read each one's branch and
        // workflow state under the slot/cache lock.
        let worktrees = self.worktrees();
        let prepared: Vec<Prepared> = {
            let mut inner = lock(&self.inner);
            worktrees
                .into_iter()
                .map(|w| {
                    let branch = inner.branches.get(&w.path);
                    let slot = inner.workflow_slots.entry(w.path.clone()).or_default();
                    let workflow =
                        workflow::read_workflow(slot, &w.path, branch.as_deref(), now_ms);
                    Prepared {
                        wt: w,
                        branch,
                        workflow,
                    }
                })
                .collect()
        };

        // One UBT engine tick over the shared log, keyed by composite worktree id.
        let wt_paths: Vec<(String, String)> = prepared
            .iter()
            .map(|p| {
                (
                    composite(&p.wt.project, &p.wt.worktree),
                    normalize_either_separator(&p.wt.path.to_string_lossy()),
                )
            })
            .collect();
        let build = self.ubt.tick(&wt_paths, now_ms); // Option<(composite, BuildProgress)>

        // Busy worktrees from herdr (all projects, one call). Failure → nobody busy.
        let busy = working_keys(client.list_sessions().unwrap_or_default());

        // Drive state: one cached read-only handle per project DB, one read per tick.
        let mut project_drives: HashMap<PathBuf, Option<ProjectDrives>> = HashMap::new();
        {
            let mut cache = lock(&self.drives);
            let keep: HashSet<&PathBuf> = prepared.iter().map(|p| &p.wt.db_path).collect();
            cache.retain_paths(&keep);
            for p in &prepared {
                if !project_drives.contains_key(&p.wt.db_path) {
                    let info = cache.read(&p.wt.db_path);
                    project_drives.insert(p.wt.db_path.clone(), info);
                }
            }
        }

        prepared
            .into_iter()
            .map(|p| {
                let key = composite(&p.wt.project, &p.wt.worktree);
                let drive = drive_fields(
                    project_drives.get(&p.wt.db_path).and_then(Option::as_ref),
                    &p.wt.worktree,
                );
                let bp = build
                    .as_ref()
                    .filter(|(k, _)| *k == key)
                    .map(|(_, bp)| bp.clone());
                // `working` means active work NOW: a build still running, or a busy tracked
                // agent. A finished build lingers (retention) as a bar but is not "working",
                // so the lane doesn't show a misleading busy dot after a build completes.
                let building = bp
                    .as_ref()
                    .is_some_and(|b| b.state == progress::BuildState::Running);
                let working = building || busy.contains(&key);
                WorktreeActivity {
                    project: p.wt.project,
                    worktree: p.wt.worktree,
                    cli: p.wt.cli,
                    branch: p.branch,
                    workflow: p.workflow,
                    build: bp,
                    working,
                    driven_by: drive.driven_by,
                    drives: drive.drives,
                    pending_prompts: drive.pending_prompts,
                    drive_paused: drive.drive_paused,
                }
            })
            .collect()
    }
}

/// Activity for every project's worktree: workflow stage, git branch, UBT build, working.
#[tauri::command(async)]
pub fn worktree_activity(
    client: State<SharedBackend>,
    state: State<ActivityState>,
) -> Vec<WorktreeActivity> {
    state.compute(&client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_recovers_a_mutex_poisoned_by_a_panicking_holder() {
        let m = Mutex::new(1);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = m.lock().unwrap();
            *g = 2;
            panic!("holder panics");
        }));
        assert!(m.is_poisoned());
        let mut g = lock(&m);
        assert_eq!(*g, 2, "the value written before the panic survives");
        *g = 3;
        drop(g);
        assert_eq!(*lock(&m), 3, "and the lock keeps working");
    }

    /// A temp project DB with `alpha` driving `bravo` and `charlie`, `charlie`'s drive
    /// paused, and one pending prompt on `bravo`.
    fn bound_project_db(dir: &Path) -> PathBuf {
        let path = dir.join("tasks.db");
        let local = keitora_tasks::LocalStore::open(&path.to_string_lossy()).unwrap();
        local.drive_bind("alpha", "bravo", None, false).unwrap();
        local.drive_bind("alpha", "charlie", None, false).unwrap();
        local.drive_set_paused("charlie", true).unwrap();
        local
            .drive_prompt_insert(&keitora_tasks::NewDrivePrompt {
                driven: "bravo".to_string(),
                provider: "claude".to_string(),
                kind: "permission".to_string(),
                tool_name: "Bash".to_string(),
                tool_input: "{}".to_string(),
                summary: "ls".to_string(),
                hook_pid: None,
            })
            .unwrap();
        path
    }

    #[test]
    fn drive_fields_for_a_bound_pair_and_a_paused_drive() {
        let tmp = tempfile::tempdir().unwrap();
        let info = read_project_drives(&bound_project_db(tmp.path())).expect("readable");

        let alpha = drive_fields(Some(&info), "alpha");
        assert_eq!(alpha.driven_by, None);
        assert_eq!(
            alpha.drives,
            vec!["bravo".to_string(), "charlie".to_string()]
        );
        assert_eq!(alpha.pending_prompts, 0);
        assert!(!alpha.drive_paused);

        let bravo = drive_fields(Some(&info), "bravo");
        assert_eq!(bravo.driven_by.as_deref(), Some("alpha"));
        assert!(bravo.drives.is_empty());
        assert_eq!(bravo.pending_prompts, 1);
        assert!(!bravo.drive_paused);

        let charlie = drive_fields(Some(&info), "charlie");
        assert_eq!(charlie.driven_by.as_deref(), Some("alpha"));
        assert!(charlie.drive_paused);

        assert_eq!(drive_fields(Some(&info), "main"), DriveFields::default());
    }

    #[test]
    fn a_missing_or_old_db_reads_as_no_drives() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing file: no info this tick, and the fields default.
        assert!(read_project_drives(&tmp.path().join("absent.db")).is_none());
        assert_eq!(drive_fields(None, "bravo"), DriveFields::default());
        // An empty file is a valid DB without the drive tables: no drives.
        let old = tmp.path().join("old.db");
        std::fs::write(&old, b"").unwrap();
        let info = read_project_drives(&old).expect("readable");
        assert!(info.drives.is_empty());
        assert_eq!(drive_fields(Some(&info), "bravo"), DriveFields::default());
    }

    #[test]
    fn drive_cache_skips_unchanged_reads_and_picks_up_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tasks.db");
        let writer = keitora_tasks::LocalStore::open(&path.to_string_lossy()).unwrap();
        let mut cache = DriveCache::default();

        // Missing DB: nothing cached, nothing read.
        assert!(cache.read(&tmp.path().join("absent.db")).is_none());
        assert_eq!(cache.full_reads, 0);

        assert!(cache.read(&path).unwrap().drives.is_empty());
        assert_eq!(cache.full_reads, 1);
        // Unchanged DB: the cached read is reused.
        assert!(cache.read(&path).unwrap().drives.is_empty());
        assert!(cache.read(&path).unwrap().drives.is_empty());
        assert_eq!(cache.full_reads, 1, "no re-read while data_version holds");

        // Another connection commits a new drive: picked up on the next read.
        writer.drive_bind("alpha", "bravo", None, false).unwrap();
        let info = cache.read(&path).unwrap();
        assert_eq!(info.drives.len(), 1);
        assert_eq!(cache.full_reads, 2);
    }

    #[test]
    fn same_named_worktrees_in_two_projects_do_not_share_drive_state() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("proj");
        let b = tmp.path().join("other-proj");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let bound = read_project_drives(&bound_project_db(&a)).unwrap();
        let empty_path = b.join("tasks.db");
        keitora_tasks::LocalStore::open(&empty_path.to_string_lossy()).unwrap();
        let empty = read_project_drives(&empty_path).unwrap();
        assert_eq!(
            drive_fields(Some(&bound), "bravo").driven_by.as_deref(),
            Some("alpha")
        );
        assert_eq!(drive_fields(Some(&empty), "bravo"), DriveFields::default());
    }

    #[test]
    fn drive_fields_serialize_only_when_set() {
        let mut a = WorktreeActivity {
            project: "proj".to_string(),
            worktree: "bravo".to_string(),
            cli: keitora_tasks::CliKind::Claude,
            branch: None,
            workflow: None,
            build: None,
            working: false,
            driven_by: None,
            drives: vec![],
            pending_prompts: 0,
            drive_paused: false,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert!(v.get("driven_by").is_none());
        assert!(v.get("drives").is_none());
        assert!(v.get("drive_paused").is_none());
        assert_eq!(v["pending_prompts"], 0);
        a.driven_by = Some("alpha".to_string());
        a.drives = vec!["charlie".to_string()];
        a.pending_prompts = 2;
        a.drive_paused = true;
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["driven_by"], "alpha");
        assert_eq!(v["drives"], serde_json::json!(["charlie"]));
        assert_eq!(v["pending_prompts"], 2);
        assert_eq!(v["drive_paused"], true);
    }

    #[test]
    fn composite_key_disambiguates_same_named_worktrees_across_projects() {
        // Two projects can each have a worktree called "alpha"; the composite key keeps the
        // UBT-build and busy-session joins from cross-attributing between them.
        assert_eq!(composite("proj", "alpha"), "proj/alpha");
        assert_ne!(composite("proj", "alpha"), composite("keitora", "alpha"));
    }

    #[test]
    fn activity_working_dot_counts_waiting() {
        let summary = |w: &str, hint: Option<&str>| SessionSummary {
            key: crate::backend::SessionKey {
                project: "proj".into(),
                worktree: w.into(),
            },
            gen: 1,
            state_hint: hint.map(str::to_string),
        };
        let keys = working_keys(vec![
            summary("alpha", Some(HINT_BUSY)),
            summary("bravo", Some(HINT_WAITING)),
            summary("charlie", None),
        ]);
        assert_eq!(
            keys,
            HashSet::from(["proj/alpha".to_string(), "proj/bravo".to_string()])
        );
    }
}
