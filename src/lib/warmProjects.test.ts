import { describe, expect, it } from "vitest";
import type { View } from "../components/TopBar";
import type { ProjectSummary, ProjectWorktrees, WorktreeInfo } from "./ipc";
import {
  activeProject,
  emptyWarmState,
  firstAvailable,
  rememberSelection,
  rememberView,
  sameWorktrees,
  setActive,
  warmDecision,
  warmProject,
  worktreesFor,
} from "./warmProjects";

// ── Fixtures ──────────────────────────────────────────────────────────────────

function makeSummary(name: string): ProjectSummary {
  return {
    name,
    resolved_name: name,
    root: `/projects/${name}`,
    active: false,
    db_path: `/projects/${name}/keitora.db`,
    worktrees: ["alpha", "bravo"],
    backend: "sqlite",
  };
}

const wtAvailable = (name: string): WorktreeInfo => ({
  name,
  path: `/projects/x/${name}`,
  status: "available",
  cli: "claude",
  terminated: false,
});

const wtMissing = (name: string): WorktreeInfo => ({
  name,
  path: `/projects/x/${name}`,
  status: "missing",
  cli: "claude",
  terminated: false,
});

const worktreesA: WorktreeInfo[] = [wtAvailable("alpha"), wtAvailable("bravo")];
const worktreesB: WorktreeInfo[] = [wtAvailable("gamma"), wtAvailable("delta")];
const summaryA = makeSummary("projectA");
const summaryB = makeSummary("projectB");

// ── sameWorktrees ─────────────────────────────────────────────────────────────

describe("sameWorktrees", () => {
  it("returns true for the same reference", () => {
    expect(sameWorktrees(worktreesA, worktreesA)).toBe(true);
  });

  it("returns true for distinct arrays with identical contents (the resync case)", () => {
    const clone = worktreesA.map((w) => ({ ...w }));
    expect(clone).not.toBe(worktreesA);
    expect(sameWorktrees(worktreesA, clone)).toBe(true);
  });

  it("returns false when length differs", () => {
    expect(sameWorktrees(worktreesA, [worktreesA[0]])).toBe(false);
  });

  it("returns false when a status changes", () => {
    const changed = [wtAvailable("alpha"), wtMissing("bravo")];
    expect(sameWorktrees(worktreesA, changed)).toBe(false);
  });

  it("returns false when a name or path changes", () => {
    expect(sameWorktrees(worktreesA, [wtAvailable("alpha"), wtAvailable("zeta")])).toBe(false);
    const movedPath: WorktreeInfo[] = [
      {
        name: "alpha",
        path: "/elsewhere/alpha",
        status: "available",
        cli: "claude",
        terminated: false,
      },
      wtAvailable("bravo"),
    ];
    expect(sameWorktrees(worktreesA, movedPath)).toBe(false);
  });

  it("differs when only the cli changed (drives the rail glyph/menu refresh)", () => {
    const switched: WorktreeInfo[] = [
      {
        name: "alpha",
        path: "/projects/x/alpha",
        status: "available",
        cli: "codex",
        terminated: false,
      },
      wtAvailable("bravo"),
    ];
    expect(sameWorktrees(worktreesA, switched)).toBe(false);
  });

  it("differs when only the terminated flag changed (drives the rail and pane state)", () => {
    const terminated: WorktreeInfo[] = [
      { ...wtAvailable("alpha"), terminated: true },
      wtAvailable("bravo"),
    ];
    expect(sameWorktrees(worktreesA, terminated)).toBe(false);
  });
});

// ── firstAvailable ────────────────────────────────────────────────────────────

describe("firstAvailable", () => {
  it("returns the first available worktree name", () => {
    expect(firstAvailable(worktreesA)).toBe("alpha");
  });

  it("skips non-available statuses and returns the first available one", () => {
    const wts: WorktreeInfo[] = [wtMissing("missing1"), wtAvailable("ok"), wtAvailable("ok2")];
    expect(firstAvailable(wts)).toBe("ok");
  });

  it("returns null when all worktrees are non-available", () => {
    const wts: WorktreeInfo[] = [
      wtMissing("a"),
      { name: "b", path: "/b", status: "not_dir", cli: "claude", terminated: false },
      { name: "c", path: "/c", status: "not_git", cli: "claude", terminated: false },
    ];
    expect(firstAvailable(wts)).toBeNull();
  });

  it("returns null for an empty worktree list", () => {
    expect(firstAvailable([])).toBeNull();
  });
});

// ── warmProject: first warm ───────────────────────────────────────────────────

describe("warmProject — first warm seeds the project", () => {
  it("seeds the project, sets it active, defaults selected to first-available and view to 'grid'", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    expect(s1.active).toBe("projectA");
    expect(s1.projects["projectA"]).toBeDefined();
    expect(s1.projects["projectA"].summary).toBe(summaryA);
    expect(s1.projects["projectA"].worktrees).toBe(worktreesA);
    expect(s1.projects["projectA"].selected).toBe("alpha"); // first available
    expect(s1.projects["projectA"].view).toBe("grid");
  });

  it("uses caller-supplied selected when provided", () => {
    const s1 = warmProject(emptyWarmState, {
      summary: summaryA,
      worktrees: worktreesA,
      selected: "bravo",
    });
    expect(s1.projects["projectA"].selected).toBe("bravo");
  });

  it("uses caller-supplied view when provided", () => {
    const s1 = warmProject(emptyWarmState, {
      summary: summaryA,
      worktrees: worktreesA,
      view: "focus",
    });
    expect(s1.projects["projectA"].view).toBe("focus");
  });

  it("defaults selected to null when no worktrees are available", () => {
    const wts: WorktreeInfo[] = [wtMissing("x")];
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: wts });
    expect(s1.projects["projectA"].selected).toBeNull();
  });
});

// ── warmProject: second project promotes + first stays intact ─────────────────

describe("warmProject — warming a second project", () => {
  it("promotes the second project and sets it active; first stays in projects with worktrees intact", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });

    expect(s2.active).toBe("projectB");
    expect(s2.projects["projectB"]).toBeDefined();
    expect(s2.projects["projectA"]).toBeDefined();
    expect(s2.projects["projectA"].worktrees).toBe(worktreesA);
    expect(s2.projects["projectA"].selected).toBe("alpha");
    expect(s2.projects["projectA"].view).toBe("grid");
  });
});

// ── warmProject: re-warm keeps remembered selected/view, refreshes worktrees ──

describe("warmProject — re-warming an existing project", () => {
  it("keeps remembered selected and view but refreshes summary + worktrees", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = rememberSelection(s1, "projectA", "bravo");
    const s3 = rememberView(s2, "projectA", "focus");

    const newWorktrees: WorktreeInfo[] = [
      wtAvailable("alpha"),
      wtAvailable("bravo"),
      wtAvailable("charlie"),
    ];
    const newSummary: ProjectSummary = { ...summaryA, worktrees: ["alpha", "bravo", "charlie"] };
    const s4 = warmProject(s3, { summary: newSummary, worktrees: newWorktrees });

    expect(s4.active).toBe("projectA");
    expect(s4.projects["projectA"].selected).toBe("bravo"); // remembered; not clobbered
    expect(s4.projects["projectA"].view).toBe("focus"); // remembered; not clobbered
    expect(s4.projects["projectA"].worktrees).toBe(newWorktrees); // refreshed
    expect(s4.projects["projectA"].summary).toBe(newSummary); // refreshed
  });

  it("does not mutate the previous state object (immutability)", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const prevProjectEntry = s1.projects["projectA"];
    const newWorktrees: WorktreeInfo[] = [wtAvailable("alpha")];
    warmProject(s1, { summary: summaryA, worktrees: newWorktrees });
    // s1 must be unchanged
    expect(s1.projects["projectA"]).toBe(prevProjectEntry);
    expect(s1.projects["projectA"].worktrees).toBe(worktreesA);
  });
});

// ── setActive ─────────────────────────────────────────────────────────────────

describe("setActive", () => {
  it("flips the active project without changing any stored selected/view", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });
    // Both are warm; A is the previously active project, B is current.
    // setActive back to A.
    const s3 = setActive(s2, "projectA");

    expect(s3.active).toBe("projectA");
    // B's stored entry is untouched
    expect(s3.projects["projectB"]).toStrictEqual(s2.projects["projectB"]);
    // A's stored entry is untouched
    expect(s3.projects["projectA"]).toStrictEqual(s2.projects["projectA"]);
  });

  it("does not mutate the input state object", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });
    setActive(s2, "projectA");
    expect(s2.active).toBe("projectB"); // s2 unchanged
  });
});

// ── rememberSelection ─────────────────────────────────────────────────────────

describe("rememberSelection", () => {
  it("updates only the targeted project's selected; other projects are unchanged", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });

    const s3 = rememberSelection(s2, "projectA", "bravo");

    expect(s3.projects["projectA"].selected).toBe("bravo");
    // projectB untouched
    expect(s3.projects["projectB"]).toBe(s2.projects["projectB"]);
  });

  it("does NOT mutate the input state object", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const before = s1.projects["projectA"];
    rememberSelection(s1, "projectA", "bravo");
    expect(s1.projects["projectA"]).toBe(before);
    expect(s1.projects["projectA"].selected).toBe("alpha"); // original unchanged
  });

  it("is a no-op when the project name is not in state", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = rememberSelection(s1, "unknownProject", "bravo");
    expect(s2).toBe(s1); // same reference — no change
  });
});

// ── rememberView ──────────────────────────────────────────────────────────────

describe("rememberView", () => {
  it("updates only the targeted project's view; other projects are unchanged", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });

    const s3 = rememberView(s2, "projectA", "focus");

    expect(s3.projects["projectA"].view).toBe("focus");
    expect(s3.projects["projectB"]).toBe(s2.projects["projectB"]);
  });

  it("does NOT mutate the input state object", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const before = s1.projects["projectA"];
    rememberView(s1, "projectA", "focus");
    expect(s1.projects["projectA"]).toBe(before);
    expect(s1.projects["projectA"].view).toBe("grid"); // original unchanged
  });

  it("is a no-op when the project name is not in state", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = rememberView(s1, "unknownProject", "focus");
    expect(s2).toBe(s1);
  });
});

// ── activeProject ─────────────────────────────────────────────────────────────

describe("activeProject", () => {
  it("returns null from emptyWarmState", () => {
    expect(activeProject(emptyWarmState)).toBeNull();
  });

  it("returns the active project entry", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const proj = activeProject(s1);
    expect(proj).not.toBeNull();
    expect(proj?.summary.name).toBe("projectA");
  });

  it("returns the correct entry after setActive switches projects", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });
    const s3 = setActive(s2, "projectA");
    expect(activeProject(s3)?.summary.name).toBe("projectA");
  });
});

// ── Restore semantics ───────────────────────────────────

describe("restore semantics — switch away and back restores remembered state", () => {
  it("remembers selected/view on A, switch to B, switch back to A → A's values returned", () => {
    // Warm A and B
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });

    // Remember specific state on A
    const s3 = rememberSelection(s2, "projectA", "bravo");
    const s4 = rememberView(s3, "projectA", "focus");

    // Switch to B (A is still warm in the background)
    const s5 = setActive(s4, "projectB");
    expect(s5.active).toBe("projectB");
    // A's values are preserved in projects map
    expect(s5.projects["projectA"].selected).toBe("bravo");
    expect(s5.projects["projectA"].view).toBe("focus");

    // Switch back to A
    const s6 = setActive(s5, "projectA");
    expect(s6.active).toBe("projectA");
    const proj = activeProject(s6);
    expect(proj?.selected).toBe("bravo");
    expect(proj?.view).toBe("focus");
  });

  it("a genuinely new project on first warm defaults to first-available/grid", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });

    // Switch back to A (already warm, has defaults)
    const s3 = setActive(s2, "projectA");
    expect(s3.projects["projectA"].selected).toBe("alpha");
    expect(s3.projects["projectA"].view).toBe("grid");

    // A brand-new project C gets first-available/grid
    const summaryC = makeSummary("projectC");
    const worktreesC: WorktreeInfo[] = [wtMissing("dead"), wtAvailable("live")];
    const s4 = warmProject(s3, { summary: summaryC, worktrees: worktreesC });
    expect(s4.projects["projectC"].selected).toBe("live");
    expect(s4.projects["projectC"].view).toBe("grid");
  });

  it("re-warming A after remembering state does not lose that state (idempotency)", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = rememberSelection(s1, "projectA", "bravo");
    const s3 = rememberView(s2, "projectA", "focus");

    // Warm A again (active project refresh path)
    const newWorktrees: WorktreeInfo[] = [wtAvailable("alpha"), wtAvailable("bravo")];
    const s4 = warmProject(s3, { summary: summaryA, worktrees: newWorktrees });
    expect(s4.projects["projectA"].selected).toBe("bravo");
    expect(s4.projects["projectA"].view).toBe("focus");
  });
});

// ── Registry-update invariant ─────────────────────────────────────────────────

describe("registry-update invariant — only targeted project mutates", () => {
  it("warmProject for B does not touch A's entry", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const entryA = s1.projects["projectA"];
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });
    expect(s2.projects["projectA"]).toBe(entryA); // same reference
  });

  it("rememberSelection for B does not touch A's entry", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });
    const entryA = s2.projects["projectA"];
    const s3 = rememberSelection(s2, "projectB", "delta");
    expect(s3.projects["projectA"]).toBe(entryA);
  });

  it("rememberView for B does not touch A's entry", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const s2 = warmProject(s1, { summary: summaryB, worktrees: worktreesB });
    const entryA = s2.projects["projectA"];
    const s3 = rememberView(s2, "projectB", "focus");
    expect(s3.projects["projectA"]).toBe(entryA);
  });
});

// ── View type coverage ────────────────────────────────────────────────────────

describe("view type coverage", () => {
  it("accepts all View values via rememberView", () => {
    const s1 = warmProject(emptyWarmState, { summary: summaryA, worktrees: worktreesA });
    const views: View[] = ["board", "focus", "grid"];
    let state = s1;
    for (const v of views) {
      state = rememberView(state, "projectA", v);
      expect(state.projects["projectA"].view).toBe(v);
    }
  });
});

// ── Tagged worktree lists ─────────────────────────────────────────────────────

const tagged = (project: string | null, names: string[] = ["alpha"]): ProjectWorktrees => ({
  project,
  worktrees: names.map(wtAvailable),
});

describe("worktreesFor", () => {
  it("returns the list read for the named project", () => {
    const t = tagged("proj", ["alpha", "bravo"]);
    expect(worktreesFor("proj", t)).toBe(t.worktrees);
  });

  it("refuses a list read for another project", () => {
    expect(worktreesFor("proj", tagged("other-proj"))).toBeNull();
  });

  it("refuses a list read with no active project", () => {
    expect(worktreesFor("proj", tagged(null, []))).toBeNull();
  });

  it("is null without a project or a list", () => {
    expect(worktreesFor(null, tagged("proj"))).toBeNull();
    expect(worktreesFor("proj", null)).toBeNull();
  });
});

describe("warmDecision", () => {
  it("warms when the list was read for the active project", () => {
    const t = tagged("proj");
    expect(warmDecision("proj", t, false)).toEqual({ kind: "warm", worktrees: t.worktrees });
  });

  it("resyncs when the list was read for a different project", () => {
    expect(warmDecision("proj", tagged("other-proj"), false)).toEqual({
      kind: "resync",
      listProject: "other-proj",
    });
  });

  it("skips a list read for no project (a brief resolve failure, not a switch)", () => {
    expect(warmDecision("proj", tagged(null, []), false)).toEqual({ kind: "skip" });
  });

  it("skips while a switch is in flight, even on a mismatch", () => {
    expect(warmDecision("proj", tagged("other-proj"), true)).toEqual({ kind: "skip" });
    expect(warmDecision("proj", tagged("proj"), true)).toEqual({ kind: "skip" });
  });

  it("skips without a project or a list", () => {
    expect(warmDecision(null, tagged("proj"), false)).toEqual({ kind: "skip" });
    expect(warmDecision("proj", null, false)).toEqual({ kind: "skip" });
  });
});
