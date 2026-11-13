// Single source of truth for "what happens if this card lands on that lane" —
// drives both the drag affordance (Board droppable columns) and the per-card
// "Dispatch to…" menu labels. Given a dragged/selected card + the live lane context,
// decide each lane's outcome, and for a refused drop, why.
//
// Accent discipline (proposal §UX): SOLID accent = "act now" (the drop sends it);
// DASHED/dim accent = "defer" (the drop assigns it and it waits as pending).
// `release` is neutral; `invalid` is refused, with a reason the board toasts.

import { useMemo } from "react";
import { wouldSendNow } from "./cardState";
import type { WorktreeSession } from "./sessions";
import type { TaskWithDeps } from "./tasks";

export type Decision = "dispatch" | "queue" | "release" | "invalid";

export interface LaneRef {
  key: string;
  isWorktree: boolean;
  /** Worktree lanes only: in the dispatch pool (may receive tasks). */
  inPool?: boolean;
}

/** What the lane rules read besides the card: sessions, every board task, and the
 *  ids of cards already sent (and not yet claimed). */
export interface LaneContext {
  sessions: Map<string, WorktreeSession>;
  tasks: TaskWithDeps[];
  sent: Set<number>;
}

export interface LaneJudgement {
  decision: Decision;
  /** Why an `invalid` drop is refused; `null` for a drop back where it was. */
  reason: string | null;
}

/** Judge a drop of `card` on `lane`. */
export function judgeLane(card: TaskWithDeps, lane: LaneRef, ctx: LaneContext): LaneJudgement {
  const invalid = (reason: string | null): LaneJudgement => ({ decision: "invalid", reason });
  const id = card.id;
  if (card.status === "done" || card.status === "cancelled") {
    return invalid(`#${id} is ${card.status}`);
  }

  // The Open lane: releases an assigned card; anything else is already there.
  if (!lane.isWorktree) {
    if (lane.key === "open") {
      return card.assigned_to ? { decision: "release", reason: null } : invalid(null);
    }
    return invalid(null);
  }

  if (card.status === "backlog") return invalid(`#${id} is in the backlog; reopen it first`);
  if (lane.inPool === false) {
    return invalid(`@${lane.key} is not in the dispatch pool, so it takes no tasks`);
  }
  // A blocked card: the backend `assign` would refuse it.
  if (card.blocked_by.length > 0) {
    return invalid(`#${id} is blocked by ${card.blocked_by.map((b) => `#${b}`).join(", ")}`);
  }
  if (card.status === "review") {
    return invalid(
      `#${id} is in review on @${card.assigned_to ?? "someone"}; mark it done once reviewed`,
    );
  }
  if (card.status === "active") {
    return invalid(
      card.assigned_to === lane.key
        ? `#${id} is already active on @${lane.key}`
        : `#${id} is active on @${card.assigned_to ?? "someone"}; release it first`,
    );
  }
  if (card.assigned_to && ctx.sent.has(id)) {
    return invalid(
      card.assigned_to === lane.key
        ? `#${id} was already sent to @${lane.key}; waiting for its claim`
        : `#${id} was already sent to @${card.assigned_to}; release it first`,
    );
  }
  // Open or pending (here: a retry; elsewhere: a move). Sends now only if the lane is
  // free and the card would be its front; otherwise it waits there as pending.
  return {
    decision: wouldSendNow(card, lane.key, ctx.tasks, ctx.sessions, ctx.sent)
      ? "dispatch"
      : "queue",
    reason: null,
  };
}

/** Resolve the decision for one lane given the card and the lane context. */
export function decideLane(card: TaskWithDeps, lane: LaneRef, ctx: LaneContext): Decision {
  return judgeLane(card, lane, ctx).decision;
}

/** Per-lane decisions for the currently dragged/selected card, keyed by lane key.
 *  `null` card → an empty map (nothing in flight). */
export function useDispatchTargets(
  card: TaskWithDeps | null,
  lanes: LaneRef[],
  ctx: LaneContext,
): Map<string, Decision> {
  return useMemo(() => {
    const out = new Map<string, Decision>();
    if (!card) return out;
    for (const lane of lanes) {
      out.set(lane.key, decideLane(card, lane, ctx));
    }
    return out;
  }, [card, lanes, ctx]);
}
