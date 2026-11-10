import { describe, expect, it } from "vitest";
import { newFailures } from "./Board";
import { stateChip } from "./board/TaskCard";

describe("newFailures", () => {
  it("returns only failures not seen before (by task, lane and time)", () => {
    const f = (task_id: number, at: string) => ({ task_id, worktree: "a", reason: "x", at });
    const seen = new Set(["1:a:t1"]);
    expect(newFailures(seen, [f(1, "t1"), f(1, "t2"), f(2, "t1")])).toEqual([
      f(1, "t2"),
      f(2, "t1"),
    ]);
  });
});

describe("stateChip", () => {
  it("labels pending, failed, sent, active and backlog cards", () => {
    expect(
      stateChip({ kind: "pending", worktree: "a", reason: "pending: @a is busy", failure: null }),
    ).toEqual({ text: "pending", title: "@a is busy" });
    expect(
      stateChip({ kind: "pending", worktree: "a", reason: "pending: x", failure: "herdr down" }),
    ).toEqual({ text: "send failed", title: "herdr down" });
    expect(stateChip({ kind: "sent", worktree: "a" })?.text).toBe("sent ✓");
    expect(stateChip({ kind: "active", worktree: "a" })?.text).toBe("active");
    expect(stateChip({ kind: "backlog" })?.text).toBe("backlog");
    expect(stateChip({ kind: "open" })).toBeNull();
  });
});
