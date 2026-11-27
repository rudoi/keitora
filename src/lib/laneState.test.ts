import { describe, expect, it } from "vitest";
import type { CliKind } from "./ipc";
import {
  actionDisabledReason,
  blockAnimates,
  blockStateCls,
  laneMenuAction,
  menuItemKey,
  sessionItemDisabledReason,
  type WorktreeMenuItem,
  worktreeMenuItems,
} from "./laneState";
import type { PendingAction } from "./pendingIntent";
import { CLI_KINDS } from "./providers";
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

const restart = { kind: "restart" } as const;
const fresh = { kind: "fresh" } as const;
const terminate = { kind: "terminate" } as const;
const launch = { kind: "launch" } as const;
const launchFresh = { kind: "launch-fresh" } as const;
const focus = { kind: "focus" } as const;

describe("worktreeMenuItems", () => {
  it("offers a transfer to Codex and a switch to Antigravity from Claude", () => {
    expect(worktreeMenuItems("claude", false)).toEqual([
      restart,
      fresh,
      { kind: "transfer", to: "codex" },
      { kind: "switch", to: "antigravity" },
      terminate,
    ]);
  });

  it("offers a transfer to Claude and a switch to Antigravity from Codex", () => {
    expect(worktreeMenuItems("codex", false)).toEqual([
      restart,
      fresh,
      { kind: "transfer", to: "claude" },
      { kind: "switch", to: "antigravity" },
      terminate,
    ]);
  });

  it("offers only switches from Antigravity", () => {
    expect(worktreeMenuItems("antigravity", false)).toEqual([
      restart,
      fresh,
      { kind: "switch", to: "claude" },
      { kind: "switch", to: "codex" },
      terminate,
    ]);
  });

  it("offers only launch and launch fresh for a terminated session of any provider", () => {
    for (const cli of CLI_KINDS) {
      expect(worktreeMenuItems(cli, true)).toEqual([launch, launchFresh]);
    }
  });

  it("leads with focus for a live session in an inactive project", () => {
    expect(worktreeMenuItems("claude", false, { inactive: true })).toEqual([
      focus,
      restart,
      fresh,
      { kind: "transfer", to: "codex" },
      { kind: "switch", to: "antigravity" },
      terminate,
    ]);
  });

  it("leads with focus for a terminated session in an inactive project", () => {
    expect(worktreeMenuItems("claude", true, { inactive: true })).toEqual([
      focus,
      launch,
      launchFresh,
    ]);
  });

  it("omits focus when the project is active", () => {
    expect(worktreeMenuItems("codex", false, { inactive: false })[0]).toEqual(restart);
    expect(worktreeMenuItems("claude", true, {})).toEqual([launch, launchFresh]);
  });
});

describe("menuItemKey", () => {
  it("names transfer and switch entries by target and others by kind", () => {
    expect(menuItemKey({ kind: "switch", to: "claude" })).toBe("switch:claude");
    expect(menuItemKey({ kind: "transfer", to: "codex" })).toBe("transfer:codex");
    expect(menuItemKey(restart)).toBe("restart");
  });

  it("is unique within every provider's menu", () => {
    for (const cli of CLI_KINDS) {
      for (const terminated of [false, true]) {
        const keys = worktreeMenuItems(cli, terminated, { inactive: true }).map(menuItemKey);
        expect(new Set(keys).size).toBe(keys.length);
      }
    }
  });
});

describe("laneMenuAction", () => {
  it("maps each menu entry to its lane action", () => {
    expect(laneMenuAction(restart)).toEqual({ kind: "restart", fresh: false });
    expect(laneMenuAction(fresh)).toEqual({ kind: "restart", fresh: true });
    expect(laneMenuAction(terminate)).toEqual({ kind: "terminate" });
    expect(laneMenuAction(launch)).toEqual({ kind: "launch", fresh: false });
    expect(laneMenuAction(launchFresh)).toEqual({ kind: "launch", fresh: true });
  });

  it("transfers to the given CLI", () => {
    expect(laneMenuAction({ kind: "transfer", to: "codex" })).toEqual({
      kind: "transfer",
      to: "codex",
    });
    expect(laneMenuAction({ kind: "transfer", to: "claude" })).toEqual({
      kind: "transfer",
      to: "claude",
    });
  });

  it("switches to the given CLI", () => {
    expect(laneMenuAction({ kind: "switch", to: "antigravity" })).toEqual({
      kind: "switch",
      to: "antigravity",
    });
  });

  it("runs nothing for focus", () => {
    expect(laneMenuAction(focus)).toBeNull();
  });

  it("only produces pending-action kinds", () => {
    const kinds: PendingAction["kind"][] = [
      "restart",
      "transfer",
      "switch",
      "terminate",
      "launch",
      "diff",
      "new-worktree",
    ];
    for (const cli of CLI_KINDS) {
      for (const terminated of [false, true]) {
        for (const item of worktreeMenuItems(cli, terminated, { inactive: true })) {
          const action = laneMenuAction(item);
          if (action !== null) expect(kinds).toContain(action.kind);
        }
      }
    }
  });
});

describe("actionDisabledReason", () => {
  const all: Record<CliKind, boolean> = { claude: true, codex: true, antigravity: true };

  it("lets a busy action win over a missing install and names the action", () => {
    expect(actionDisabledReason("transfer", "codex", { ...all, codex: false }, true)).toBe(
      "A transfer is already in progress",
    );
    expect(actionDisabledReason("switch", "antigravity", all, true)).toBe(
      "A switch is already in progress",
    );
  });

  it("names a missing target, Claude included", () => {
    expect(
      actionDisabledReason("switch", "antigravity", { ...all, antigravity: false }, false),
    ).toBe("Antigravity isn't installed");
    expect(actionDisabledReason("transfer", "claude", { ...all, claude: false }, false)).toBe(
      "Claude isn't installed",
    );
  });

  it("is null when the target is installed and nothing is in flight", () => {
    expect(actionDisabledReason("transfer", "codex", all, false)).toBeNull();
    expect(actionDisabledReason("switch", "antigravity", all, false)).toBeNull();
  });
});

describe("sessionItemDisabledReason", () => {
  it("disables restart, start fresh and terminate only while a transfer or switch is in flight", () => {
    for (const item of [restart, fresh, terminate]) {
      expect(sessionItemDisabledReason(item, false)).toBeNull();
      expect(sessionItemDisabledReason(item, true)).toBe("A transfer or switch is in progress");
    }
    const others: WorktreeMenuItem[] = [
      focus,
      { kind: "transfer", to: "codex" },
      { kind: "switch", to: "antigravity" },
      launch,
      launchFresh,
    ];
    for (const item of others) {
      expect(sessionItemDisabledReason(item, true)).toBeNull();
    }
  });
});
