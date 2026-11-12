import { describe, expect, it } from "vitest";
import {
  blockAnimates,
  blockStateCls,
  laneMenuAction,
  openItemDisabledReason,
  sessionItemDisabledReason,
  transferDisabledReason,
  worktreeMenuItems,
} from "./laneState";
import type { SessionState, WorktreeSession } from "./sessions";

// Minimal session fixture — blockStateCls only reads `state` (and whether a session exists).
function session(state: SessionState): WorktreeSession {
  return {
    project: "p",
    worktree: "w",
    state,
    current_task_id: null,
    pid: null,
    last_heartbeat: "",
  };
}

describe("blockStateCls", () => {
  it("renders a finished worktree as steady green (idle), NOT orange (#82)", () => {
    // The whole point of #82: finished falls through to the live idle session state.
    expect(blockStateCls("finished", session("idle"), true)).toBe("idle");
  });

  it("keeps a waiting (permission-prompt) worktree on its own colour", () => {
    expect(blockStateCls("waiting", session("idle"), true)).toBe("waiting");
    // waiting wins even over a busy live state.
    expect(blockStateCls("waiting", session("busy"), true)).toBe("waiting");
  });

  it("uses the live session state when there is no attention", () => {
    expect(blockStateCls(undefined, session("busy"), true)).toBe("busy");
    expect(blockStateCls(undefined, session("idle"), true)).toBe("idle");
    expect(blockStateCls(undefined, session("offline"), true)).toBe("offline");
  });

  it("falls back to filesystem availability when there is no session", () => {
    expect(blockStateCls(undefined, undefined, true)).toBe("idle");
    expect(blockStateCls(undefined, undefined, false)).toBe("offline");
  });

  it("tints 'working' when a build is running — even with no session", () => {
    expect(blockStateCls(undefined, undefined, true, true)).toBe("working");
    // Building wins over the live session state (idle/busy).
    expect(blockStateCls(undefined, session("idle"), true, true)).toBe("working");
    expect(blockStateCls(undefined, session("busy"), true, true)).toBe("working");
  });

  it("keeps a blocking 'waiting' prompt above a running build", () => {
    expect(blockStateCls("waiting", session("idle"), true, true)).toBe("waiting");
  });

  it("gives a terminated session precedence over every other state", () => {
    expect(blockStateCls("waiting", session("busy"), true, true, true)).toBe("terminated");
    expect(blockStateCls(undefined, undefined, true, true, true)).toBe("terminated");
    for (const s of ["idle", "busy", "waiting", "offline"] as const) {
      expect(blockStateCls(undefined, session(s), true, false, true)).toBe("terminated");
    }
    expect(blockStateCls(undefined, undefined, false, false, true)).toBe("terminated");
  });

  it("never returns the orange 'done' class for any input", () => {
    const attns = [undefined, "waiting", "finished"] as const;
    const states: SessionState[] = ["idle", "busy", "waiting", "offline"];
    for (const attn of attns) {
      for (const s of states) {
        for (const ok of [true, false]) {
          expect(blockStateCls(attn, session(s), ok)).not.toBe("done");
        }
      }
      expect(blockStateCls(attn, undefined, true)).not.toBe("done");
      expect(blockStateCls(attn, undefined, false)).not.toBe("done");
    }
  });
});

describe("blockAnimates", () => {
  it("animates only for a blocking 'waiting' attention", () => {
    expect(blockAnimates("waiting")).toBe(true);
  });

  it("does NOT animate a finished worktree (steady, reads as available) (#82)", () => {
    expect(blockAnimates("finished")).toBe(false);
  });

  it("does NOT animate when there is no attention", () => {
    expect(blockAnimates(undefined)).toBe(false);
  });
});

describe("worktreeMenuItems", () => {
  it("offers restart, fresh, transfer, then terminate for a live session", () => {
    expect(worktreeMenuItems(false)).toEqual(["restart", "fresh", "transfer", "terminate"]);
  });

  it("offers only launch and launch fresh for a terminated session", () => {
    expect(worktreeMenuItems(true)).toEqual(["launch", "launch-fresh"]);
  });

  it("leads with focus for a live session in an inactive project", () => {
    expect(worktreeMenuItems(false, { inactive: true })).toEqual([
      "focus",
      "restart",
      "fresh",
      "transfer",
      "terminate",
    ]);
  });

  it("leads with focus for a terminated session in an inactive project", () => {
    expect(worktreeMenuItems(true, { inactive: true })).toEqual([
      "focus",
      "launch",
      "launch-fresh",
    ]);
  });

  it("omits focus when the project is active", () => {
    expect(worktreeMenuItems(false, { inactive: false })).toEqual([
      "restart",
      "fresh",
      "transfer",
      "terminate",
    ]);
    expect(worktreeMenuItems(true, {})).toEqual(["launch", "launch-fresh"]);
  });
});

describe("laneMenuAction", () => {
  it("maps each menu entry to its lane action", () => {
    expect(laneMenuAction("restart", "codex")).toEqual({ kind: "restart", fresh: false });
    expect(laneMenuAction("fresh", "codex")).toEqual({ kind: "restart", fresh: true });
    expect(laneMenuAction("terminate", "codex")).toEqual({ kind: "terminate" });
    expect(laneMenuAction("launch", "codex")).toEqual({ kind: "launch", fresh: false });
    expect(laneMenuAction("launch-fresh", "codex")).toEqual({ kind: "launch", fresh: true });
  });

  it("transfers to the given CLI", () => {
    expect(laneMenuAction("transfer", "codex")).toEqual({ kind: "transfer", to: "codex" });
    expect(laneMenuAction("transfer", "claude")).toEqual({ kind: "transfer", to: "claude" });
  });

  it("runs nothing for focus", () => {
    expect(laneMenuAction("focus", "codex")).toBeNull();
  });
});

describe("transferDisabledReason", () => {
  it("is null when enabled, names a missing Codex, and lets a busy transfer win", () => {
    expect(transferDisabledReason({ codexMissing: false, busy: false })).toBeNull();
    expect(transferDisabledReason({ codexMissing: true, busy: false })).toBe(
      "Codex isn't installed",
    );
    expect(transferDisabledReason({ codexMissing: false, busy: true })).toBe(
      "A transfer is already in progress",
    );
    expect(transferDisabledReason({ codexMissing: true, busy: true })).toBe(
      "A transfer is already in progress",
    );
  });
});

describe("sessionItemDisabledReason", () => {
  it("disables restart, start fresh and terminate only while a transfer is in flight", () => {
    for (const item of ["restart", "fresh", "terminate"] as const) {
      expect(sessionItemDisabledReason(item, false)).toBeNull();
      expect(sessionItemDisabledReason(item, true)).toBe("A transfer is in progress");
    }
    for (const item of ["focus", "transfer", "launch", "launch-fresh"] as const) {
      expect(sessionItemDisabledReason(item, true)).toBeNull();
    }
  });
});

describe("openItemDisabledReason", () => {
  it("enables the open items whenever the directory exists", () => {
    expect(openItemDisabledReason("available")).toBeNull();
    expect(openItemDisabledReason("not_git")).toBeNull();
  });

  it("disables them when the path is gone or not a directory", () => {
    expect(openItemDisabledReason("missing")).toMatch(/not found/);
    expect(openItemDisabledReason("not_dir")).toMatch(/not a directory/);
  });
});
