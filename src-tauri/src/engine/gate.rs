//! The one send gate: whether text may be typed into a worktree's agent now. Drive
//! sends and task dispatch both ask it, so an agent one of them would wait on never
//! gets input from the other. Each consumer words the verdict its own way.

use keitora_tasks::StatusSession;

use super::model::ViewQuery;
use super::{HerdrLink, ProjectViews};

/// Whether a send may go now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict<'a> {
    Go,
    /// A transient condition: ask again later.
    Wait(Hold),
    Refuse(Refusal<'a>),
}

/// Why a send has to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// herdr is recovering, down or unavailable: every view is stale.
    Link(HerdrLink),
    /// No snapshot has been taken yet.
    FirstSnapshot,
    /// The worktree's view is stale.
    Stale,
    /// The agent is still starting, or not ready for input.
    Starting,
    /// Idle, but not settled yet.
    Unsettled,
    /// A send just typed into it has not started its turn yet.
    SendSettling,
}

impl Hold {
    /// Whether the hold is about herdr's freshness rather than the agent itself.
    pub fn is_stale(self) -> bool {
        matches!(self, Hold::Link(_) | Hold::FirstSnapshot | Hold::Stale)
    }
}

/// Why a send is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal<'a> {
    /// The engine has a fresh snapshot and no view of the worktree: offline.
    NoSession,
    /// The view is fresh and not idle (busy, waiting or offline).
    NotIdle(&'a StatusSession),
}

impl Refusal<'_> {
    /// The state the refusal is about (`"offline"` without a view).
    pub fn state(&self) -> &str {
        match self {
            Refusal::NoSession => "offline",
            Refusal::NotIdle(v) => &v.state,
        }
    }
}

/// Pure: whether a send to `worktree` may go, in order: no view while ready is
/// offline; herdr not up, no snapshot or a stale view wait; a starting or unsettled
/// agent waits; a settled idle goes unless `settling` (a send just typed into it)
/// holds it; anything else is refused as not idle.
pub fn send_gate<'a>(
    views: &'a ProjectViews,
    worktree: &str,
    settling: impl FnOnce(&StatusSession) -> bool,
) -> Verdict<'a> {
    let view = views.views.get(worktree);
    if view.is_none() && views.ready {
        return Verdict::Refuse(Refusal::NoSession);
    }
    match views.link {
        HerdrLink::Recovering | HerdrLink::Down | HerdrLink::Unavailable => {
            return Verdict::Wait(Hold::Link(views.link));
        }
        HerdrLink::Starting | HerdrLink::Up if !views.ready => {
            return Verdict::Wait(Hold::FirstSnapshot);
        }
        HerdrLink::Starting | HerdrLink::Up => {}
    }
    let Some(view) = view else {
        return Verdict::Refuse(Refusal::NoSession);
    };
    if view.stale {
        return Verdict::Wait(Hold::Stale);
    }
    if view.is_starting() {
        return Verdict::Wait(Hold::Starting);
    }
    if !view.is_idle() {
        return Verdict::Refuse(Refusal::NotIdle(view));
    }
    if !view.is_idle_settled() {
        return Verdict::Wait(Hold::Unsettled);
    }
    if settling(view) {
        return Verdict::Wait(Hold::SendSettling);
    }
    Verdict::Go
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn view(agent: &str, state: &str, settled: bool, ready: Option<bool>) -> StatusSession {
        StatusSession {
            project: "proj".into(),
            worktree: "alpha".into(),
            state: state.into(),
            agent: agent.into(),
            agent_kind: None,
            herdr_status: None,
            settled,
            stale: false,
            interactive_ready: ready,
            pane_id: None,
            terminal_id: None,
            agent_session: None,
            state_change_seq: None,
            completion_seq: None,
            brokered: false,
            escalated_reason: None,
            last_change_at: 0,
        }
    }

    fn views(ready: bool, link: HerdrLink, v: Option<StatusSession>) -> ProjectViews {
        ProjectViews {
            ready,
            link,
            views: v
                .map(|v| ("alpha".to_string(), v))
                .into_iter()
                .collect::<HashMap<_, _>>(),
        }
    }

    fn up(v: StatusSession) -> ProjectViews {
        views(true, HerdrLink::Up, Some(v))
    }

    #[test]
    fn a_settled_input_ready_idle_goes_unless_a_send_holds_it() {
        let pv = up(view("present", "idle", true, Some(true)));
        assert_eq!(send_gate(&pv, "alpha", |_| false), Verdict::Go);
        assert_eq!(
            send_gate(&pv, "alpha", |_| true),
            Verdict::Wait(Hold::SendSettling)
        );
    }

    #[test]
    fn freshness_holds_come_before_the_view() {
        assert_eq!(
            send_gate(&views(true, HerdrLink::Up, None), "alpha", |_| false),
            Verdict::Refuse(Refusal::NoSession)
        );
        assert_eq!(
            send_gate(&views(false, HerdrLink::Up, None), "alpha", |_| false),
            Verdict::Wait(Hold::FirstSnapshot)
        );
        let idle = view("present", "idle", true, Some(true));
        assert_eq!(
            send_gate(
                &views(false, HerdrLink::Recovering, Some(idle.clone())),
                "alpha",
                |_| false
            ),
            Verdict::Wait(Hold::Link(HerdrLink::Recovering))
        );
        let mut stale = idle;
        stale.stale = true;
        assert_eq!(
            send_gate(&up(stale), "alpha", |_| false),
            Verdict::Wait(Hold::Stale)
        );
        assert!(Hold::Stale.is_stale() && Hold::FirstSnapshot.is_stale());
        assert!(!Hold::Starting.is_stale() && !Hold::Unsettled.is_stale());
    }

    #[test]
    fn a_starting_or_unsettled_agent_waits() {
        for v in [
            view("starting", "busy", false, None),
            view("present", "idle", true, Some(false)),
        ] {
            assert_eq!(
                send_gate(&up(v), "alpha", |_| false),
                Verdict::Wait(Hold::Starting)
            );
        }
        assert_eq!(
            send_gate(
                &up(view("present", "idle", false, Some(true))),
                "alpha",
                |_| { false }
            ),
            Verdict::Wait(Hold::Unsettled)
        );
    }

    #[test]
    fn a_fresh_view_that_is_not_idle_is_refused_by_its_state() {
        for state in ["busy", "waiting"] {
            let pv = up(view("present", state, false, Some(true)));
            let verdict = send_gate(&pv, "alpha", |_| false);
            assert!(
                matches!(verdict, Verdict::Refuse(r @ Refusal::NotIdle(_)) if r.state() == state)
            );
        }
        let pv = up(view("absent", "offline", false, None));
        assert!(matches!(
            send_gate(&pv, "alpha", |_| false),
            Verdict::Refuse(r) if r.state() == "offline"
        ));
        assert_eq!(Refusal::NoSession.state(), "offline");
    }
}
