import { describe, expect, it } from "vitest";
import type { AttentionKind } from "./attention";
import type { BuildProgress, ProjectSummary, WorktreeActivity, WorktreeInfo } from "./ipc";
import type { ProjectOverview } from "./projectsOverview";
import {
  activeRegistryKey,
  type BuildGroupsInput,
  buildGroups,
  buildLane,
  sameData,
  shareGroups,
  summarize,
} from "./railGroups";
import type { SessionState, WorktreeSession } from "./sessions";

// ── Fixtures ──────────────────────────────────────────────────────────────────
// Every project's registry key differs from its resolved name, so any join done on the
// wrong string shows up as a missing lane/count.

function summary(key: string, resolved: string): ProjectSummary {
  return {
    name: key,
    resolved_name: resolved,
    root: `/p/${key}`,
    active: false,
    db_path: `/p/${key}/tasks.db`,
    worktrees: [],
    backend: "sqlite",
  };
}

function wt(name: string, extra: Partial<WorktreeInfo> = {}): WorktreeInfo {
  return {
    name,
    path: `/w/${name}`,
    status: "available",
    cli: "claude",
    terminated: false,
    ...extra,
  };
}

function session(project: string, worktree: string, state: SessionState): WorktreeSession {
  return { project, worktree, state, current_task_id: null, pid: null, last_heartbeat: "" };
}

const running: BuildProgress = {
  adapter: "ubt",
  label: "Build",
  done: 1,
  total: 2,
  phase: null,
  state: "running",
  startedAt: 0,
  etaSecs: null,
  percent: 50,
  message: null,
};

function activity(
  project: string,
  worktree: string,
  opts: { working?: boolean; building?: boolean } = {},
): [string, WorktreeActivity] {
  return [
    `${project}/${worktree}`,
    {
      project,
      worktree,
      cli: "claude",
      branch: null,
      workflow: null,
      build: opts.building ? running : null,
      working: opts.working ?? false,
    },
  ];
}

const alpha = summary("alpha", "alpha-res");
const beta = summary("beta", "beta-res");

const betaOverview: ProjectOverview = {
  key: "beta",
  resolved_name: "beta-res",
  worktrees: [wt("b1"), wt("b2"), wt("b3", { status: "missing" })],
  sessions: [session("beta-res", "b1", "waiting"), session("beta-res", "b2", "busy")],
  active_tasks: [
    { worktree: "b2", id: 12, title: "Beta work" },
    { worktree: "gone", id: 13, title: "Unconfigured worktree" },
  ],
};
const alphaOverview: ProjectOverview = {
  key: "alpha",
  resolved_name: "alpha-res",
  worktrees: [wt("ov-only")],
  sessions: [],
  active_tasks: [{ worktree: "ov-only", id: 99, title: "Stale overview task" }],
};

function input(over: Partial<BuildGroupsInput> = {}): BuildGroupsInput {
  return {
    projects: [alpha, beta],
    activeResolvedName: "alpha-res",
    activeKey: "alpha",
    activeReady: true,
    activeWorktrees: [wt("a1"), wt("a2")],
    sessions: new Map([["a1", session("alpha-res", "a1", "idle")]]),
    attention: new Map<string, AttentionKind>(),
    activeTaskByWorktree: new Map([["a1", { id: 7, title: "Fix it" }]]),
    overview: [alphaOverview, betaOverview],
    activity: new Map(),
    liveCounts: new Map(),
    selected: null,
    maxDots: 8,
    ...over,
  };
}

// ── buildGroups ───────────────────────────────────────────────────────────────

describe("activeRegistryKey", () => {
  it("prefers the entry the registry marks active among same-named entries", () => {
    const twin = summary("alpha-twin", "alpha-res");
    const active = { ...summary("alpha", "alpha-res"), active: true };
    expect(activeRegistryKey([twin, active], "alpha-res")).toBe("alpha");
  });

  it("falls back to the first match, then to the resolved name", () => {
    expect(activeRegistryKey([alpha, beta], "beta-res")).toBe("beta");
    expect(activeRegistryKey([alpha], "unknown")).toBe("unknown");
    expect(activeRegistryKey([alpha], null)).toBeNull();
  });
});

describe("buildGroups", () => {
  it("renders live lanes only for the active registry key when two resolve alike", () => {
    // A second registration that resolves to the active project's name.
    const twin = summary("alpha-twin", "alpha-res");
    const twinOverview: ProjectOverview = {
      key: "alpha-twin",
      resolved_name: "alpha-res",
      worktrees: [wt("t1")],
      sessions: [],
      active_tasks: [],
    };
    const [a, t] = buildGroups(
      input({ projects: [alpha, twin], overview: [alphaOverview, twinOverview] }),
    );
    expect(a.lanes.map((l) => l.name)).toEqual(["a1", "a2"]);
    // The twin shares the resolved name (so it reads as active for session data) but
    // shows its own worktrees, not the active project's.
    expect(t.lanes.map((l) => l.name)).toEqual(["t1"]);
  });

  it("carries each lane's heartbeat and the group's latest one", () => {
    const hb = (w: string, state: SessionState, t: string) => ({
      ...session("beta-res", w, state),
      last_heartbeat: t,
    });
    const overview = [
      alphaOverview,
      {
        ...betaOverview,
        sessions: [
          hb("b1", "idle", "2026-10-16 09:00:00"),
          hb("b2", "idle", "2026-10-16 11:00:00"),
        ],
      },
    ];
    const [a, b] = buildGroups(input({ overview }));
    expect(b.lanes.map((l) => l.lastHeartbeat)).toEqual([
      "2026-10-16 09:00:00",
      "2026-10-16 11:00:00",
      null,
    ]);
    expect(b.summaryStats.lastActivity).toBe("2026-10-16 11:00:00");
    // Mixed formats compare as times, not strings.
    const mixed = [
      alphaOverview,
      {
        ...betaOverview,
        sessions: [
          hb("b1", "idle", "2026-10-16T09:00:00Z"),
          hb("b2", "idle", "2026-10-16 12:00:00"),
        ],
      },
    ];
    expect(buildGroups(input({ overview: mixed }))[1].summaryStats.lastActivity).toBe(
      "2026-10-16 12:00:00",
    );
    expect(b.summaryStats.active).toBe(false);
    // A busy session counts as working right now.
    const [, busy] = buildGroups(input());
    expect(busy.summaryStats.active).toBe(true);
    // An empty heartbeat (the fixture default) counts as none.
    expect(a.lanes[0].lastHeartbeat).toBeNull();
    expect(a.summaryStats.lastActivity).toBeNull();
  });

  it("keeps project order and reports key and resolved name separately", () => {
    const groups = buildGroups(input({ projects: [beta, alpha] }));
    expect(groups.map((g) => g.key)).toEqual(["beta", "alpha"]);
    expect(groups.map((g) => g.resolvedName)).toEqual(["beta-res", "alpha-res"]);
    expect(groups[0].summary).toBe(beta);
  });

  it("marks the active group by resolved name", () => {
    const [a, b] = buildGroups(input());
    expect(a.isActive).toBe(true);
    expect(b.isActive).toBe(false);
    // A key that happens to equal the active resolved name is not a match.
    const [a2] = buildGroups(input({ activeResolvedName: "alpha" }));
    expect(a2.isActive).toBe(false);
  });

  it("builds the active group from the rich active-only sources", () => {
    const [a] = buildGroups(
      input({ attention: new Map<string, AttentionKind>([["a2", "waiting"]]) }),
    );
    expect(a.loading).toBe(false);
    expect(a.lanes.map((l) => l.name)).toEqual(["a1", "a2"]);
    expect(a.lanes[0].sessionState).toBe("idle");
    expect(a.lanes[0].task).toEqual({ id: 7, title: "Fix it" });
    expect(a.lanes[1].attn).toBe("waiting");
    expect(a.lanes[1].blockState).toBe("waiting");
  });

  it("carries a finished attention on the active group without turning it orange", () => {
    const [a] = buildGroups(
      input({ attention: new Map<string, AttentionKind>([["a1", "finished"]]) }),
    );
    expect(a.lanes[0].attn).toBe("finished");
    expect(a.lanes[0].blockState).toBe("idle");
  });

  it("builds a non-active group from its overview entry, matched by key", () => {
    const [, b] = buildGroups(input());
    expect(b.loading).toBe(false);
    expect(b.lanes.map((l) => l.name)).toEqual(["b1", "b2", "b3"]);
    expect(b.lanes.map((l) => l.sessionState)).toEqual(["waiting", "busy", undefined]);
  });

  it("shows a non-active group's active tasks from its overview entry", () => {
    const [a, b] = buildGroups(input());
    expect(b.lanes.map((l) => l.task)).toEqual([
      undefined,
      { id: 12, title: "Beta work" },
      undefined,
    ]);
    // The warm active group reads its own task list, never the overview's.
    expect(a.lanes.map((l) => l.task)).toEqual([{ id: 7, title: "Fix it" }, undefined]);
  });

  it("surfaces only a waiting session as attention in a non-active group", () => {
    const [, b] = buildGroups(input());
    expect(b.lanes[0].attn).toBe("waiting");
    expect(b.lanes[0].blockState).toBe("waiting");
    expect(b.lanes[1].attn).toBeUndefined();
    expect(b.lanes[1].blockState).toBe("busy");
    // An unavailable worktree never carries attention.
    const [, b2] = buildGroups(
      input({
        overview: [
          {
            ...betaOverview,
            worktrees: [wt("b1", { status: "missing" })],
            sessions: [session("beta-res", "b1", "waiting")],
          },
        ],
      }),
    );
    expect(b2.lanes[0].attn).toBeUndefined();
  });

  it("falls back to the overview for the active group until its rich sources are ready", () => {
    const [a] = buildGroups(input({ activeReady: false }));
    expect(a.isActive).toBe(true);
    expect(a.lanes.map((l) => l.name)).toEqual(["ov-only"]);
    expect(a.summaryStats.focusedDot).toBeNull();
  });

  it("flags loading while the overview has not arrived (except the ready active group)", () => {
    const [a, b] = buildGroups(input({ overview: null }));
    expect(a.loading).toBe(false);
    expect(b.loading).toBe(true);
    expect(b.lanes).toEqual([]);
    const [a2] = buildGroups(input({ overview: null, activeReady: false }));
    expect(a2.loading).toBe(true);
    expect(a2.lanes).toEqual([]);
  });

  it("gives a project missing from the overview no lanes, not loading", () => {
    const [, b] = buildGroups(input({ overview: [alphaOverview] }));
    expect(b.loading).toBe(false);
    expect(b.lanes).toEqual([]);
    expect(b.summaryStats.dots).toEqual([]);
  });

  it("joins activity by resolved name, only for available worktrees", () => {
    const [, b] = buildGroups(
      input({
        activity: new Map([
          activity("beta-res", "b1", { building: true }),
          activity("beta-res", "b2", { working: true }),
          activity("beta-res", "b3", { working: true }),
          // Keyed by registry key: must not be picked up.
          activity("beta", "b1", { working: true }),
        ]),
      }),
    );
    expect(b.lanes[0].building).toBe(true);
    expect(b.lanes[0].working).toBe(false);
    expect(b.lanes[0].blockState).toBe("waiting"); // waiting wins over a build
    expect(b.lanes[1].working).toBe(true);
    expect(b.lanes[2].activity).toBeUndefined(); // missing worktree
    expect(b.lanes[2].working).toBe(false);
  });

  it("tints a building lane 'working' in the active group", () => {
    const [a] = buildGroups(
      input({ activity: new Map([activity("alpha-res", "a1", { building: true })]) }),
    );
    expect(a.lanes[0].building).toBe(true);
    expect(a.lanes[0].blockState).toBe("working");
  });

  it("marks terminated only on available worktrees", () => {
    const [a] = buildGroups(
      input({
        activeWorktrees: [
          wt("a1", { terminated: true }),
          wt("a2", { terminated: true, status: "not_git" }),
        ],
      }),
    );
    expect(a.lanes[0].terminated).toBe(true);
    expect(a.lanes[0].blockState).toBe("terminated");
    expect(a.lanes[1].terminated).toBe(false);
    expect(a.lanes[1].blockState).toBe("offline");
  });

  it("counts waiting, busy and building lanes and takes live from liveCounts by resolved name", () => {
    const [, b] = buildGroups(
      input({
        overview: [
          {
            ...betaOverview,
            worktrees: [wt("b1"), wt("b2"), wt("b3"), wt("b4")],
            sessions: [session("beta-res", "b1", "waiting"), session("beta-res", "b4", "waiting")],
          },
        ],
        activity: new Map([
          activity("beta-res", "b2", { working: true }),
          activity("beta-res", "b3", { working: true, building: true }),
        ]),
        liveCounts: new Map([
          ["beta-res", 3],
          ["beta", 99],
        ]),
      }),
    );
    expect(b.summaryStats.waiting).toBe(2);
    expect(b.summaryStats.busy).toBe(1); // working but not building
    expect(b.summaryStats.building).toBe(1);
    expect(b.summaryStats.live).toBe(3);
  });

  it("reports zero live sessions when the project has none", () => {
    const [a] = buildGroups(input());
    expect(a.summaryStats.live).toBe(0);
  });

  it("lists dots for available lanes only, in lane order", () => {
    const [, b] = buildGroups(input());
    expect(b.summaryStats.dots).toEqual(["waiting", "busy"]);
    expect(b.summaryStats.overflow).toBe(0);
  });

  it("marks the selected lane's dot in the active group only", () => {
    const [a, b] = buildGroups(input({ selected: "a2" }));
    expect(a.summaryStats.focusedDot).toBe(1);
    expect(b.summaryStats.focusedDot).toBeNull();
    const [a2] = buildGroups(input({ selected: "nope" }));
    expect(a2.summaryStats.focusedDot).toBeNull();
  });

  it("indexes the focused dot among available lanes", () => {
    const [a] = buildGroups(
      input({
        activeWorktrees: [wt("a0", { status: "missing" }), wt("a1"), wt("a2")],
        selected: "a2",
      }),
    );
    expect(a.summaryStats.dots).toHaveLength(2);
    expect(a.summaryStats.focusedDot).toBe(1);
  });

  it("caps dots at maxDots and reports the rest as overflow", () => {
    const many = Array.from({ length: 11 }, (_, n) => wt(`a${n}`));
    const [a] = buildGroups(input({ activeWorktrees: many, sessions: new Map(), maxDots: 8 }));
    expect(a.summaryStats.dots).toHaveLength(8);
    expect(a.summaryStats.overflow).toBe(3);
  });

  it("keeps an overflowing focused dot visible in the last slot", () => {
    const many = Array.from({ length: 11 }, (_, n) => wt(`a${n}`));
    const [a] = buildGroups(
      input({
        activeWorktrees: many,
        sessions: new Map([["a10", session("alpha-res", "a10", "busy")]]),
        selected: "a10",
        maxDots: 8,
      }),
    );
    expect(a.summaryStats.dots).toHaveLength(8);
    expect(a.summaryStats.dots[7]).toBe("busy");
    expect(a.summaryStats.focusedDot).toBe(7);
    expect(a.summaryStats.overflow).toBe(3);
  });

  it("leaves a focused dot that already fits in place", () => {
    const many = Array.from({ length: 11 }, (_, n) => wt(`a${n}`));
    const [a] = buildGroups(
      input({ activeWorktrees: many, sessions: new Map(), selected: "a3", maxDots: 8 }),
    );
    expect(a.summaryStats.focusedDot).toBe(3);
  });
});

// ── summarize ─────────────────────────────────────────────────────────────────

describe("summarize", () => {
  it("shows no dots and drops the focus when maxDots is zero", () => {
    const [a] = buildGroups(input({ selected: "a1" }));
    const s = summarize(a.lanes, 0, "a1", 0);
    expect(s.dots).toEqual([]);
    expect(s.focusedDot).toBeNull();
    expect(s.overflow).toBe(2);
  });
});

describe("buildLane", () => {
  const acts = new Map([activity("res", "a", { building: true, working: true })]);

  it("derives the block state, activity and attention for an available worktree", () => {
    const s = session("res", "a", "busy");
    const l = buildLane(wt("a"), "res", s, "waiting", undefined, acts);
    expect(l.blockState).toBe("waiting");
    expect(l.attn).toBe("waiting");
    expect(l.building).toBe(true);
    expect(l.working).toBe(true);
    expect(l.sessionState).toBe("busy");
  });

  it("ignores attention, activity and terminated for an unavailable worktree", () => {
    const l = buildLane(
      wt("a", { status: "missing", terminated: true }),
      "res",
      undefined,
      "waiting",
      undefined,
      acts,
    );
    expect(l.attn).toBeUndefined();
    expect(l.activity).toBeUndefined();
    expect(l.terminated).toBe(false);
    expect(l.blockState).toBe("offline");
  });

  it("marks an available terminated worktree terminated", () => {
    const l = buildLane(
      wt("a", { terminated: true }),
      "res",
      undefined,
      undefined,
      undefined,
      new Map(),
    );
    expect(l.terminated).toBe(true);
    expect(l.blockState).toBe("terminated");
  });
});

// ── sameData / shareGroups ────────────────────────────────────────────────────

describe("sameData", () => {
  it("compares plain data structurally", () => {
    expect(sameData({ a: [1, { b: "x" }], c: null }, { a: [1, { b: "x" }], c: null })).toBe(true);
    expect(sameData({ a: [1, 2] }, { a: [1, 3] })).toBe(false);
    expect(sameData([1], { 0: 1 })).toBe(false);
    expect(sameData(null, {})).toBe(false);
  });

  it("treats an undefined field like a missing one", () => {
    expect(sameData({ a: 1, b: undefined }, { a: 1 })).toBe(true);
    expect(sameData({ a: 1, b: 2 }, { a: 1 })).toBe(false);
  });
});

describe("shareGroups", () => {
  // A fresh build each call, as each activity tick produces (new Maps, new objects).
  const build = (over: Partial<BuildGroupsInput> = {}) =>
    buildGroups(
      input({
        activity: new Map([activity("alpha-res", "a1"), activity("beta-res", "b1")]),
        ...over,
      }),
    );

  it("returns the next groups as they are without a previous list", () => {
    const next = build();
    expect(shareGroups(null, next)).toBe(next);
  });

  it("returns the previous list itself when a rebuild changed nothing", () => {
    const prev = build();
    expect(shareGroups(prev, build())).toBe(prev);
  });

  it("keeps unchanged groups and lanes, replacing only what changed", () => {
    const prev = build();
    const next = build({
      activity: new Map([
        activity("alpha-res", "a1"),
        activity("beta-res", "b1", { working: true }),
      ]),
    });
    const shared = shareGroups(prev, next);
    expect(shared).not.toBe(prev);
    const [a, b] = shared;
    expect(a).toBe(prev[0]);
    expect(b).not.toBe(prev[1]);
    expect(b.lanes[0]).not.toBe(prev[1].lanes[0]);
    expect(b.lanes[0].working).toBe(true);
    // Siblings of the changed lane, and the unchanged summary, keep their identity.
    expect(b.lanes[1]).toBe(prev[1].lanes[1]);
    expect(b.lanes[2]).toBe(prev[1].lanes[2]);
    expect(b.summary).toBe(prev[1].summary);
    expect(b.summaryStats).not.toBe(prev[1].summaryStats);
  });

  it("follows the next order and drops or adds groups by key", () => {
    const prev = build();
    const gamma = summary("gamma", "gamma-res");
    const shared = shareGroups(prev, build({ projects: [beta, gamma] }));
    expect(shared.map((g) => g.key)).toEqual(["beta", "gamma"]);
    expect(shared[0]).toBe(prev[1]);
  });

  it("does not keep a group whose flags changed even when its lanes match", () => {
    // beta has no lanes either way; only `loading` flips once an overview arrives.
    const prev = build({ overview: null });
    const [, b] = shareGroups(prev, build({ overview: [alphaOverview] }));
    expect(prev[1].loading).toBe(true);
    expect(b.loading).toBe(false);
    expect(b).not.toBe(prev[1]);
    expect(b.lanes).toBe(prev[1].lanes);
  });

  it("keeps an equal lane across a reordered lane list but not the lane array", () => {
    const prev = build();
    const [a] = shareGroups(prev, build({ activeWorktrees: [wt("a2"), wt("a1")] }));
    expect(a.lanes.map((l) => l.name)).toEqual(["a2", "a1"]);
    expect(a.lanes).not.toBe(prev[0].lanes);
    expect(a.lanes[0]).toBe(prev[0].lanes[1]);
    expect(a.lanes[1]).toBe(prev[0].lanes[0]);
  });
});
