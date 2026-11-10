//! Background-project session notifications.
//!
//! Fires an OS notification when a **non-active** project's session needs input —
//! goes `waiting`, or finishes a turn (`busy`/`waiting` → `idle`). The active
//! project's attention logic stays owned by the frontend `useAttention` hook
//! (no double-fire): the watcher reads `UserConfig.active` fresh on every event
//! and, for a project that is currently active, *suppresses firing* while still
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
    sync::{mpsc, Mutex},
    thread,
    time::Duration,
};

use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;

// ── Live watcher registry (idempotent restart) ──────────────────────────────────

/// Stop handles for the currently-live background watcher threads — one
/// `SyncSender<()>` per project watcher.
///
/// `start_bg_notifier` is restartable: each call replaces this whole set
/// (dropping the previous senders). Each previous watcher thread observes its
/// stop receiver disconnect and exits, so re-calling after onboard/remove fully
/// stops the old set and spawns a fresh one — no duplicate watchers (which would
/// otherwise each fire the same notification) and no orphan accumulation. Mirrors
/// `host::TASKS_WATCHER_STOP` / `replace_stop`, generalised to N senders.
static BG_WATCHERS: Mutex<Vec<mpsc::SyncSender<()>>> = Mutex::new(Vec::new());

/// Install `new_senders` as the live watcher set, returning the previous set so
/// the caller can drop it. Dropping the returned senders disconnects each
/// previous watcher thread's stop receiver, signalling it to exit on its next
/// loop iteration (the loop polls with `recv_timeout`, so even an idle DB wakes
/// to observe the disconnect).
///
/// Pulled out as a standalone function so the swap semantics are unit-testable
/// without spawning real `notify` watchers or filesystem threads.
fn replace_bg_watchers(new_senders: Vec<mpsc::SyncSender<()>>) -> Vec<mpsc::SyncSender<()>> {
    let mut guard = BG_WATCHERS.lock().unwrap();
    std::mem::replace(&mut *guard, new_senders)
}

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

/// The ball is in the user's court: blocked on a permission prompt (`waiting`),
/// or just finished a turn (`idle` with the turn-latch set).
///
/// Mirrors `needsInput` in `src/lib/attention.ts`.
fn needs_input(state: &str, idle_from_turn: bool) -> bool {
    state == "waiting" || (state == "idle" && idle_from_turn)
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
fn step_session(
    prev: Option<(&str, bool, bool)>,
    next_state: &str,
    active: bool,
) -> ((String, bool, bool), bool) {
    let is_new = prev.is_none();
    let (prev_state, was_latch, prev_needs_input) = prev.unwrap_or((next_state, false, false));
    let next_latch = latch_idle_from_turn(Some(prev_state), next_state, was_latch);
    let now_needs_input = needs_input(next_state, next_latch);
    let should_fire = !active && !is_new && !prev_needs_input && now_needs_input;
    // Pin to false while active so the first background pass sees a rising edge.
    let stored_needs_input = if active { false } else { now_needs_input };
    (
        (next_state.to_string(), next_latch, stored_needs_input),
        should_fire,
    )
}

// ── Watcher orchestration ───────────────────────────────────────────────────────

/// Enumerate every registered project (`UserConfig.projects` →
/// `ProjectConfig::load` → `tasks.db_path`, the same pattern as
/// `host::list_projects`) and spawn one watcher thread per project DB.
///
/// Idempotent: each call snapshots `UserConfig`, stops the previous watcher set,
/// and spawns a fresh watcher per project found. Re-calling after onboard/remove
/// therefore does NOT double-watch pre-existing projects (which would duplicate
/// their notifications) and does not accumulate orphan threads. The per-event
/// `active` check (read fresh) handles the active/background boundary dynamically —
/// firing is suppressed while a project is active but its state is still tracked,
/// so no restart on `set_active_project` is needed.
pub fn start_bg_notifier<R: Runtime + 'static>(app: AppHandle<R>) {
    let user_cfg = match keitora_tasks::UserConfig::load() {
        Ok(cfg) => cfg,
        Err(_) => {
            // No registry yet — stop any prior set so we don't leave stale watchers.
            drop(replace_bg_watchers(Vec::new()));
            return;
        }
    };
    let mut new_senders = Vec::new();
    for (project_name, project_ref) in &user_cfg.projects {
        let pc = match keitora_tasks::ProjectConfig::load(&project_ref.path) {
            Ok(pc) => pc,
            Err(_) => continue, // stale/unloadable registry entry — skip
        };
        // Rendezvous channel (cap 0): we only care about connect/disconnect, the
        // watcher thread observes its receiver to know when it is superseded.
        let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(0);
        spawn_project_watcher(
            app.clone(),
            project_name.clone(),
            pc.project.name.clone(),
            pc.tasks.db_path.clone(),
            stop_rx,
        );
        new_senders.push(stop_tx);
    }
    // Install the fresh set and drop the previous senders → the old watcher
    // threads observe their receivers disconnect and exit on their next loop.
    drop(replace_bg_watchers(new_senders));
}

/// Watch a single project's DB dir and fire OS notifications for that project's
/// background sessions on the `false → true` edge of needs-input. The same watcher
/// also hands the project's pending CLI dispatch requests to the dispatcher, so a
/// request for a registered project runs whether or not it is the active one.
///
/// Modelled on `host::start_tasks_watcher`: same `notify` recommended watcher,
/// 300 ms debounce, and `session_revision()` skip for heartbeat-only writes.
/// `project_name` is the registry key (shown in notifications); `resolved_name` is
/// the project's `[project].name`, which the dispatcher looks it up by.
fn spawn_project_watcher<R: Runtime + 'static>(
    app: AppHandle<R>,
    project_name: String,
    resolved_name: String,
    db_path: std::path::PathBuf,
    stop_rx: mpsc::Receiver<()>,
) {
    let dir = match db_path.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    // Match tasks.db, tasks.db-wal, tasks.db-shm — any write to the DB family.
    let db_file = match db_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => return,
    };
    let db_path_str = db_path.to_string_lossy().to_string();

    thread::spawn(move || {
        // Two long-lived handles: `LocalStore` for session revision + session list,
        // `SqliteBackend` for task list (notification body only, skipped while active).
        // Both point at the same file (WAL-safe, fully sync — no async runtime needed).
        let local = match keitora_tasks::LocalStore::open(&db_path_str) {
            Ok(s) => s,
            Err(_) => return,
        };
        let backend = keitora_tasks::SqliteBackend::open(&db_path_str).ok();

        let mut last_session_revision = String::new();
        // Per-worktree latch state: (prev_state, idle_from_turn_latch, prev_needs_input).
        let mut state_map: HashMap<String, (String, bool, bool)> = HashMap::new();

        // Shared notify-setup + poll/stop/debounce loop; process one debounced
        // burst per call (each early `return` here skips just this burst).
        crate::db_watch::watch_dir(
            dir,
            db_file,
            Duration::from_millis(300),
            stop_rx,
            move || {
                // CLI dispatch requests don't change session state, so check them
                // before the heartbeat skip below.
                crate::dispatch::spawn_pending_requests(&app, &local, &resolved_name);

                // Skip if session states haven't changed (heartbeat-only write).
                let srev = match local.session_revision() {
                    Ok(r) => r,
                    Err(_) => return,
                };
                if srev == last_session_revision {
                    return;
                }
                last_session_revision = srev;

                // Whether THIS project is the active one — read fresh per pass so a
                // mid-event switch is handled correctly. While active the frontend owns
                // surfacing, so we suppress *firing* but still seed/track state below:
                // a session that goes waiting while active must stay armed to
                // fire on the first background pass after the user switches away.
                let active = keitora_tasks::UserConfig::load()
                    .ok()
                    .and_then(|cfg| cfg.active)
                    .map(|a| a == project_name)
                    .unwrap_or(false);

                // Read all sessions for this project.
                let sessions = match local.sessions_list() {
                    Ok(s) => s,
                    Err(_) => return,
                };

                // Tasks are only needed for the notification body, which we never build
                // while active — skip the query in that case.
                let tasks = if active {
                    None
                } else {
                    backend
                        .as_ref()
                        .and_then(|b| b.list_blocking(&Default::default()).ok())
                };

                for session in &sessions {
                    let wt = &session.worktree;

                    // Root the borrow in an owned local so the `&str` handed to
                    // `step_session` borrows `prev_owned`, not `state_map` — otherwise it
                    // would still be live at the `insert` below and conflict.
                    let prev_owned = state_map.get(wt.as_str()).cloned();
                    let prev = prev_owned.as_ref().map(|(s, l, n)| (s.as_str(), *l, *n));
                    let (next_tuple, should_fire) =
                        step_session(prev, session.state.as_str(), active);
                    state_map.insert(wt.clone(), next_tuple);

                    // Fire only on the false→true edge; suppressed while active and on
                    // first observation (seed) — see `step_session`.
                    if should_fire {
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

                        let _ = app
                            .notification()
                            .builder()
                            .title(format!("Keitora \u{2014} {project_name}/{wt}"))
                            .body(task_title)
                            .show();
                    }
                }

                // Prune worktrees no longer in the DB so a stale latch can't re-trigger
                // if the same row is later re-inserted.
                let live: HashSet<&str> = sessions.iter().map(|s| s.worktree.as_str()).collect();
                state_map.retain(|k, _| live.contains(k.as_str()));
            },
        );
    });
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
        assert!(needs_input("waiting", false));
        assert!(needs_input("waiting", true));
    }

    #[test]
    fn needs_input_idle_with_latch() {
        // idle + latch=true: just finished a turn, needs input.
        assert!(needs_input("idle", true));
    }

    #[test]
    fn needs_input_idle_no_latch() {
        // idle + latch=false: fresh/resting idle — does NOT need input.
        assert!(!needs_input("idle", false));
    }

    #[test]
    fn needs_input_busy() {
        // busy: agent is working — not blocked on user, even if latch somehow true.
        assert!(!needs_input("busy", true));
    }

    #[test]
    fn needs_input_offline() {
        // offline: session gone — not blocked on user.
        assert!(!needs_input("offline", true));
    }

    // ── step_session: the per-worktree state-map step (production logic) ─────────

    /// Drive `step_session` for a background project (`active = false`), threading
    /// the stored tuple forward as `prev` — exactly what the watcher loop does.
    fn step_bg(prev: &Option<(String, bool, bool)>, next: &str) -> ((String, bool, bool), bool) {
        let p = prev.as_ref().map(|(s, l, n)| (s.as_str(), *l, *n));
        step_session(p, next, false)
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
        let (tup, fire) = step_session(None, "waiting", false);
        assert!(!fire, "seed observation must not fire");
        assert_eq!(tup.0, "waiting");
    }

    // ── active-window suppression + background handoff ───────────────────

    #[test]
    fn active_window_seeds_but_never_fires() {
        // While active: seed "busy", then go busy→waiting. Neither fires, and the
        // stored needs-input flag stays pinned to false so the edge stays armed.
        let (seed, fire) = step_session(None, "busy", true);
        assert!(!fire, "active seed must not fire");
        let prev = (seed.0.as_str(), seed.1, seed.2);
        let (after, fire) = step_session(Some(prev), "waiting", true);
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
        let (_tup, fire) = step_session(Some(prev), "waiting", false);
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
        let (seed, _) = step_session(None, "busy", true);
        let prev = (seed.0.as_str(), seed.1, seed.2);
        let (_tup, fire) = step_session(Some(prev), "waiting", false);
        assert!(fire, "busy(seeded active)→waiting(background) should fire");
    }

    #[test]
    fn active_then_active_waiting_stays_armed_across_passes() {
        // Repeated active passes on a stuck-waiting session keep needs-input pinned
        // false, so it never self-suppresses before the handoff to background.
        let mut st = step_session(None, "waiting", true).0; // active seed, no fire
        for _ in 0..3 {
            let prev = (st.0.as_str(), st.1, st.2);
            let (next, fire) = step_session(Some(prev), "waiting", true);
            assert!(!fire, "active waiting passes never fire");
            assert!(!next.2, "needs-input stays pinned false while active");
            st = next;
        }
        // Now background: the armed edge fires once.
        let prev = (st.0.as_str(), st.1, st.2);
        let (_n, fire) = step_session(Some(prev), "waiting", false);
        assert!(fire, "armed waiting edge fires on background handoff");
    }

    // ── idempotent restart: the live-watcher registry swap ──────────────────────

    /// `replace_bg_watchers` installs the new sender set and returns the previous
    /// one so the caller can drop it (stopping the old threads). After replacement
    /// the global slot holds the *live* senders, and the returned previous senders,
    /// once dropped, disconnect their receivers. This is the exact handoff
    /// `start_bg_notifier` performs on every restart, and is what makes a re-call
    /// idempotent — the prior watcher set is stopped, never duplicated.
    #[test]
    fn bg_watchers_replaced_on_restart_stops_prior_set() {
        // Start from a known-clean slot (no live watcher runs in unit tests, but
        // be defensive against prior state so the test is order-independent).
        replace_bg_watchers(Vec::new());

        // First install: two project watchers' senders. Slot was just cleared, so
        // no previous senders.
        let (tx1, rx1) = mpsc::sync_channel::<()>(0);
        let (tx2, rx2) = mpsc::sync_channel::<()>(0);
        let prev0 = replace_bg_watchers(vec![tx1, tx2]);
        assert!(
            prev0.is_empty(),
            "install after clear should report no prior senders"
        );

        // Slot now holds the two live senders; both receivers are still connected.
        assert_eq!(
            BG_WATCHERS.lock().unwrap().len(),
            2,
            "slot holds the live set"
        );
        assert_eq!(rx1.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(rx2.try_recv(), Err(mpsc::TryRecvError::Empty));

        // Second install (a restart) replaces the set and returns the old senders.
        let (tx3, _rx3) = mpsc::sync_channel::<()>(0);
        let prev1 = replace_bg_watchers(vec![tx3]);
        assert_eq!(prev1.len(), 2, "restart must return the previous set");

        // Dropping the previous senders disconnects rx1/rx2 → both old watcher
        // threads would exit. This is what prevents duplicate watchers (and thus
        // duplicate notifications) for pre-existing projects on re-call.
        drop(prev1);
        assert_eq!(
            rx1.try_recv(),
            Err(mpsc::TryRecvError::Disconnected),
            "old watcher 1's receiver must disconnect once the prior set is dropped",
        );
        assert_eq!(
            rx2.try_recv(),
            Err(mpsc::TryRecvError::Disconnected),
            "old watcher 2's receiver must disconnect once the prior set is dropped",
        );

        // Clean up the global slot so this test leaves no live sender behind.
        replace_bg_watchers(Vec::new());
    }
}
