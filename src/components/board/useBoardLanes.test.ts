import { describe, expect, it } from "vitest";
import type { WorktreeSession } from "../../lib/sessions";
import type { TaskWithDeps } from "../../lib/tasks";
import { buildLanes } from "./useBoardLanes";

// ── Helpers ───────────────────────────────────────────────────────────────────

function task(
  id: number,
  status: TaskWithDeps["status"],
  assigned_to: string | null = null,
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
    created_at: "2026-01-01 00:00:00",
    updated_at: "2026-01-01 00:00:00",
    blocked_by: [],
    blocks: [],
  };
}

function session(worktree: string, state: WorktreeSession["state"]): WorktreeSession {
  return {
    project: "test-project",
    worktree,
    state,
    current_task_id: null,
    pid: null,
    last_heartbeat: "2026-01-01 00:00:00",
  };
}

const noSessions = new Map<string, WorktreeSession>();

// ── Tests ─────────────────────────────────────────────────────────────────────

describe("buildLanes", () => {
  it("routes an assigned task to its worktree lane", () => {
    const tasks = [task(1, "open", "wt-a")];
    const lanes = buildLanes(tasks, ["wt-a", "wt-b"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const wta = lanes.find((l) => l.key === "wt-a")!;
    expect(wta.tasks).toHaveLength(1);
    expect(wta.tasks[0].id).toBe(1);
  });

  it("routes a task assigned to an unknown worktree to Unassigned", () => {
    const tasks = [task(1, "open", "wt-unknown")];
    const lanes = buildLanes(tasks, ["wt-a"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const unassigned = lanes.find((l) => l.kind === "unassigned")!;
    expect(unassigned.tasks).toHaveLength(1);
    expect(unassigned.tasks[0].id).toBe(1);
    // Also confirm wt-a is empty
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const wta = lanes.find((l) => l.key === "wt-a")!;
    expect(wta.tasks).toHaveLength(0);
  });

  it("routes an unassigned open task to Unassigned", () => {
    const tasks = [task(2, "open", null)];
    const lanes = buildLanes(tasks, ["wt-a"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const unassigned = lanes.find((l) => l.kind === "unassigned")!;
    expect(unassigned.tasks).toHaveLength(1);
  });

  it("excludes backlog tasks from all lanes", () => {
    const tasks = [task(1, "backlog"), task(2, "backlog", "wt-a")];
    const lanes = buildLanes(tasks, ["wt-a"], noSessions);
    for (const lane of lanes) {
      expect(lane.tasks).toHaveLength(0);
    }
  });

  it("shows backlog tasks in the Open lane, after open ones, when asked", () => {
    const tasks = [task(1, "backlog"), task(2, "open")];
    const lanes = buildLanes(tasks, ["wt-a"], new Map(), true);
    const open = lanes.find((l) => l.kind === "unassigned");
    expect(open?.tasks.map((t) => t.id)).toEqual([2, 1]);
    expect(lanes.find((l) => l.key === "wt-a")?.tasks).toEqual([]);
  });

  it("routes done tasks to the Done lane", () => {
    const tasks = [task(1, "done")];
    const lanes = buildLanes(tasks, ["wt-a"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const doneLane = lanes.find((l) => l.kind === "done")!;
    expect(doneLane.tasks).toHaveLength(1);
    expect(doneLane.tasks[0].id).toBe(1);
  });

  it("routes cancelled tasks to the Done lane", () => {
    const tasks = [task(1, "cancelled")];
    const lanes = buildLanes(tasks, ["wt-a"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const doneLane = lanes.find((l) => l.kind === "done")!;
    expect(doneLane.tasks).toHaveLength(1);
  });

  it("produces lanes in order: Unassigned, worktrees (input order), Done", () => {
    const tasks: TaskWithDeps[] = [];
    const lanes = buildLanes(tasks, ["wt-b", "wt-a"], noSessions);
    expect(lanes.map((l) => l.key)).toEqual(["open", "wt-b", "wt-a", "done"]);
  });

  it("preserves worktree input order with tasks present", () => {
    const tasks = [task(1, "open", "wt-z"), task(2, "open", "wt-a")];
    const lanes = buildLanes(tasks, ["wt-z", "wt-a"], noSessions);
    const keys = lanes.map((l) => l.key);
    expect(keys.indexOf("wt-z")).toBeLessThan(keys.indexOf("wt-a"));
  });

  describe("isEmptyOffline", () => {
    it("is true for a worktree lane with no tasks and no session (absent = offline)", () => {
      const lanes = buildLanes([], ["wt-a"], noSessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.isEmptyOffline).toBe(true);
    });

    it("is true for a worktree lane with no tasks and an offline session", () => {
      const sessions = new Map([["wt-a", session("wt-a", "offline")]]);
      const lanes = buildLanes([], ["wt-a"], sessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.isEmptyOffline).toBe(true);
    });

    it("is false for a worktree lane with tasks (even if offline)", () => {
      const sessions = new Map([["wt-a", session("wt-a", "offline")]]);
      const lanes = buildLanes([task(1, "open", "wt-a")], ["wt-a"], sessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.isEmptyOffline).toBe(false);
    });

    it("is false for a worktree lane with no tasks but an idle session", () => {
      const sessions = new Map([["wt-a", session("wt-a", "idle")]]);
      const lanes = buildLanes([], ["wt-a"], sessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.isEmptyOffline).toBe(false);
    });

    it("is false for a worktree lane with no tasks but a busy session", () => {
      const sessions = new Map([["wt-a", session("wt-a", "busy")]]);
      const lanes = buildLanes([], ["wt-a"], sessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.isEmptyOffline).toBe(false);
    });

    it("is false for a worktree lane with no tasks but a waiting session", () => {
      const sessions = new Map([["wt-a", session("wt-a", "waiting")]]);
      const lanes = buildLanes([], ["wt-a"], sessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.isEmptyOffline).toBe(false);
    });

    it("is always false for Unassigned and Done lanes", () => {
      const lanes = buildLanes([], ["wt-a"], noSessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const unassigned = lanes.find((l) => l.kind === "unassigned")!;
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const done = lanes.find((l) => l.kind === "done")!;
      expect(unassigned.isEmptyOffline).toBe(false);
      expect(done.isEmptyOffline).toBe(false);
    });
  });

  it("attaches the session to a worktree lane when present", () => {
    const sess = session("wt-a", "idle");
    const sessions = new Map([["wt-a", sess]]);
    const lanes = buildLanes([], ["wt-a"], sessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const wta = lanes.find((l) => l.key === "wt-a")!;
    expect(wta.session).toBe(sess);
  });

  it("leaves session undefined for a worktree lane with no session", () => {
    const lanes = buildLanes([], ["wt-a"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const wta = lanes.find((l) => l.key === "wt-a")!;
    expect(wta.session).toBeUndefined();
  });

  it("count reflects the number of tasks in the lane", () => {
    const tasks = [task(1, "open", "wt-a"), task(2, "open", "wt-a"), task(3, "open")];
    const lanes = buildLanes(tasks, ["wt-a"], noSessions);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const wta = lanes.find((l) => l.key === "wt-a")!;
    expect(wta.count).toBe(2);
    // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
    const unassigned = lanes.find((l) => l.kind === "unassigned")!;
    expect(unassigned.count).toBe(1);
  });

  describe("active-first ordering in worktree lanes", () => {
    it("puts active tasks before open tasks in a worktree lane", () => {
      const tasks = [task(1, "open", "wt-a"), task(2, "active", "wt-a"), task(3, "open", "wt-a")];
      const lanes = buildLanes(tasks, ["wt-a"], noSessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.tasks[0].id).toBe(2); // active task first
      expect(wta.tasks[0].status).toBe("active");
    });

    it("preserves relative order among non-active tasks", () => {
      const tasks = [task(1, "open", "wt-a"), task(2, "open", "wt-a")];
      const lanes = buildLanes(tasks, ["wt-a"], noSessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.tasks.map((t) => t.id)).toEqual([1, 2]);
    });

    it("handles a lane with only an active task", () => {
      const tasks = [task(1, "active", "wt-a")];
      const lanes = buildLanes(tasks, ["wt-a"], noSessions);
      // biome-ignore lint/style/noNonNullAssertion: test asserts presence of expected lane; find result guaranteed non-null by test setup
      const wta = lanes.find((l) => l.key === "wt-a")!;
      expect(wta.tasks[0].id).toBe(1);
    });
  });
});
