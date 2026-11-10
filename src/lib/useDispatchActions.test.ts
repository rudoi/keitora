import { describe, expect, it } from "vitest";
import type { DispatchOutcome } from "./dispatch";
import { classifyDispatch } from "./useDispatchActions";

const outcome = (
  state: DispatchOutcome["state"],
  reason: string | null = null,
  worktree = "wt-a",
): DispatchOutcome => ({
  dispatched: state === "sent" || state === "already_sent",
  state,
  worktree,
  reason,
});

describe("classifyDispatch", () => {
  it("treats a fresh send as 'injected'", () => {
    expect(classifyDispatch(outcome("sent"), 5)).toEqual({
      kind: "injected",
      toast: "Sent #5 to @wt-a",
    });
  });

  it("treats an already-sent card as a no-op, and says so", () => {
    expect(classifyDispatch(outcome("already_sent"), 5)).toEqual({
      kind: "noop",
      toast: "#5 was already sent to @wt-a",
    });
  });

  it("reports a pending card with its reason and the (picked) worktree", () => {
    expect(
      classifyDispatch(outcome("pending", "pending: @wt-b is busy, behind 1 card", "wt-b"), 42),
    ).toEqual({
      kind: "pending",
      toast: "Assigned #42 to @wt-b (pending — @wt-b is busy, behind 1 card)",
    });
    expect(classifyDispatch(outcome("pending"), 7).toast).toBe("Assigned #7 to @wt-a (pending)");
  });

  it("reports a failed send as a failure with its reason", () => {
    expect(
      classifyDispatch(outcome("failed", "could not send to @wt-a (herdr unreachable)"), 3),
    ).toEqual({
      kind: "failed",
      toast: "Send failed: #3 → @wt-a — could not send to @wt-a (herdr unreachable)",
    });
  });
});
