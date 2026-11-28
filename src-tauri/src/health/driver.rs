//! The health monitor in the app: gathers [`HealthInputs`] cheaply, evaluates and
//! reconciles them on the `keitora-pipeline` thread, writes findings through the
//! status publisher, runs remedies off-thread and tells the window.
//!
//! - Every [`EVALUATE_EVERY`] the whole app is evaluated.
//! - A view change marks its worktree dirty (from the engine's subscriber); a dirty
//!   worktree is re-evaluated [`DEBOUNCE`] after its first mark.
//! - Hook facts read files, so they are cached per worktree and re-read every
//!   [`FACTS_EVERY`], after a relaunch, or shortly after a remedy for them.
//! - The PATH `keitora --version` probe runs off-thread at start and every
//!   [`VERSION_EVERY`].
//!
//! Nothing here calls herdr: the engine's views and link state are in memory.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keitora_tasks::{CliKind, FindingWrite, ProjectConfig};

use super::commands::HealthFinding;
use super::reconcile::{Reconciler, Scope};
use super::{
    evaluate, HealthInputs, LinkFacts, PathCli, ProjectInputs, ProjectPipeline, Remedy,
    WorktreeInputs,
};
use crate::backend::herdr::status_loop::due;
use crate::backend::SessionKey;
use crate::engine::writer::{LeaseState, StatusPublisher};
use crate::engine::{SharedEngine, ViewChange};
use crate::hooks::HookFacts;

/// How often the whole app is evaluated.
pub const EVALUATE_EVERY: Duration = Duration::from_secs(30);
/// How long after its first mark a dirty worktree is re-evaluated.
pub const DEBOUNCE: Duration = Duration::from_secs(2);
/// How often a worktree's hook facts are re-read from disk.
pub const FACTS_EVERY: Duration = Duration::from_secs(5 * 60);
/// How soon after a remedy its worktree's facts are re-read.
pub const FACTS_AFTER_REMEDY: Duration = Duration::from_secs(5);
/// How often the PATH `keitora` is asked for its version.
pub const VERSION_EVERY: Duration = Duration::from_secs(10 * 60);

/// The pipeline's side of one tick: the registered projects, and what it knows of
/// one (by resolved name), asked only when the monitor evaluates.
pub struct PipelineFacts<'a> {
    pub projects: &'a [(String, ProjectConfig)],
    pub pipeline: &'a dyn Fn(&str) -> ProjectPipeline,
}

/// A worktree's file-backed facts.
#[derive(Clone, Debug)]
pub struct WorktreeFacts {
    pub provider: CliKind,
    pub folder: Option<String>,
    /// `None` when the worktree's folder is missing.
    pub facts: Option<HookFacts>,
    pub codex_config: Option<String>,
}

/// Reads one worktree's [`WorktreeFacts`] (real files in the app, fakes in tests).
pub type FactsFn = Arc<dyn Fn(&ProjectConfig, &str) -> WorktreeFacts + Send + Sync>;
/// Probes the PATH `keitora` (runs off-thread).
pub type VersionFn = Arc<dyn Fn() -> Option<PathCli> + Send + Sync>;
/// Carries out one remedy (runs off-thread).
pub type RemedyFn = Arc<dyn Fn(Remedy) + Send + Sync>;
/// Runs a job off the pipeline thread.
pub type Spawn = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;
/// Hears the full list whenever a finding was inserted, changed, cleared or dropped,
/// or its `last_seen` was flushed.
pub type Notify = Arc<dyn Fn(&[HealthFinding]) + Send + Sync>;

/// Where findings are written.
pub trait FindingSink: Send + Sync {
    fn lease(&self) -> LeaseState;
    fn apply(&self, writes: &[FindingWrite]);
}

impl FindingSink for StatusPublisher {
    fn lease(&self) -> LeaseState {
        StatusPublisher::lease(self)
    }
    fn apply(&self, writes: &[FindingWrite]) {
        self.findings_apply(writes);
    }
}

/// The monitor's findings as the IPC reads them (active and recently cleared).
pub type HealthList = Arc<Mutex<Vec<HealthFinding>>>;

/// Worktrees marked dirty by view changes, with their first mark.
pub type DirtyMarks = Arc<Mutex<HashMap<(String, String), Instant>>>;

/// Mark the worktrees of `changes` dirty at `now` (an empty batch: the link moved,
/// which is app-level). Keeps the first mark of a key.
pub fn mark_dirty(marks: &DirtyMarks, changes: &[ViewChange], now: Instant) {
    let mut m = marks.lock().unwrap();
    if changes.is_empty() {
        m.entry((String::new(), String::new())).or_insert(now);
    }
    for c in changes {
        m.entry((c.key.project.clone(), c.key.worktree.clone()))
            .or_insert(now);
    }
}

struct CachedFacts {
    facts: WorktreeFacts,
    due: Instant,
    /// The launch the facts were read after.
    launch_at: Option<i64>,
}

/// The PATH `keitora` probe's last answer, refreshed off-thread.
struct VersionProbe {
    run: VersionFn,
    result: Arc<Mutex<Option<PathCli>>>,
    running: Arc<AtomicBool>,
    last: Option<Instant>,
}

impl VersionProbe {
    fn poll(&mut self, now: Instant, spawn: &Spawn) {
        if !due(now, self.last, VERSION_EVERY) || self.running.load(Ordering::SeqCst) {
            return;
        }
        self.last = Some(now);
        self.running.store(true, Ordering::SeqCst);
        let (run, result, running) = (
            Arc::clone(&self.run),
            Arc::clone(&self.result),
            Arc::clone(&self.running),
        );
        spawn(Box::new(move || {
            let found = run();
            *result.lock().unwrap() = found;
            running.store(false, Ordering::SeqCst);
        }));
    }

    fn current(&self) -> Option<PathCli> {
        self.result.lock().unwrap().clone()
    }
}

/// What the monitor runs on. See the module docs.
pub struct HealthDeps {
    pub engine: Option<SharedEngine>,
    pub sink: Option<Arc<dyn FindingSink>>,
    pub facts: FactsFn,
    pub version: VersionFn,
    pub remedy: RemedyFn,
    pub spawn: Spawn,
    pub notify: Notify,
    /// Whether remedies may run (global writes allowed).
    pub remedies_allowed: bool,
}

/// See the module docs.
pub struct HealthMonitor {
    deps: HealthDeps,
    reconciler: Reconciler,
    facts: HashMap<(String, String), CachedFacts>,
    version: VersionProbe,
    dirty: DirtyMarks,
    list: HealthList,
    last_full: Option<Instant>,
    /// The lease as of the last tick was held: a fresh hold rewrites the table.
    owned: bool,
    /// Evaluations run so far.
    pub evaluations: usize,
}

impl HealthMonitor {
    pub fn new(deps: HealthDeps) -> HealthMonitor {
        let version = VersionProbe {
            run: Arc::clone(&deps.version),
            result: Arc::default(),
            running: Arc::default(),
            last: None,
        };
        HealthMonitor {
            reconciler: Reconciler::new(deps.remedies_allowed),
            deps,
            facts: HashMap::new(),
            version,
            dirty: Arc::default(),
            list: Arc::default(),
            last_full: None,
            owned: false,
            evaluations: 0,
        }
    }

    /// The dirty marks the engine subscriber writes.
    pub fn dirty(&self) -> DirtyMarks {
        Arc::clone(&self.dirty)
    }

    /// The findings list the IPC reads.
    pub fn list(&self) -> HealthList {
        Arc::clone(&self.list)
    }

    /// One pipeline tick at `now` (wall clock `now_ms`).
    pub fn tick(&mut self, now: Instant, now_ms: i64, pipeline: &PipelineFacts) {
        self.version.poll(now, &self.deps.spawn);
        let scope = if due(now, self.last_full, EVALUATE_EVERY) {
            self.last_full = Some(now);
            self.dirty.lock().unwrap().clear();
            None
        } else {
            let ripe: Scope = {
                let mut marks = self.dirty.lock().unwrap();
                let ripe: Scope = marks
                    .iter()
                    .filter(|(_, at)| now.saturating_duration_since(**at) >= DEBOUNCE)
                    .map(|(k, _)| k.clone())
                    .collect();
                marks.retain(|k, _| !ripe.contains(k));
                ripe
            };
            if ripe.is_empty() {
                self.sync_lease();
                return;
            }
            Some(ripe)
        };
        let inputs = self.gather(now, pipeline, scope.as_ref());
        let found = evaluate(&inputs, now_ms);
        self.evaluations += 1;
        let out = self.reconciler.reconcile(found, scope.as_ref(), now_ms);
        if !self.sync_lease() {
            if let Some(sink) = &self.deps.sink {
                if !out.writes.is_empty() {
                    sink.apply(&out.writes);
                }
            }
        }
        for r in out.remedies {
            if let Remedy::ReinstallHooks { project, worktree }
            | Remedy::SyncAgyHooks { project, worktree } = &r
            {
                if let Some(c) = self.facts.get_mut(&(project.clone(), worktree.clone())) {
                    c.due = now + FACTS_AFTER_REMEDY;
                }
            }
            let run = Arc::clone(&self.deps.remedy);
            (self.deps.spawn)(Box::new(move || run(r)));
        }
        // The list the IPC reads follows every evaluation (`last_seen` moves each
        // time); the window is told on a change, or when a `last_seen` was flushed
        // (at most once a minute per finding).
        let all = self.reconciler.list();
        *self.list.lock().unwrap() = all.clone();
        if out.changed || !out.writes.is_empty() {
            (self.deps.notify)(&all);
        }
    }

    /// Rewrite the table when this instance just took the lease. `true` when it did
    /// (the rewrite carries every current row).
    fn sync_lease(&mut self) -> bool {
        let Some(sink) = &self.deps.sink else {
            return false;
        };
        let owned = sink.lease() == LeaseState::Owned;
        let fresh = owned && !self.owned;
        self.owned = owned;
        if fresh {
            sink.apply(&self.reconciler.resync());
        }
        fresh
    }

    /// One worktree's facts: cached, re-read when due or after a new launch.
    fn worktree_facts(
        &mut self,
        pc: &ProjectConfig,
        key: &SessionKey,
        now: Instant,
    ) -> WorktreeFacts {
        let launch = crate::hooks::last_launch(key);
        let launch_at = launch.as_ref().map(|l| l.at_ms);
        let id = (key.project.clone(), key.worktree.clone());
        let fresh = self
            .facts
            .get(&id)
            .is_some_and(|c| now < c.due && c.launch_at == launch_at);
        if !fresh {
            let facts = (self.deps.facts)(pc, &key.worktree);
            self.facts.insert(
                id.clone(),
                CachedFacts {
                    facts,
                    due: now + FACTS_EVERY,
                    launch_at,
                },
            );
        }
        let mut wf = self.facts[&id].facts.clone();
        if let Some(f) = wf.facts.as_mut() {
            f.last_launch = launch;
        }
        wf
    }

    /// Everything the checks read, from memory and the cached facts. With `scope` (a
    /// debounced re-evaluation) only the projects it touches are gathered, with only
    /// its worktrees: the reconciler ignores findings outside the scope anyway.
    fn gather(
        &mut self,
        now: Instant,
        pipeline: &PipelineFacts,
        scope: Option<&Scope>,
    ) -> HealthInputs {
        let engine = self.deps.engine.clone();
        let mut views = HashMap::new();
        let mut keys = HashMap::new();
        let mut turns = HashMap::new();
        if let Some(e) = &engine {
            views.extend(e.all_views());
            keys.extend(e.key_health());
            turns.extend(e.turn_health());
        }
        let mut projects = Vec::with_capacity(pipeline.projects.len());
        let mut known = HashSet::new();
        for (_, pc) in pipeline.projects {
            let name = pc.project.name.clone();
            known.insert(name.clone());
            if scope.is_some_and(|s| !s.iter().any(|(p, _)| *p == name)) {
                continue;
            }
            let evidence = engine.as_ref().and_then(|e| e.project_evidence(&name));
            let pipe = (pipeline.pipeline)(&name);
            let mut worktrees = Vec::new();
            for wt in pc.worktrees() {
                if scope.is_some_and(|s| !s.contains(&(name.clone(), wt.clone()))) {
                    continue;
                }
                let key = SessionKey {
                    project: name.clone(),
                    worktree: wt.clone(),
                };
                let wf = self.worktree_facts(pc, &key, now);
                worktrees.push(WorktreeInputs {
                    name: wt.clone(),
                    folder: wf.folder,
                    provider: wf.provider,
                    driven: evidence
                        .as_ref()
                        .is_some_and(|ev| ev.drive.driven.contains(&wt)),
                    view: views.get(&key).cloned(),
                    key: keys.get(&key).cloned(),
                    hook: evidence.as_ref().and_then(|ev| ev.rows.get(&wt).cloned()),
                    turns: turns.get(&key).cloned(),
                    facts: wf.facts,
                    codex_config: wf.codex_config,
                });
            }
            projects.push(ProjectInputs {
                name,
                pipeline: pipe,
                hook_rows: evidence
                    .map(|ev| {
                        ev.rows
                            .into_iter()
                            .map(|(w, h)| (w, h.written_ms))
                            .collect()
                    })
                    .unwrap_or_default(),
                worktrees,
            });
        }
        // Facts of projects that left the registry go.
        if scope.is_none() {
            self.facts.retain(|(p, _), _| known.contains(p));
        }
        HealthInputs {
            link: engine.as_ref().map(|e| {
                let l = e.link_state();
                LinkFacts {
                    link: l.link,
                    since_ms: l.since_ms,
                    last_snapshot_ms: l.last_snapshot_ms,
                }
            }),
            lease: self.deps.sink.as_ref().map(|s| s.lease()),
            path_cli: self.version.current(),
            projects,
        }
    }
}

// ── The app's real sources ────────────────────────────────────────────────────

/// The real facts reader: per worktree its provider, folder, hook facts (when the
/// folder exists) and Codex config path, reading the user config once per call and
/// the files every worktree shares through one cache.
pub fn real_facts() -> FactsFn {
    let files = Arc::new(crate::hooks::SharedFactsFiles::default());
    Arc::new(move |pc: &ProjectConfig, wt: &str| {
        let user = keitora_tasks::UserConfig::load().unwrap_or_default();
        let provider = keitora_tasks::resolve_cli(pc.worktree_entry(wt).as_ref(), &user);
        let path = pc.worktree_path(wt);
        let folder = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let paths = crate::hooks::real_facts_paths();
        let facts = path.is_dir().then(|| {
            let key = SessionKey {
                project: pc.project.name.clone(),
                worktree: wt.to_string(),
            };
            let sources = crate::hooks::KeitoraBinSources::gather(
                Some(pc),
                crate::hooks::keitora_on_path_cached().clone(),
            );
            crate::hooks::hook_facts_in(&path, provider, &sources, &key, &paths, &files)
        });
        WorktreeFacts {
            provider,
            folder,
            facts,
            codex_config: paths.codex_config.map(|c| c.to_string_lossy().into_owned()),
        }
    })
}

/// `<bin> --version`'s version (`keitora 0.6.0` -> `0.6.0`), through the shared
/// dependency probe (bounded, no console window).
fn version_of(bin: &std::path::Path) -> Option<String> {
    crate::deps::binary_version(&crate::deps::RealProc, bin)
        .ok()
        .map(|(v, _)| v.to_string())
}

/// `cell`'s value, or what `probe` finds (kept): a missing answer isn't cached, so
/// the next call asks again.
fn kept_once_found(
    cell: &std::sync::OnceLock<String>,
    probe: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if let Some(v) = cell.get() {
        return Some(v.clone());
    }
    let found = probe()?;
    Some(cell.get_or_init(|| found).clone())
}

/// The bundled CLI's version: it ships with the app, so once found it is kept for
/// the process; a failed probe is retried on the next PATH probe.
fn bundled_version() -> Option<String> {
    static BUNDLED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    kept_once_found(&BUNDLED, || {
        crate::hooks::app_bundled_cli().and_then(|b| version_of(b.as_ref()))
    })
}

/// The real PATH probe: the PATH `keitora`'s version, the bundled CLI's, and whether
/// PATH's is a stale managed copy.
pub fn real_version() -> Option<PathCli> {
    let path = crate::hooks::keitora_on_path_cached().clone()?;
    let on_path = std::path::PathBuf::from(&path);
    let version = version_of(&on_path)?;
    let bundled = crate::hooks::app_bundled_cli().map(std::path::PathBuf::from);
    let bundled_version = bundled_version();
    let managed = crate::cli_install::default_link_path();
    let managed_stale = match (&bundled, &managed) {
        (Some(b), Some(m)) => {
            keitora_tasks::same_file_name(&on_path, m)
                && !keitora_tasks::same_file_name(b, m)
                && keitora_tasks::mirror_is_stale(b, m)
        }
        _ => false,
    };
    let refreshable = crate::cli_install::REFRESHES_MANAGED_COPY
        && managed
            .as_ref()
            .is_some_and(|m| keitora_tasks::same_file_name(&on_path, m));
    Some(PathCli {
        dev_build: crate::hooks::exe_is_untrusted_for_bundled_cli(&on_path),
        path,
        version,
        bundled_version,
        managed_stale,
        refreshable,
    })
}

/// The real remedies. `request_pass` asks for a pass over a project.
pub fn real_remedy(request_pass: Arc<dyn Fn(&str) + Send + Sync>) -> RemedyFn {
    Arc::new(move |r: Remedy| {
        let label = r.label();
        match &r {
            Remedy::ReinstallHooks { project, worktree } => {
                let registry = crate::project_stores::registry();
                let Some(pc) = registry.by_name(project) else {
                    return;
                };
                let user = keitora_tasks::UserConfig::load().unwrap_or_default();
                let cli = keitora_tasks::resolve_cli(pc.worktree_entry(worktree).as_ref(), &user);
                crate::hooks::prepare_new_worktree(pc, &pc.worktree_path(worktree), cli);
            }
            Remedy::SyncAgyHooks { project, worktree } => {
                let registry = crate::project_stores::registry();
                if let Some(pc) = registry.by_name(project) {
                    crate::antigravity_hooks::sync_worktree(&pc.worktree_path(worktree));
                }
                crate::antigravity_hooks::sync_registry();
            }
            Remedy::RequestPass { project } => request_pass(project),
            Remedy::RefreshCli => crate::host::refresh_managed_cli(),
        }
        log::info!("health: remedy ran: {label} ({r:?})");
    })
}

/// Spawn on a plain thread.
pub fn thread_spawn() -> Spawn {
    Arc::new(|job| {
        let _ = std::thread::Builder::new()
            .name("keitora-health".into())
            .spawn(job);
    })
}

#[cfg(test)]
#[path = "driver_tests.rs"]
mod tests;
