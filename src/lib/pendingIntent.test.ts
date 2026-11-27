import { describe, expect, it } from "vitest";
import {
  canConsume,
  intentForSwitch,
  type PendingAction,
  type PendingIntent,
  type SettleState,
  shouldDrop,
} from "./pendingIntent";

const intent: PendingIntent = {
  project: "beta",
  worktree: "w1",
  action: { kind: "diff" },
};

// The settled state for `intent`: target active, no switch in flight, warm and visible.
const settled: SettleState = {
  activeProjectName: "beta",
  switchingTo: null,
  warmActive: "beta",
  warmHas: true,
};

describe("canConsume", () => {
  it("is true once the switch to the target has fully settled", () => {
    expect(canConsume(intent, settled)).toBe(true);
  });

  it("is false while the target is not the active project", () => {
    expect(canConsume(intent, { ...settled, activeProjectName: "alpha" })).toBe(false);
    expect(canConsume(intent, { ...settled, activeProjectName: null })).toBe(false);
  });

  it("is false while a switch is still in flight", () => {
    expect(canConsume(intent, { ...settled, switchingTo: "beta" })).toBe(false);
  });

  it("is false until the warm registry shows the target", () => {
    expect(canConsume(intent, { ...settled, warmActive: "alpha" })).toBe(false);
    expect(canConsume(intent, { ...settled, warmActive: null })).toBe(false);
  });

  it("is false until the warm registry holds the target", () => {
    expect(canConsume(intent, { ...settled, warmHas: false })).toBe(false);
  });
});

describe("shouldDrop", () => {
  it("drops the intent when the switch ended on a different project", () => {
    expect(shouldDrop(intent, { ...settled, activeProjectName: "alpha" })).toBe(true);
    expect(shouldDrop(intent, { ...settled, activeProjectName: null })).toBe(true);
  });

  it("keeps the intent while a switch is in flight", () => {
    expect(
      shouldDrop(intent, { ...settled, activeProjectName: "alpha", switchingTo: "beta" }),
    ).toBe(false);
    expect(
      shouldDrop(intent, { ...settled, activeProjectName: "alpha", switchingTo: "gamma" }),
    ).toBe(false);
  });

  it("keeps the intent when the target is active but not yet warm", () => {
    expect(shouldDrop(intent, { ...settled, warmActive: "alpha", warmHas: false })).toBe(false);
  });
});

describe("intentForSwitch", () => {
  it("parks a switch action like any other", () => {
    const action: PendingAction = { kind: "switch", to: "antigravity" };
    const parked = intentForSwitch("beta", { select: "w1", action }, true);
    expect(parked).toEqual({ project: "beta", worktree: "w1", action });
    expect(parked !== null && canConsume(parked, settled)).toBe(true);
  });

  it("parks nothing for a plain switch", () => {
    expect(intentForSwitch("beta", undefined, false)).toBeNull();
    expect(intentForSwitch("beta", { silent: true }, false)).toBeNull();
    expect(intentForSwitch("beta", { select: null, action: null }, true)).toBeNull();
  });

  it("parks a selection unless it was applied up front", () => {
    expect(intentForSwitch("beta", { select: "w1" }, false)).toEqual({
      project: "beta",
      worktree: "w1",
      action: null,
    });
    expect(intentForSwitch("beta", { select: "w1" }, true)).toBeNull();
  });

  it("always parks an action, with its lane", () => {
    const action = { kind: "restart", fresh: true } as const;
    for (const upFront of [true, false]) {
      expect(intentForSwitch("beta", { select: "w1", action }, upFront)).toEqual({
        project: "beta",
        worktree: "w1",
        action,
      });
    }
  });

  it("lets the parked intent of an already-active, settled project be consumed at once", () => {
    const parked = intentForSwitch("beta", { select: "w1" }, false);
    expect(parked).not.toBeNull();
    if (parked) expect(canConsume(parked, settled)).toBe(true);
  });

  it("parks a project-level action with no lane", () => {
    expect(intentForSwitch("beta", { action: { kind: "new-worktree" } }, true)).toEqual({
      project: "beta",
      worktree: null,
      action: { kind: "new-worktree" },
    });
  });
});
