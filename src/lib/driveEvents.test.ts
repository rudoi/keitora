import { describe, expect, it, vi } from "vitest";
import {
  createDriveEventHandlers,
  type DriveEventDeps,
  drivePrecheck,
  driveRestartKind,
  driveRestartOutcome,
  driveSwapConfirmPrefix,
  openProjectsFromKey,
  openProjectsKey,
  PROJECT_NOT_OPEN,
  wakeRetryReason,
  wakeStep,
} from "./driveEvents";
import type { DriveRestartEvent, ProjectSummary, WorktreeInfo } from "./ipc";
import { ALL_STATUSES } from "./paneStatus";
import { type TransferEntry, type TransferMap, transferKey } from "./transferPhase";
import { emptyWarmState, type WarmState } from "./warmProjects";

function wt(name: string, extra: Partial<WorktreeInfo> = {}): WorktreeInfo {
  return {
    name,
    path: `D:/repos/my-game/${name}`,
    status: "available",
    cli: "claude",
    terminated: false,
    ...extra,
  };
}

function summary(name: string): ProjectSummary {
  return {
    name,
    resolved_name: name,
    root: `D:/repos/${name}`,
    active: false,
    db_path: `D:/repos/${name}/tasks.db`,
    worktrees: [],
    backend: "sqlite",
  };
}

/** `my-game` open with alpha, bravo and a missing charlie; `proj` active. */
function warmState(): WarmState {
  return {
    active: "proj",
    projects: {
      "my-game": {
        summary: summary("my-game"),
        worktrees: [wt("alpha"), wt("bravo"), wt("charlie", { status: "missing" })],
        selected: "alpha",
        view: "focus",
      },
      proj: { summary: summary("proj"), worktrees: [wt("main")], selected: "main", view: "grid" },
    },
  };
}

function transferring(project: string, worktree: string): TransferMap {
  const e: TransferEntry = {
    project,
    worktree,
    to: "codex",
    mode: "transfer",
    phase: "importing",
    startedAt: 0,
    progress: null,
    cancelling: false,
    result: null,
  };
  return new Map([[transferKey(project, worktree), e]]);
}

describe("drivePrecheck", () => {
  it("passes a worktree with a pane in an open project", () => {
    expect(drivePrecheck(warmState(), new Map(), "my-game", "bravo")).toBeNull();
  });

  it("refuses a project that is not open", () => {
    expect(drivePrecheck(warmState(), new Map(), "other-proj", "bravo")).toBe(
      "project not open in Keitora",
    );
    expect(drivePrecheck(emptyWarmState, new Map(), "my-game", "bravo")).toBe(PROJECT_NOT_OPEN);
  });

  it("refuses a worktree without a pane (unknown or not available)", () => {
    expect(drivePrecheck(warmState(), new Map(), "my-game", "delta")).toBe(
      "@delta has no pane in Keitora",
    );
    expect(drivePrecheck(warmState(), new Map(), "my-game", "charlie")).toBe(
      "@charlie has no pane in Keitora",
    );
  });

  it("refuses while a transfer owns the pane, and only for that project's worktree", () => {
    expect(drivePrecheck(warmState(), transferring("my-game", "bravo"), "my-game", "bravo")).toBe(
      "a transfer or switch is in progress for @bravo",
    );
    expect(
      drivePrecheck(warmState(), transferring("proj", "bravo"), "my-game", "bravo"),
    ).toBeNull();
  });
});

describe("driveSwapConfirmPrefix", () => {
  it("names the driver, worktree, project and target CLI, then a blank line", () => {
    expect(driveSwapConfirmPrefix("alpha", "bravo", "my-game", "codex")).toBe(
      "alpha asked to transfer bravo in my-game to Codex.\n\n",
    );
    expect(driveSwapConfirmPrefix("alpha", "bravo", "my-game", "claude")).toBe(
      "alpha asked to transfer bravo in my-game to Claude.\n\n",
    );
  });
});

describe("driveRestartOutcome", () => {
  it("applies a spawned reset and refuses a failed one with its error", () => {
    expect(driveRestartOutcome({ ok: true })).toEqual({ outcome: "applied", reason: "" });
    expect(driveRestartOutcome({ ok: false, error: "The session is terminated." })).toEqual({
      outcome: "refused",
      reason: "The session is terminated.",
    });
  });
});

describe("openProjectsKey", () => {
  it("is the sorted project set and round-trips", () => {
    const key = openProjectsKey(warmState());
    expect(key).toBe("my-game\nproj");
    expect(openProjectsFromKey(key)).toEqual(["my-game", "proj"]);
    expect(openProjectsFromKey(openProjectsKey(emptyWarmState))).toEqual([]);
  });
});

describe("createDriveEventHandlers", () => {
  function setup(transfers: TransferMap = new Map()) {
    const warm = warmState();
    const before = structuredClone(warm);
    const deps = {
      warm: vi.fn(() => warm),
      transfers: vi.fn(() => transfers),
      resolve: vi.fn<DriveEventDeps["resolve"]>(async () => {}),
      restart: vi.fn<DriveEventDeps["restart"]>(),
      swap: vi.fn<DriveEventDeps["swap"]>(),
    };
    return { warm, before, deps, h: createDriveEventHandlers(deps) };
  }
  const restartEv = (project = "my-game", worktree = "bravo"): DriveRestartEvent => ({
    request_id: 7,
    project,
    worktree,
    driver: "alpha",
    mode: "reset",
    fresh: true,
    replace: true,
  });

  it("restarts an admitted reset for its request", () => {
    const { deps, h } = setup();
    h.onRestart(restartEv());
    expect(deps.restart).toHaveBeenCalledWith("my-game", "bravo", 7, {
      mode: "reset",
      fresh: true,
      replace: true,
    });
    expect(deps.resolve).not.toHaveBeenCalled();
  });

  it("passes a wake's mode, freshness and replace through to the pane", () => {
    const { deps, h } = setup();
    h.onRestart({ ...restartEv(), mode: "wake", fresh: false, replace: false });
    expect(deps.restart).toHaveBeenCalledWith("my-game", "bravo", 7, {
      mode: "wake",
      fresh: false,
      replace: false,
    });
    h.onRestart({ ...restartEv(), mode: "wake", fresh: false, replace: true });
    expect(deps.restart).toHaveBeenLastCalledWith("my-game", "bravo", 7, {
      mode: "wake",
      fresh: false,
      replace: true,
    });
  });

  it("refuses a wake for a worktree with no pane, without waking anything", () => {
    const { deps, h } = setup();
    h.onRestart({ ...restartEv("my-game", "charlie"), mode: "wake", fresh: false });
    expect(deps.restart).not.toHaveBeenCalled();
    expect(deps.resolve).toHaveBeenCalledWith(
      "my-game",
      7,
      "refused",
      "@charlie has no pane in Keitora",
    );
  });

  it("hands an admitted swap to the transfer flow in driven mode", () => {
    const { deps, h } = setup();
    h.onSwap({ ...restartEv(), to: "codex" });
    expect(deps.swap).toHaveBeenCalledWith("my-game", "bravo", "codex", {
      requestId: 7,
      driver: "alpha",
    });
  });

  it("refuses each precheck failure with its exact reason and runs nothing", () => {
    const cases: [ReturnType<typeof setup>, ReturnType<typeof restartEv>, string][] = [
      [setup(), restartEv("other-proj"), "project not open in Keitora"],
      [setup(), restartEv("my-game", "charlie"), "@charlie has no pane in Keitora"],
      [
        setup(transferring("my-game", "bravo")),
        restartEv(),
        "a transfer or switch is in progress for @bravo",
      ],
    ];
    for (const [{ deps, h }, ev, reason] of cases) {
      h.onRestart(ev);
      h.onSwap({ ...ev, to: "claude" });
      expect(deps.restart).not.toHaveBeenCalled();
      expect(deps.swap).not.toHaveBeenCalled();
      expect(deps.resolve.mock.calls).toEqual([
        [ev.project, 7, "refused", reason],
        [ev.project, 7, "refused", reason],
      ]);
    }
  });

  it("swallows a rejected refusal (the runner already gave up)", async () => {
    const { deps, h } = setup();
    deps.resolve.mockRejectedValue("request is no longer waiting for Keitora");
    h.onRestart(restartEv("other-proj"));
    await Promise.resolve();
    expect(deps.resolve).toHaveBeenCalledTimes(1);
  });

  it("never touches the selection, view or active project", () => {
    const { warm, before, h } = setup();
    h.onRestart(restartEv());
    h.onSwap({ ...restartEv(), to: "codex" });
    h.onRestart(restartEv("other-proj"));
    expect(warm).toEqual(before);
  });
});

describe("wakeStep", () => {
  it("launches a terminated session whatever the pane shows", () => {
    for (const s of ALL_STATUSES) expect(wakeStep(s, true)).toBe("launch");
    expect(wakeStep("terminated", false)).toBe("launch");
  });

  it("leaves a running or returning session alone", () => {
    for (const s of ["running", "attaching", "detached-alive", "recovering"] as const) {
      expect(wakeStep(s, false)).toBe("awake");
    }
  });

  it("spawns an agent that exited or never started", () => {
    for (const s of ["ended", "ended-while-away", "lost", "spawn-failed"] as const) {
      expect(wakeStep(s, false)).toBe("spawn");
    }
  });

  it("asks recovery for a parked session and gives up while herdr is down", () => {
    expect(wakeStep("suspended", false)).toBe("resume");
    expect(wakeStep("daemon-down", false)).toBe("unavailable");
  });

  it("never keeps or resumes a session when the wake needs a new one", () => {
    // A fresh wake, or one onto another provider: the old session must not be reported
    // as the woken one.
    expect(wakeStep("suspended", false, true)).toBe("restart");
    for (const s of ["running", "attaching", "detached-alive", "recovering"] as const) {
      expect(wakeStep(s, false, true)).toBe("retry");
    }
    for (const s of ["ended", "ended-while-away", "lost", "spawn-failed"] as const) {
      expect(wakeStep(s, false, true)).toBe("spawn");
    }
    for (const s of ALL_STATUSES) expect(wakeStep(s, true, true)).toBe("launch");
    expect(wakeStep("daemon-down", false, true)).toBe("unavailable");
  });

  it("explains a retry by what the pane is doing", () => {
    expect(wakeRetryReason("bravo", "attaching")).toBe("@bravo is attaching; retry the wake");
    expect(wakeRetryReason("bravo", "recovering")).toBe("@bravo is recovering; retry the wake");
    expect(wakeRetryReason("bravo", "detached-alive")).toBe(
      "@bravo is reattaching; retry the wake",
    );
    expect(wakeRetryReason("bravo", "running")).toBe("@bravo is already running; retry the wake");
  });
});

describe("driveRestartKind", () => {
  it("maps a reset and each kind of wake to the pane's restart kind", () => {
    expect(driveRestartKind({ mode: "reset", replace: true })).toBe("drive-reset");
    expect(driveRestartKind({ mode: "wake", replace: false })).toBe("drive-wake");
    expect(driveRestartKind({ mode: "wake", replace: true })).toBe("drive-wake-replace");
  });
});
