import { describe, expect, it } from "vitest";
import {
  classifyAttention,
  type DriveFacts,
  driveFacts,
  escalationBody,
  isWorktreeFocused,
  latchIdleFromTurn,
  latchState,
  needsInput,
  nextAcknowledged,
  projectChanged,
  shouldSuppressNotification,
} from "./attention";
import type { SessionState, WorktreeSession } from "./sessions";

describe("latchIdleFromTurn", () => {
  it("latches on busy→idle (turn finished)", () => {
    expect(latchIdleFromTurn("busy", "idle", false)).toBe(true);
  });

  it("latches on waiting→idle (permission resolved, then done)", () => {
    expect(latchIdleFromTurn("waiting", "idle", false)).toBe(true);
  });

  it("does NOT latch a fresh idle (SessionStart, no prior state)", () => {
    expect(latchIdleFromTurn(undefined, "idle", false)).toBe(false);
  });

  it("preserves the latch on idle→idle", () => {
    expect(latchIdleFromTurn("idle", "idle", true)).toBe(true);
    expect(latchIdleFromTurn("idle", "idle", false)).toBe(false);
  });

  it("clears the latch when work resumes (→busy)", () => {
    expect(latchIdleFromTurn("idle", "busy", true)).toBe(false);
  });

  it("clears the latch when the session goes offline", () => {
    expect(latchIdleFromTurn("idle", "offline", true)).toBe(false);
  });
});

describe("latch guard (latchState into latchIdleFromTurn)", () => {
  const ws = (state: SessionState, starting?: boolean) => ({
    state,
    agent: starting ? ("starting" as const) : undefined,
  });
  /** Step the latch across states, the way useAttention does after its seed. */
  const run = (steps: { state: SessionState; agent?: "starting" }[]): boolean => {
    let prev: SessionState | undefined;
    let latch = false;
    for (const s of steps) {
      latch = latchIdleFromTurn(prev, latchState(s), latch);
      prev = latchState(s);
    }
    return latch;
  };

  it("maps a starting agent to offline and leaves the rest alone", () => {
    expect(latchState(ws("busy", true))).toBe("offline");
    expect(latchState(ws("busy", false))).toBe("busy");
    expect(latchState(ws("busy"))).toBe("busy");
    expect(latchState(ws("idle"))).toBe("idle");
  });

  it("a first-seen idle only seeds", () => {
    expect(latchIdleFromTurn(undefined, "idle", false)).toBe(false);
    expect(run([ws("idle")])).toBe(false);
  });

  it("an offline -> idle lift does not latch", () => {
    expect(latchIdleFromTurn("offline", "idle", false)).toBe(false);
    expect(run([ws("offline"), ws("idle")])).toBe(false);
  });

  it("starting -> idle does not latch", () => {
    expect(run([ws("busy", true), ws("busy", true), ws("idle")])).toBe(false);
  });

  it("a stale view keeps its last state, so it causes no transition", () => {
    // The engine holds the last state while stale; the first fresh view compares to it.
    expect(run([ws("idle"), ws("idle"), ws("idle")])).toBe(false);
    expect(run([ws("busy"), ws("busy"), ws("busy")])).toBe(false);
  });

  it("a real busy -> idle of a started agent still latches", () => {
    expect(run([ws("busy", true), ws("busy"), ws("idle")])).toBe(true);
    expect(run([ws("busy"), ws("idle")])).toBe(true);
  });
});

describe("needsInput", () => {
  it("is true for waiting regardless of latch", () => {
    expect(needsInput("waiting", false)).toBe(true);
    expect(needsInput("waiting", true)).toBe(true);
  });

  it("is true for idle only when latched from a turn", () => {
    expect(needsInput("idle", true)).toBe(true);
    expect(needsInput("idle", false)).toBe(false);
  });

  it("is false for busy and offline", () => {
    expect(needsInput("busy", true)).toBe(false);
    expect(needsInput("offline", true)).toBe(false);
  });
});

describe("classifyAttention", () => {
  it("classifies waiting as 'waiting' regardless of latch", () => {
    expect(classifyAttention("waiting", false)).toBe("waiting");
    expect(classifyAttention("waiting", true)).toBe("waiting");
  });

  it("classifies idle as 'finished' only when latched from a turn", () => {
    expect(classifyAttention("idle", true)).toBe("finished");
    expect(classifyAttention("idle", false)).toBeNull();
  });

  it("returns null for busy and offline", () => {
    expect(classifyAttention("busy", true)).toBeNull();
    expect(classifyAttention("offline", true)).toBeNull();
  });

  it("is non-null exactly when needsInput is true (stays consistent)", () => {
    const states: SessionState[] = ["idle", "busy", "waiting", "offline"];
    for (const state of states) {
      for (const latch of [false, true]) {
        expect(classifyAttention(state, latch) !== null).toBe(needsInput(state, latch));
      }
    }
  });
});

describe("isWorktreeFocused", () => {
  it("is true only for the selected worktree in a non-board view", () => {
    expect(isWorktreeFocused("focus", "a", "a")).toBe(true);
    expect(isWorktreeFocused("grid", "a", "a")).toBe(true);
  });

  it("is false for a non-selected worktree", () => {
    expect(isWorktreeFocused("grid", "a", "b")).toBe(false);
    expect(isWorktreeFocused("focus", "a", "b")).toBe(false);
  });

  it("is false in board view (no terminal focused)", () => {
    expect(isWorktreeFocused("board", "a", "a")).toBe(false);
  });
});

describe("shouldSuppressNotification", () => {
  // The exact regression behind #21: in grid view with the app focused, a NON-selected
  // worktree must still notify (it was being suppressed before this fix).
  it("does NOT suppress a non-selected worktree in grid view (the bug)", () => {
    expect(shouldSuppressNotification(true, "grid", "a", "b")).toBe(false);
  });

  it("suppresses the selected/focused worktree when the app is focused", () => {
    expect(shouldSuppressNotification(true, "grid", "a", "a")).toBe(true);
    expect(shouldSuppressNotification(true, "focus", "a", "a")).toBe(true);
  });

  it("does NOT suppress when the app is not focused (any view)", () => {
    expect(shouldSuppressNotification(false, "grid", "a", "a")).toBe(false);
    expect(shouldSuppressNotification(false, "focus", "a", "a")).toBe(false);
    expect(shouldSuppressNotification(false, "board", "a", "a")).toBe(false);
  });

  it("never suppresses in board view", () => {
    expect(shouldSuppressNotification(true, "board", "a", "a")).toBe(false);
  });
});

describe("nextAcknowledged", () => {
  // focused === appFocused && isWorktreeFocused(...) — the worktree the user is viewing.
  it("acknowledges a worktree the user is viewing while it needs input", () => {
    expect(nextAcknowledged(false, "waiting", true, true)).toBe(true);
    expect(nextAcknowledged(false, "idle", true, true)).toBe(true);
  });

  it("re-arms on a new turn (→busy) even if previously acknowledged", () => {
    expect(nextAcknowledged(true, "busy", false, false)).toBe(false);
    expect(nextAcknowledged(true, "busy", false, true)).toBe(false);
  });

  it("re-arms when the session goes offline", () => {
    expect(nextAcknowledged(true, "offline", false, false)).toBe(false);
  });

  it("drops the ack once the worktree no longer needs input", () => {
    expect(nextAcknowledged(true, "idle", false, true)).toBe(false);
  });

  it("preserves the prior ack for an unfocused worktree still needing input", () => {
    expect(nextAcknowledged(true, "waiting", true, false)).toBe(true);
    expect(nextAcknowledged(false, "waiting", true, false)).toBe(false);
  });
});

describe("projectChanged", () => {
  it("is true when transitioning from null to a project name", () => {
    expect(projectChanged(null, "a")).toBe(true);
  });

  it("is false when the project name is unchanged", () => {
    expect(projectChanged("a", "a")).toBe(false);
  });

  it("is true when switching between two different project names", () => {
    expect(projectChanged("a", "b")).toBe(true);
  });

  it("is true when transitioning from a project name back to null", () => {
    expect(projectChanged("a", null)).toBe(true);
  });
});

describe("classifyAttention seed-path invariant", () => {
  // The seed path calls classifyAttention(state, false) for every worktree.
  // This invariant ensures a reseed after a project switch can never emit a stale
  // 'finished' kind (which would require idleFromTurn=true, impossible on a fresh seed).
  it("idle with idleFromTurn=false yields null (no stale finished on reseed)", () => {
    expect(classifyAttention("idle", false)).toBeNull();
  });

  it("waiting with idleFromTurn=false still yields 'waiting' (genuine need preserved)", () => {
    expect(classifyAttention("waiting", false)).toBe("waiting");
  });
});

describe("driven sessions", () => {
  const brokered: DriveFacts = { drivenBy: "alpha", brokered: true };
  const escalated: DriveFacts = { drivenBy: "alpha", brokered: false };

  // [label, state, latch, drive, needsInput, kind]
  const rows: [string, SessionState, boolean, DriveFacts | undefined, boolean, string | null][] = [
    ["not driven, waiting", "waiting", false, undefined, true, "waiting"],
    ["driven, waiting, brokered", "waiting", false, brokered, false, null],
    ["driven, waiting, escalated", "waiting", false, escalated, true, "waiting"],
    ["driven, idle after a turn", "idle", true, escalated, false, null],
    ["driven, busy", "busy", true, escalated, false, null],
    ["driven, offline", "offline", false, escalated, false, null],
    ["not driven, idle after a turn", "idle", true, undefined, true, "finished"],
  ];

  it.each(rows)("%s", (_label, state, latch, drive, needs, kind) => {
    expect(needsInput(state, latch, drive)).toBe(needs);
    expect(classifyAttention(state, latch, drive)).toBe(kind);
  });

  it("keeps classifyAttention non-null exactly when needsInput, with and without a drive", () => {
    const states: SessionState[] = ["idle", "busy", "waiting", "offline"];
    for (const drive of [undefined, brokered, escalated]) {
      for (const state of states) {
        for (const latch of [false, true]) {
          expect(classifyAttention(state, latch, drive) !== null).toBe(
            needsInput(state, latch, drive),
          );
        }
      }
    }
  });

  it("never classifies a driven idle as finished, even with the latch set", () => {
    expect(classifyAttention("idle", true, brokered)).toBeNull();
    expect(classifyAttention("idle", true, escalated)).toBeNull();
  });

  it("seeds a brokered waiting session without attention", () => {
    // The seed path passes latch=false and the current facts.
    expect(classifyAttention("waiting", false, brokered)).toBeNull();
    expect(classifyAttention("waiting", false, escalated)).toBe("waiting");
  });

  it("treats brokered without a driver as no drive", () => {
    expect(needsInput("waiting", false, { drivenBy: null, brokered: true })).toBe(true);
  });
});

function sess(extra: Partial<WorktreeSession>): WorktreeSession {
  return {
    project: "my-game",
    worktree: "bravo",
    state: "waiting",
    current_task_id: null,
    pid: null,
    last_heartbeat: "",
    ...extra,
  };
}

describe("driveFacts", () => {
  it("is undefined for a missing or undriven session", () => {
    expect(driveFacts(undefined)).toBeUndefined();
    expect(driveFacts(sess({}))).toBeUndefined();
    expect(driveFacts(sess({ driven_by: null, brokered: true }))).toBeUndefined();
  });

  it("carries the driver and whether a prompt is brokered", () => {
    expect(driveFacts(sess({ driven_by: "alpha", brokered: true }))).toEqual({
      drivenBy: "alpha",
      brokered: true,
    });
    expect(driveFacts(sess({ driven_by: "alpha" }))).toEqual({
      drivenBy: "alpha",
      brokered: false,
    });
  });
});

describe("escalationBody", () => {
  it("names the driver and the reason for an escalated waiting session", () => {
    const s = sess({ driven_by: "alpha", escalated_reason: "needs prod credentials" });
    expect(escalationBody(s)).toBe("alpha escalated: needs prod credentials");
  });

  it("is null without a driver, a reason, or a waiting state", () => {
    expect(escalationBody(sess({ escalated_reason: "why" }))).toBeNull();
    expect(escalationBody(sess({ driven_by: "alpha" }))).toBeNull();
    const idle = sess({ driven_by: "alpha", escalated_reason: "why", state: "idle" });
    expect(escalationBody(idle)).toBeNull();
  });

  it("cuts a long reason at 140 characters without splitting a character", () => {
    const reason = `${"a".repeat(139)}${"\u2603".repeat(5)}`;
    expect(escalationBody(sess({ driven_by: "alpha", escalated_reason: reason }))).toBe(
      `alpha escalated: ${"a".repeat(139)}\u2603\u2026`,
    );
    const exact = "b".repeat(140);
    expect(escalationBody(sess({ driven_by: "alpha", escalated_reason: exact }))).toBe(
      `alpha escalated: ${exact}`,
    );
  });
});
