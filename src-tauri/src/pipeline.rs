//! The request pipeline: how a requested pass over a project's pending drive and
//! dispatch requests gets run.
//!
//! - [`on_db_event`]: a tasks-DB watcher saw a write.
//! - [`install`]: passes requested through [`DriveRuntime::request_pass`] run on the
//!   async runtime when due; the engine's view and link changes request them too.
//! - [`Pipeline`] on the `keitora-pipeline` thread ([`start_thread`]): the safety
//!   sweep, watcher supervision, the projects' presence rows in `status.db`, and the
//!   health monitor.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use keitora_tasks::{AppProject, AppProjectTouch, DriveRequest, ProjectConfig};
use tauri::{AppHandle, Manager, Runtime};

use crate::backend::herdr::status_loop::due;
use crate::drive::{DriveRuntime, PassRunner, Trigger};
use crate::engine::writer::StatusPublisher;
use crate::engine::SharedEngine;
use crate::health::driver::{HealthMonitor, PipelineFacts};
use crate::health::{PendingFacts, ProjectPipeline, RestartNote};
use crate::project_stores::ReadStores;
use crate::watchers::{
    OldestPending, Restarted, WatchState, WatcherRegistry, WatcherSummary, RESTART_WINDOW_MS,
};

/// A tasks-DB watcher saw a write to resolved project `project`'s DB: ask for a pass
/// when it holds pending requests, unless only the last pass's own writes (a wait
/// reason, an outcome) changed it since that pass began reading. Reads through the
/// shared read-only handle (status only), so it works whatever happened to the
/// watcher's own store.
pub(crate) fn on_db_event<R: Runtime>(app: &AppHandle<R>, project: &str) {
    let Some(rt) = app.try_state::<DriveRuntime>() else {
        return; // still starting up; the sweep catches up
    };
    let registry = crate::project_stores::registry();
    let Some(pc) = registry.by_name(project) else {
        return;
    };
    if rt.unchanged_since_pass(&pc.tasks.db_path.to_string_lossy()) {
        return;
    }
    crate::drive::request_pending_passes_in(
        &rt,
        std::iter::once(pc),
        &crate::project_stores::shared(),
        |_| true,
        Trigger::Watcher,
    );
}

/// A project's pending drive requests as one read found them.
#[derive(Debug, Default)]
pub(crate) enum DrivePending {
    #[default]
    None,
    Rows(Vec<DriveRequest>),
    /// Pending, in a table too old to list.
    Unlisted,
}

/// What a project's DB at `path` holds pending, read through one of `stores`' shared
/// read-only handles: the drive rows, and how many dispatch rows. A DB of any age
/// works; a missing one has nothing pending.
pub(crate) fn pending_in(stores: &ReadStores, path: &Path) -> (DrivePending, i64) {
    stores
        .with(path, |rs| {
            let s = &rs.store;
            let drive = match s.drive_requests_pending() {
                Ok(rows) if rows.is_empty() => DrivePending::None,
                Ok(rows) => DrivePending::Rows(rows),
                // Listing needs the current columns; the status probe doesn't.
                Err(_) if s.drive_requests_has_pending()? => DrivePending::Unlisted,
                Err(_) => DrivePending::None,
            };
            Ok((drive, s.dispatch_requests_pending_count()?))
        })
        .unwrap_or_default()
}

/// Runs requested passes on the async runtime: at the due time, if the request is
/// still queued, a dispatch pass (when dispatch rows are pending) and a drive pass.
struct AppRunner<R: Runtime>(AppHandle<R>);

impl<R: Runtime> PassRunner for AppRunner<R> {
    fn run_at(&self, project: String, due: Instant, _after: Duration, why: Trigger) {
        let app = self.0.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
            let Some(rt) = app.try_state::<DriveRuntime>() else {
                return;
            };
            if !rt.take_due(&project, due) {
                return; // a pass that served this request already ran
            }
            if dispatch_pending(&project) > 0 {
                tauri::async_runtime::spawn(crate::dispatch::run_dispatch_requests(
                    app.clone(),
                    project.clone(),
                ));
            }
            crate::drive::run_drive_requests(app, project, why).await;
        });
    }
}

/// How many dispatch requests `project`'s DB holds pending.
fn dispatch_pending(project: &str) -> i64 {
    let registry = crate::project_stores::registry();
    registry.by_name(project).map_or(0, |pc| {
        pending_in(&crate::project_stores::shared(), &pc.tasks.db_path).1
    })
}

/// Run requested passes from now on, and request them on engine changes.
pub(crate) fn install<R: Runtime>(app: &AppHandle<R>, engine: &SharedEngine) {
    let Some(rt) = app.try_state::<DriveRuntime>() else {
        return;
    };
    rt.set_runner(Arc::new(AppRunner(app.clone())));
    crate::drive::watch_engine(engine, &rt);
}

// ── The pipeline thread ─────────────────────────────────────────────────────────

/// How often the pipeline thread wakes.
pub(crate) const TICK: Duration = Duration::from_secs(1);
/// How often every registered project is checked for pending requests.
pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(10);
/// How often the request-carrying watchers are supervised.
pub(crate) const SUPERVISE_EVERY: Duration = Duration::from_secs(10);

/// Where a project's pipeline state (`status.db` `app_projects`) goes.
pub(crate) trait PresenceSink: Send + Sync {
    /// Write the whole row now; `false` when it wasn't written (retried later).
    fn set(&self, row: &AppProject) -> bool;
    /// Queue the throttled columns for the next heartbeat.
    fn touch(&self, project: &str, touch: AppProjectTouch);
    /// The project left the registry.
    fn delete(&self, project: &str);
}

impl PresenceSink for StatusPublisher {
    fn set(&self, row: &AppProject) -> bool {
        self.app_project_set(row)
    }
    fn touch(&self, project: &str, touch: AppProjectTouch) {
        self.app_project_touch(project, touch);
    }
    fn delete(&self, project: &str) {
        self.app_project_delete(project);
    }
}

/// The registered projects, as `(registry key, effective config)`.
pub(crate) type ProjectLister = Box<dyn Fn() -> Vec<(String, ProjectConfig)> + Send>;

/// What the last sweep found in one project's DB.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Found {
    pending: i64,
    oldest: Option<OldestPending>,
    /// A pass has examined the oldest pending request.
    oldest_seen: bool,
}

/// The `keitora-pipeline` thread's work, on an injected clock: every
/// [`SWEEP_EVERY`] a safety sweep asks for a pass over each project with pending
/// requests (so a request is picked up even when every watcher is dead), every
/// [`SUPERVISE_EVERY`] the watchers are supervised, and every tick the projects'
/// presence rows are brought up to date (written only when something changed).
pub(crate) struct Pipeline {
    projects: ProjectLister,
    stores: Arc<ReadStores>,
    runtime: DriveRuntime,
    watchers: Arc<WatcherRegistry>,
    presence: Option<Arc<dyn PresenceSink>>,
    last_sweep: Option<Instant>,
    last_supervise: Option<Instant>,
    /// The registered projects as of the last sweep.
    registered: Vec<(String, ProjectConfig)>,
    found: HashMap<String, Found>,
    /// Each project's row as last written.
    written: HashMap<String, AppProject>,
    /// The supervisor's restarts in the last [`RESTART_WINDOW_MS`], by project.
    restarts: HashMap<String, Vec<RestartNote>>,
    health: Option<HealthMonitor>,
}

impl Pipeline {
    pub(crate) fn new(
        projects: ProjectLister,
        stores: Arc<ReadStores>,
        runtime: DriveRuntime,
        watchers: Arc<WatcherRegistry>,
        presence: Option<Arc<dyn PresenceSink>>,
    ) -> Pipeline {
        Pipeline {
            projects,
            stores,
            runtime,
            watchers,
            presence,
            last_sweep: None,
            last_supervise: None,
            registered: Vec::new(),
            found: HashMap::new(),
            written: HashMap::new(),
            restarts: HashMap::new(),
            health: None,
        }
    }

    /// Run `monitor` on every tick.
    pub(crate) fn with_health(mut self, monitor: HealthMonitor) -> Pipeline {
        self.health = Some(monitor);
        self
    }

    /// One wake of the pipeline thread at `now` (wall clock `now_ms`).
    pub(crate) fn tick(&mut self, now: Instant, now_ms: i64) {
        if due(now, self.last_sweep, SWEEP_EVERY) {
            self.last_sweep = Some(now);
            self.sweep();
        }
        if due(now, self.last_supervise, SUPERVISE_EVERY) {
            self.last_supervise = Some(now);
            self.supervise(now_ms);
        }
        self.publish_presence(now_ms);
        self.health_tick(now, now_ms);
    }

    /// The health monitor's tick, with what the pipeline knows of each project
    /// (gathered only when the monitor evaluates).
    fn health_tick(&mut self, now: Instant, now_ms: i64) {
        let Some(health) = self.health.as_mut() else {
            return;
        };
        window_restarts(&mut self.restarts, now_ms);
        let (found, runtime, restarts) = (&self.found, &self.runtime, &self.restarts);
        let pipeline = |name: &str| {
            let found = found.get(name);
            ProjectPipeline {
                oldest_pending: found.and_then(|f| {
                    f.oldest.map(|o| PendingFacts {
                        id: o.id,
                        created_at_ms: o.created_at_ms,
                        seen: f.oldest_seen,
                    })
                }),
                last_pass_ms: runtime.last_pass(name).map(|(at, _)| at),
                restarts: restarts.get(name).cloned().unwrap_or_default(),
            }
        };
        health.tick(
            now,
            now_ms,
            &PipelineFacts {
                projects: &self.registered,
                pipeline: &pipeline,
            },
        );
    }

    /// Ask for a pass over every registered project with pending drive or dispatch
    /// requests, and note what each holds. Read-only, status probes first (a DB of
    /// any age works; a missing one has nothing pending).
    fn sweep(&mut self) {
        let projects = (self.projects)();
        let mut found = HashMap::new();
        for (_, pc) in &projects {
            let name = &pc.project.name;
            let (drive, dispatch) = pending_in(&self.stores, &pc.tasks.db_path);
            // The rows feed the engine trigger's hint and the watchers' silent rule; a
            // table too old to list them still counts as pending.
            let mut f = Found {
                pending: dispatch,
                oldest: None,
                oldest_seen: false,
            };
            match &drive {
                DrivePending::Rows(rows) => {
                    f.pending += rows.len() as i64;
                    f.oldest = rows.first().and_then(|r| {
                        keitora_tasks::sqlite_utc_to_ms(&r.created_at).map(|at| OldestPending {
                            id: r.id,
                            created_at_ms: at,
                        })
                    });
                    f.oldest_seen = rows.first().is_some_and(|r| r.seen_at.is_some());
                }
                DrivePending::Unlisted => f.pending += 1,
                DrivePending::None => {}
            }
            let rows = match &drive {
                DrivePending::Rows(rows) => rows.as_slice(),
                _ => &[],
            };
            // An unlisted table keeps the last hint it had.
            if !matches!(drive, DrivePending::Unlisted) {
                self.runtime.set_pending_rows(name, rows);
            }
            if f.pending > 0
                && self
                    .runtime
                    .request_pass(name, Duration::ZERO, Trigger::Sweep)
            {
                log::info!("pipeline sweep: project={name} pending, pass requested");
            }
            found.insert(name.clone(), f);
        }
        let paths: HashSet<String> = projects
            .iter()
            .map(|(_, pc)| pc.tasks.db_path.to_string_lossy().into_owned())
            .collect();
        self.runtime.retain_stores(|p| paths.contains(p));
        self.found = found;
        self.registered = projects;
    }

    /// Restart dead, stalled or silent watchers.
    fn supervise(&mut self, now_ms: i64) {
        let oldest: HashMap<String, OldestPending> = self
            .found
            .iter()
            .filter_map(|(p, f)| f.oldest.map(|o| (p.clone(), o)))
            .collect();
        let done = self.watchers.supervise(now_ms, &oldest);
        note_restarts(&mut self.restarts, &done, now_ms);
    }

    /// A registered project's presence row as of now, from its watchers' `watcher`
    /// summary.
    fn presence_row(
        &self,
        key: &str,
        pc: &ProjectConfig,
        watcher: Option<&WatcherSummary>,
    ) -> AppProject {
        let name = &pc.project.name;
        let found = self.found.get(name).cloned().unwrap_or_default();
        let last_pass = self.runtime.last_pass(name);
        let (last_error, last_error_at) = watcher
            .and_then(|w| w.last_error.clone())
            .unwrap_or_default();
        AppProject {
            project: name.clone(),
            registry_key: key.to_string(),
            db_path: pc.tasks.db_path.to_string_lossy().into_owned(),
            open: self.runtime.is_open(name),
            open_reported_at: self.runtime.open_reported_at(),
            watcher_state: watcher
                .map_or(WatchState::Stopped, |w| w.state)
                .as_str()
                .to_string(),
            watcher_started_at: watcher.and_then(|w| w.started_at_ms),
            watcher_last_event_at: watcher.and_then(|w| w.last_event_at_ms),
            last_pass_at: last_pass.map(|(at, _)| at),
            last_pass_trigger: last_pass.map(|(_, t)| t.as_str().to_string()),
            pending_count: found.pending,
            oldest_pending_at: found.oldest.map(|o| o.created_at_ms),
            restarts: watcher.map_or(0, |w| w.restarts),
            last_error,
            last_error_at,
        }
    }

    /// Bring every registered project's row up to date: a change a reader acts on is
    /// written at once, the rest is queued for the heartbeat, and nothing is written
    /// while nothing changed. Rows of projects that left the registry are deleted.
    fn publish_presence(&mut self, now_ms: i64) {
        let Some(sink) = self.presence.clone() else {
            return;
        };
        let watchers = self.watchers.summaries(now_ms);
        for (key, pc) in &self.registered {
            let row = self.presence_row(key, pc, watchers.get(&pc.project.name));
            let written = match self.written.get(&row.project) {
                Some(prev) if !presence_urgent(prev, &row) => {
                    let Some(touch) = presence_touch(prev, &row) else {
                        continue;
                    };
                    sink.touch(&row.project, touch);
                    // The heartbeat doesn't carry the report time: keep the written one.
                    AppProject {
                        open_reported_at: prev.open_reported_at,
                        ..row
                    }
                }
                _ if sink.set(&row) => row,
                _ => continue,
            };
            self.written.insert(written.project.clone(), written);
        }
        let registered: HashSet<&str> = self
            .registered
            .iter()
            .map(|(_, pc)| pc.project.name.as_str())
            .collect();
        let gone: Vec<String> = self
            .written
            .keys()
            .filter(|p| !registered.contains(p.as_str()))
            .cloned()
            .collect();
        for p in gone {
            sink.delete(&p);
            self.written.remove(&p);
        }
    }
}

/// Pure: add the supervisor's restarts `done` at `now_ms` to `notes` (by project).
fn note_restarts(notes: &mut HashMap<String, Vec<RestartNote>>, done: &[Restarted], now_ms: i64) {
    for r in done {
        notes
            .entry(r.id.project.clone())
            .or_default()
            .push(RestartNote {
                at_ms: now_ms,
                watcher: r.id.kind,
                why: r.why.clone(),
            });
    }
}

/// Pure: forget restarts older than [`RESTART_WINDOW_MS`] at `now_ms`: the health
/// checks read `notes` as the window's restarts.
fn window_restarts(notes: &mut HashMap<String, Vec<RestartNote>>, now_ms: i64) {
    for v in notes.values_mut() {
        v.retain(|n| now_ms - n.at_ms < RESTART_WINDOW_MS);
    }
    notes.retain(|_, v| !v.is_empty());
}

/// Pure: whether `next` differs from the written `prev` in a column a reader acts on
/// (written at once): the window's report (open, and whether it reported at all),
/// the watcher's state, start or error, pending turning on or off, a restart, or
/// the project's key or DB.
fn presence_urgent(prev: &AppProject, next: &AppProject) -> bool {
    prev.registry_key != next.registry_key
        || prev.db_path != next.db_path
        || prev.open != next.open
        || prev.open_reported_at.is_some() != next.open_reported_at.is_some()
        || prev.watcher_state != next.watcher_state
        || prev.watcher_started_at != next.watcher_started_at
        || prev.last_error != next.last_error
        || prev.last_error_at != next.last_error_at
        || (prev.pending_count > 0) != (next.pending_count > 0)
        || prev.restarts != next.restarts
}

/// Pure: the throttled columns that moved, `None` when none did.
fn presence_touch(prev: &AppProject, next: &AppProject) -> Option<AppProjectTouch> {
    let t = AppProjectTouch {
        last_pass_at: (prev.last_pass_at != next.last_pass_at)
            .then_some(next.last_pass_at)
            .flatten(),
        last_pass_trigger: (prev.last_pass_trigger != next.last_pass_trigger)
            .then(|| next.last_pass_trigger.clone())
            .flatten(),
        watcher_last_event_at: (prev.watcher_last_event_at != next.watcher_last_event_at)
            .then_some(next.watcher_last_event_at)
            .flatten(),
        pending_count: (prev.pending_count != next.pending_count).then_some(next.pending_count),
        oldest_pending_at: (prev.oldest_pending_at != next.oldest_pending_at)
            .then_some(next.oldest_pending_at),
    };
    (t != AppProjectTouch::default()).then_some(t)
}

/// Start the `keitora-pipeline` thread: it ticks the pipeline every second for the
/// app's lifetime, whatever the PTY backend (it never depends on herdr's status
/// loop, so it keeps running through recovery, a stop, or no herdr at all).
pub(crate) fn start_thread<R: Runtime>(app: &AppHandle<R>) {
    let Some(rt) = app.try_state::<DriveRuntime>() else {
        return;
    };
    let presence = app
        .try_state::<Arc<StatusPublisher>>()
        .map(|p| Arc::clone(p.inner()) as Arc<dyn PresenceSink>);
    let monitor = start_health(app, rt.inner());
    let mut pipeline = Pipeline::new(
        Box::new(|| crate::project_stores::registry().projects.clone()),
        crate::project_stores::shared(),
        rt.inner().clone(),
        crate::watchers::global(),
        presence,
    )
    .with_health(monitor);
    let spawned = std::thread::Builder::new()
        .name("keitora-pipeline".into())
        .spawn(move || loop {
            pipeline.tick(Instant::now(), keitora_tasks::now_ms());
            std::thread::sleep(TICK);
        });
    if let Err(e) = spawned {
        log::error!("pipeline: could not start its thread: {e}");
    }
}

/// The app's health monitor: real facts, the PATH probe and remedies, findings
/// written through the status publisher and announced on `health://changed`, and
/// worktrees marked dirty by the engine's view changes. Its list is managed for
/// `health_list`.
fn start_health<R: Runtime>(app: &AppHandle<R>, runtime: &DriveRuntime) -> HealthMonitor {
    use crate::health::commands::{HealthState, HEALTH_CHANGED};
    use crate::health::driver::{self, FindingSink, HealthDeps};
    use tauri::Emitter;
    let engine = app.try_state::<SharedEngine>().map(|e| e.inner().clone());
    let sink = app
        .try_state::<Arc<StatusPublisher>>()
        .map(|p| Arc::clone(p.inner()) as Arc<dyn FindingSink>);
    let rt = runtime.clone();
    let emitter = app.clone();
    let monitor = HealthMonitor::new(HealthDeps {
        engine: engine.clone(),
        sink,
        facts: driver::real_facts(),
        version: Arc::new(driver::real_version),
        remedy: driver::real_remedy(Arc::new(move |project: &str| {
            rt.request_pass(project, Duration::ZERO, Trigger::Sweep);
        })),
        spawn: driver::thread_spawn(),
        notify: Arc::new(move |rows| {
            let _ = emitter.emit(HEALTH_CHANGED, rows);
        }),
        remedies_allowed: crate::global_writes::allowed(),
    });
    app.manage(HealthState(monitor.list()));
    if let Some(e) = engine {
        let marks = monitor.dirty();
        e.subscribe(Box::new(move |changes| {
            driver::mark_dirty(&marks, changes, Instant::now())
        }));
    }
    monitor
}

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
