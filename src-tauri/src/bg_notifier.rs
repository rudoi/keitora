//! Background-project session notifications.
//!
//! Fires an OS notification when a **non-active** project's session needs input —
//! goes `waiting`, or finishes a turn (`busy`/`waiting` → `idle`). The active
//! project's attention logic stays owned by the frontend `useAttention` hook
//! (no double-fire): the watcher reads the active project from the cached registry
//! snapshot (re-read on every switch) and, for a project that is currently active,
//! *suppresses firing* while still
//! seeding/tracking its session state. Tracking-but-not-firing (rather than an
//! early skip) is what lets a session that went `waiting` while its project was
//! active surface on the first background pass after the user switches away;
//! otherwise the transition is lost and re-seeded as a silent baseline.
//!
//! Each registered project's `tasks.db` is a separate file (`worktree_sessions`
//! has no project column — isolation is physical, per-DB). We open each project's
//! DB independently and spawn one `notify` watcher thread per project DB dir,
//! modelled on `host::start_tasks_watcher` (same 300 ms debounce + the
//! `session_revision()` change-detection that skips heartbeat-only writes).
//!
//! The needs-input decision is a faithful Rust port of the frontend's pure
//! `latchIdleFromTurn` + `needsInput` (`src/lib/attention.ts`). Those two
//! functions are kept I/O-free so the state machine is unit-testable without an
//! `AppHandle`, a database, or the notification plugin (see the `tests` module).

use std::{
    collections::{HashMap, HashSet},
    sync::{mpsc, Arc, LazyLock, Mutex, OnceLock},
    thread,
    time::Duration,
};

use tauri::{AppHandle, Manager, Runtime};

use crate::engine::{HerdrLink, ProjectViews, SessionEngine, SharedEngine, ViewChange};
use crate::project_stores::{self, ReadStores};
use crate::watchers::{WatcherId, WatcherKind};

// ── Pure needs-input logic (ported verbatim from src/lib/attention.ts) ──────────

/// Latch: did this worktree just finish a turn (was busy/waiting, now idle)?
/// - `→busy` or `→offline` clear the latch
/// - `busy→idle` or `waiting→idle` set it (agent finished, awaiting user)
/// - any other transition (including a fresh `idle` with no prior) preserves `was`
///
/// Mirrors `latchIdleFromTurn` in `src/lib/attention.ts`.
fn latch_idle_from_turn(prev: Option<&str>, next: &str, was: bool) -> bool {
    match next {
        "busy" | "offline" => false,
        "idle" if matches!(prev, Some("busy") | Some("waiting")) => true,
        _ => was,
    }
}

/// The state a session feeds to the turn latch. A launching agent reads `busy`, but no
/// turn is running, so it feeds `offline`: the idle that follows its launch never latches
/// "finished". Every other session feeds its own state.
///
/// Mirrors `latchState` in `src/lib/attention.ts`.
fn latch_input_state(session: &keitora_tasks::WorktreeSession) -> &str {
    if session.is_starting() {
        "offline"
    } else {
        session.state.as_str()
    }
}

/// The ball is in the user's court: blocked on a permission prompt (`waiting`),
/// or just finished a turn (`idle` with the turn-latch set).
///
/// A driven worktree is quieter: while its driver is deciding a brokered prompt
/// (`waiting` + `brokered`) the human is not needed, and its finished turns are read by
/// the driver, so a driven `idle` never needs input. An escalated prompt (driven,
/// `waiting`, not brokered) needs the human like any other.
///
/// Mirrors `needsInput` in `src/lib/attention.ts`.
fn needs_input(state: &str, idle_from_turn: bool, driven: bool, brokered: bool) -> bool {
    if driven {
        if state == "waiting" && brokered {
            return false;
        }
        if state == "idle" {
            return false;
        }
    }
    state == "waiting" || (state == "idle" && idle_from_turn)
}

/// Longest escalation reason (in chars) a toast body carries before it is cut.
const TOAST_REASON_MAX: usize = 140;

/// The toast body for a session that needs input: `{driver} escalated: {reason}` for a
/// driven session waiting on an escalated prompt with a reason, else the task title.
fn toast_body(session: &keitora_tasks::WorktreeSession, task_title: String) -> String {
    match (&session.driven_by, &session.escalated_reason) {
        (Some(driver), Some(reason)) if session.state == "waiting" => {
            format!(
                "{driver} escalated: {}",
                truncate_chars(reason, TOAST_REASON_MAX)
            )
        }
        _ => task_title,
    }
}

/// `s` cut to at most `max` chars (never inside a char), with `…` when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    let cut = keitora_tasks::truncate_chars(s, max);
    if cut.len() < s.len() {
        format!("{cut}\u{2026}")
    } else {
        cut
    }
}

/// One worktree's state-map step. Given its prior stored tuple
/// (`(prev_state, was_latch, prev_needs_input)`, or `None` on first observation)
/// and the freshly-read state, return the tuple to store and whether to fire an OS
/// notification.
///
/// While `active`, the frontend's `useAttention` owns surfacing, so we never fire
/// and we pin the stored needs-input flag to `false`. This keeps `prev_state` +
/// latch tracking reality while leaving the rising edge "armed", so the first
/// background pass after a switch-away fires for a session that went `waiting`
/// while its project was active. On the first observation (`prev == None`)
/// we seed a baseline and never fire (mirrors `useAttention`'s seed guard).
///
/// `driven`/`brokered` are the session's current drive facts. The stored
/// `prev_needs_input` already holds the previous result, so a brokered -> escalated
/// flip is a rising edge without any extra state.
fn step_session(
    prev: Option<(&str, bool, bool)>,
    next_state: &str,
    active: bool,
    driven: bool,
    brokered: bool,
) -> ((String, bool, bool), bool) {
    let is_new = prev.is_none();
    let (prev_state, was_latch, prev_needs_input) = prev.unwrap_or((next_state, false, false));
    let next_latch = latch_idle_from_turn(Some(prev_state), next_state, was_latch);
    let now_needs_input = needs_input(next_state, next_latch, driven, brokered);
    let should_fire = !active && !is_new && !prev_needs_input && now_needs_input;
    // Pin to false while active so the first background pass sees a rising edge.
    let stored_needs_input = if active { false } else { now_needs_input };
    (
        (next_state.to_string(), next_latch, stored_needs_input),
        should_fire,
    )
}

// ── Shared latch state ──────────────────────────────────────────────────────────

/// One worktree's latch state: `(prev_state, idle_from_turn_latch, prev_needs_input)`.
type LatchState = (String, bool, bool);

/// Every project's latch state, keyed by resolved project name, then worktree.
///
/// Shared by the per-project DB watchers and the engine subscription: a transition
/// seen by both paths steps the same state, so it toasts once, whichever sees it first.
#[derive(Default)]
struct Latches(Mutex<HashMap<String, HashMap<String, LatchState>>>);

impl Latches {
    /// Step every session of `project` (see [`step_session`]), forget worktrees that
    /// are gone (so a stale latch can't re-trigger if the row comes back), and return
    /// the sessions to toast.
    #[cfg(test)]
    fn step(
        &self,
        project: &str,
        sessions: &[keitora_tasks::WorktreeSession],
        active: bool,
    ) -> Vec<keitora_tasks::WorktreeSession> {
        self.step_built(project, active, || Some(sessions.to_vec()))
    }

    /// Like [`Latches::step`], but builds the session list while holding the latch
    /// lock. Two producers (the DB watcher and the engine worker) therefore apply
    /// lists in the order they were read: an older list can never land after a newer
    /// one. `build` returning `None` leaves the latches untouched.
    fn step_built(
        &self,
        project: &str,
        active: bool,
        build: impl FnOnce() -> Option<Vec<keitora_tasks::WorktreeSession>>,
    ) -> Vec<keitora_tasks::WorktreeSession> {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sessions) = build() else {
            return Vec::new();
        };
        let sessions = sessions.as_slice();
        let state_map = all.entry(project.to_string()).or_default();
        let mut fire = Vec::new();
        for session in sessions {
            let wt = &session.worktree;
            // A stale row is a fallback shown while the engine isn't ready: it proves
            // no transition, so keep the last latched state untouched.
            if session.stale {
                continue;
            }
            let prev_owned = state_map.get(wt.as_str()).cloned();
            let prev = prev_owned.as_ref().map(|(s, l, n)| (s.as_str(), *l, *n));
            // A starting agent's `busy` and its latch input `offline` both need no
            // input, so `needs_input` reads the same either way.
            let (next_tuple, should_fire) = step_session(
                prev,
                latch_input_state(session),
                active,
                session.driven_by.is_some(),
                session.brokered,
            );
            state_map.insert(wt.clone(), next_tuple);
            // Fire only on the false→true edge; suppressed while active and on first
            // observation (seed); see `step_session`.
            if should_fire {
                fire.push(session.clone());
            }
        }
        let live: HashSet<&str> = sessions.iter().map(|s| s.worktree.as_str()).collect();
        state_map.retain(|k, _| live.contains(k.as_str()));
        fire
    }
}

impl Latches {
    /// Forget `project`'s latches: its next pass re-seeds them without toasting.
    fn reset(&self, project: &str) {
        crate::locks::lock(&self.0).remove(project);
    }
}

static LATCHES: LazyLock<Latches> = LazyLock::new(Latches::default);

/// Guards the one engine subscription a process registers.
static ENGINE_SUBSCRIBED: OnceLock<()> = OnceLock::new();

/// A project's sessions as the rail shows them: the engine's views over the hook
/// rows of its DB, read through its cached read-only handle (never creates the
/// file). `None` when the DB can't be read.
fn project_sessions(
    engine: &SessionEngine,
    pc: &keitora_tasks::ProjectConfig,
    stores: &ReadStores,
) -> Option<Vec<keitora_tasks::WorktreeSession>> {
    let project = &pc.project.name;
    let views = engine.views(project);
    stores
        .with(&pc.tasks.db_path, |rs| {
            crate::tasks::reconciled_sessions_in(&rs.store, &views, project, pc)
                .map_err(keitora_tasks::Error::Config)
        })
        .ok()
        .map(|r| r.sessions)
}

/// The projects a batch of view changes touches, sorted, each once.
fn changed_projects(changes: &[ViewChange]) -> Vec<String> {
    let mut projects: Vec<String> = changes.iter().map(|c| c.key.project.clone()).collect();
    projects.sort();
    projects.dedup();
    projects
}

/// Toast the sessions of `resolved` (shown as registry key `project_name`) that now
/// need input. Whether the project is active comes from the registry snapshot
/// (invalidated on every switch): while it is, the
/// frontend owns surfacing, so firing is suppressed but state is still tracked (a
/// session that goes waiting while active stays armed for the first background pass
/// after the user switches away).
fn evaluate_project<R: Runtime>(
    app: &AppHandle<R>,
    project_name: &str,
    resolved: &str,
    build: impl FnOnce() -> Option<Vec<keitora_tasks::WorktreeSession>>,
    db_path: &str,
) {
    let active = project_stores::registry().is_active(project_name);
    let fire = LATCHES.step_built(resolved, active, build);
    if fire.is_empty() {
        return;
    }
    // Tasks are only needed for the notification body.
    let tasks = keitora_tasks::SqliteBackend::open(db_path)
        .ok()
        .and_then(|b| b.list_blocking(&Default::default()).ok());
    for session in fire {
        let task_title = session
            .current_task_id
            .and_then(|tid| {
                tasks
                    .as_ref()?
                    .iter()
                    .find(|t| t.task.id == tid)
                    .map(|t| t.task.title.clone())
            })
            .unwrap_or_else(|| "needs attention".to_string());
        crate::session_toast::show_session_toast(
            app,
            format!("Keitora \u{2014} {project_name}/{}", session.worktree),
            toast_body(&session, task_title),
            crate::session_toast::ToastTarget {
                project: resolved.to_string(),
                worktree: session.worktree.clone(),
            },
        );
    }
}

/// Register the engine subscription once per process: a view change (an agent going
/// waiting or finishing a turn with no DB write at all) is evaluated like a DB
/// event, for every registered project the batch touches. The subscriber only hands
/// the touched project names to a worker thread, so the status loop never waits on
/// the DB reads or the toasts.
fn subscribe_engine<R: Runtime + 'static>(app: &AppHandle<R>) {
    let Some(engine) = app.try_state::<SharedEngine>() else {
        return;
    };
    let engine: SharedEngine = engine.inner().clone();
    ENGINE_SUBSCRIBED.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        // The engine owns its subscribers: hold it weakly to avoid a cycle.
        let weak = Arc::downgrade(&engine);
        let app = app.clone();
        let spawned = thread::Builder::new()
            .name("keitora-bg-notifier".into())
            .spawn(move || {
                while let Some(projects) = next_batch(&rx) {
                    let Some(engine) = weak.upgrade() else {
                        return;
                    };
                    let registry = project_stores::registry();
                    let stores = project_stores::shared();
                    for (key, pc) in &registry.projects {
                        if !projects.contains(&pc.project.name) {
                            continue;
                        }
                        let db_path = pc.tasks.db_path.to_string_lossy();
                        evaluate_project(
                            &app,
                            key,
                            &pc.project.name,
                            || project_sessions(&engine, pc, &stores),
                            &db_path,
                        );
                    }
                }
            });
        if let Err(e) = spawned {
            log::error!("bg notifier: could not start the engine worker: {e}");
            return;
        }
        engine.subscribe(Box::new(move |changes| {
            // A link-only batch changes no session.
            if changes.is_empty() {
                return;
            }
            // The worker only exits with the engine, so a failed send needs nothing.
            let _ = tx.send(changed_projects(changes));
        }));
    });
}

/// The next batch of changed projects: one blocking receive, merged with every batch
/// already queued behind it (sorted, each once). `None` once the sender is gone.
fn next_batch(rx: &mpsc::Receiver<Vec<String>>) -> Option<Vec<String>> {
    let mut projects = rx.recv().ok()?;
    projects.extend(rx.try_iter().flatten());
    projects.sort();
    projects.dedup();
    Some(projects)
}

// ── Watcher orchestration ───────────────────────────────────────────────────────

/// Enumerate every registered project (`UserConfig.projects` →
/// `effective_config` → `tasks.db_path`, so projects without a `keitora.toml`
/// count too) and spawn one watcher thread per project DB. Also registers, once per
/// process, the engine subscription that evaluates projects on view changes.
///
/// Idempotent: each call re-reads the registry snapshot, stops the previous watcher set,
/// and spawns a fresh watcher per project found. Re-calling after onboard/remove
/// therefore does NOT double-watch pre-existing projects (which would duplicate
/// their notifications) and does not accumulate orphan threads. The per-event
/// `active` check (from the registry snapshot, re-read on every switch) handles the active/background boundary dynamically —
/// firing is suppressed while a project is active but its state is still tracked,
/// so no restart on `set_active_project` is needed.
pub fn start_bg_notifier<R: Runtime + 'static>(app: AppHandle<R>) {
    // A restart follows an onboard/remove: read the registry afresh. With no registry
    // yet the set is empty, which stops any prior watchers.
    project_stores::invalidate_registry();
    start_bg_watchers(&app, &project_stores::registry());
}

/// [`start_bg_notifier`] over an already-read `registry` snapshot: one fresh watcher
/// per registered project (each supersedes, and so stops, the one it replaces), and
/// the watchers of projects that left the registry stop.
pub(crate) fn start_bg_watchers<R: Runtime + 'static>(
    app: &AppHandle<R>,
    registry: &project_stores::Registry,
) {
    subscribe_engine(app);
    let registered: HashSet<&str> = registry
        .projects
        .iter()
        .map(|(_, pc)| pc.project.name.as_str())
        .collect();
    crate::watchers::global().retain_kind(WatcherKind::Bg, |p| registered.contains(p));
    for (project_name, pc) in registry.projects.iter().cloned() {
        spawn_project_watcher(app.clone(), project_name, pc);
    }
}

/// Respawn the background watcher of resolved project `project` alone (the
/// supervisor's restart). Its toast latches re-seed silently. `false` when the
/// project is no longer registered.
pub(crate) fn restart_bg_watcher<R: Runtime + 'static>(app: &AppHandle<R>, project: &str) -> bool {
    project_stores::invalidate_registry();
    let registry = project_stores::registry();
    let Some((key, pc)) = registry
        .projects
        .iter()
        .find(|(_, pc)| pc.project.name == project)
        .cloned()
    else {
        crate::watchers::global().remove_project(project);
        return false;
    };
    LATCHES.reset(project);
    spawn_project_watcher(app.clone(), key, pc);
    true
}

/// Whether `registry`'s projects differ from the ones with a background watcher
/// (a project was registered or removed outside the app's own paths).
pub(crate) fn watched_set_changed(registry: &project_stores::Registry) -> bool {
    let mut registered: Vec<String> = registry
        .projects
        .iter()
        .map(|(_, pc)| pc.project.name.clone())
        .collect();
    registered.sort_unstable();
    registered.dedup();
    registered != crate::watchers::global().projects_of(WatcherKind::Bg)
}

/// Watch a single project's DB dir and fire OS notifications for that project's
/// background sessions on the `false → true` edge of needs-input. The same watcher
/// also carries the project's pending drive and dispatch requests to the pipeline,
/// so a request for a registered project runs whether or not it is the active one.
///
/// Modelled on `host::start_tasks_watcher`: same `notify` recommended watcher,
/// 300 ms debounce, and `session_revision()` skip for heartbeat-only writes.
/// `project_name` is the registry key (shown in notifications); `pc.project.name` is
/// the resolved name, which a clicked toast switches to and the pipeline looks it
/// up by. Supersedes the project's previous background watcher.
fn spawn_project_watcher<R: Runtime + 'static>(
    app: AppHandle<R>,
    project_name: String,
    pc: keitora_tasks::ProjectConfig,
) {
    let db_path = pc.tasks.db_path.clone();
    let resolved_name = pc.project.name.clone();
    let id = WatcherId::new(WatcherKind::Bg, &resolved_name);
    let db_path_str = db_path.to_string_lossy().to_string();
    crate::watchers::global().spawn_db_watcher(
        id,
        &db_path,
        Duration::from_millis(300),
        move |_health| {
            // One long-lived handle for the session revision and the session list.
            let local = keitora_tasks::LocalStore::open(&db_path_str)
                .map_err(|e| format!("store open failed: {e}"))?;
            let mut last_session_revision = String::new();
            // One debounced burst per call (each early `return` skips just this burst).
            Ok(move || {
                // Requests don't change session state, so check them before the
                // heartbeat skip below.
                crate::pipeline::on_db_event(&app, &resolved_name);

                // Skip if session states haven't changed (heartbeat-only write).
                let srev = match local.session_revision() {
                    Ok(r) => r,
                    Err(_) => return,
                };
                if srev == last_session_revision {
                    return;
                }
                last_session_revision = srev;

                // The rows with the engine's views laid over them, as the rail shows
                // them; the rows as written (marked stale) while the engine has none.
                // Built under the latch lock so a slower read can't apply late.
                let build = || {
                    let views = match app.try_state::<SharedEngine>() {
                        Some(engine) => engine.views(&resolved_name),
                        None => ProjectViews {
                            ready: false,
                            link: HerdrLink::Starting,
                            views: HashMap::new(),
                        },
                    };
                    crate::tasks::reconciled_sessions_in(&local, &views, &resolved_name, &pc)
                        .ok()
                        .map(|r| r.sessions)
                };
                evaluate_project(&app, &project_name, &resolved_name, build, &db_path_str);
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── latch_idle_from_turn (mirrors attention.test.ts) ────────────────────────

    #[test]
    fn latch_idle_from_turn_sets_on_busy_to_idle() {
        // busy → idle with latch=false: agent finished a turn → latch activates.
        assert!(latch_idle_from_turn(Some("busy"), "idle", false));
    }

    #[test]
    fn latch_idle_from_turn_sets_on_waiting_to_idle() {
        // waiting → idle: agent handled the permission prompt → latch activates.
        assert!(latch_idle_from_turn(Some("waiting"), "idle", false));
    }

    #[test]
    fn latch_idle_from_turn_clears_on_busy() {
        // → busy clears the latch regardless of prev or was.
        assert!(!latch_idle_from_turn(Some("idle"), "busy", true));
    }

    #[test]
    fn latch_idle_from_turn_clears_on_offline() {
        // → offline clears the latch.
        assert!(!latch_idle_from_turn(Some("idle"), "offline", true));
    }

    #[test]
    fn latch_idle_from_turn_preserves_on_idle_to_idle() {
        // idle → idle: latch stays as it was (true stays true).
        assert!(latch_idle_from_turn(Some("idle"), "idle", true));
    }

    #[test]
    fn latch_idle_from_turn_preserves_false_on_idle_to_idle() {
        // idle → idle: latch stays as it was (false stays false).
        assert!(!latch_idle_from_turn(Some("idle"), "idle", false));
    }

    #[test]
    fn latch_idle_from_turn_no_change_on_fresh_idle() {
        // prev=None, next=idle: no prior state — preserve was=false (no spurious latch).
        assert!(!latch_idle_from_turn(None, "idle", false));
    }

    #[test]
    fn latch_idle_from_turn_preserves_on_waiting_to_waiting() {
        // waiting → waiting: stays waiting, latch unchanged.
        assert!(!latch_idle_from_turn(Some("waiting"), "waiting", false));
    }

    // ── needs_input (mirrors attention.test.ts) ─────────────────────────────────

    #[test]
    fn needs_input_waiting() {
        // waiting always needs input regardless of latch.
        assert!(needs_input("waiting", false, false, false));
        assert!(needs_input("waiting", true, false, false));
    }

    #[test]
    fn needs_input_idle_with_latch() {
        // idle + latch=true: just finished a turn, needs input.
        assert!(needs_input("idle", true, false, false));
    }

    #[test]
    fn needs_input_idle_no_latch() {
        // idle + latch=false: fresh/resting idle — does NOT need input.
        assert!(!needs_input("idle", false, false, false));
    }

    #[test]
    fn needs_input_busy() {
        // busy: agent is working — not blocked on user, even if latch somehow true.
        assert!(!needs_input("busy", true, false, false));
    }

    #[test]
    fn needs_input_offline() {
        // offline: session gone — not blocked on user.
        assert!(!needs_input("offline", true, false, false));
    }

    // ── step_session: the per-worktree state-map step (production logic) ─────────

    /// Drive `step_session` for a background project (`active = false`), threading
    /// the stored tuple forward as `prev` — exactly what the watcher loop does.
    fn step_bg(prev: &Option<(String, bool, bool)>, next: &str) -> ((String, bool, bool), bool) {
        let p = prev.as_ref().map(|(s, l, n)| (s.as_str(), *l, *n));
        step_session(p, next, false, false, false)
    }

    #[test]
    fn false_to_true_edge_logic() {
        // Background run across busy→waiting→waiting→busy→idle→idle, driving the
        // real `step_session` (not a hand-inlined copy). Start seeded as "busy".
        let mut st: Option<(String, bool, bool)> = None;
        // Seed: first observation of "busy" never fires.
        let (tup, fire) = step_bg(&st, "busy");
        assert!(!fire, "seed observation must not fire");
        st = Some(tup);

        // busy→waiting → fires (false → true edge).
        let (tup, fire) = step_bg(&st, "waiting");
        assert!(fire, "busy→waiting should fire");
        st = Some(tup);

        // waiting→waiting → must NOT re-fire (already notified).
        let (tup, fire) = step_bg(&st, "waiting");
        assert!(!fire, "waiting→waiting should NOT re-fire");
        st = Some(tup);

        // waiting→busy (new agent turn) → clears needs_input, no fire.
        let (tup, fire) = step_bg(&st, "busy");
        assert!(!fire, "waiting→busy clears needs_input, no fire");
        st = Some(tup);

        // busy→idle → fires again (busy→idle latches a finished turn).
        let (tup, fire) = step_bg(&st, "idle");
        assert!(fire, "busy→idle with latch should fire again (new turn)");
        st = Some(tup);

        // idle→idle → no new turn, no re-fire.
        let (_tup, fire) = step_bg(&st, "idle");
        assert!(!fire, "idle→idle should NOT re-fire");
    }

    #[test]
    fn seed_does_not_fire_even_if_waiting() {
        // First observation of a background worktree already in "waiting": the seed
        // (prev == None) suppresses the fire (mirrors useAttention's seed path).
        let (tup, fire) = step_session(None, "waiting", false, false, false);
        assert!(!fire, "seed observation must not fire");
        assert_eq!(tup.0, "waiting");
    }

    // ── active-window suppression + background handoff ───────────────────

    #[test]
    fn active_window_seeds_but_never_fires() {
        // While active: seed "busy", then go busy→waiting. Neither fires, and the
        // stored needs-input flag stays pinned to false so the edge stays armed.
        let (seed, fire) = step_session(None, "busy", true, false, false);
        assert!(!fire, "active seed must not fire");
        let prev = (seed.0.as_str(), seed.1, seed.2);
        let (after, fire) = step_session(Some(prev), "waiting", true, false, false);
        assert!(
            !fire,
            "busy→waiting while active must not fire (frontend owns it)"
        );
        assert_eq!(after.0, "waiting", "prev_state still tracks reality");
        assert!(!after.2, "stored needs-input pinned false while active");
    }

    #[test]
    fn background_pass_after_active_waiting_fires() {
        // A worktree that went waiting while active (tuple = (waiting, latch=false,
        // needs_input pinned false)). The user switches away → first background pass
        // sees the still-waiting state as a rising edge and fires.
        let active_stored = ("waiting".to_string(), false, false);
        let prev = (active_stored.0.as_str(), active_stored.1, active_stored.2);
        let (_tup, fire) = step_session(Some(prev), "waiting", false, false, false);
        assert!(
            fire,
            "session waiting since switch-time must surface on first bg pass"
        );
    }

    #[test]
    fn single_delayed_pass_fires_when_active_flips_mid_event() {
        // Seeded "busy" while active; the active flag flips to background before the
        // busy→waiting transition is processed → that single pass fires (is_new is
        // false because the worktree was seeded during the active window).
        let (seed, _) = step_session(None, "busy", true, false, false);
        let prev = (seed.0.as_str(), seed.1, seed.2);
        let (_tup, fire) = step_session(Some(prev), "waiting", false, false, false);
        assert!(fire, "busy(seeded active)→waiting(background) should fire");
    }

    #[test]
    fn active_then_active_waiting_stays_armed_across_passes() {
        // Repeated active passes on a stuck-waiting session keep needs-input pinned
        // false, so it never self-suppresses before the handoff to background.
        let mut st = step_session(None, "waiting", true, false, false).0; // active seed, no fire
        for _ in 0..3 {
            let prev = (st.0.as_str(), st.1, st.2);
            let (next, fire) = step_session(Some(prev), "waiting", true, false, false);
            assert!(!fire, "active waiting passes never fire");
            assert!(!next.2, "needs-input stays pinned false while active");
            st = next;
        }
        // Now background: the armed edge fires once.
        let prev = (st.0.as_str(), st.1, st.2);
        let (_n, fire) = step_session(Some(prev), "waiting", false, false, false);
        assert!(fire, "armed waiting edge fires on background handoff");
    }

    // ── driven sessions (mirrors the drive rows of attention.test.ts) ────────────

    #[test]
    fn needs_input_truth_table_with_drive() {
        // (state, latch, driven, brokered) -> needs input
        let cases = [
            ("waiting", false, false, false, true), // not driven, waiting
            ("waiting", false, true, true, false),  // driven, brokered: driver deciding
            ("waiting", false, true, false, true),  // driven, escalated
            ("idle", true, true, false, false),     // driven, latched idle: driver reads it
            ("idle", false, true, false, false),
            ("busy", false, true, false, false),
            ("offline", false, true, false, false),
            ("idle", true, false, false, true), // not driven, finished turn
            ("idle", false, false, false, false),
            // brokered without a driver is not a drive fact: plain waiting
            ("waiting", false, false, true, true),
        ];
        for (state, latch, driven, brokered, want) in cases {
            assert_eq!(
                needs_input(state, latch, driven, brokered),
                want,
                "{state} latch={latch} driven={driven} brokered={brokered}"
            );
        }
    }

    /// Background step with drive facts, threading the stored tuple.
    fn step_drive(
        prev: &Option<(String, bool, bool)>,
        next: &str,
        driven: bool,
        brokered: bool,
    ) -> ((String, bool, bool), bool) {
        let p = prev.as_ref().map(|(s, l, n)| (s.as_str(), *l, *n));
        step_session(p, next, false, driven, brokered)
    }

    #[test]
    fn brokered_waiting_is_quiet_then_escalation_fires() {
        let (tup, _) = step_drive(&None, "busy", true, false);
        let st = Some(tup);
        // busy -> waiting while the driver decides: no toast.
        let (tup, fire) = step_drive(&st, "waiting", true, true);
        assert!(!fire, "brokered waiting must not fire");
        let st = Some(tup);
        let (tup, fire) = step_drive(&st, "waiting", true, true);
        assert!(!fire, "still brokered, still quiet");
        let st = Some(tup);
        // The driver escalates: same state, no longer brokered -> rising edge.
        let (tup, fire) = step_drive(&st, "waiting", true, false);
        assert!(fire, "brokered -> escalated must fire");
        let st = Some(tup);
        let (_, fire) = step_drive(&st, "waiting", true, false);
        assert!(!fire, "escalated waiting fires once");
    }

    #[test]
    fn driven_idle_latch_does_not_fire() {
        let (tup, _) = step_drive(&None, "busy", true, false);
        let (tup, fire) = step_drive(&Some(tup), "idle", true, false);
        assert!(!fire, "a driven finished turn belongs to the driver");
        assert!(tup.1, "the latch itself still tracks the finished turn");
        assert!(!tup.2);
    }

    #[test]
    fn release_of_a_latched_idle_drive_fires() {
        let (tup, _) = step_drive(&None, "busy", true, false);
        let (tup, _) = step_drive(&Some(tup), "idle", true, false);
        // The drive is released: idle with the latch still set now needs the human.
        let (_, fire) = step_drive(&Some(tup), "idle", false, false);
        assert!(fire, "release while latched idle surfaces the unread turn");
    }

    fn session(
        state: &str,
        driven_by: Option<&str>,
        reason: Option<&str>,
    ) -> keitora_tasks::WorktreeSession {
        keitora_tasks::WorktreeSession {
            project: "my-game".into(),
            worktree: "bravo".into(),
            state: state.into(),
            current_task_id: None,
            pid: None,
            last_heartbeat: String::new(),
            driven_by: driven_by.map(str::to_string),
            brokered: false,
            escalated_reason: reason.map(str::to_string),
            agent: None,
            herdr_status: None,
            stale: false,
        }
    }

    #[test]
    fn toast_body_names_the_escalating_driver() {
        let s = session("waiting", Some("alpha"), Some("needs prod credentials"));
        assert_eq!(
            toast_body(&s, "task".into()),
            "alpha escalated: needs prod credentials"
        );
    }

    #[test]
    fn toast_body_falls_back_to_the_task_title() {
        let title = || "Fix the build".to_string();
        // No reason, not driven, or not waiting: the task title.
        assert_eq!(
            toast_body(&session("waiting", Some("alpha"), None), title()),
            "Fix the build"
        );
        assert_eq!(
            toast_body(&session("waiting", None, Some("why")), title()),
            "Fix the build"
        );
        assert_eq!(
            toast_body(&session("idle", Some("alpha"), Some("why")), title()),
            "Fix the build"
        );
    }

    #[test]
    fn toast_body_truncates_the_reason_on_a_char_boundary() {
        // 139 ASCII chars then multi-byte chars: the cut lands after the 140th char,
        // which is a 3-byte char, and must not split it.
        let reason = format!("{}{}", "a".repeat(139), "\u{2603}".repeat(5));
        let body = toast_body(
            &session("waiting", Some("alpha"), Some(&reason)),
            "t".into(),
        );
        let want = format!("alpha escalated: {}\u{2603}\u{2026}", "a".repeat(139));
        assert_eq!(body, want);
        // Exactly 140 chars is not cut.
        let exact = "b".repeat(140);
        let body = toast_body(&session("waiting", Some("alpha"), Some(&exact)), "t".into());
        assert_eq!(body, format!("alpha escalated: {exact}"));
    }

    // ── which projects are watched ──────────────────────────────────────────────

    #[test]
    fn watched_projects_include_toml_less_projects_and_skip_stale_ones() {
        use crate::project_stores::watched_projects;
        let tmp = tempfile::tempdir().unwrap();
        // A registered project with no keitora.toml: watched, at its scaffold DB.
        let bare = tmp.path().join("proj");
        std::fs::create_dir_all(&bare).unwrap();
        // A registered project with a keitora.toml pinning its DB.
        let pinned = tmp.path().join("other-proj");
        std::fs::create_dir_all(&pinned).unwrap();
        std::fs::write(
            pinned.join("keitora.toml"),
            "[project]\nname = \"other-proj\"\n\n[tasks]\ndb_path = \"shared/tasks.db\"\n",
        )
        .unwrap();
        let mut cfg = keitora_tasks::UserConfig::default();
        cfg.register("proj", bare);
        cfg.register("other-proj", pinned);
        cfg.register("gone", tmp.path().join("gone")); // root no longer exists

        let watched = watched_projects(&cfg);
        let names: Vec<(&str, &str)> = watched
            .iter()
            .map(|(k, pc)| (k.as_str(), pc.project.name.as_str()))
            .collect();
        assert_eq!(names, vec![("other-proj", "other-proj"), ("proj", "proj")]);
        // The committed db_path wins where there is one; the bare project gets one too.
        let other_db = watched[0]
            .1
            .tasks
            .db_path
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            other_db.ends_with("other-proj/shared/tasks.db"),
            "{other_db}"
        );
        assert!(watched[1].1.tasks.db_path.ends_with("tasks.db"));
    }

    #[test]
    fn a_latch_reset_reseeds_without_toasting() {
        let latches = Latches::default();
        let row = |state: &str| session(state, None, None);
        assert!(
            latches.step("proj", &[row("busy")], false).is_empty(),
            "seed"
        );
        latches.reset("proj");
        // After a reset the first sight of `waiting` is a silent seed again.
        assert!(latches.step("proj", &[row("waiting")], false).is_empty());
        // A later transition toasts as usual.
        assert!(latches.step("proj", &[row("busy")], false).is_empty());
        assert_eq!(latches.step("proj", &[row("waiting")], false).len(), 1);
    }

    // ── engine overlay and subscription ─────────────────────────────────────────

    /// An agent going `waiting` with no DB write at all toasts through the engine
    /// subscription, exactly once: the DB watcher's later pass over the same state
    /// and an unchanged snapshot add nothing.
    #[test]
    fn engine_only_flip_to_waiting_fires_once() {
        use crate::backend::SessionKey;
        use crate::engine::model::obs;
        use crate::engine::tests::{ingest_settled, FakeEvidence};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut pc = keitora_tasks::ProjectConfig::scaffold(&root);
        pc.effective_worktrees = Some(vec![keitora_tasks::WorktreeEntry::name_only("alpha")]);
        let db = dir.path().join("tasks.db").to_string_lossy().into_owned();
        pc.tasks.db_path = db.clone().into();
        let local = keitora_tasks::LocalStore::open(&db).unwrap();
        local.session_upsert("alpha", "busy", None, None).unwrap();

        let engine = SessionEngine::new_for_test(Arc::new(FakeEvidence::default()));
        let stores = Arc::new(ReadStores::default());
        let latches = Arc::new(Latches::default());
        let fired: Arc<Mutex<Vec<String>>> = Arc::default();
        {
            // The subscription path, as `subscribe_engine` wires it (minus the toast).
            let weak = Arc::downgrade(&engine);
            let (latches, fired, pc) = (Arc::clone(&latches), Arc::clone(&fired), pc.clone());
            let stores = Arc::clone(&stores);
            engine.subscribe(Box::new(move |changes| {
                let engine = weak.upgrade().unwrap();
                if !changed_projects(changes).contains(&pc.project.name) {
                    return;
                }
                let sessions = project_sessions(&engine, &pc, &stores).unwrap();
                for s in latches.step(&pc.project.name, &sessions, false) {
                    fired.lock().unwrap().push(s.worktree);
                }
            }));
        }
        // The DB watcher's path: the same overlay over its own handle.
        let db_pass = || {
            let r =
                crate::tasks::reconciled_sessions_in(&local, &engine.views("proj"), "proj", &pc)
                    .unwrap();
            latches.step("proj", &r.sessions, false).len()
        };
        let alpha = SessionKey {
            project: "proj".into(),
            worktree: "alpha".into(),
        };

        // Working: seeded by the first view, no toast from either path.
        ingest_settled(
            &engine,
            vec![(alpha.clone(), Some(obs("w1:p1", "working", 1, None)))],
        );
        assert_eq!(db_pass(), 0);
        assert!(fired.lock().unwrap().is_empty());

        // Blocked on the user, with no DB write: the subscription toasts once.
        ingest_settled(
            &engine,
            vec![(alpha.clone(), Some(obs("w1:p1", "blocked", 2, None)))],
        );
        assert_eq!(*fired.lock().unwrap(), vec!["alpha".to_string()]);
        // A later DB pass and an unchanged snapshot add nothing.
        assert_eq!(db_pass(), 0);
        ingest_settled(
            &engine,
            vec![(alpha, Some(obs("w1:p1", "blocked", 2, None)))],
        );
        assert_eq!(fired.lock().unwrap().len(), 1);
    }

    #[test]
    fn latches_are_kept_per_project_and_pruned_per_worktree() {
        let latches = Latches::default();
        let s = |wt: &str, state: &str| keitora_tasks::WorktreeSession {
            worktree: wt.into(),
            ..session(state, None, None)
        };
        // Seed both projects busy, then flip only proj's alpha to waiting.
        assert!(latches
            .step("proj", &[s("alpha", "busy")], false)
            .is_empty());
        assert!(latches
            .step("other-proj", &[s("alpha", "busy")], false)
            .is_empty());
        assert_eq!(
            latches.step("proj", &[s("alpha", "waiting")], false).len(),
            1
        );
        assert!(latches
            .step("other-proj", &[s("alpha", "busy")], false)
            .is_empty());
        // A worktree that disappears is forgotten: when it comes back it seeds again.
        assert!(latches.step("proj", &[], false).is_empty());
        assert!(latches
            .step("proj", &[s("alpha", "waiting")], false)
            .is_empty());
    }

    // ── Latch guard: a synthesized idle never latches "finished" ───────────────

    /// The stored turn latch of `wt` in `project`.
    fn latch_of(latches: &Latches, project: &str, wt: &str) -> bool {
        let all = latches.0.lock().unwrap();
        all.get(project).and_then(|m| m.get(wt)).expect(wt).1
    }

    /// An engine view of `bravo` in `proj`.
    fn engine_view(state: &str, agent: &str) -> keitora_tasks::StatusSession {
        keitora_tasks::StatusSession {
            project: "proj".into(),
            worktree: "bravo".into(),
            state: state.into(),
            agent: agent.into(),
            agent_kind: Some("agy".into()),
            herdr_status: Some(state.into()),
            settled: true,
            stale: false,
            interactive_ready: Some(true),
            pane_id: None,
            terminal_id: None,
            agent_session: None,
            state_change_seq: Some(1),
            completion_seq: None,
            brokered: false,
            escalated_reason: None,
            last_change_at: 0,
        }
    }

    /// `rows` overlaid by one engine view of `bravo`, as the rail reads them.
    fn overlaid(
        rows: Vec<keitora_tasks::WorktreeSession>,
        view: keitora_tasks::StatusSession,
    ) -> Vec<keitora_tasks::WorktreeSession> {
        let views = HashMap::from([(view.worktree.clone(), view)]);
        keitora_tasks::overlay_sessions(rows, Some(&views), &[], &HashMap::new())
    }

    #[test]
    fn latch_input_state_maps_starting_to_offline() {
        let mut s = session("busy", None, None);
        assert_eq!(latch_input_state(&s), "busy");
        s.agent = Some("starting".into());
        assert_eq!(latch_input_state(&s), "offline");
        s.agent = Some("present".into());
        s.state = "idle".into();
        assert_eq!(latch_input_state(&s), "idle");
    }

    #[test]
    fn first_seen_synthesized_idle_does_not_latch_finished() {
        let latches = Latches::default();
        let fire = latches.step("proj", &[session("idle", None, None)], false);
        assert!(fire.is_empty());
        assert!(!latch_of(&latches, "proj", "bravo"));
    }

    #[test]
    fn offline_to_idle_lift_does_not_latch_finished() {
        let latches = Latches::default();
        latches.step("proj", &[session("offline", None, None)], false);
        let fire = latches.step("proj", &[session("idle", None, None)], false);
        assert!(fire.is_empty());
        assert!(!latch_of(&latches, "proj", "bravo"));
    }

    #[test]
    fn starting_to_idle_does_not_latch_finished() {
        let latches = Latches::default();
        let starting = overlaid(vec![], engine_view("busy", "starting"));
        assert!(starting[0].is_starting());
        assert_eq!(starting[0].state, "busy");
        latches.step("proj", &starting, false);
        let fire = latches.step("proj", &starting, false);
        assert!(fire.is_empty());
        let ready = overlaid(vec![], engine_view("idle", "present"));
        let fire = latches.step("proj", &ready, false);
        assert!(
            fire.is_empty(),
            "a launch becoming ready is not a finished turn"
        );
        assert!(!latch_of(&latches, "proj", "bravo"));
    }

    #[test]
    fn real_busy_to_idle_still_latches_finished() {
        let latches = Latches::default();
        let busy = overlaid(
            vec![session("busy", None, None)],
            engine_view("busy", "present"),
        );
        latches.step("proj", &busy, false);
        latches.step("proj", &busy, false);
        let idle = overlaid(
            vec![session("idle", None, None)],
            engine_view("idle", "present"),
        );
        let fire = latches.step("proj", &idle, false);
        assert_eq!(fire.len(), 1, "a finished turn toasts in the background");
        assert!(latch_of(&latches, "proj", "bravo"));
        assert!(latches.step("proj", &idle, false).is_empty(), "once");
    }

    #[test]
    fn engine_synthesized_idle_for_a_rowless_agent_never_fires() {
        let latches = Latches::default();
        let idle = overlaid(vec![], engine_view("idle", "present"));
        assert_eq!(idle[0].state, "idle");
        for _ in 0..5 {
            assert!(latches.step("proj", &idle, false).is_empty());
        }
        assert!(!latch_of(&latches, "proj", "bravo"));
    }

    #[test]
    fn stale_row_between_fresh_waiting_views_toasts_once() {
        let latches = Latches::default();
        let s = |state: &str, stale: bool| {
            vec![keitora_tasks::WorktreeSession {
                stale,
                ..session(state, None, None)
            }]
        };
        assert!(latches.step("proj", &s("busy", false), false).is_empty());
        assert_eq!(latches.step("proj", &s("waiting", false), false).len(), 1);
        // The recovery fallback row changes nothing...
        assert!(latches.step("proj", &s("busy", true), false).is_empty());
        // ...so the fresh waiting view is not a second rising edge.
        assert!(latches.step("proj", &s("waiting", false), false).is_empty());
    }

    #[test]
    fn lists_are_built_under_the_latch_lock_so_order_is_preserved() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let latches = Arc::new(Latches::default());
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let inside = Arc::new(AtomicBool::new(false));
        let older = {
            let (latches, log, inside) =
                (Arc::clone(&latches), Arc::clone(&log), Arc::clone(&inside));
            thread::spawn(move || {
                latches.step_built("proj", false, || {
                    inside.store(true, Ordering::SeqCst);
                    log.lock().unwrap().push("older-build");
                    thread::sleep(Duration::from_millis(150));
                    log.lock().unwrap().push("older-built");
                    Some(vec![])
                });
            })
        };
        while !inside.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        // The newer producer cannot read until the older list has been applied.
        latches.step_built("proj", false, || {
            log.lock().unwrap().push("newer-build");
            Some(vec![])
        });
        older.join().unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["older-build", "older-built", "newer-build"]
        );
    }

    #[test]
    fn a_stale_view_causes_no_transition() {
        let latches = Latches::default();
        let busy = overlaid(vec![], engine_view("busy", "present"));
        latches.step("proj", &busy, false);
        let mut stale = engine_view("busy", "present");
        stale.stale = true;
        let stale = overlaid(vec![], stale);
        for _ in 0..3 {
            assert!(latches.step("proj", &stale, false).is_empty());
        }
        assert!(!latch_of(&latches, "proj", "bravo"));
        // The first fresh view compares against the last fresh state (busy).
        let idle = overlaid(vec![], engine_view("idle", "present"));
        assert_eq!(latches.step("proj", &idle, false).len(), 1);
    }

    #[test]
    fn queued_batches_are_merged_into_one() {
        let (tx, rx) = mpsc::channel::<Vec<String>>();
        tx.send(vec!["proj".into()]).unwrap();
        tx.send(vec!["other-proj".into(), "proj".into()]).unwrap();
        assert_eq!(
            next_batch(&rx),
            Some(vec!["other-proj".to_string(), "proj".to_string()])
        );
        tx.send(vec!["proj".into()]).unwrap();
        assert_eq!(next_batch(&rx), Some(vec!["proj".to_string()]));
        drop(tx);
        assert_eq!(next_batch(&rx), None);
    }

    #[test]
    fn changed_projects_are_each_listed_once() {
        use crate::backend::SessionKey;
        let change = |p: &str, w: &str| ViewChange {
            key: SessionKey {
                project: p.into(),
                worktree: w.into(),
            },
            prev: None,
            next: None,
        };
        assert_eq!(
            changed_projects(&[
                change("proj", "b"),
                change("other-proj", "a"),
                change("proj", "a")
            ]),
            vec!["other-proj".to_string(), "proj".to_string()]
        );
    }
}
