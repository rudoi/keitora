import { beforeEach, describe, expect, it, vi } from "vitest";

const transport = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn(), channel: vi.fn() }));
vi.mock("./transport", () => ({
  getTransport: () => transport,
}));

import {
  ageText,
  createHealthStore,
  glyphFindings,
  glyphLabel,
  glyphTitle,
  glyphTone,
  glyphView,
  groupFindings,
  type HealthFinding,
  OPEN_HEALTH_EVENT,
  requestHealthSettings,
  scopedSelector,
  scopeFindings,
  statusJson,
  subscribeHealth,
  takeHealthFocus,
} from "./health";

const T0 = Date.UTC(2026, 0, 1, 10, 0, 0);

function finding(over: Partial<HealthFinding> = {}): HealthFinding {
  return {
    project: "proj",
    worktree: "bravo",
    checkId: "hooks_vs_herdr",
    severity: "warn",
    message: "@bravo: herdr sees a codex agent (idle) but its hooks have not reported",
    detail: {},
    firstSeen: T0,
    lastSeen: T0 + 60_000,
    occurrences: 1,
    clearedAt: null,
    remedy: "",
    remedyAt: null,
    ...over,
  };
}

describe("glyph", () => {
  it("shows nothing for info or no findings", () => {
    expect(glyphTone([])).toBeNull();
    expect(glyphTone([finding({ severity: "info" })])).toBeNull();
    expect(glyphTitle([finding({ severity: "info" })])).toBe("");
  });

  it("is amber for warn and red when any error is there", () => {
    expect(glyphTone([finding()])).toBe("warn");
    expect(glyphTone([finding(), finding({ checkId: "hooks_missing", severity: "error" })])).toBe(
      "error",
    );
  });

  it("ignores cleared findings", () => {
    expect(glyphTone([finding({ clearedAt: T0 })])).toBeNull();
  });

  it("titles the highest severity, then the newest, with +n more", () => {
    const older = finding({ checkId: "a", message: "older warn", lastSeen: T0 });
    const newer = finding({ checkId: "b", message: "newer warn", lastSeen: T0 + 5 });
    const error = finding({ checkId: "c", message: "the error", severity: "error", lastSeen: 1 });
    const info = finding({ checkId: "d", message: "info", severity: "info", lastSeen: T0 + 9 });
    expect(glyphFindings([older, newer, error, info]).map((f) => f.message)).toEqual([
      "the error",
      "newer warn",
      "older warn",
    ]);
    expect(glyphTitle([older, newer, error, info])).toBe("the error\n(+2 more)");
    expect(glyphTitle([older])).toBe("older warn");
  });

  it("labels the issue count", () => {
    expect(glyphLabel(1)).toBe("Health: 1 issue");
    expect(glyphLabel(3)).toBe("Health: 3 issues");
  });
});

describe("scopeFindings", () => {
  const all = [
    finding(),
    finding({ worktree: "alpha" }),
    finding({ worktree: "", checkId: "pipeline_stalled" }),
    finding({ project: "other-proj" }),
    finding({ project: "", worktree: "", checkId: "herdr_link" }),
    finding({ checkId: "cleared", clearedAt: T0 }),
  ];
  it("takes one worktree's active findings", () => {
    expect(scopeFindings(all, "proj", "bravo").map((f) => f.checkId)).toEqual(["hooks_vs_herdr"]);
  });
  it("takes a project's worktree and project-level findings", () => {
    expect(scopeFindings(all, "proj").map((f) => `${f.worktree}:${f.checkId}`)).toEqual([
      "bravo:hooks_vs_herdr",
      "alpha:hooks_vs_herdr",
      ":pipeline_stalled",
    ]);
  });
});

describe("scopedSelector", () => {
  it("keeps a scope's array while only other scopes change", () => {
    const select = scopedSelector("proj", "bravo");
    const first = select([finding(), finding({ worktree: "alpha" })]);
    // A fresh event: new objects, same content for bravo, alpha moved on.
    const second = select([finding(), finding({ worktree: "alpha", severity: "error" })]);
    expect(second).toBe(first);
    // bravo's own finding changes: a new array.
    const third = select([finding({ message: "changed" })]);
    expect(third).not.toBe(first);
    expect(third.map((f) => f.message)).toEqual(["changed"]);
    expect(select([finding({ message: "changed" })])).toBe(third);
  });

  it("serves useSyncExternalStore from a store that tells its listeners", () => {
    const store = createHealthStore();
    const select = scopedSelector("proj");
    const seen: HealthFinding[][] = [];
    const off = store.subscribe(() => seen.push(select(store.get())));
    store.set([finding()]);
    store.set([finding(), finding({ project: "other-proj" })]);
    expect(seen).toHaveLength(2);
    expect(seen[1]).toBe(seen[0]);
    off();
    store.set([]);
    expect(seen).toHaveLength(2);
  });
});

describe("glyphView", () => {
  it("is one pass's tone, title and count", () => {
    expect(glyphView([finding({ severity: "info" })])).toBeNull();
    expect(
      glyphView([finding(), finding({ checkId: "x", severity: "error", message: "bad" })]),
    ).toEqual({ tone: "error", title: "bad\n(+1 more)", count: 2 });
  });
});

describe("groupFindings", () => {
  it("groups by project (app first) then worktree (project-level first)", () => {
    const groups = groupFindings([
      finding({ worktree: "bravo" }),
      finding({ worktree: "alpha", checkId: "x", severity: "info" }),
      finding({ worktree: "alpha", checkId: "y", severity: "error" }),
      finding({ worktree: "", checkId: "pipeline_stalled" }),
      finding({ project: "", worktree: "", checkId: "herdr_link" }),
      finding({ project: "my-game", worktree: "main" }),
    ]);
    expect(groups.map((g) => g.project)).toEqual(["", "my-game", "proj"]);
    const proj = groups[2];
    expect(proj.worktrees.map((w) => w.worktree)).toEqual(["", "alpha", "bravo"]);
    expect(proj.worktrees[1].findings.map((f) => f.checkId)).toEqual(["y", "x"]);
  });
});

describe("statusJson", () => {
  it("copies the active findings in keitora status --json's shape", () => {
    const json = JSON.parse(
      statusJson([
        finding({ remedy: "reinstalled hooks", remedyAt: T0 }),
        finding({ checkId: "gone", clearedAt: T0 }),
      ]),
    );
    expect(json).toEqual({
      findings: [
        {
          project: "proj",
          worktree: "bravo",
          check_id: "hooks_vs_herdr",
          severity: "warn",
          message: "@bravo: herdr sees a codex agent (idle) but its hooks have not reported",
          first_seen: "2026-01-01T10:00:00Z",
          last_seen: "2026-01-01T10:01:00Z",
          occurrences: 1,
          remedy: "reinstalled hooks",
        },
      ],
    });
  });
});

describe("ageText", () => {
  it("steps from seconds to minutes to hours", () => {
    expect(ageText(119_999)).toBe("119s");
    expect(ageText(120_000)).toBe("2m");
    expect(ageText(7_200_000)).toBe("2h");
    expect(ageText(-1)).toBe("0s");
  });
});

/** Let every queued promise callback run. */
function settle(): Promise<void> {
  return new Promise((r) => setTimeout(r, 0));
}

describe("subscribeHealth", () => {
  beforeEach(() => {
    transport.invoke.mockReset();
    transport.listen.mockReset();
  });

  it("subscribes, reads the snapshot, follows events and unsubscribes", async () => {
    const hook: { emit?: (p: HealthFinding[]) => void } = {};
    const unlisten = vi.fn();
    transport.listen.mockImplementation((channel: string, cb: (p: HealthFinding[]) => void) => {
      expect(channel).toBe("health://changed");
      hook.emit = cb;
      return Promise.resolve(unlisten);
    });
    transport.invoke.mockResolvedValue([finding()]);
    const seen: HealthFinding[][] = [];
    const stop = subscribeHealth((f) => seen.push(f));
    await settle();
    expect(transport.invoke).toHaveBeenCalledWith("health_list", undefined);
    expect(seen).toEqual([[finding()]]);
    hook.emit?.([]);
    expect(seen).toEqual([[finding()], []]);
    stop();
    expect(unlisten).toHaveBeenCalled();
    hook.emit?.([finding()]);
    expect(seen.length).toBe(2);
  });

  it("reads the snapshot only once the listener is registered", async () => {
    let resolve: (fn: () => void) => void = () => {};
    transport.listen.mockReturnValue(new Promise((r) => (resolve = r)));
    transport.invoke.mockResolvedValue([finding()]);
    const seen: HealthFinding[][] = [];
    subscribeHealth((f) => seen.push(f));
    await settle();
    expect(transport.invoke).not.toHaveBeenCalled();
    resolve(() => {});
    await settle();
    expect(transport.invoke).toHaveBeenCalledWith("health_list", undefined);
    expect(seen).toEqual([[finding()]]);
  });

  it("ignores a snapshot that arrives after an event", async () => {
    const hook: { emit?: (p: HealthFinding[]) => void } = {};
    transport.listen.mockImplementation((_: string, cb: (p: HealthFinding[]) => void) => {
      hook.emit = cb;
      return Promise.resolve(() => {});
    });
    let reply: (f: HealthFinding[]) => void = () => {};
    transport.invoke.mockReturnValue(new Promise((r) => (reply = r)));
    const seen: HealthFinding[][] = [];
    subscribeHealth((f) => seen.push(f));
    await settle();
    expect(transport.invoke).toHaveBeenCalled();
    hook.emit?.([]);
    reply([finding()]);
    await settle();
    expect(seen).toEqual([[]]);
  });

  it("still reads the snapshot when the listener can't be registered", async () => {
    transport.listen.mockRejectedValue(new Error("no event bridge"));
    transport.invoke.mockResolvedValue([finding()]);
    const seen: HealthFinding[][] = [];
    subscribeHealth((f) => seen.push(f));
    await settle();
    expect(seen).toEqual([[finding()]]);
  });

  it("an unsubscribe before the listener resolves still unlistens", async () => {
    const unlisten = vi.fn();
    let resolve: (fn: () => void) => void = () => {};
    transport.listen.mockReturnValue(new Promise((r) => (resolve = r)));
    transport.invoke.mockResolvedValue([]);
    const stop = subscribeHealth(() => {});
    stop();
    resolve(unlisten);
    await Promise.resolve();
    await Promise.resolve();
    expect(unlisten).toHaveBeenCalled();
  });
});

describe("requestHealthSettings", () => {
  it("dispatches the open event and leaves one pending focus", () => {
    const target = new EventTarget();
    const heard = vi.fn();
    target.addEventListener(OPEN_HEALTH_EVENT, heard);
    requestHealthSettings(target);
    expect(heard).toHaveBeenCalledTimes(1);
    expect(takeHealthFocus()).toBe(true);
    expect(takeHealthFocus()).toBe(false);
  });
});
