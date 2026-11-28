//! The pure per-key derivation: one herdr `agent list` entry plus the worktree's hook
//! row and the key's memory -> one [`View`]. No I/O and no clock reads; the caller
//! passes `now`.
//!
//! herdr's `agent list` is the primary signal for presence and status. Hook rows are
//! evidence only: a `busy`/`waiting` row strictly newer than herdr's last status
//! change vetoes an `idle`, and nothing a hook writes can make a present agent
//! `offline` or an absent one live.

use std::time::Instant;

use super::model::{AgentObs, HookEvidence, KeyMemory, Presence, View};
use super::{LAUNCH_GRACE, SETTLE};
use crate::backend::herdr::ops::Generation;

/// Everything [`derive`] reads for one key.
#[derive(Clone, Copy, Debug)]
pub struct DeriveInput<'a> {
    /// The pane's `agent list` entry; `None` when herdr has no entry for it.
    pub obs: Option<&'a AgentObs>,
    /// The worktree's hook row, if any.
    pub hook: Option<&'a HookEvidence>,
    /// The key's memory from the previous snapshot.
    pub prev: Option<&'a KeyMemory>,
    /// When Keitora last started a launch for the key that has not finished.
    pub launch_noted: Option<Instant>,
    /// The keymap generation of this snapshot.
    pub generation: Generation,
    pub now: Instant,
    pub now_ms: i64,
}

/// The derived view and the memory to carry into the next snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derived {
    pub view: View,
    pub memory: KeyMemory,
}

/// Derive one key's view from herdr's entry, the hook row and the key's memory.
pub fn derive(input: &DeriveInput) -> Derived {
    let now = input.now;
    let now_ms = input.now_ms;
    // A new server resets settling and seq tracking.
    let prev = input.prev.filter(|p| p.generation == input.generation);
    let obs = input.obs;

    let scs_now = obs.map(|o| o.state_change_seq);
    let herdr_changed = match prev {
        Some(p) if p.last_scs == scs_now => p.herdr_changed,
        _ => (now, now_ms),
    };

    // A row written by a previous occupant of the pane is not evidence about this one.
    let hook = input
        .hook
        .filter(|h| !h.is_from_other_session(obs.and_then(|o| o.agent_session.as_deref())));

    // The previous state, when the previous view had an agent.
    let prev_state = prev
        .filter(|p| p.view.agent != Presence::Absent)
        .map(|p| p.view.state);

    let mut idle_first = None;
    let mut unknown = false;
    let mut settled = false;

    let occupant = obs.filter(|o| o.agent.is_some() || o.launch_pending);
    let (presence, state): (Presence, &'static str) = match occupant {
        None => {
            let launching = input
                .launch_noted
                .is_some_and(|t| now.saturating_duration_since(t) <= LAUNCH_GRACE);
            if launching {
                (Presence::Starting, "busy")
            } else {
                (Presence::Absent, "offline")
            }
        }
        Some(o) if o.launch_pending || o.interactive_ready == Some(false) => {
            (Presence::Starting, "busy")
        }
        Some(o) => match o.agent_status.as_str() {
            "working" => (Presence::Present, "busy"),
            "blocked" => (Presence::Present, "waiting"),
            "idle" | "done" => {
                let scs = o.state_change_seq;
                match prev.and_then(|p| p.idle_first) {
                    Some((t, s)) if s == scs => {
                        idle_first = Some((t, s));
                        settled = now.saturating_duration_since(t) >= SETTLE;
                    }
                    _ => idle_first = Some((now, scs)),
                }
                let veto = hook.and_then(|h| {
                    let state = match h.state.as_str() {
                        "busy" => "busy",
                        "waiting" => "waiting",
                        _ => return None,
                    };
                    let newer = h
                        .written_ms
                        .is_some_and(|w| w.div_euclid(1000) > herdr_changed.1.div_euclid(1000));
                    newer.then_some(state)
                });
                let state = if let Some(state) = veto {
                    settled = false;
                    state
                } else if settled {
                    "idle"
                } else {
                    prev_state
                        .filter(|s| matches!(*s, "busy" | "waiting" | "idle"))
                        .unwrap_or("idle")
                };
                (Presence::Present, state)
            }
            _ => {
                unknown = true;
                (Presence::Present, prev_state.unwrap_or("busy"))
            }
        },
    };

    // Identity comes from herdr's entry whenever the pane has an agent (or a launch
    // in flight); an absent agent has none.
    let ident = obs.filter(|_| presence != Presence::Absent);
    let waiting_hook = hook.filter(|_| state == "waiting");
    let mut view = View {
        state,
        agent: presence,
        agent_kind: ident.and_then(|o| o.agent.clone()),
        herdr_status: ident.map(|o| o.agent_status.clone()),
        settled,
        stale: false,
        interactive_ready: ident.and_then(|o| o.interactive_ready),
        pane_id: ident.map(|o| o.pane_id.clone()),
        terminal_id: ident.map(|o| o.terminal_id.clone()),
        agent_session: ident.and_then(|o| o.agent_session.clone()),
        state_change_seq: ident.map(|o| o.state_change_seq),
        completion_seq: ident.and_then(|o| o.completion_seq),
        brokered: waiting_hook.is_some_and(|h| h.brokered),
        escalated_reason: waiting_hook.and_then(|h| h.escalated_reason.clone()),
        last_change_at_ms: now_ms,
    };
    if let Some(p) = prev.filter(|p| p.view.same_content(&view)) {
        view.last_change_at_ms = p.view.last_change_at_ms;
    }

    let present_since_ms = (presence == Presence::Present).then(|| {
        prev.filter(|p| p.view.agent == Presence::Present)
            .and_then(|p| p.present_since_ms)
            .unwrap_or(now_ms)
    });
    // Kept from the first unknown snapshot of the run; cleared by any known status.
    let (unknown_since, unknown_since_ms) = match prev.filter(|p| p.unknown_since.is_some()) {
        _ if !unknown => (None, None),
        Some(p) => (p.unknown_since, p.unknown_since_ms.or(Some(now_ms))),
        None => (Some(now), Some(now_ms)),
    };

    let memory = KeyMemory {
        view: view.clone(),
        idle_first,
        herdr_changed,
        last_scs: scs_now,
        unknown_since,
        unknown_since_ms,
        present_since_ms,
        generation: input.generation,
    };
    Derived { view, memory }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::engine::model::obs;

    const T0_MS: i64 = 1_700_000_000_000;

    /// Feeds snapshots to [`derive`] one after another, carrying memory, on a fake
    /// clock that starts at `base`/`T0_MS` and only moves when told.
    struct Clock {
        base: Instant,
        mem: Option<KeyMemory>,
        generation: Generation,
        launch_noted: Option<Instant>,
    }

    impl Clock {
        fn new() -> Self {
            Clock {
                base: Instant::now(),
                mem: None,
                generation: Generation(1),
                launch_noted: None,
            }
        }

        fn at(&self, ms: u64) -> Instant {
            self.base + Duration::from_millis(ms)
        }

        /// One snapshot at `ms` after the start.
        fn step(&mut self, ms: u64, o: Option<&AgentObs>, hook: Option<&HookEvidence>) -> View {
            let d = derive(&DeriveInput {
                obs: o,
                hook,
                prev: self.mem.as_ref(),
                launch_noted: self.launch_noted,
                generation: self.generation,
                now: self.at(ms),
                now_ms: T0_MS + ms as i64,
            });
            self.mem = Some(d.memory);
            d.view
        }

        fn mem(&self) -> &KeyMemory {
            self.mem.as_ref().unwrap()
        }
    }

    /// One derive with no memory.
    fn once(o: Option<&AgentObs>, hook: Option<&HookEvidence>) -> View {
        Clock::new().step(0, o, hook)
    }

    /// A hook row in `state` written at `ms` after the start.
    fn row(state: &str, ms: i64) -> HookEvidence {
        HookEvidence {
            state: state.to_string(),
            written_ms: Some(T0_MS + ms),
            brokered: false,
            escalated_reason: None,
            session_id: None,
        }
    }

    fn agy(status: &str, scs: u64) -> AgentObs {
        AgentObs {
            agent: Some("agy".into()),
            agent_session_source: Some("herdr:antigravity_cli".into()),
            ..obs("w1:p1", status, scs, None)
        }
    }

    fn bare_shell() -> AgentObs {
        AgentObs {
            agent: None,
            agent_session: None,
            agent_session_kind: None,
            agent_session_source: None,
            ..obs("w1:p1", "unknown", 0, None)
        }
    }

    /// Settle `o` (an idle entry) over two snapshots a second apart, ending at 1 s.
    fn settle(c: &mut Clock, o: &AgentObs, hook: Option<&HookEvidence>) -> View {
        c.step(0, Some(o), hook);
        c.step(1_000, Some(o), hook)
    }

    #[test]
    fn absent_agent_is_offline_whatever_the_row_says() {
        for state in [None, Some("idle"), Some("busy"), Some("waiting")] {
            let hook = state.map(|s| row(s, 5_000));
            let v = once(None, hook.as_ref());
            assert_eq!(v.state, "offline", "row {state:?}");
            assert_eq!(v.agent, Presence::Absent);
            assert_eq!(v.pane_id, None);
            assert_eq!(v.terminal_id, None);
            assert_eq!(v.agent_session, None);
            assert_eq!(v.state_change_seq, None);
            assert_eq!(v.completion_seq, None);
            assert!(!v.brokered);
            assert_eq!(v.escalated_reason, None);
        }
    }

    #[test]
    fn bare_shell_entry_without_agent_is_offline() {
        let v = once(Some(&bare_shell()), None);
        assert_eq!(v.state, "offline");
        assert_eq!(v.agent, Presence::Absent);
        assert_eq!(v.pane_id, None);
    }

    #[test]
    fn bare_shell_with_an_idle_row_is_offline() {
        let v = once(Some(&bare_shell()), Some(&row("idle", 5_000)));
        assert_eq!(v.state, "offline");
        assert_eq!(v.agent, Presence::Absent);
    }

    #[test]
    fn noted_launch_without_entry_is_starting_until_grace() {
        for (ms, want) in [
            (59_000, (Presence::Starting, "busy")),
            (61_000, (Presence::Absent, "offline")),
        ] {
            let mut c = Clock::new();
            c.launch_noted = Some(c.base);
            let v = c.step(ms, None, None);
            assert_eq!((v.agent, v.state), want, "at {ms} ms");
        }
        // A bare shell entry is no agent either: the noted launch still reads Starting.
        let mut c = Clock::new();
        c.launch_noted = Some(c.base);
        let v = c.step(1_000, Some(&bare_shell()), None);
        assert_eq!((v.agent, v.state), (Presence::Starting, "busy"));
    }

    #[test]
    fn launch_pending_or_not_ready_is_starting_busy() {
        let pending = AgentObs {
            agent: None,
            launch_pending: true,
            ..obs("w1:p1", "unknown", 0, None)
        };
        let not_ready = AgentObs {
            interactive_ready: Some(false),
            ..obs("w1:p1", "idle", 3, None)
        };
        for o in [pending, not_ready] {
            let v = once(Some(&o), Some(&row("idle", 5_000)));
            assert_eq!(v.agent, Presence::Starting);
            assert_eq!(v.state, "busy");
            assert!(!v.settled);
            assert_eq!(v.pane_id.as_deref(), Some("w1:p1"));
        }
    }

    #[test]
    fn interactive_ready_none_counts_as_ready() {
        let o = AgentObs {
            interactive_ready: None,
            ..obs("w1:p1", "idle", 3, None)
        };
        let mut c = Clock::new();
        let v = settle(&mut c, &o, None);
        assert_eq!(v.agent, Presence::Present);
        assert_eq!(v.state, "idle");
        assert!(v.settled);
        assert_eq!(v.interactive_ready, None);
    }

    #[test]
    fn working_is_busy_blocked_is_waiting() {
        let v = once(Some(&obs("w1:p1", "working", 3, None)), None);
        assert_eq!(
            (v.agent, v.state, v.settled),
            (Presence::Present, "busy", false)
        );
        assert_eq!(v.herdr_status.as_deref(), Some("working"));
        let v = once(Some(&obs("w1:p1", "blocked", 3, None)), None);
        assert_eq!(
            (v.agent, v.state, v.settled),
            (Presence::Present, "waiting", false)
        );
    }

    #[test]
    fn idle_settles_after_two_snapshots_a_second_apart() {
        let mut c = Clock::new();
        let idle5 = obs("w1:p1", "idle", 5, Some(5));
        let v = c.step(0, Some(&idle5), None);
        assert!(!v.settled);
        let v = c.step(500, Some(&idle5), None);
        assert!(!v.settled);
        let v = c.step(1_000, Some(&idle5), None);
        assert!(v.settled);
        assert_eq!(v.state, "idle");
        let v = c.step(2_000, Some(&obs("w1:p1", "idle", 6, Some(5))), None);
        assert!(!v.settled, "a new seq restarts settling");
        assert_eq!(c.mem().idle_first, Some((c.at(2_000), 6)));
        // Any non-idle status clears the idle run.
        c.step(3_000, Some(&obs("w1:p1", "working", 7, Some(5))), None);
        assert_eq!(c.mem().idle_first, None);
    }

    #[test]
    fn done_counts_as_idle() {
        let mut c = Clock::new();
        let v = settle(&mut c, &obs("w1:p1", "done", 4, Some(4)), None);
        assert_eq!((v.state, v.settled), ("idle", true));
        assert_eq!(v.herdr_status.as_deref(), Some("done"));
    }

    #[test]
    fn unsettled_idle_keeps_the_previous_busy() {
        let mut c = Clock::new();
        c.step(0, Some(&obs("w1:p1", "working", 3, None)), None);
        let v = c.step(1_000, Some(&obs("w1:p1", "idle", 4, Some(4))), None);
        assert_eq!((v.state, v.settled), ("busy", false));
        let v = c.step(2_000, Some(&obs("w1:p1", "idle", 4, Some(4))), None);
        assert_eq!((v.state, v.settled), ("idle", true));
    }

    #[test]
    fn first_unsettled_idle_reads_idle() {
        let v = once(Some(&obs("w1:p1", "idle", 4, None)), None);
        assert_eq!(
            (v.agent, v.state, v.settled),
            (Presence::Present, "idle", false)
        );
    }

    #[test]
    fn unknown_keeps_the_last_state_and_first_unknown_is_busy() {
        let v = once(Some(&obs("w1:p1", "unknown", 2, None)), None);
        assert_eq!((v.agent, v.state), (Presence::Present, "busy"));

        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 2, None);
        settle(&mut c, &idle, None);
        let v = c.step(2_000, Some(&obs("w1:p1", "unknown", 3, None)), None);
        assert_eq!(v.state, "idle", "keeps the last state");
        assert!(!v.settled);
        assert_eq!(c.mem().unknown_since, Some(c.at(2_000)));
        assert_eq!(c.mem().unknown_since_ms, Some(T0_MS + 2_000));
        // An unrecognised status string reads as unknown; the first entry is kept.
        let v = c.step(3_000, Some(&obs("w1:p1", "thinking-hard", 3, None)), None);
        assert_eq!(v.state, "idle");
        assert_eq!(c.mem().unknown_since, Some(c.at(2_000)));
        assert_eq!(c.mem().unknown_since_ms, Some(T0_MS + 2_000));
        c.step(4_000, Some(&obs("w1:p1", "working", 4, None)), None);
        assert_eq!(c.mem().unknown_since, None);
        assert_eq!(c.mem().unknown_since_ms, None);

        // After an absent view, unknown reads busy.
        let mut c = Clock::new();
        c.step(0, None, None);
        let v = c.step(1_000, Some(&obs("w1:p1", "unknown", 1, None)), None);
        assert_eq!(v.state, "busy");
    }

    #[test]
    fn hook_busy_newer_than_the_herdr_change_vetoes_idle() {
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 5, Some(5));
        settle(&mut c, &idle, None);
        // herdr's seq 5 was first seen at 0 ms; the row is from the next second.
        let busy = row("busy", 1_000);
        let v = c.step(2_000, Some(&idle), Some(&busy));
        assert_eq!((v.state, v.settled), ("busy", false));
        // A new herdr change after the row ends the veto (once it settles).
        let idle6 = obs("w1:p1", "idle", 6, Some(6));
        c.step(3_000, Some(&idle6), Some(&busy));
        let v = c.step(4_000, Some(&idle6), Some(&busy));
        assert_eq!((v.state, v.settled), ("idle", true));
    }

    #[test]
    fn hook_busy_in_the_same_second_loses() {
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 5, Some(5));
        c.step(200, Some(&idle), None);
        // Same whole second as herdr's change (200 ms vs 900 ms): not strictly newer.
        let busy = row("busy", 900);
        let v = c.step(1_200, Some(&idle), Some(&busy));
        assert_eq!((v.state, v.settled), ("idle", true));
    }

    #[test]
    fn hook_waiting_newer_vetoes_idle_as_waiting() {
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 5, Some(5));
        settle(&mut c, &idle, None);
        let v = c.step(3_000, Some(&idle), Some(&row("waiting", 2_000)));
        assert_eq!((v.state, v.settled), ("waiting", false));
    }

    #[test]
    fn hook_idle_or_offline_never_overrides_presence_or_working() {
        for status in ["idle", "working", "blocked", "unknown"] {
            for hook_state in ["idle", "offline"] {
                let mut c = Clock::new();
                let o = obs("w1:p1", status, 5, None);
                let base = settle(&mut c, &o, None);
                let v = c.step(2_000, Some(&o), Some(&row(hook_state, 1_500)));
                assert_eq!(v.agent, Presence::Present, "{status} + {hook_state}");
                assert_eq!(v.state, base.state, "{status} + {hook_state}");
                assert_eq!(v.settled, base.settled, "{status} + {hook_state}");
            }
        }
    }

    #[test]
    fn stale_busy_after_a_missed_stop_loses_to_the_next_herdr_change() {
        let mut c = Clock::new();
        // The hook wrote busy at turn start; the Stop hook never came.
        let busy = row("busy", 500);
        c.step(0, Some(&obs("w1:p1", "working", 4, None)), Some(&busy));
        // herdr's idle change comes later than the row: the row is older evidence.
        let idle = obs("w1:p1", "idle", 5, Some(5));
        c.step(2_000, Some(&idle), Some(&busy));
        let v = c.step(3_000, Some(&idle), Some(&busy));
        assert_eq!((v.state, v.settled), ("idle", true));
    }

    #[test]
    fn late_session_end_after_reset_leaves_the_view_idle() {
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 2, None);
        settle(&mut c, &idle, None);
        // The previous occupant's SessionEnd lands after herdr already shows idle.
        let v = c.step(5_000, Some(&idle), Some(&row("offline", 4_000)));
        assert_eq!(
            (v.agent, v.state, v.settled),
            (Presence::Present, "idle", true)
        );
    }

    #[test]
    fn missing_row_with_agy_present_and_idle_reads_idle() {
        let mut c = Clock::new();
        let o = agy("idle", 3);
        let v = c.step(0, Some(&o), None);
        assert_eq!((v.agent, v.state), (Presence::Present, "idle"));
        let v = c.step(1_000, Some(&o), None);
        assert_eq!((v.state, v.settled), ("idle", true));
        assert_eq!(v.agent_kind.as_deref(), Some("agy"));
    }

    #[test]
    fn stale_offline_row_with_an_agent_present_reads_idle() {
        let mut c = Clock::new();
        let row = row("offline", -60_000);
        let v = settle(&mut c, &obs("w1:p1", "idle", 3, None), Some(&row));
        assert_eq!(
            (v.agent, v.state, v.settled),
            (Presence::Present, "idle", true)
        );
    }

    #[test]
    fn a_stale_previous_view_never_turns_offline() {
        let mut c = Clock::new();
        c.step(0, Some(&obs("w1:p1", "working", 3, None)), None);
        // The engine marks the remembered view stale while herdr is unreachable.
        c.mem.as_mut().unwrap().view.stale = true;
        let v = c.step(30_000, Some(&obs("w1:p1", "idle", 4, None)), None);
        assert_eq!(
            (v.agent, v.state, v.stale),
            (Presence::Present, "busy", false)
        );
    }

    #[test]
    fn generation_change_resets_settling() {
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 5, None);
        c.step(0, Some(&idle), None);
        c.generation = Generation(2);
        let v = c.step(1_000, Some(&idle), None);
        assert!(!v.settled);
        assert_eq!(c.mem().idle_first, Some((c.at(1_000), 5)));
        assert_eq!(c.mem().herdr_changed, (c.at(1_000), T0_MS + 1_000));
        assert_eq!(c.mem().generation, Generation(2));
        let v = c.step(2_000, Some(&idle), None);
        assert!(v.settled);
    }

    #[test]
    fn brokered_and_escalated_only_while_waiting() {
        let gated = HookEvidence {
            brokered: true,
            escalated_reason: Some("needs a human".into()),
            ..row("waiting", 1_500)
        };
        // herdr blocked: waiting, with the gate's prompt details.
        let v = once(Some(&obs("w1:p1", "blocked", 3, None)), Some(&gated));
        assert!(v.brokered);
        assert_eq!(v.escalated_reason.as_deref(), Some("needs a human"));
        // A waiting row that vetoes an idle carries them too.
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 3, None);
        settle(&mut c, &idle, None);
        let v = c.step(2_000, Some(&idle), Some(&gated));
        assert_eq!(v.state, "waiting");
        assert!(v.brokered);
        // Any other state drops them.
        let v = once(Some(&obs("w1:p1", "working", 3, None)), Some(&gated));
        assert_eq!(v.state, "busy");
        assert!(!v.brokered);
        assert_eq!(v.escalated_reason, None);
    }

    #[test]
    fn last_change_at_moves_only_on_change() {
        let mut c = Clock::new();
        let idle = obs("w1:p1", "idle", 3, None);
        let v = c.step(0, Some(&idle), None);
        assert_eq!(v.last_change_at_ms, T0_MS);
        let v = c.step(500, Some(&idle), None);
        assert_eq!(v.last_change_at_ms, T0_MS, "nothing changed");
        let v = c.step(1_000, Some(&idle), None);
        assert_eq!(v.last_change_at_ms, T0_MS + 1_000, "settled flipped");
        // A stale flag on the remembered view is not a change.
        c.mem.as_mut().unwrap().view.stale = true;
        let v = c.step(2_000, Some(&idle), None);
        assert_eq!(v.last_change_at_ms, T0_MS + 1_000);
        assert!(!v.stale);
    }

    #[test]
    fn hook_row_from_another_session_is_ignored_as_evidence() {
        let cases = [
            (Some("s-old"), Some("s-new"), "idle"),
            (Some("s-new"), Some("s-new"), "busy"),
            (None, Some("s-new"), "busy"),
            (Some("s-old"), None, "busy"),
        ];
        for (row_session, live_session, want) in cases {
            let o = AgentObs {
                agent_session: live_session.map(str::to_string),
                ..obs("w1:p1", "idle", 5, None)
            };
            let busy = HookEvidence {
                session_id: row_session.map(str::to_string),
                ..row("busy", 2_000)
            };
            let mut c = Clock::new();
            settle(&mut c, &o, None);
            let v = c.step(3_000, Some(&o), Some(&busy));
            assert_eq!(v.state, want, "row {row_session:?} live {live_session:?}");
        }
    }

    #[test]
    fn present_since_is_set_on_present_and_cleared_on_absent() {
        let mut c = Clock::new();
        c.launch_noted = Some(c.base);
        c.step(0, None, None);
        assert_eq!(c.mem().present_since_ms, None, "starting is not present");
        c.launch_noted = None;
        c.step(1_000, Some(&obs("w1:p1", "working", 1, None)), None);
        assert_eq!(c.mem().present_since_ms, Some(T0_MS + 1_000));
        c.step(2_000, Some(&obs("w1:p1", "idle", 2, None)), None);
        assert_eq!(c.mem().present_since_ms, Some(T0_MS + 1_000), "kept");
        c.step(3_000, None, None);
        assert_eq!(c.mem().present_since_ms, None);
        c.step(4_000, Some(&obs("w1:p1", "idle", 3, None)), None);
        assert_eq!(c.mem().present_since_ms, Some(T0_MS + 4_000));
    }

    #[test]
    fn herdr_changed_tracks_the_first_sight_of_each_seq() {
        let mut c = Clock::new();
        c.step(0, Some(&obs("w1:p1", "working", 3, None)), None);
        c.step(1_000, Some(&obs("w1:p1", "working", 3, None)), None);
        assert_eq!(c.mem().herdr_changed, (c.at(0), T0_MS));
        assert_eq!(c.mem().last_scs, Some(3));
        c.step(2_000, Some(&obs("w1:p1", "idle", 4, None)), None);
        assert_eq!(c.mem().herdr_changed, (c.at(2_000), T0_MS + 2_000));
        c.step(3_000, None, None);
        assert_eq!(c.mem().last_scs, None);
        assert_eq!(c.mem().herdr_changed, (c.at(3_000), T0_MS + 3_000));
    }

    #[test]
    fn starting_then_idle_reads_busy_until_settled() {
        let mut c = Clock::new();
        let pending = AgentObs {
            launch_pending: true,
            ..obs("w1:p1", "unknown", 0, None)
        };
        c.step(0, Some(&pending), None);
        let idle = obs("w1:p1", "idle", 1, None);
        let v = c.step(1_000, Some(&idle), None);
        assert_eq!((v.agent, v.state), (Presence::Present, "busy"));
        let v = c.step(2_000, Some(&idle), None);
        assert_eq!((v.state, v.settled), ("idle", true));
    }
}
