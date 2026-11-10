import { describe, expect, it } from "vitest";
import type { WorktreeSession } from "./sessions";
import type { TaskWithDeps } from "./tasks";
import { decideLane, judgeLane, type LaneContext } from "./useDispatchTargets";

function task(
  id: number,
  status: TaskWithDeps["status"],
  assigned_to: string | null = null,
  blocked_by: number[] = [],
): TaskWithDeps {
  return {
    id,
    title: `Task ${id}`,
    description: "",
    status,
    priority: "normal",
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

function ctx(
  states: Record<string, WorktreeSession["state"]>,
  tasks: TaskWithDeps[] = [],
  sent: number[] = [],
): LaneContext {
  return {
    sessions: new Map(
      Object.entries(states).map(([worktree, state]) => [
        worktree,
        { project: "p", worktree, state, current_task_id: null, pid: null, last_heartbeat: "" },
      ]),
    ),
    tasks,
    sent: new Set(sent),
  };
}

const wt = (key: string, inPool = true) => ({ key, isWorktree: true, inPool });
const open = { key: "open", isWorktree: false };

describe("decideLane", () => {
  it("sends on a free lane and assigns as pending on any other pool lane", () => {
    const card = task(5, "open");
    const c = ctx({ idle: "idle", busy: "busy", waiting: "waiting" }, [card]);
    expect(decideLane(card, wt("idle"), c)).toBe("dispatch");
    expect(decideLane(card, wt("busy"), c)).toBe("queue");
    expect(decideLane(card, wt("waiting"), c)).toBe("queue");
    // Offline lanes take the card as pending too (no more silent snap-back).
    expect(decideLane(card, wt("offline"), c)).toBe("queue");
  });

  it("retries a pending card dropped on its own lane", () => {
    const card = task(5, "open", "idle");
    expect(decideLane(card, wt("idle"), ctx({ idle: "idle" }, [card]))).toBe("dispatch");
    expect(decideLane(card, wt("idle"), ctx({ idle: "busy" }, [card]))).toBe("queue");
  });

  it("moves a pending card to another lane", () => {
    const card = task(5, "open", "a");
    expect(decideLane(card, wt("b"), ctx({ a: "busy", b: "idle" }, [card]))).toBe("dispatch");
  });

  it("releases an assigned card dropped on Open", () => {
    expect(decideLane(task(5, "open", "a"), open, ctx({}))).toBe("release");
    expect(judgeLane(task(5, "open"), open, ctx({}))).toEqual({
      decision: "invalid",
      reason: null,
    });
  });
});

describe("judgeLane refusals carry a reason", () => {
  const reason = (card: TaskWithDeps, lane = wt("a"), c = ctx({ a: "idle" }, [card])) =>
    judgeLane(card, lane, c);

  it("refuses a worktree outside the dispatch pool", () => {
    expect(reason(task(5, "open"), wt("fellowship", false))).toEqual({
      decision: "invalid",
      reason: "@fellowship is not in the dispatch pool, so it takes no tasks",
    });
  });

  it("refuses blocked, backlog, terminal, active and sent cards", () => {
    expect(reason(task(5, "open", null, [2])).reason).toBe("#5 is blocked by #2");
    expect(reason(task(5, "backlog")).reason).toBe("#5 is in the backlog; reopen it first");
    expect(reason(task(5, "done")).reason).toBe("#5 is done");
    expect(reason(task(5, "active", "a")).reason).toBe("#5 is already active on @a");
    expect(reason(task(5, "active", "b")).reason).toBe("#5 is active on @b; release it first");
    const sent = task(5, "open", "a");
    expect(reason(sent, wt("a"), ctx({ a: "idle" }, [sent], [5])).reason).toBe(
      "#5 was already sent to @a; waiting for its claim",
    );
    expect(reason(sent, wt("b"), ctx({ b: "idle" }, [sent], [5])).reason).toBe(
      "#5 was already sent to @a; release it first",
    );
  });
});
