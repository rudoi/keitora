//! The registry of request-carrying database watchers and their supervision.
//!
//! Each `tasks.db` watcher (the active project's, and one background watcher per
//! registered project) is started through [`WatcherRegistry::spawn_db_watcher`], which
//! registers its [`WatcherHealth`] and stop sender here. The pipeline's supervisor
//! calls [`WatcherRegistry::supervise`] on a schedule, which restarts a watcher that
//! exited (with backoff), one whose loop stopped stamping its liveness, and a
//! background watcher that stayed silent while a request it should have seen aged.
//! Restarts go through a per-kind closure that respawns exactly that watcher; the new
//! thread registers its own fresh health.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{mpsc, Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::locks::lock;

pub(crate) use crate::db_watch::{WatchState, WatcherHealth};

/// A running loop that hasn't stamped its liveness for this long is stalled (it
/// wakes every 500 ms).
pub(crate) const HUNG_AFTER_MS: i64 = 5_000;
/// A pending request this old whose project's watcher saw no event since it was
/// filed means the watcher missed it.
pub(crate) const SILENT_AGE_MS: i64 = 15_000;
/// The exit reason of a watcher whose loop stopped stamping its liveness.
const LOOP_STALLED: &str = "loop stalled";
/// Restarts older than this no longer count towards the backoff (nor towards the
/// health monitor's restart findings).
pub(crate) const RESTART_WINDOW_MS: i64 = 10 * 60 * 1000;
const BACKOFF_BASE_MS: i64 = 10_000;
const BACKOFF_MAX_MS: i64 = 120_000;

/// Which watcher of a project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum WatcherKind {
    /// The active project's tasks/sessions watcher (`host.rs`): UI events.
    Tasks,
    /// A registered project's background watcher (`bg_notifier.rs`): notifications,
    /// and the one that carries the project's requests to the pipeline.
    Bg,
}

impl WatcherKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            WatcherKind::Tasks => "tasks",
            WatcherKind::Bg => "bg",
        }
    }

    /// The name people read in messages (`tasks`, `background`).
    pub(crate) fn label(self) -> &'static str {
        match self {
            WatcherKind::Tasks => "tasks",
            WatcherKind::Bg => "background",
        }
    }
}

/// One watcher: its kind and the resolved project name it watches.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct WatcherId {
    pub kind: WatcherKind,
    pub project: String,
}

impl WatcherId {
    pub(crate) fn new(kind: WatcherKind, project: &str) -> WatcherId {
        WatcherId {
            kind,
            project: project.to_string(),
        }
    }
}

struct Entry {
    health: Arc<WatcherHealth>,
    /// Dropping it stops the watcher's thread (its loop sees the channel disconnect).
    stop: Option<mpsc::SyncSender<()>>,
    /// Unix ms of restarts in the last ten minutes.
    restarts: VecDeque<i64>,
    /// The request a "silent" restart was last made for (one restart per request).
    restarted_for_request: Option<i64>,
    /// The supervisor ended the current health's loop as stalled.
    stalled: bool,
    next_restart_not_before_ms: i64,
}

/// Respawns one watcher of a kind for a resolved project; `false` when it couldn't
/// (or respawned a watcher for another project instead).
pub(crate) type RestartFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// A project's oldest pending request, as the supervisor's silent rule reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OldestPending {
    pub id: i64,
    pub created_at_ms: i64,
}

/// Why the supervisor restarted a watcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestartWhy {
    /// It ended on its own (the reason it recorded).
    Exited(String),
    /// Its loop stopped stamping its liveness, so the supervisor ended it.
    Stalled,
    /// It saw no event while request `request` aged `age_s` seconds.
    Silent { request: i64, age_s: i64 },
}

/// One restart the supervisor made: `n` restarts of this watcher in the last ten
/// minutes, this one included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Restarted {
    pub id: WatcherId,
    pub why: RestartWhy,
    pub n: usize,
    /// The restart closure respawned it.
    pub ok: bool,
}

/// The project-level view of its watchers, for presence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WatcherSummary {
    pub state: WatchState,
    pub started_at_ms: Option<i64>,
    pub last_event_at_ms: Option<i64>,
    pub restarts: i64,
    /// The latest error or exit reason, with its time when known.
    pub last_error: Option<(String, Option<i64>)>,
}

/// Every supervised watcher, plus how to restart each kind.
#[derive(Default)]
pub(crate) struct WatcherRegistry {
    entries: Mutex<HashMap<WatcherId, Entry>>,
    restart: Mutex<HashMap<WatcherKind, RestartFn>>,
}

static GLOBAL: LazyLock<Arc<WatcherRegistry>> = LazyLock::new(Arc::default);

/// The process-wide registry the app's watchers register in.
pub(crate) fn global() -> Arc<WatcherRegistry> {
    Arc::clone(&GLOBAL)
}

/// The wait before the next restart once a watcher has restarted `n` times in the
/// window: 10 s, 20 s, 40 s ... capped at 120 s.
fn backoff_ms(n: usize) -> i64 {
    let shift = n.saturating_sub(1).min(16) as u32;
    (BACKOFF_BASE_MS << shift).min(BACKOFF_MAX_MS)
}

/// One project's watchers (`mine`, non-empty) combined into a summary.
fn summarize(mine: &[&Entry], now_ms: i64) -> WatcherSummary {
    let rank = |s: WatchState| match s {
        WatchState::Running => 0,
        WatchState::Starting => 1,
        WatchState::Exited => 2,
        WatchState::Stopped => 3,
    };
    let best = mine
        .iter()
        .min_by_key(|e| {
            (
                rank(e.health.state()),
                std::cmp::Reverse(e.health.started_at_ms()),
            )
        })
        .expect("non-empty");
    let last_error = mine
        .iter()
        .filter_map(|e| {
            e.health
                .last_error()
                .map(|(m, at)| (m, Some(at)))
                .or_else(|| {
                    (e.health.state() == WatchState::Exited)
                        .then(|| e.health.exit_reason().map(|m| (m, None)))
                        .flatten()
                })
        })
        .max_by_key(|(_, at)| at.unwrap_or(0));
    WatcherSummary {
        state: best.health.state(),
        started_at_ms: best.health.started_at_ms(),
        last_event_at_ms: mine
            .iter()
            .filter_map(|e| e.health.last_event_at_ms())
            .max(),
        restarts: mine
            .iter()
            .map(|e| {
                e.restarts
                    .iter()
                    .filter(|at| now_ms - **at < RESTART_WINDOW_MS)
                    .count() as i64
            })
            .sum(),
        last_error,
    }
}

impl WatcherRegistry {
    /// Set how `kind` watchers are respawned.
    pub(crate) fn set_restart(&self, kind: WatcherKind, f: RestartFn) {
        lock(&self.restart).insert(kind, f);
    }

    /// A watcher thread for `id` spawned with `health`, with no stop sender.
    #[cfg(test)]
    pub(crate) fn register(&self, id: WatcherId, health: Arc<WatcherHealth>) {
        self.install(id, health, None);
    }

    /// A watcher thread for `id` spawned with `health` and `stop`. A watcher it
    /// replaces is marked stopped (superseded) before its stop sender drops; the
    /// restart history is kept.
    fn install(
        &self,
        id: WatcherId,
        health: Arc<WatcherHealth>,
        stop: Option<mpsc::SyncSender<()>>,
    ) {
        let mut entries = lock(&self.entries);
        match entries.get_mut(&id) {
            Some(e) => {
                if !Arc::ptr_eq(&e.health, &health) {
                    e.health.mark_stopped();
                }
                e.health = health;
                e.stop = stop;
                e.stalled = false;
            }
            None => {
                entries.insert(
                    id,
                    Entry {
                        health,
                        stop,
                        restarts: VecDeque::new(),
                        restarted_for_request: None,
                        stalled: false,
                        next_restart_not_before_ms: 0,
                    },
                );
            }
        }
    }

    /// Start a supervised watcher of `db_path`'s file family for `id` on its own
    /// thread, superseding (and so stopping) the one it replaces. Its health and stop
    /// sender are registered before anything can fail, so a watcher that never ran is
    /// supervised (and restarted) like one that died. `setup` runs on the thread and
    /// builds the per-burst handler, or says why it can't (the exit reason). An exit
    /// nobody asked for is logged.
    pub(crate) fn spawn_db_watcher<F: FnMut()>(
        &self,
        id: WatcherId,
        db_path: &Path,
        debounce: Duration,
        setup: impl FnOnce(&Arc<WatcherHealth>) -> Result<F, String> + Send + 'static,
    ) {
        // Rendezvous channel (cap 0): only connect/disconnect matters.
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        let health = Arc::new(WatcherHealth::default());
        self.install(id.clone(), Arc::clone(&health), Some(stop_tx));
        let what = format!("{} watcher {}", id.kind.as_str(), id.project);
        let fail = |why: String| {
            log::warn!("{what}: {why}");
            health.mark_exited(why);
        };
        let Some(dir) = db_path.parent().map(Path::to_path_buf) else {
            return fail("the task DB path has no directory".into());
        };
        // Any write to the DB family: `tasks.db`, `-wal`, `-shm`.
        let Some(file) = db_path.file_name().and_then(|n| n.to_str()) else {
            return fail("the task DB path has no file name".into());
        };
        let file = file.to_string();
        let (h, w) = (Arc::clone(&health), what.clone());
        let spawned = std::thread::Builder::new()
            .name(format!("keitora-{}-watcher", id.kind.as_str()))
            .spawn(move || {
                let on_event = match setup(&h) {
                    Ok(f) => f,
                    Err(why) => {
                        log::warn!("{w}: {why}");
                        h.mark_exited(why);
                        return;
                    }
                };
                match crate::db_watch::watch_dir(dir, file, debounce, stop_rx, h, on_event) {
                    crate::db_watch::WatchExit::Stopped => {}
                    exit => log::warn!("{w} ended: {exit:?}"),
                }
            });
        if let Err(e) = spawned {
            fail(format!("could not start its thread: {e}"));
        }
    }

    /// The projects with a registered `kind` watcher, sorted.
    pub(crate) fn projects_of(&self, kind: WatcherKind) -> Vec<String> {
        let mut p: Vec<String> = lock(&self.entries)
            .keys()
            .filter(|id| id.kind == kind)
            .map(|id| id.project.clone())
            .collect();
        p.sort();
        p
    }

    /// Stop and forget the `kind` watchers whose project `keep` rejects.
    pub(crate) fn retain_kind(&self, kind: WatcherKind, keep: impl Fn(&str) -> bool) {
        lock(&self.entries).retain(|id, e| {
            if id.kind != kind || keep(&id.project) {
                return true;
            }
            e.health.mark_stopped();
            false
        });
    }

    /// Stop and forget every watcher of `project` (it left the registry).
    pub(crate) fn remove_project(&self, project: &str) {
        lock(&self.entries).retain(|id, e| {
            if id.project != project {
                return true;
            }
            e.health.mark_stopped();
            false
        });
    }

    /// `id`'s current health.
    #[cfg(test)]
    pub(crate) fn health(&self, id: &WatcherId) -> Option<Arc<WatcherHealth>> {
        lock(&self.entries).get(id).map(|e| Arc::clone(&e.health))
    }

    /// How `project`'s watchers stand, combined (see [`Self::summaries`]). `None`
    /// without a registered watcher.
    #[cfg(test)]
    pub(crate) fn summary(&self, project: &str, now_ms: i64) -> Option<WatcherSummary> {
        self.summaries(now_ms).remove(project)
    }

    /// How each project's watchers stand, combined, by project: the best state
    /// (running, then starting, then exited), the latest event, the restarts in the
    /// window and the latest error.
    pub(crate) fn summaries(&self, now_ms: i64) -> HashMap<String, WatcherSummary> {
        let entries = lock(&self.entries);
        let mut by_project: HashMap<&str, Vec<&Entry>> = HashMap::new();
        for (id, e) in entries.iter() {
            by_project.entry(id.project.as_str()).or_default().push(e);
        }
        by_project
            .into_iter()
            .map(|(p, mine)| (p.to_string(), summarize(&mine, now_ms)))
            .collect()
    }

    /// Decide which watchers to restart at `now_ms` and update their bookkeeping
    /// (restart history, backoff, the request a silent restart was for):
    /// 1. a running loop with no liveness stamp for [`HUNG_AFTER_MS`] is marked exited
    ///    (`loop stalled`);
    /// 2. an exited watcher restarts once its backoff allows (immediately, then
    ///    10 s, 20 s, 40 s ... after each restart in the last ten minutes);
    /// 3. a live background watcher (the one that carries requests) whose project's
    ///    oldest pending request (`oldest`) is older than [`SILENT_AGE_MS`] and was
    ///    filed after both its start and its last event restarts once per request. The sweep serves the
    ///    request either way; the restart keeps the watcher's UI events and
    ///    notifications flowing.
    ///
    /// Stopped watchers are never restarted. Pure apart from the registry itself.
    pub(crate) fn plan(
        &self,
        now_ms: i64,
        oldest: &HashMap<String, OldestPending>,
    ) -> Vec<(WatcherId, RestartWhy, usize)> {
        let mut entries = lock(&self.entries);
        let mut out = Vec::new();
        let mut ids: Vec<WatcherId> = entries.keys().cloned().collect();
        ids.sort();
        for id in ids {
            let e = entries.get_mut(&id).expect("listed");
            e.restarts.retain(|at| now_ms - *at < RESTART_WINDOW_MS);
            let h = &e.health;
            if h.state() == WatchState::Running
                && h.alive_at_ms().is_some_and(|a| now_ms - a > HUNG_AFTER_MS)
            {
                log::warn!(
                    "pipeline: watcher {} {} stopped looping",
                    id.kind.as_str(),
                    id.project
                );
                h.mark_exited(LOOP_STALLED);
                e.stalled = true;
            }
            let why = match h.state() {
                WatchState::Exited if now_ms >= e.next_restart_not_before_ms => {
                    Some(if e.stalled {
                        RestartWhy::Stalled
                    } else {
                        RestartWhy::Exited(h.exit_reason().unwrap_or_else(|| "exited".to_string()))
                    })
                }
                WatchState::Running | WatchState::Starting if id.kind == WatcherKind::Bg => {
                    oldest.get(&id.project).and_then(|o| {
                        let aged = now_ms - o.created_at_ms;
                        // Only a write made while it watched can be missed: a request
                        // filed before this watcher started is the sweep's to serve.
                        let watching_since = h.last_event_at_ms().max(h.started_at_ms());
                        let missed = watching_since.unwrap_or(0) < o.created_at_ms;
                        (aged > SILENT_AGE_MS && missed && e.restarted_for_request != Some(o.id))
                            .then_some(RestartWhy::Silent {
                                request: o.id,
                                age_s: aged / 1000,
                            })
                    })
                }
                _ => None,
            };
            let Some(why) = why else { continue };
            if let RestartWhy::Silent { request, .. } = &why {
                e.restarted_for_request = Some(*request);
            }
            e.restarts.push_back(now_ms);
            let n = e.restarts.len();
            e.next_restart_not_before_ms = now_ms + backoff_ms(n);
            out.push((id, why, n));
        }
        out
    }

    /// Restart what [`Self::plan`] picks, calling each kind's restart closure outside
    /// the registry lock (the respawned thread registers itself). Logs each restart.
    pub(crate) fn supervise(
        &self,
        now_ms: i64,
        oldest: &HashMap<String, OldestPending>,
    ) -> Vec<Restarted> {
        let plan = self.plan(now_ms, oldest);
        let mut done = Vec::with_capacity(plan.len());
        for (id, why, n) in plan {
            let f = lock(&self.restart).get(&id.kind).cloned();
            let ok = f.is_some_and(|f| f(&id.project));
            let failed = if ok { "" } else { " [restart failed]" };
            let what = format!("pipeline: watcher {} {}", id.kind.as_str(), id.project);
            match &why {
                RestartWhy::Exited(reason) => {
                    log::warn!("{what} exited ({reason}); restarted (n={n}){failed}")
                }
                RestartWhy::Stalled => {
                    log::warn!("{what} stalled ({LOOP_STALLED}); restarted (n={n}){failed}")
                }
                RestartWhy::Silent { request, age_s } => log::warn!(
                    "{what} silent while request {request} aged {age_s}s; restarted{failed}"
                ),
            }
            done.push(Restarted { id, why, n, ok });
        }
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const T0: i64 = 1_800_000_000_000;

    fn running() -> Arc<WatcherHealth> {
        let h = Arc::new(WatcherHealth::default());
        h.force_state(WatchState::Running);
        h
    }

    /// A registry whose restart closures count calls and register a fresh running
    /// health stamped at `*now`, as a respawned thread would.
    fn rig() -> (Arc<WatcherRegistry>, Arc<AtomicUsize>, Arc<Mutex<i64>>) {
        let reg: Arc<WatcherRegistry> = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let now = Arc::new(Mutex::new(T0));
        for kind in [WatcherKind::Tasks, WatcherKind::Bg] {
            let (weak, calls, now) = (Arc::downgrade(&reg), Arc::clone(&calls), Arc::clone(&now));
            reg.set_restart(
                kind,
                Arc::new(move |project: &str| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let h = running();
                    let at = *now.lock().unwrap();
                    h.set_times(at, 0);
                    weak.upgrade()
                        .unwrap()
                        .register(WatcherId::new(kind, project), h);
                    true
                }),
            );
        }
        (reg, calls, now)
    }

    /// `proj`'s oldest pending request: `id`, filed at `created_at_ms`.
    fn oldest(id: i64, created_at_ms: i64) -> HashMap<String, OldestPending> {
        HashMap::from([("proj".to_string(), OldestPending { id, created_at_ms })])
    }

    fn exit(reg: &WatcherRegistry, id: &WatcherId) {
        reg.health(id)
            .unwrap()
            .mark_exited("event channel disconnected");
    }

    #[test]
    fn an_exited_watcher_restarts_with_backoff() {
        let (reg, calls, now) = rig();
        let id = WatcherId::new(WatcherKind::Bg, "proj");
        reg.register(id.clone(), running());
        let none = HashMap::new();
        let at = |ms: i64| {
            *now.lock().unwrap() = T0 + ms;
            T0 + ms
        };

        assert!(
            reg.supervise(at(0), &none).is_empty(),
            "running: left alone"
        );
        exit(&reg, &id);
        let r = reg.supervise(at(0), &none);
        assert_eq!(r.len(), 1, "immediately");
        assert_eq!(r[0].n, 1);
        assert_eq!(
            r[0].why,
            RestartWhy::Exited("event channel disconnected".into())
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(reg.health(&id).unwrap().state(), WatchState::Running);

        // It dies again at once: the next restart waits 10 s.
        exit(&reg, &id);
        assert!(reg.supervise(at(9_999), &none).is_empty());
        let r = reg.supervise(at(10_000), &none);
        assert_eq!((r.len(), r[0].n), (1, 2));
        // Then 20 s.
        exit(&reg, &id);
        assert!(reg.supervise(at(29_999), &none).is_empty());
        assert_eq!(reg.supervise(at(30_000), &none)[0].n, 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(reg.summary("proj", T0 + 30_000).unwrap().restarts, 3);
        // Restarts older than ten minutes stop counting.
        assert_eq!(reg.summary("proj", T0 + 700_000).unwrap().restarts, 0);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 30].map(backoff_ms),
            [10_000, 20_000, 40_000, 80_000, 120_000, 120_000, 120_000]
        );
    }

    #[test]
    fn a_stopped_watcher_is_never_restarted() {
        let (reg, calls, _) = rig();
        let id = WatcherId::new(WatcherKind::Tasks, "proj");
        let h = running();
        reg.register(id.clone(), Arc::clone(&h));
        h.mark_stopped();
        h.set_times(T0 - 60_000, 0);
        assert!(reg.supervise(T0, &oldest(7, T0 - 60_000)).is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_hung_loop_is_detected_after_five_seconds_of_stale_liveness() {
        let (reg, calls, _) = rig();
        let id = WatcherId::new(WatcherKind::Bg, "proj");
        let h = running();
        h.set_times(T0, 0);
        reg.register(id.clone(), Arc::clone(&h));
        let none = HashMap::new();
        assert!(
            reg.supervise(T0 + 5_000, &none).is_empty(),
            "5 s: still fine"
        );
        let r = reg.supervise(T0 + 5_001, &none);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].why, RestartWhy::Stalled);
        assert_eq!(
            h.state(),
            WatchState::Stopped,
            "the old health is superseded"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_silent_watcher_restarts_once_per_aging_request() {
        let (reg, calls, _) = rig();
        let id = WatcherId::new(WatcherKind::Bg, "proj");
        let h = running();
        // Alive, last event long before the request.
        h.set_times(T0 + 20_000, T0 - 100_000);
        reg.register(id.clone(), h);
        let req = oldest;
        // 15 s old: not yet.
        assert!(reg.supervise(T0 + 15_000, &req(7, T0)).is_empty());
        let r = reg.supervise(T0 + 15_001, &req(7, T0));
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].why,
            RestartWhy::Silent {
                request: 7,
                age_s: 15
            }
        );
        // The respawned watcher is still silent about request 7: no second restart.
        reg.health(&id)
            .unwrap()
            .set_times(T0 + 40_000, T0 - 100_000);
        assert!(reg.supervise(T0 + 40_000, &req(7, T0)).is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // A newer oldest request it also missed: one more restart.
        reg.health(&id)
            .unwrap()
            .set_times(T0 + 60_000, T0 - 100_000);
        assert_eq!(reg.supervise(T0 + 60_000, &req(8, T0 + 30_000)).len(), 1);
        // A watcher that saw an event after the request was filed is not silent.
        reg.health(&id).unwrap().set_times(T0 + 80_000, T0 + 50_000);
        assert!(reg.supervise(T0 + 80_000, &req(9, T0 + 45_000)).is_empty());
    }

    #[test]
    fn a_watcher_started_after_the_request_was_filed_is_not_silent() {
        let (reg, calls, _) = rig();
        let id = WatcherId::new(WatcherKind::Bg, "proj");
        // Just (re)started, after the request was filed, and no event since.
        let h = running();
        h.set_started(T0 + 10_000);
        h.set_times(T0 + 20_000, 0);
        reg.register(id.clone(), h);
        assert!(reg.supervise(T0 + 20_000, &oldest(7, T0)).is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // A request filed after it started, and missed: restarted.
        reg.health(&id).unwrap().set_times(T0 + 40_000, 0);
        let r = reg.supervise(T0 + 40_000, &oldest(8, T0 + 15_000));
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0].why,
            RestartWhy::Silent {
                request: 8,
                age_s: 25
            }
        );
    }

    #[test]
    fn register_supersedes_and_retain_drops_other_projects() {
        let reg = WatcherRegistry::default();
        let a = WatcherId::new(WatcherKind::Bg, "alpha");
        let b = WatcherId::new(WatcherKind::Bg, "bravo");
        let t = WatcherId::new(WatcherKind::Tasks, "alpha");
        let (ha, hb, ht) = (running(), running(), running());
        reg.register(a.clone(), Arc::clone(&ha));
        reg.register(b.clone(), Arc::clone(&hb));
        reg.register(t.clone(), Arc::clone(&ht));
        let ha2 = running();
        reg.register(a.clone(), Arc::clone(&ha2));
        assert_eq!(ha.state(), WatchState::Stopped, "superseded");
        assert_eq!(ha2.state(), WatchState::Running);

        reg.retain_kind(WatcherKind::Bg, |p| p == "alpha");
        assert_eq!(hb.state(), WatchState::Stopped);
        assert!(reg.health(&b).is_none());
        assert!(reg.health(&t).is_some(), "another kind is untouched");
        assert_eq!(reg.projects_of(WatcherKind::Bg), ["alpha"]);
        assert_eq!(reg.projects_of(WatcherKind::Tasks), ["alpha"]);

        reg.remove_project("alpha");
        assert_eq!(ht.state(), WatchState::Stopped);
        assert!(reg.projects_of(WatcherKind::Bg).is_empty());
        assert!(reg.projects_of(WatcherKind::Tasks).is_empty());
    }

    #[test]
    fn a_superseded_or_dropped_watcher_loses_its_stop_sender() {
        let reg = WatcherRegistry::default();
        let id = WatcherId::new(WatcherKind::Bg, "alpha");
        let (tx1, rx1) = mpsc::sync_channel::<()>(0);
        reg.install(id.clone(), running(), Some(tx1));
        assert_eq!(rx1.try_recv(), Err(mpsc::TryRecvError::Empty), "still live");

        // A respawn replaces the sender: the old thread sees its channel disconnect.
        let (tx2, rx2) = mpsc::sync_channel::<()>(0);
        reg.install(id.clone(), running(), Some(tx2));
        assert_eq!(rx1.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        assert_eq!(rx2.try_recv(), Err(mpsc::TryRecvError::Empty));

        // A project that left the registry: its watcher stops.
        reg.retain_kind(WatcherKind::Bg, |_| false);
        assert_eq!(rx2.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn only_the_background_watcher_is_restarted_for_silence() {
        let (reg, calls, _) = rig();
        for kind in [WatcherKind::Tasks, WatcherKind::Bg] {
            let h = running();
            h.set_times(T0 + 20_000, T0 - 100_000);
            reg.register(WatcherId::new(kind, "proj"), h);
        }
        let r = reg.supervise(T0 + 20_000, &oldest(7, T0));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id.kind, WatcherKind::Bg);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn summary_prefers_a_running_watcher_and_reports_the_latest_error() {
        let reg = WatcherRegistry::default();
        assert_eq!(reg.summary("proj", T0), None);
        let dead = Arc::new(WatcherHealth::default());
        dead.mark_exited("store open failed: locked");
        dead.set_times(0, T0 - 5_000);
        reg.register(WatcherId::new(WatcherKind::Bg, "proj"), dead);
        let s = reg.summary("proj", T0).unwrap();
        assert_eq!(s.state, WatchState::Exited);
        assert_eq!(
            s.last_error,
            Some(("store open failed: locked".to_string(), None))
        );

        let live = running();
        live.set_times(T0, T0 - 1_000);
        live.record_error("notify error: rescan");
        reg.register(WatcherId::new(WatcherKind::Tasks, "proj"), live);
        let s = reg.summary("proj", T0).unwrap();
        assert_eq!(s.state, WatchState::Running);
        assert_eq!(s.last_event_at_ms, Some(T0 - 1_000), "the latest of both");
        assert_eq!(
            s.last_error.as_ref().map(|(m, _)| m.as_str()),
            Some("notify error: rescan"),
            "a timed error wins over an undated exit reason"
        );
    }
}
