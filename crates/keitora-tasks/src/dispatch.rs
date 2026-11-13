//! Pure rendering for custom dispatch steps, the pure dispatch precondition
//! ([`task_refusal`]) a CLI dispatch request is checked against, and the lane rules
//! shared by the board and the app's auto-send: [`lane_queue`] (send order),
//! [`lane_hold`] (what keeps a lane from taking a send), [`pending_reason`] and
//! [`pick_worktree`] (one-click Dispatch).
//!
//! A dispatched task's prompt lines (configured in `[dispatch].steps`, see
//! [`crate::DispatchConfig`]) are templates with a fixed placeholder set. This
//! module turns a template + a [`DispatchContext`] into the final text. It is
//! deliberately Tauri-free and fully unit-tested; the orchestration (writing the
//! lines into a PTY, the submit byte, inter-step delays) lives in the app crate.

use std::collections::HashSet;

use crate::{Task, TaskWithDeps};

/// Everything a dispatch step can interpolate: the reserved task plus the
/// worktree it is being dispatched into (authoritative — equals `assigned_to`
/// once the task is reserved, but passed explicitly so rendering never depends on
/// read-after-write ordering).
pub struct DispatchContext<'a> {
    pub task: &'a Task,
    pub worktree: &'a str,
}

/// The 4-digit zero-padded work number for a task id: `19` → `"0019"`. Ids with
/// more than four digits render at full width (`12345` → `"12345"`).
pub fn work_number(id: i64) -> String {
    format!("{id:04}")
}

/// Substitute the fixed placeholder set into `template`:
///
/// | placeholder          | source                                |
/// |----------------------|---------------------------------------|
/// | `{task_number}`      | [`work_number`] of the task id        |
/// | `{task_id}`          | the raw task id                       |
/// | `{task_title}`       | the task title                        |
/// | `{task_description}` | the task description                  |
/// | `{branch}`           | the task branch, or `""` when unset   |
/// | `{worktree}`         | the dispatch target worktree          |
///
/// Unknown `{...}` tokens are left literal so templates can contain stray braces
/// without surprises. Substitution is SINGLE-PASS: a substituted value is appended
/// to the output and never re-scanned, so a task title/description that happens to
/// contain a placeholder token (e.g. a title literally `"{worktree}"`) is emitted
/// verbatim rather than being recursively expanded.
pub fn interpolate(template: &str, ctx: &DispatchContext) -> String {
    let resolve = |key: &str| -> Option<String> {
        Some(match key {
            "task_number" => work_number(ctx.task.id),
            "task_id" => ctx.task.id.to_string(),
            "task_title" => ctx.task.title.clone(),
            "task_description" => ctx.task.description.clone(),
            "branch" => ctx.task.branch.clone().unwrap_or_default(),
            "worktree" => ctx.worktree.to_string(),
            _ => return None,
        })
    };

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        // Look for the matching close brace for this token.
        match rest[open + 1..].find('}') {
            Some(close_off) => {
                let close = open + 1 + close_off;
                let key = &rest[open + 1..close];
                match resolve(key) {
                    Some(val) => out.push_str(&val),
                    None => out.push_str(&rest[open..=close]), // unknown → literal
                }
                rest = &rest[close + 1..];
            }
            // Unbalanced `{` with no closing brace: emit the remainder verbatim.
            None => {
                out.push_str(&rest[open..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Pure: a dispatch is a redundant re-apply (skip the inject) iff the task is still
/// reserved to THIS worktree (open + assigned here) and its most recent dispatch
/// decision into this worktree was already `dispatched`.
pub fn already_dispatched(
    status: &str,
    assigned_to: Option<&str>,
    worktree: &str,
    latest_decision: Option<&str>,
) -> bool {
    status == "open" && assigned_to == Some(worktree) && latest_decision == Some("dispatched")
}

/// Pure: why `task` can't be dispatched onto `worktree`, judged from the task alone,
/// or `None` when it can. The task-side half of the board's lane rule (`decideLane`):
/// terminal and blocked cards are refused, and so is anything `assign` would reject
/// (backlog, active, in review, reserved to another worktree), worded so a CLI caller knows what
/// to do. A task already reserved to `worktree` passes: re-dispatching it there sends
/// a card queued on that lane once the session is idle, and an already-sent one no-ops.
pub fn task_refusal(task: &TaskWithDeps, worktree: &str) -> Option<String> {
    let t = &task.task;
    let id = t.id;
    match t.status.as_str() {
        "done" | "cancelled" => return Some(format!("task #{id} is {}", t.status)),
        "backlog" => {
            return Some(format!(
                "task #{id} is in the backlog; reopen it first (`keitora task reopen {id}`)"
            ))
        }
        "active" => {
            let who = t.assigned_to.as_deref().unwrap_or("someone");
            return Some(format!("task #{id} is already active on @{who}"));
        }
        "review" => {
            let who = t.assigned_to.as_deref().unwrap_or("someone");
            return Some(format!(
                "task #{id} is in review on @{who}; mark it done once reviewed"
            ));
        }
        _ => {}
    }
    if !task.blocked_by.is_empty() {
        let ids = task
            .blocked_by
            .iter()
            .map(|b| format!("#{b}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!("task #{id} is blocked by {ids}"));
    }
    match t.assigned_to.as_deref() {
        Some(other) if other != worktree => Some(format!(
            "task #{id} is reserved for @{other}; release it first"
        )),
        _ => None,
    }
}

/// Send order within a lane: priority rank (urgent first). Unknown priorities sort
/// with `normal`.
pub fn priority_rank(priority: &str) -> u8 {
    match priority {
        "urgent" => 0,
        "high" => 1,
        "low" => 3,
        _ => 2,
    }
}

/// Pure: a worktree lane's queue of *pending* cards, in the order they are sent:
/// open cards reserved to `worktree` that have not been sent yet (`sent` holds the
/// ids of cards already typed into a session) and are not blocked, highest priority
/// first, then lowest id. The board mirrors this order (`laneQueue` in the frontend).
pub fn lane_queue<'a>(
    tasks: &'a [TaskWithDeps],
    worktree: &str,
    sent: &HashSet<i64>,
) -> Vec<&'a TaskWithDeps> {
    let mut q: Vec<&TaskWithDeps> = tasks
        .iter()
        .filter(|t| {
            t.task.status == "open"
                && t.task.assigned_to.as_deref() == Some(worktree)
                && !sent.contains(&t.task.id)
                && t.blocked_by.is_empty()
        })
        .collect();
    q.sort_by_key(|t| (priority_rank(&t.task.priority), t.task.id));
    q
}

/// Why a lane can't take a send right now. A lane is *free* (its front pending card
/// may be typed in) only when none of these hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneHold {
    /// No live session: dispatch never starts one.
    Offline,
    /// The worktree's agent has an active (claimed) task. Its session may be idle
    /// between workflow stages; typing a new task in would break into that work.
    Working(i64),
    /// A card was sent and the agent has not claimed it yet.
    AwaitingClaim(i64),
    /// The session is `busy` or `waiting`.
    Busy(String),
}

impl LaneHold {
    /// A short reason, e.g. "@alpha is busy".
    pub fn describe(&self, worktree: &str) -> String {
        match self {
            LaneHold::Offline => format!("@{worktree} is offline"),
            LaneHold::Working(id) => format!("@{worktree} is working on #{id}"),
            LaneHold::AwaitingClaim(id) => {
                format!("@{worktree} was sent #{id} and has not claimed it yet")
            }
            LaneHold::Busy(state) => format!("@{worktree} is {state}"),
        }
    }
}

/// Pure: what holds `worktree`'s lane (session in `state`), or `None` when it is
/// free. Offline is checked first, then an active task, then a sent-but-unclaimed
/// card, then a non-idle session.
pub fn lane_hold(
    tasks: &[TaskWithDeps],
    worktree: &str,
    state: &str,
    sent: &HashSet<i64>,
) -> Option<LaneHold> {
    if state == "offline" || state.is_empty() {
        return Some(LaneHold::Offline);
    }
    let here = |t: &&TaskWithDeps| t.task.assigned_to.as_deref() == Some(worktree);
    if let Some(t) = tasks
        .iter()
        .filter(here)
        .find(|t| t.task.status == "active")
    {
        return Some(LaneHold::Working(t.task.id));
    }
    if let Some(t) = tasks
        .iter()
        .filter(here)
        .find(|t| t.task.status == "open" && sent.contains(&t.task.id))
    {
        return Some(LaneHold::AwaitingClaim(t.task.id));
    }
    if state != "idle" {
        return Some(LaneHold::Busy(state.to_string()));
    }
    None
}

/// Pure: the reason shown for a pending card on `worktree`: what holds the lane (if
/// anything) and how many pending cards are ahead of it.
pub fn pending_reason(worktree: &str, hold: Option<&LaneHold>, ahead: usize) -> String {
    let behind = match ahead {
        0 => String::new(),
        1 => "behind 1 card".to_string(),
        n => format!("behind {n} cards"),
    };
    match (hold, behind.is_empty()) {
        (Some(h), true) => format!("pending: {}", h.describe(worktree)),
        (Some(h), false) => format!("pending: {}, {behind}", h.describe(worktree)),
        (None, false) => format!("pending: {behind} on @{worktree}"),
        (None, true) => format!("pending: next on @{worktree}"),
    }
}

/// One pool worktree as auto-pick sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolLane {
    pub worktree: String,
    /// The lane is free (live, idle, nothing active or awaiting a claim).
    pub free: bool,
    /// Pending cards already queued on the lane.
    pub queue_len: usize,
    /// When the lane was last sent a card (dispatch-log `created_at`), if ever.
    pub last_dispatched: Option<String>,
}

/// Pure: auto-pick a worktree for a one-click Dispatch from the pool `lanes` (in
/// config order). A free lane with an empty queue wins, least recently dispatched
/// first (never-dispatched before any); otherwise the lane with the shortest queue,
/// then least recently dispatched, so the card waits there as pending. Ties keep
/// config order. `None` when the pool is empty.
pub fn pick_worktree(lanes: &[PoolLane]) -> Option<&PoolLane> {
    // `None` (never dispatched) sorts before any timestamp; `min_by_key` keeps the
    // first of equal keys, so ties fall back to config order.
    let free = lanes
        .iter()
        .filter(|l| l.free && l.queue_len == 0)
        .min_by_key(|l| l.last_dispatched.clone());
    free.or_else(|| {
        lanes
            .iter()
            .min_by_key(|l| (l.queue_len, l.last_dispatched.clone()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Task {
        Task {
            id: 19,
            title: "custom dispatch".to_string(),
            description: "wire it up".to_string(),
            status: "active".to_string(),
            priority: "high".to_string(),
            assigned_to: Some("sol".to_string()),
            created_by: None,
            tags: vec![],
            branch: Some("llm/user/keitora/0019".to_string()),
            pr_url: None,
            created_at: "2026-06-21".to_string(),
            updated_at: "2026-06-21".to_string(),
        }
    }

    #[test]
    fn work_number_pads_to_four_then_overflows() {
        assert_eq!(work_number(1), "0001");
        assert_eq!(work_number(19), "0019");
        assert_eq!(work_number(1234), "1234");
        assert_eq!(work_number(12345), "12345");
    }

    #[test]
    fn each_placeholder_substitutes() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("{task_number}", &ctx), "0019");
        assert_eq!(interpolate("{task_id}", &ctx), "19");
        assert_eq!(interpolate("{task_title}", &ctx), "custom dispatch");
        assert_eq!(interpolate("{task_description}", &ctx), "wire it up");
        assert_eq!(interpolate("{branch}", &ctx), "llm/user/keitora/0019");
        assert_eq!(interpolate("{worktree}", &ctx), "sol");
    }

    #[test]
    fn all_placeholders_in_one_template() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(
            interpolate(
                "/workflow #{task_number} ({task_id}) {task_title} -> {worktree}",
                &ctx
            ),
            "/workflow #0019 (19) custom dispatch -> sol"
        );
    }

    #[test]
    fn branch_is_empty_when_none() {
        let mut t = task();
        t.branch = None;
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("b=[{branch}]", &ctx), "b=[]");
    }

    #[test]
    fn unknown_token_left_literal_and_plain_text_unchanged() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("{nope} stays", &ctx), "{nope} stays");
        assert_eq!(interpolate("/clear", &ctx), "/clear");
    }

    #[test]
    fn substituted_values_are_not_re_expanded() {
        // A title that literally contains a placeholder token must be emitted
        // verbatim, NOT recursively expanded into the worktree/branch value.
        let mut t = task();
        t.title = "{worktree} and {branch}".to_string();
        t.branch = Some("BR".to_string());
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(
            interpolate("title={task_title}; wt={worktree}", &ctx),
            "title={worktree} and {branch}; wt=sol"
        );
    }

    #[test]
    fn unbalanced_open_brace_is_literal() {
        let t = task();
        let ctx = DispatchContext {
            task: &t,
            worktree: "sol",
        };
        assert_eq!(interpolate("a {task_id} b {oops", &ctx), "a 19 b {oops");
    }

    #[test]
    fn already_dispatched_truth_table() {
        // True ONLY when open + assigned to this worktree + latest decision == dispatched.
        assert!(already_dispatched(
            "open",
            Some("feng"),
            "feng",
            Some("dispatched")
        ));

        // Non-open status → false.
        assert!(!already_dispatched(
            "active",
            Some("feng"),
            "feng",
            Some("dispatched")
        ));
        assert!(!already_dispatched(
            "done",
            Some("feng"),
            "feng",
            Some("dispatched")
        ));

        // Wrong assignee → false.
        assert!(!already_dispatched(
            "open",
            Some("sayo"),
            "feng",
            Some("dispatched")
        ));

        // None assignee → false.
        assert!(!already_dispatched(
            "open",
            None,
            "feng",
            Some("dispatched")
        ));

        // Latest decision is not dispatched → false.
        assert!(!already_dispatched(
            "open",
            Some("feng"),
            "feng",
            Some("queued")
        ));
        assert!(!already_dispatched(
            "open",
            Some("feng"),
            "feng",
            Some("suggested")
        ));
        assert!(!already_dispatched("open", Some("feng"), "feng", None));
    }

    fn card(status: &str, assigned_to: Option<&str>, blocked_by: Vec<i64>) -> TaskWithDeps {
        let mut t = task();
        t.status = status.to_string();
        t.assigned_to = assigned_to.map(str::to_string);
        TaskWithDeps {
            task: t,
            blocked_by,
            blocks: vec![],
        }
    }

    #[test]
    fn task_refusal_passes_open_unassigned_and_reserved_here() {
        assert_eq!(task_refusal(&card("open", None, vec![]), "alpha"), None);
        // Reserved to this worktree: re-dispatch sends a queued task / no-ops a sent one.
        assert_eq!(
            task_refusal(&card("open", Some("alpha"), vec![]), "alpha"),
            None
        );
    }

    #[test]
    fn task_refusal_rejects_what_the_board_or_assign_would() {
        let refuse = |c: TaskWithDeps| task_refusal(&c, "alpha").expect("refused");
        assert_eq!(refuse(card("done", None, vec![])), "task #19 is done");
        assert_eq!(
            refuse(card("cancelled", None, vec![])),
            "task #19 is cancelled"
        );
        assert_eq!(
            refuse(card("backlog", None, vec![])),
            "task #19 is in the backlog; reopen it first (`keitora task reopen 19`)"
        );
        assert_eq!(
            refuse(card("active", Some("bravo"), vec![])),
            "task #19 is already active on @bravo"
        );
        assert_eq!(
            refuse(card("review", Some("alpha"), vec![])),
            "task #19 is in review on @alpha; mark it done once reviewed"
        );
        assert_eq!(
            refuse(card("open", None, vec![3, 4])),
            "task #19 is blocked by #3, #4"
        );
        assert_eq!(
            refuse(card("open", Some("bravo"), vec![])),
            "task #19 is reserved for @bravo; release it first"
        );
        // Terminal beats blocked, matching the board's order of checks.
        assert_eq!(refuse(card("done", None, vec![3])), "task #19 is done");
    }

    fn lane_card(id: i64, status: &str, wt: Option<&str>, priority: &str) -> TaskWithDeps {
        let mut c = card(status, wt, vec![]);
        c.task.id = id;
        c.task.priority = priority.to_string();
        c
    }

    #[test]
    fn lane_queue_orders_by_priority_then_id_and_skips_sent_blocked_and_others() {
        let mut blocked = lane_card(1, "open", Some("alpha"), "urgent");
        blocked.blocked_by = vec![9];
        let tasks = vec![
            blocked,
            lane_card(7, "open", Some("alpha"), "normal"),
            lane_card(3, "open", Some("alpha"), "normal"),
            lane_card(9, "open", Some("alpha"), "high"),
            lane_card(4, "open", Some("alpha"), "urgent"), // sent
            lane_card(5, "open", Some("bravo"), "urgent"), // another lane
            lane_card(6, "active", Some("alpha"), "urgent"),
            lane_card(8, "open", None, "urgent"),
        ];
        let sent: HashSet<i64> = [4].into();
        let ids: Vec<i64> = lane_queue(&tasks, "alpha", &sent)
            .iter()
            .map(|t| t.task.id)
            .collect();
        assert_eq!(ids, vec![9, 3, 7]);
    }

    #[test]
    fn lane_hold_checks_offline_then_active_then_sent_then_state() {
        let none = HashSet::new();
        let idle_lane = vec![lane_card(3, "open", Some("alpha"), "normal")];
        assert_eq!(lane_hold(&idle_lane, "alpha", "idle", &none), None);
        assert_eq!(
            lane_hold(&idle_lane, "alpha", "offline", &none),
            Some(LaneHold::Offline)
        );
        for state in ["busy", "waiting"] {
            assert_eq!(
                lane_hold(&idle_lane, "alpha", state, &none),
                Some(LaneHold::Busy(state.to_string()))
            );
        }
        // An active task holds an idle lane (an agent idle between workflow stages).
        let working = vec![
            lane_card(3, "open", Some("alpha"), "normal"),
            lane_card(6, "active", Some("alpha"), "normal"),
        ];
        assert_eq!(
            lane_hold(&working, "alpha", "idle", &none),
            Some(LaneHold::Working(6))
        );
        // A sent, unclaimed card holds it too.
        let sent: HashSet<i64> = [3].into();
        assert_eq!(
            lane_hold(&idle_lane, "alpha", "idle", &sent),
            Some(LaneHold::AwaitingClaim(3))
        );
        // Another lane's active task doesn't hold this one.
        let other = vec![lane_card(6, "active", Some("bravo"), "normal")];
        assert_eq!(lane_hold(&other, "alpha", "idle", &none), None);
        // A card in review stays on the lane but doesn't hold it: the agent is done.
        let reviewing = vec![
            lane_card(3, "open", Some("alpha"), "normal"),
            lane_card(6, "review", Some("alpha"), "normal"),
        ];
        assert_eq!(lane_hold(&reviewing, "alpha", "idle", &none), None);
    }

    #[test]
    fn pending_reason_names_the_hold_and_the_position() {
        assert_eq!(
            pending_reason("alpha", Some(&LaneHold::Busy("busy".into())), 0),
            "pending: @alpha is busy"
        );
        assert_eq!(
            pending_reason("alpha", Some(&LaneHold::Offline), 2),
            "pending: @alpha is offline, behind 2 cards"
        );
        assert_eq!(
            pending_reason("alpha", None, 1),
            "pending: behind 1 card on @alpha"
        );
        assert_eq!(
            pending_reason("alpha", Some(&LaneHold::Working(6)), 0),
            "pending: @alpha is working on #6"
        );
    }

    fn pool(name: &str, free: bool, queue_len: usize, last: Option<&str>) -> PoolLane {
        PoolLane {
            worktree: name.to_string(),
            free,
            queue_len,
            last_dispatched: last.map(str::to_string),
        }
    }

    #[test]
    fn pick_prefers_a_free_lane_least_recently_dispatched() {
        let lanes = vec![
            pool("a", true, 0, Some("2026-11-10 10:00:00")),
            pool("b", false, 0, None),
            pool("c", true, 0, Some("2026-11-10 09:00:00")),
            pool("d", true, 0, Some("2026-11-10 11:00:00")),
        ];
        assert_eq!(pick_worktree(&lanes).unwrap().worktree, "c");
        // Never dispatched beats any timestamp.
        let lanes = vec![
            pool("a", true, 0, Some("2026-11-10 09:00:00")),
            pool("b", true, 0, None),
        ];
        assert_eq!(pick_worktree(&lanes).unwrap().worktree, "b");
    }

    #[test]
    fn pick_falls_back_to_the_shortest_queue() {
        let lanes = vec![
            pool("a", false, 2, None),
            pool("b", false, 1, Some("2026-11-10 10:00:00")),
            pool("c", false, 1, Some("2026-11-10 09:00:00")),
            // Free, but already has a queue: not "idle first".
            pool("d", true, 3, None),
        ];
        assert_eq!(pick_worktree(&lanes).unwrap().worktree, "c");
        // Ties keep config order.
        let lanes = vec![pool("x", false, 0, None), pool("y", false, 0, None)];
        assert_eq!(pick_worktree(&lanes).unwrap().worktree, "x");
        assert_eq!(pick_worktree(&[]), None);
    }
}
