import { describe, expect, it } from "vitest";

import { formatActions, formatEta, showBuildBar, stageChipText, stageChipTone } from "./activity";
import type { BuildProgress, WorkflowState, WorktreeActivity } from "./ipc";

function wf(partial: Partial<WorkflowState>): WorkflowState {
  return {
    stage: "implementing",
    stageOrdinal: 7,
    stageTotal: 12,
    stageDetail: null,
    workNumber: null,
    branch: null,
    branchMismatch: false,
    done: null,
    total: null,
    prUrl: null,
    complexity: null,
    fileMtime: 0,
    stale: false,
    unreadable: false,
    ...partial,
  };
}

describe("stageChipText", () => {
  it("shows done/total (prettified, middot-separated) when both present and total>0", () => {
    expect(stageChipText(wf({ stage: "implementing", done: 4, total: 7 }))).toBe(
      "implementing · 4/7",
    );
  });

  it("omits progress when total is 0 or missing", () => {
    expect(stageChipText(wf({ stage: "plan", done: 0, total: 0 }))).toBe("plan");
    expect(stageChipText(wf({ stage: "refine", done: null, total: null }))).toBe("refine");
  });

  it("prettifies underscores and shows the PR number for review_gate with a pull url", () => {
    expect(
      stageChipText(wf({ stage: "review_gate", prUrl: "https://github.com/o/r/pull/126" })),
    ).toBe("review gate · PR #126");
  });

  it("review_gate without a PR url falls back to the prettified stage (+progress)", () => {
    expect(stageChipText(wf({ stage: "review_gate", prUrl: null }))).toBe("review gate");
    expect(stageChipText(wf({ stage: "review_gate", done: 7, total: 7 }))).toBe(
      "review gate · 7/7",
    );
  });

  it("keeps an unknown stage verbatim", () => {
    expect(stageChipText(wf({ stage: "rescoped", stageOrdinal: null }))).toBe("rescoped");
  });

  it("shows a stale stage's text verbatim (staleness is a tone, not text)", () => {
    expect(stageChipText(wf({ stage: "plan", stale: true }))).toBe("plan");
  });

  it("drops the fraction for a complete stage (no confusing 'complete 18/19')", () => {
    expect(stageChipText(wf({ stage: "complete", done: 18, total: 19 }))).toBe("complete");
    expect(stageChipText(wf({ stage: "complete", done: 19, total: 19 }))).toBe("complete");
  });
});

describe("stageChipTone — complete", () => {
  it("maps complete to the done tone, winning over staleness", () => {
    expect(stageChipTone(wf({ stage: "complete" }))).toBe("done");
    expect(stageChipTone(wf({ stage: "complete", stale: true }))).toBe("done");
  });
});

describe("stageChipTone", () => {
  it("maps stages and flags to tones", () => {
    expect(stageChipTone(undefined)).toBe("none");
    expect(stageChipTone(null)).toBe("none");
    expect(stageChipTone(wf({ stage: "review_gate" }))).toBe("gate");
    expect(stageChipTone(wf({ stage: "implementing", stale: true }))).toBe("stale");
    expect(stageChipTone(wf({ stage: "implementing", unreadable: true }))).toBe("warn");
    expect(stageChipTone(wf({ stage: "implementing", branchMismatch: true }))).toBe("warn");
    expect(stageChipTone(wf({ stage: "implementing" }))).toBe("default");
  });

  it("gives review_gate precedence over staleness", () => {
    expect(stageChipTone(wf({ stage: "review_gate", stale: true }))).toBe("gate");
  });

  it("gives staleness precedence over a warn flag", () => {
    expect(stageChipTone(wf({ stage: "implementing", stale: true, unreadable: true }))).toBe(
      "stale",
    );
  });
});

describe("formatEta", () => {
  it("formats seconds/minutes/hours and hides null/negative", () => {
    expect(formatEta(null)).toBeNull();
    expect(formatEta(-5)).toBeNull();
    expect(formatEta(0)).toBe("~0s");
    expect(formatEta(59)).toBe("~59s");
    expect(formatEta(60)).toBe("~1m");
    expect(formatEta(3599)).toBe("~60m");
    expect(formatEta(3600)).toBe("~1h 0m");
    expect(formatEta(7260)).toBe("~2h 1m");
  });
});

describe("formatActions", () => {
  it("formats done/total or an unknown total", () => {
    expect(formatActions(12, 340)).toBe("12/340 actions");
    expect(formatActions(0, 0)).toBe("—/— actions");
  });
});

describe("showBuildBar", () => {
  function act(partial: Partial<WorktreeActivity>): WorktreeActivity {
    return {
      project: "p",
      worktree: "a",
      cli: "claude",
      branch: null,
      workflow: null,
      build: null,
      working: false,
      drivenBy: null,
      drives: [],
      pendingPrompts: 0,
      drivePaused: false,
      ...partial,
    };
  }
  const build: BuildProgress = {
    adapter: "ubt",
    label: "UBT · Compile",
    done: 1,
    total: 2,
    phase: null,
    state: "running",
    startedAt: 0,
    etaSecs: null,
    percent: 50,
    message: null,
  };

  it("shows the bar whenever a build is present, in any state, else nothing", () => {
    expect(showBuildBar(undefined)).toBe(false);
    expect(showBuildBar(act({}))).toBe(false);
    expect(showBuildBar(act({ build }))).toBe(true);
    expect(showBuildBar(act({ build: { ...build, state: "succeeded" } }))).toBe(true);
    expect(showBuildBar(act({ build: { ...build, state: "failed" } }))).toBe(true);
  });
});
