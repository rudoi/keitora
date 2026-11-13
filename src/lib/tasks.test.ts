import { describe, expect, it } from "vitest";
import {
  type BoardSnapshotResponse,
  finishActions,
  loadBoard,
  resyncIntervalForBackend,
  type TaskBoardResponse,
  TaskCache,
  type TaskWithDeps,
  tasksForProject,
} from "./tasks";
import { setTransport, type Transport } from "./transport";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

function makeTask(id: number, title = `task ${id}`): TaskWithDeps {
  return {
    id,
    title,
    description: "",
    status: "open",
    priority: "normal",
    assigned_to: null,
    created_by: null,
    tags: [],
    branch: null,
    pr_url: null,
    created_at: "2026-06-29T00:00:00Z",
    updated_at: "2026-06-29T00:00:00Z",
    blocked_by: [],
    blocks: [],
  };
}

function makeResponse(project: string, ids: number[]): TaskBoardResponse {
  return { project, tasks: ids.map((id) => makeTask(id)) };
}

// ---------------------------------------------------------------------------
// loadBoard — one board_snapshot call replaces two co-fired commands
// ---------------------------------------------------------------------------

describe("loadBoard", () => {
  it("issues exactly one board_snapshot invoke (not two commands)", async () => {
    const calls: string[] = [];
    const reply: BoardSnapshotResponse = { project: "projA", tasks: [], reservations: [] };
    const transport = {
      invoke: <T>(cmd: string) => {
        calls.push(cmd);
        return Promise.resolve(reply as unknown as T);
      },
    } as unknown as Transport;
    setTransport(transport);

    const board = await loadBoard();

    expect(calls).toEqual(["board_snapshot"]);
    expect(board.reservations).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// tasksForProject — the supersession guard
// ---------------------------------------------------------------------------

describe("tasksForProject", () => {
  it("returns the tasks when the response is for the active project", () => {
    const resp = makeResponse("projA", [1, 2]);
    const out = tasksForProject(resp, "projA");
    expect(out).toBe(resp.tasks);
    expect(out?.map((t) => t.id)).toEqual([1, 2]);
  });

  it("returns null for a response from a DIFFERENT project (stale / superseded)", () => {
    // A late tasks_list response for projB arriving after we switched back to projA
    // must NOT land on projA's board — this is the stale-overwrite bug.
    expect(tasksForProject(makeResponse("projB", [9]), "projA")).toBeNull();
  });

  it("returns null when there is no response yet", () => {
    expect(tasksForProject(null, "projA")).toBeNull();
  });

  it("returns null when there is no active project", () => {
    expect(tasksForProject(makeResponse("projA", [1]), null)).toBeNull();
  });

  it("passes through a matching EMPTY response as [] (a definitive loaded-empty)", () => {
    const resp = makeResponse("projA", []);
    expect(tasksForProject(resp, "projA")).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// resyncIntervalForBackend — backend-tiered safety-net poll cadence
// ---------------------------------------------------------------------------

describe("resyncIntervalForBackend", () => {
  it("polls github every 5 minutes (push is dead → gated poll is primary)", () => {
    expect(resyncIntervalForBackend("github")).toBe(300_000);
  });

  it("polls sqlite every 30 seconds (fs watcher is primary → poll is a net)", () => {
    expect(resyncIntervalForBackend("sqlite")).toBe(30_000);
  });

  it("defaults an unknown/absent backend to the sqlite tier", () => {
    expect(resyncIntervalForBackend(null)).toBe(30_000);
    expect(resyncIntervalForBackend(undefined)).toBe(30_000);
  });
});

// ---------------------------------------------------------------------------
// TaskCache — per-project last-known board
// ---------------------------------------------------------------------------

describe("TaskCache", () => {
  it("round-trips a per-project board", () => {
    const cache = new TaskCache();
    cache.set("projA", [makeTask(1)]);
    expect(cache.get("projA")?.map((t) => t.id)).toEqual([1]);
  });

  it("returns undefined for an unseen project", () => {
    expect(new TaskCache().get("nope")).toBeUndefined();
  });

  it("isolates projects from one another", () => {
    const cache = new TaskCache();
    cache.set("projA", [makeTask(1)]);
    cache.set("projB", [makeTask(2), makeTask(3)]);
    expect(cache.get("projA")?.map((t) => t.id)).toEqual([1]);
    expect(cache.get("projB")?.map((t) => t.id)).toEqual([2, 3]);
  });

  it("stores an independent copy: mutating the source array does not corrupt the cache", () => {
    const cache = new TaskCache();
    const source = [makeTask(1)];
    cache.set("projA", source);
    source.push(makeTask(2)); // mutate after caching
    expect(cache.get("projA")?.map((t) => t.id)).toEqual([1]);
  });
});

describe("finishActions", () => {
  it("offers Mark done in a lane and Reopen once finished", () => {
    expect(finishActions("active")).toEqual({ markDone: true, reopen: false });
    expect(finishActions("review")).toEqual({ markDone: true, reopen: false });
    expect(finishActions("done")).toEqual({ markDone: false, reopen: true });
    expect(finishActions("cancelled")).toEqual({ markDone: false, reopen: true });
    for (const s of ["open", "backlog"] as const) {
      expect(finishActions(s)).toEqual({ markDone: false, reopen: false });
    }
  });
});
