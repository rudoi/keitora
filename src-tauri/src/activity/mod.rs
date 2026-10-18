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
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::State;

use crate::backend::herdr::keymap::normalize_either_separator;
use crate::backend::{SharedBackend, HINT_BUSY};
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
}

/// One enumerated worktree (resolved from the registry).
#[derive(Clone)]
struct WtRef {
    project: String,
    worktree: String,
    path: PathBuf,
    cli: keitora_tasks::CliKind,
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
            });
        }
    }
    out
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
        let busy: HashSet<String> = client
            .list_sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.state_hint.as_deref() == Some(HINT_BUSY))
            .map(|s| composite(&s.key.project, &s.key.worktree))
            .collect();

        prepared
            .into_iter()
            .map(|p| {
                let key = composite(&p.wt.project, &p.wt.worktree);
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

    #[test]
    fn composite_key_disambiguates_same_named_worktrees_across_projects() {
        // Two projects can each have a worktree called "alpha"; the composite key keeps the
        // UBT-build and busy-session joins from cross-attributing between them.
        assert_eq!(composite("proj", "alpha"), "proj/alpha");
        assert_ne!(composite("proj", "alpha"), composite("keitora", "alpha"));
    }
}
