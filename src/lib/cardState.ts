// Pure lane rules for the board, mirroring `keitora_tasks::dispatch` (lane_queue,
// lane_hold, pending_reason) so the board shows exactly what the app's auto-send will
// do: a worktree lane is the queue; its front *pending* card (open, assigned, not yet
// sent, not blocked — highest priority, then lowest id) is typed into the session
// only when the lane is free (live and idle, nothing active, nothing sent and
// waiting for its claim).

import type { DispatchFailure, Reservation } from "./dispatch";
import type { WorktreeSession } from "./sessions";
import type { TaskWithDeps } from "./tasks";

const PRIORITY_RANK: Record<string, number> = { urgent: 0, high: 1, normal: 2, low: 3 };

/** What holds a lane back from a send, or `null` when the lane is free. */
export type LaneHold =
  | { kind: "offline" }
  | { kind: "working"; taskId: number }
  | { kind: "awaiting"; taskId: number }
  | { kind: "busy"; state: string };

/** The one state every card shows. */
export type CardState =
  | { kind: "open" }
  | { kind: "backlog" }
  | { kind: "pending"; worktree: string; reason: string; failure: string | null }
  | { kind: "sent"; worktree: string }
  | { kind: "active"; worktree: string | null }
  | { kind: "blocked"; by: number[] }
  | { kind: "done" };

/** The lane's pending card ids in send order. */
export function laneQueue(tasks: TaskWithDeps[], worktree: string, sent: Set<number>): number[] {
  return tasks
    .filter(
      (t) =>
        t.status === "open" &&
        t.assigned_to === worktree &&
        !sent.has(t.id) &&
        t.blocked_by.length === 0,
    )
    .sort((a, b) => {
      const r = (PRIORITY_RANK[a.priority] ?? 2) - (PRIORITY_RANK[b.priority] ?? 2);
      return r !== 0 ? r : a.id - b.id;
    })
    .map((t) => t.id);
}

/** What holds `worktree`'s lane: offline, then an active task, then a sent card
 *  waiting for its claim, then a non-idle session. */
export function laneHold(
  tasks: TaskWithDeps[],
  worktree: string,
  state: string | undefined,
  sent: Set<number>,
): LaneHold | null {
  if (!state || state === "offline") return { kind: "offline" };
  const here = tasks.filter((t) => t.assigned_to === worktree);
  const active = here.find((t) => t.status === "active");
  if (active) return { kind: "working", taskId: active.id };
  const awaiting = here.find((t) => t.status === "open" && sent.has(t.id));
  if (awaiting) return { kind: "awaiting", taskId: awaiting.id };
  if (state !== "idle") return { kind: "busy", state };
  return null;
}

export function describeHold(hold: LaneHold, worktree: string): string {
  switch (hold.kind) {
    case "offline":
      return `@${worktree} is offline`;
    case "working":
      return `@${worktree} is working on #${hold.taskId}`;
    case "awaiting":
      return `@${worktree} was sent #${hold.taskId} and has not claimed it yet`;
    case "busy":
      return `@${worktree} is ${hold.state}`;
  }
}

/** The pending reason (matches the backend's `pending_reason` wording). */
export function pendingReason(worktree: string, hold: LaneHold | null, ahead: number): string {
  const behind = ahead === 0 ? "" : ahead === 1 ? "behind 1 card" : `behind ${ahead} cards`;
  if (hold) {
    const h = describeHold(hold, worktree);
    return behind ? `pending: ${h}, ${behind}` : `pending: ${h}`;
  }
  return behind ? `pending: ${behind} on @${worktree}` : `pending: next on @${worktree}`;
}

/** The set of sent-and-unclaimed card ids, from the snapshot's reservations. */
export function sentIds(reservations: Reservation[]): Set<number> {
  return new Set(reservations.map((r) => r.task_id));
}

/** Every card's state, keyed by task id. `worktreeNames` are the board's lanes;
 *  `failures` the standing failed sends from the snapshot. */
export function cardStates(
  tasks: TaskWithDeps[],
  backlog: TaskWithDeps[],
  worktreeNames: string[],
  sessions: Map<string, WorktreeSession>,
  reservations: Reservation[],
  failures: DispatchFailure[],
  /** The dispatch pool; a card left on a lane outside it is never sent. */
  pool?: Set<string>,
): Map<number, CardState> {
  const sent = sentIds(reservations);
  const lanes = new Set(worktreeNames);
  const out = new Map<number, CardState>();
  const holds = new Map<string, LaneHold | null>();
  const queues = new Map<string, number[]>();
  for (const wt of worktreeNames) {
    holds.set(wt, laneHold(tasks, wt, sessions.get(wt)?.state, sent));
    queues.set(wt, laneQueue(tasks, wt, sent));
  }
  const failureFor = new Map(failures.map((f) => [`${f.task_id}:${f.worktree}`, f.reason]));
  for (const t of backlog) out.set(t.id, { kind: "backlog" });
  for (const t of tasks) {
    if (t.status === "done" || t.status === "cancelled") {
      out.set(t.id, { kind: "done" });
    } else if (t.status === "backlog") {
      out.set(t.id, { kind: "backlog" });
    } else if (t.status === "active") {
      out.set(t.id, { kind: "active", worktree: t.assigned_to });
    } else if (t.blocked_by.length > 0) {
      out.set(t.id, { kind: "blocked", by: t.blocked_by });
    } else if (t.assigned_to && lanes.has(t.assigned_to)) {
      const wt = t.assigned_to;
      if (sent.has(t.id)) {
        out.set(t.id, { kind: "sent", worktree: wt });
      } else {
        const queue = queues.get(wt) ?? [];
        const idx = queue.indexOf(t.id);
        const ahead = idx < 0 ? queue.length : idx;
        const hold = holds.get(wt) ?? null;
        const outOfPool = pool !== undefined && !pool.has(wt);
        out.set(t.id, {
          kind: "pending",
          worktree: wt,
          reason: outOfPool
            ? `pending: @${wt} is not in the dispatch pool; move or release it`
            : pendingReason(wt, hold, ahead),
          failure: failureFor.get(`${t.id}:${wt}`) ?? null,
        });
      }
    } else {
      out.set(t.id, { kind: "open" });
    }
  }
  return out;
}

/** Whether a drop of a card onto a free lane would send it now: the lane is free and
 *  no other pending card is ahead of it. */
export function wouldSendNow(
  card: TaskWithDeps,
  worktree: string,
  tasks: TaskWithDeps[],
  sessions: Map<string, WorktreeSession>,
  sent: Set<number>,
): boolean {
  if (laneHold(tasks, worktree, sessions.get(worktree)?.state, sent)) return false;
  // Queue with this card placed on the lane.
  const placed = tasks.map((t) =>
    t.id === card.id ? { ...t, assigned_to: worktree, status: "open" as const } : t,
  );
  if (!placed.some((t) => t.id === card.id)) {
    placed.push({ ...card, assigned_to: worktree, status: "open" });
  }
  return laneQueue(placed, worktree, sent)[0] === card.id;
}
