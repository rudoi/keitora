import { describe, expect, it, vi } from "vitest";
import { buildPaneDrives, type DriveActionApi, drivesFor, runDriveAction } from "./drives";
import type { DriveView, WorktreeActivity } from "./ipc";
import type { SessionState, WorktreeSession } from "./sessions";

function act(project: string, worktree: string, extra: Partial<WorktreeActivity> = {}) {
  return {
    project,
    worktree,
    cli: "claude",
    branch: null,
    workflow: null,
    build: null,
    working: false,
    drivenBy: null,
    drives: [],
    pendingPrompts: 0,
    drivePaused: false,
    ...extra,
  } satisfies WorktreeActivity;
}

function sess(worktree: string, state: SessionState, extra: Partial<WorktreeSession> = {}) {
  return {
    project: "my-game",
    worktree,
    state,
    current_task_id: null,
    pid: null,
    last_heartbeat: "",
    ...extra,
  } satisfies WorktreeSession;
}

function view(driven: string, extra: Partial<DriveView> = {}): DriveView {
  return {
    driven,
    driver: "alpha",
    since: "",
    forced_from: null,
    paused: false,
    escalate_after_s: 300,
    awaiting_turn: null,
    pending: [],
    ...extra,
  };
}

const actMap = (...rows: WorktreeActivity[]) =>
  new Map(rows.map((r) => [`${r.project}/${r.worktree}`, r]));

describe("drivesFor", () => {
  it("keys the snapshot's views by driven worktree", () => {
    const m = drivesFor({ project: "my-game", views: [view("bravo")] }, "my-game");
    expect([...m.keys()]).toEqual(["bravo"]);
  });

  it("drops a snapshot for another project (a response that landed after a switch)", () => {
    expect(drivesFor({ project: "proj", views: [view("bravo")] }, "my-game").size).toBe(0);
    expect(drivesFor(null, "my-game").size).toBe(0);
    expect(drivesFor({ project: null, views: [] }, null).size).toBe(0);
  });
});

describe("buildPaneDrives", () => {
  it("covers driven worktrees of every project, with sessions and views only for the active one", () => {
    const activity = actMap(
      act("my-game", "bravo", { drivenBy: "alpha", pendingPrompts: 4 }),
      act("my-game", "alpha", { drives: ["bravo"] }),
      act("proj", "bravo", { drivenBy: "charlie", pendingPrompts: 2 }),
    );
    const views = new Map([
      [
        "bravo",
        view("bravo", {
          pending: [
            { id: 1, kind: "permission", tool_name: "Bash", summary: "ls", created_at: "" },
          ],
        }),
      ],
    ]);
    const m = buildPaneDrives(null, activity, "my-game", new Map(), views);
    expect([...m.keys()].sort()).toEqual(["my-game/bravo", "proj/bravo"]);
    expect(m.get("my-game/bravo")).toMatchObject({ driver: "alpha", pendingCount: 1 });
    // The other project's same-named worktree uses its own activity, not the active view.
    expect(m.get("proj/bravo")).toMatchObject({
      driver: "charlie",
      pendingCount: 2,
      firstPending: null,
    });
  });

  it("includes an active-project session the activity poll has not reported yet", () => {
    const sessions = new Map([["bravo", sess("bravo", "idle", { driven_by: "alpha" })]]);
    const m = buildPaneDrives(null, new Map(), "my-game", sessions, new Map());
    expect(m.get("my-game/bravo")?.driver).toBe("alpha");
  });

  it("hands back the previous object for an unchanged entry", () => {
    const activity = actMap(act("my-game", "bravo", { drivenBy: "alpha" }));
    const first = buildPaneDrives(null, activity, "my-game", new Map(), new Map());
    const again = buildPaneDrives(
      first,
      actMap(act("my-game", "bravo", { drivenBy: "alpha" })),
      "my-game",
      new Map(),
      new Map(),
    );
    expect(again.get("my-game/bravo")).toBe(first.get("my-game/bravo"));
    const changed = buildPaneDrives(
      first,
      actMap(act("my-game", "bravo", { drivenBy: "alpha", drivePaused: true })),
      "my-game",
      new Map(),
      new Map(),
    );
    expect(changed.get("my-game/bravo")).not.toBe(first.get("my-game/bravo"));
  });
});

describe("runDriveAction", () => {
  const api = (): DriveActionApi & { calls: string[] } => {
    const calls: string[] = [];
    return {
      calls,
      takeOver: async (p, d) => {
        calls.push(`take-over ${p} ${d}`);
        return 2;
      },
      pause: async (p, d, paused) => {
        calls.push(`pause ${p} ${d} ${paused}`);
      },
      release: async (p, d) => {
        calls.push(`release ${p} ${d}`);
      },
    };
  };

  it("runs each action against the given project and toasts like the rail menu", async () => {
    const a = api();
    const toast = vi.fn();
    await runDriveAction("my-game", "bravo", "take-over", "alpha", toast, a);
    await runDriveAction("my-game", "bravo", "pause-driver", "alpha", toast, a);
    await runDriveAction("my-game", "bravo", "resume-driver", "alpha", toast, a);
    await runDriveAction("my-game", "bravo", "release", "alpha", toast, a);
    expect(a.calls).toEqual([
      "take-over my-game bravo",
      "pause my-game bravo true",
      "pause my-game bravo false",
      "release my-game bravo",
    ]);
    expect(toast.mock.calls.map(([m]) => m)).toEqual([
      "Took over 2 prompts in bravo.",
      "Paused alpha's drive of bravo.",
      "Resumed alpha's drive of bravo.",
      "Released bravo.",
    ]);
  });

  it("toasts zero and one prompt distinctly, and an error verbatim", async () => {
    const toast = vi.fn();
    const a = api();
    await runDriveAction("p", "bravo", "take-over", "alpha", toast, {
      ...a,
      takeOver: async () => 0,
    });
    await runDriveAction("p", "bravo", "take-over", "alpha", toast, {
      ...a,
      takeOver: async () => 1,
    });
    await runDriveAction("p", "bravo", "release", "alpha", toast, {
      ...a,
      release: async () => {
        throw "@bravo isn't driven";
      },
    });
    expect(toast.mock.calls.map(([m]) => m)).toEqual([
      "No pending prompts to take over.",
      "Took over 1 prompt in bravo.",
      "@bravo isn't driven",
    ]);
  });
});
