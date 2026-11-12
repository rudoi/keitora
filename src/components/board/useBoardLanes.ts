// Pure lane-derivation hook: successor to Board.tsx's `buildColumns`.
// Given the full task list, the ordered worktree names, and the live session
// map, returns an ordered Lane[] that later stages use for rendering.
// No React hooks needed — this is a plain function + a useMemo
// wrapper exported as a hook for ergonomic use in Board.

import { useMemo } from "react";
import type { WorktreeSession } from "../../lib/sessions";
import type { TaskWithDeps } from "../../lib/tasks";

export interface Lane {
  key: string;
  kind: "unassigned" | "worktree" | "done";
  label: string;
  tasks: TaskWithDeps[];
  session?: WorktreeSession;
  count: number;
  /** True for a worktree lane that has no tasks and whose session is absent or
   *  offline. Used by later stages to collapse the lane to a slim chip. */
  isEmptyOffline: boolean;
}

/** A worktree lane's card order: the active card, then cards in review, then the rest. */
function laneRank(t: TaskWithDeps): number {
  return t.status === "active" ? 0 : t.status === "review" ? 1 : 2;
}

/**
 * Derive ordered lanes from tasks + worktrees + sessions.
 *
 * Filtering rules (preserved verbatim from buildColumns):
 *  - backlog → excluded, unless `includeBacklog` (the Open lane's backlog toggle):
 *    then shown in the Open lane, after the open cards.
 *  - done / cancelled → Done lane.
 *  - assigned to a known worktree → that worktree lane (a card in review included:
 *    it keeps its lane until a human marks it done). Active cards lead, then review.
 *  - else (open/active with no known assignee) → Unassigned.
 *
 * Lane order: Unassigned, worktrees in `worktreeNames` order, Done.
 * `worktreeNames` is the unfiltered list (includes offline worktrees).
 */
export function buildLanes(
  tasks: TaskWithDeps[],
  worktreeNames: string[],
  sessions: Map<string, WorktreeSession>,
  includeBacklog = false,
): Lane[] {
  const unassigned: TaskWithDeps[] = [];
  const backlog: TaskWithDeps[] = [];
  const done: TaskWithDeps[] = [];
  const byWt = new Map<string, TaskWithDeps[]>();
  worktreeNames.forEach((n) => {
    byWt.set(n, []);
  });

  for (const t of tasks) {
    const s = t.status;
    if (s === "done" || s === "cancelled") {
      done.push(t);
    } else if (s === "backlog") {
      if (includeBacklog) backlog.push(t);
    } else {
      const a = t.assigned_to;
      if (a && byWt.has(a)) {
        // biome-ignore lint/style/noNonNullAssertion: key a is guaranteed in byWt: populated from worktreeNames in the forEach above
        byWt.get(a)!.push(t);
      } else {
        unassigned.push(t);
      }
    }
  }

  unassigned.push(...backlog);

  const worktreeLanes: Lane[] = worktreeNames.map((n) => {
    // biome-ignore lint/style/noNonNullAssertion: key n is guaranteed in byWt: populated from worktreeNames in the forEach above
    const wtTasks = [...byWt.get(n)!].sort((a, b) => laneRank(a) - laneRank(b));
    const session = sessions.get(n);
    const state = session?.state ?? "offline";
    const isEmptyOffline = wtTasks.length === 0 && state === "offline";
    return {
      key: n,
      kind: "worktree" as const,
      label: n,
      tasks: wtTasks,
      session,
      count: wtTasks.length,
      isEmptyOffline,
    };
  });

  return [
    {
      key: "open",
      kind: "unassigned" as const,
      label: "Open",
      tasks: unassigned,
      count: unassigned.length,
      isEmptyOffline: false,
    },
    ...worktreeLanes,
    {
      key: "done",
      kind: "done" as const,
      label: "Done",
      tasks: done,
      count: done.length,
      isEmptyOffline: false,
    },
  ];
}

/** Hook wrapper for ergonomic use in Board components. */
export function useBoardLanes(
  tasks: TaskWithDeps[],
  worktreeNames: string[],
  sessions: Map<string, WorktreeSession>,
  includeBacklog = false,
): Lane[] {
  return useMemo(
    () => buildLanes(tasks, worktreeNames, sessions, includeBacklog),
    [tasks, worktreeNames, sessions, includeBacklog],
  );
}
