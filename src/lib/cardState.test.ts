import { describe, expect, it } from "vitest";
import { cardStates, laneHold, laneQueue, pendingReason, wouldSendNow } from "./cardState";
import type { WorktreeSession } from "./sessions";
import type { TaskWithDeps } from "./tasks";

function task(
  id: number,
  status: TaskWithDeps["status"],
  assigned_to: string | null = null,
  priority: TaskWithDeps["priority"] = "normal",
  blocked_by: number[] = [],
): TaskWithDeps {
  return {
    id,
    title: `Task ${id}`,
    description: "",
    status,
    priority,
    assigned_to,
    created_by: null,
    tags: [],
    branch: null,
    pr_url: null,
    created_at: "",
    updated_at: "",
    blocked_by,
    blocks: [],
  };
}

function sessions(states: Record<string, WorktreeSession["state"]>): Map<string, WorktreeSession> {
  return new Map(
    Object.entries(states).map(([worktree, state]) => [
      worktree,
      {
        project: "p",
        worktree,
        state,
        current_task_id: null,
        pid: null,
        last_heartbeat: "",
      },
    ]),
  );
}

describe("laneQueue", () => {
  it("orders pending cards by priority then id, skipping sent, blocked and others", () => {
    const tasks = [
      task(1, "open", "a", "urgent", [9]), // blocked
      task(7, "open", "a"),
      task(3, "open", "a"),
      task(9, "open", "a", "high"),
      task(4, "open", "a", "urgent"), // sent
      task(5, "open", "b", "urgent"),
      task(6, "active", "a", "urgent"),
    ];
    expect(laneQueue(tasks, "a", new Set([4]))).toEqual([9, 3, 7]);
  });
});

describe("laneHold", () => {
  it("checks offline, then an active task, then a sent card, then the session state", () => {
    const none = new Set<number>();
    expect(laneHold([], "a", "idle", none)).toBeNull();
    expect(laneHold([], "a", undefined, none)).toEqual({ kind: "offline" });
    expect(laneHold([], "a", "busy", none)).toEqual({ kind: "busy", state: "busy" });
    expect(laneHold([task(6, "active", "a")], "a", "idle", none)).toEqual({
      kind: "working",
      taskId: 6,
    });
    expect(laneHold([task(3, "open", "a")], "a", "idle", new Set([3]))).toEqual({
      kind: "awaiting",
      taskId: 3,
    });
  });
});

describe("pendingReason", () => {
  it("matches the backend wording", () => {
    expect(pendingReason("a", { kind: "busy", state: "busy" }, 0)).toBe("pending: @a is busy");
    expect(pendingReason("a", { kind: "offline" }, 2)).toBe(
      "pending: @a is offline, behind 2 cards",
    );
    expect(pendingReason("a", null, 1)).toBe("pending: behind 1 card on @a");
  });
});

describe("cardStates", () => {
  it("gives every card exactly one state, with pending reasons and failures", () => {
    const tasks = [
      task(1, "open"),
      task(2, "open", "a"), // front, lane busy
      task(3, "open", "a"), // behind 2
      task(4, "open", "b"), // sent
      task(5, "active", "b"),
      task(6, "open", null, "normal", [1]),
      task(7, "done"),
    ];
    const states = cardStates(
      tasks,
      [task(8, "backlog")],
      ["a", "b"],
      sessions({ a: "busy", b: "idle" }),
      [{ task_id: 4, worktree: "b" }],
      [{ task_id: 3, worktree: "a", reason: "could not send", at: "t" }],
    );
    expect(states.get(1)).toEqual({ kind: "open" });
    expect(states.get(2)).toEqual({
      kind: "pending",
      worktree: "a",
      reason: "pending: @a is busy",
      failure: null,
    });
    expect(states.get(3)).toEqual({
      kind: "pending",
      worktree: "a",
      reason: "pending: @a is busy, behind 1 card",
      failure: "could not send",
    });
    expect(states.get(4)).toEqual({ kind: "sent", worktree: "b" });
    expect(states.get(5)).toEqual({ kind: "active", worktree: "b" });
    expect(states.get(6)).toEqual({ kind: "blocked", by: [1] });
    expect(states.get(7)).toEqual({ kind: "done" });
    expect(states.get(8)).toEqual({ kind: "backlog" });
  });
});

describe("cardStates outside the pool", () => {
  it("says a card on a lane outside the dispatch pool is never sent", () => {
    const states = cardStates(
      [task(2, "open", "main")],
      [],
      ["main"],
      sessions({ main: "idle" }),
      [],
      [],
      new Set(["a"]),
    );
    expect(states.get(2)).toMatchObject({
      kind: "pending",
      reason: "pending: @main is not in the dispatch pool; move or release it",
    });
  });
});

describe("wouldSendNow", () => {
  it("is true only for a free lane where the card would be the front", () => {
    const s = sessions({ a: "idle", b: "busy" });
    const none = new Set<number>();
    const card = task(5, "open");
    expect(wouldSendNow(card, "a", [card], s, none)).toBe(true);
    expect(wouldSendNow(card, "b", [card], s, none)).toBe(false);
    expect(wouldSendNow(card, "c", [card], s, none)).toBe(false); // offline
    // A higher-priority card already pending on the lane goes first.
    const ahead = task(2, "open", "a", "urgent");
    expect(wouldSendNow(card, "a", [card, ahead], s, none)).toBe(false);
    // An active task holds the idle lane.
    expect(wouldSendNow(card, "a", [card, task(9, "active", "a")], s, none)).toBe(false);
  });
});
