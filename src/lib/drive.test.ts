import { describe, expect, it } from "vitest";
import {
  BANNER_SUMMARY_MAX,
  buildLaneDrive,
  type DrivenSummary,
  driveBadgeText,
  driveBadgeTitle,
  driveBannerView,
  driveChipText,
  driveChipTitle,
  driveKind,
  driveMenuDisabledReason,
  driveMenuItems,
  drivenCount,
  driveTag,
  isDriveMenuItem,
  isQuietWaiting,
  type LaneDrive,
  type PaneDrive,
  paneDrive,
} from "./drive";
import type { DriveView, WorktreeActivity } from "./ipc";
import type { SessionState, WorktreeSession } from "./sessions";

function sess(state: SessionState, extra: Partial<WorktreeSession> = {}): WorktreeSession {
  return {
    project: "my-game",
    worktree: "bravo",
    state,
    current_task_id: null,
    pid: null,
    last_heartbeat: "",
    ...extra,
  };
}

function act(extra: Partial<WorktreeActivity> = {}): WorktreeActivity {
  return {
    project: "my-game",
    worktree: "bravo",
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
  };
}

function drive(extra: Partial<LaneDrive> = {}): LaneDrive {
  return {
    drivenBy: "alpha",
    drives: [],
    pendingPrompts: 0,
    paused: false,
    brokered: false,
    escalatedReason: null,
    ...extra,
  };
}

function view(extra: Partial<DriveView> = {}): DriveView {
  return {
    driven: "bravo",
    driver: "alpha",
    since: "2026-01-01 00:00:00",
    forced_from: null,
    paused: false,
    escalate_after_s: 300,
    awaiting_turn: null,
    pending: [],
    ...extra,
  };
}

describe("buildLaneDrive", () => {
  it("is null when the lane neither drives nor is driven", () => {
    expect(buildLaneDrive(undefined, undefined)).toBeNull();
    expect(buildLaneDrive(sess("idle"), act())).toBeNull();
  });

  it("prefers the session's driver over the activity's", () => {
    const d = buildLaneDrive(sess("idle", { driven_by: "alpha" }), act({ drivenBy: "charlie" }));
    expect(d?.drivenBy).toBe("alpha");
    // Without a session row the activity covers it.
    expect(buildLaneDrive(undefined, act({ drivenBy: "charlie" }))?.drivenBy).toBe("charlie");
  });

  it("reads pending, paused and placeholder drives from the activity", () => {
    const d = buildLaneDrive(
      undefined,
      act({ drives: ["bravo", "charlie"], pendingPrompts: 0, drivePaused: false }),
    );
    expect(d).toEqual({
      drivenBy: null,
      drives: [
        { name: "bravo", visual: "idle", pending: 0 },
        { name: "charlie", visual: "idle", pending: 0 },
      ],
      pendingPrompts: 0,
      paused: false,
      brokered: false,
      escalatedReason: null,
    });
    const p = buildLaneDrive(
      undefined,
      act({ drivenBy: "alpha", pendingPrompts: 2, drivePaused: true }),
    );
    expect(p?.pendingPrompts).toBe(2);
    expect(p?.paused).toBe(true);
  });

  it("needs a driver for brokered", () => {
    const d = buildLaneDrive(sess("waiting", { brokered: true }), act({ drives: ["charlie"] }));
    expect(d?.brokered).toBe(false);
    expect(
      buildLaneDrive(sess("waiting", { driven_by: "alpha", brokered: true }), undefined)?.brokered,
    ).toBe(true);
  });

  it("keeps the escalation reason only while waiting and not brokered", () => {
    const reason = { driven_by: "alpha", escalated_reason: "needs prod credentials" };
    expect(buildLaneDrive(sess("waiting", reason), undefined)?.escalatedReason).toBe(
      "needs prod credentials",
    );
    expect(
      buildLaneDrive(sess("waiting", { ...reason, brokered: true }), undefined)?.escalatedReason,
    ).toBeNull();
    expect(buildLaneDrive(sess("busy", reason), undefined)?.escalatedReason).toBeNull();
  });
});

describe("isQuietWaiting", () => {
  it("is true only for a driven, brokered, waiting session", () => {
    expect(isQuietWaiting(sess("waiting", { driven_by: "alpha", brokered: true }))).toBe(true);
    expect(isQuietWaiting(sess("waiting", { driven_by: "alpha" }))).toBe(false);
    expect(isQuietWaiting(sess("waiting", { brokered: true }))).toBe(false);
    expect(isQuietWaiting(sess("busy", { driven_by: "alpha", brokered: true }))).toBe(false);
    expect(isQuietWaiting(undefined)).toBe(false);
  });
});

describe("driveKind", () => {
  it("is null unless the lane is driven", () => {
    expect(driveKind(null, "waiting")).toBeNull();
    expect(driveKind(drive({ drivenBy: null }), "waiting")).toBeNull();
  });

  it("splits a waiting lane into deciding and escalated", () => {
    expect(driveKind(drive({ brokered: true }), "waiting")).toBe("deciding");
    expect(driveKind(drive(), "waiting")).toBe("escalated");
    expect(driveKind(drive(), "idle")).toBe("driven");
    expect(driveKind(drive(), "busy")).toBe("driven");
    expect(driveKind(drive(), undefined)).toBe("driven");
  });
});

describe("badge text and title", () => {
  it("shows the driver, the pending count and paused", () => {
    expect(driveBadgeText(drive())).toBe("⇠ alpha");
    expect(driveBadgeText(drive({ pendingPrompts: 1 }))).toBe("⇠ alpha · 1");
    expect(driveBadgeText(drive({ pendingPrompts: 2, paused: true }))).toBe("⇠ alpha · 2 · paused");
    expect(driveBadgeText(drive({ paused: true }))).toBe("⇠ alpha · paused");
  });

  it("titles each kind", () => {
    expect(driveBadgeTitle(drive(), "driven")).toBe(
      "Driven by alpha. Anything you type here mixes with its prompts.",
    );
    expect(driveBadgeTitle(drive({ pendingPrompts: 1 }), "deciding")).toBe(
      "alpha is deciding 1 prompt.",
    );
    expect(driveBadgeTitle(drive({ pendingPrompts: 3 }), "deciding")).toBe(
      "alpha is deciding 3 prompts.",
    );
    // At least one prompt, even before the activity count catches up.
    expect(driveBadgeTitle(drive(), "deciding")).toBe("alpha is deciding 1 prompt.");
    expect(driveBadgeTitle(drive({ escalatedReason: "needs prod credentials" }), "escalated")).toBe(
      "alpha escalated: needs prod credentials",
    );
    expect(driveBadgeTitle(drive(), "escalated")).toBe("alpha escalated a prompt to you.");
  });

  it("lets paused win over driven and deciding, but not over an escalation reason", () => {
    const paused = drive({ paused: true, pendingPrompts: 1 });
    const text = "Driven by alpha (paused): prompts come to you.";
    expect(driveBadgeTitle(paused, "driven")).toBe(text);
    expect(driveBadgeTitle(paused, "deciding")).toBe(text);
    expect(driveBadgeTitle({ ...paused, escalatedReason: "why" }, "escalated")).toBe(
      "alpha escalated: why",
    );
  });
});

describe("driver chip", () => {
  const summaries: DrivenSummary[] = [
    { name: "bravo", visual: "idle", pending: 0 },
    { name: "charlie", visual: "busy", pending: 0 },
    { name: "delta", visual: "deciding", pending: 1 },
    { name: "echo", visual: "deciding", pending: 2 },
    { name: "foxtrot", visual: "needs-you", pending: 0 },
    { name: "golf", visual: "offline", pending: 0 },
  ];

  it("counts the driven worktrees", () => {
    expect(driveChipText(drive({ drivenBy: null, drives: summaries.slice(0, 2) }))).toBe(
      "drives 2",
    );
  });

  it("lists each driven worktree's state in the title", () => {
    expect(driveChipTitle(drive({ drivenBy: null, drives: summaries }))).toBe(
      [
        "Drives 6 worktrees:",
        "bravo: idle",
        "charlie: busy",
        "delta: deciding 1 prompt",
        "echo: deciding 2 prompts",
        "foxtrot: needs you",
        "golf: offline",
      ].join("\n"),
    );
    expect(driveChipTitle(drive({ drivenBy: null, drives: summaries.slice(0, 1) }))).toBe(
      "Drives 1 worktree:\nbravo: idle",
    );
  });
});

describe("driveTag", () => {
  it("is null for a lane that neither drives nor is driven", () => {
    expect(driveTag(null, "idle")).toBeNull();
    expect(driveTag(drive({ drivenBy: null }), "idle")).toBeNull();
  });

  it("is the badge on a driven lane, classed by kind and paused", () => {
    expect(driveTag(drive(), "idle")).toEqual({
      className: "lane-drive-badge",
      title: "Driven by alpha. Anything you type here mixes with its prompts.",
      text: "⇠ alpha",
    });
    expect(driveTag(drive({ brokered: true, paused: true }), "waiting")?.className).toBe(
      "lane-drive-badge deciding paused",
    );
    expect(driveTag(drive(), "waiting")?.className).toBe("lane-drive-badge escalated");
  });

  it("is the chip on a driver lane", () => {
    const d = drive({ drivenBy: null, drives: [{ name: "bravo", visual: "idle", pending: 0 }] });
    expect(driveTag(d, "idle")).toEqual({
      className: "lane-drive-chip",
      title: "Drives 1 worktree:\nbravo: idle",
      text: "drives 1",
    });
  });
});

describe("drive menu", () => {
  it("offers nothing unless the lane is driven", () => {
    expect(driveMenuItems(null)).toEqual([]);
    expect(driveMenuItems(drive({ drivenBy: null }))).toEqual([]);
  });

  it("offers take over, pause or resume, and release", () => {
    expect(driveMenuItems(drive())).toEqual(["take-over", "pause-driver", "release"]);
    expect(driveMenuItems(drive({ paused: true }))).toEqual([
      "take-over",
      "resume-driver",
      "release",
    ]);
  });

  it("recognises drive items", () => {
    expect(isDriveMenuItem("take-over")).toBe(true);
    expect(isDriveMenuItem("resume-driver")).toBe(true);
    expect(isDriveMenuItem("restart")).toBe(false);
  });

  it("disables take over only with no pending prompts", () => {
    expect(driveMenuDisabledReason("take-over", drive())).toBe("No pending prompts");
    expect(driveMenuDisabledReason("take-over", drive({ pendingPrompts: 1 }))).toBeNull();
    expect(driveMenuDisabledReason("release", drive())).toBeNull();
    expect(driveMenuDisabledReason("pause-driver", drive())).toBeNull();
  });
});

describe("drivenCount", () => {
  it("counts lanes with a driver", () => {
    expect(
      drivenCount([
        { drive: null },
        { drive: drive() },
        { drive: drive({ drivenBy: null, drives: [{ name: "x", visual: "idle", pending: 0 }] }) },
        { drive: drive({ drivenBy: "charlie" }) },
      ]),
    ).toBe(2);
  });
});

describe("paneDrive", () => {
  it("is null when the worktree is not driven", () => {
    expect(paneDrive(act(), sess("idle"), undefined)).toBeNull();
    expect(paneDrive(undefined, undefined, undefined)).toBeNull();
  });

  it("takes the oldest pending prompt and the count from the drive view", () => {
    const v = view({
      pending: [
        { id: 1, kind: "permission", tool_name: "Bash", summary: "rm -rf build", created_at: "" },
        {
          id: 2,
          kind: "question",
          tool_name: "AskUserQuestion",
          summary: "which?",
          created_at: "",
        },
      ],
    });
    const p = paneDrive(act({ drivenBy: "alpha", pendingPrompts: 5 }), sess("waiting"), v);
    expect(p).toEqual({
      driver: "alpha",
      paused: false,
      pendingCount: 2,
      firstPending: { toolName: "Bash", summary: "rm -rf build" },
      escalatedReason: null,
      forcedFrom: null,
    });
  });

  it("falls back to the activity's count without a drive view", () => {
    const p = paneDrive(
      act({ drivenBy: "alpha", pendingPrompts: 3, drivePaused: true }),
      undefined,
      undefined,
    );
    expect(p?.pendingCount).toBe(3);
    expect(p?.firstPending).toBeNull();
    expect(p?.paused).toBe(true);
  });

  it("carries the steal note and an escalation reason", () => {
    const p = paneDrive(
      undefined,
      sess("waiting", { driven_by: "alpha", escalated_reason: "needs prod credentials" }),
      view({ forced_from: "charlie" }),
    );
    expect(p?.forcedFrom).toBe("charlie");
    expect(p?.escalatedReason).toBe("needs prod credentials");
    expect(p?.pendingCount).toBe(0);
  });
});

describe("driveBannerView", () => {
  const pd = (extra: Partial<PaneDrive> = {}): PaneDrive => ({
    driver: "alpha",
    paused: false,
    pendingCount: 0,
    firstPending: null,
    escalatedReason: null,
    forcedFrom: null,
    ...extra,
  });
  const labels = (v: ReturnType<typeof driveBannerView>) => v.buttons.map((b) => b.label);

  it("default: warns that typing mixes with the driver, Take over disabled at zero pending", () => {
    const v = driveBannerView(pd());
    expect(v.text).toBe("Driven by alpha — what you type here mixes with alpha's prompts.");
    expect(v.escalated).toBe(false);
    expect(v.deciding).toBe(false);
    expect(v.pendingLine).toBeNull();
    expect(v.note).toBeNull();
    expect(labels(v)).toEqual(["Take over", "Release"]);
    expect(v.buttons[0]).toEqual({
      action: "take-over",
      label: "Take over",
      disabled: true,
      title: "No pending prompts",
    });
  });

  it("paused: says prompts come to the human and offers Resume", () => {
    const v = driveBannerView(pd({ paused: true }));
    expect(v.text).toBe(
      "Driven by alpha (paused) — prompts come to you, and alpha can't send until resumed.",
    );
    expect(labels(v)).toEqual(["Take over", "Resume", "Release"]);
  });

  it("deciding: shows the first pending prompt with a cut summary and the rest counted", () => {
    const long = "x".repeat(BANNER_SUMMARY_MAX + 5);
    const v = driveBannerView(
      pd({ pendingCount: 3, firstPending: { toolName: "Bash", summary: long } }),
    );
    expect(v.deciding).toBe(true);
    expect(v.buttons[0]).toMatchObject({ disabled: false, title: null });
    expect(v.pendingLine).toEqual({
      lead: "alpha is deciding: Bash",
      summary: `${"x".repeat(BANNER_SUMMARY_MAX)}…`,
      more: " (+2 more)",
    });
    const one = driveBannerView(
      pd({ pendingCount: 1, firstPending: { toolName: "Bash", summary: "rm -rf build" } }),
    );
    expect(one.pendingLine).toEqual({
      lead: "alpha is deciding: Bash",
      summary: "rm -rf build",
      more: "",
    });
  });

  it("cuts the summary on characters, not UTF-16 units", () => {
    const emoji = "😀".repeat(BANNER_SUMMARY_MAX);
    const v = driveBannerView(
      pd({ pendingCount: 1, firstPending: { toolName: "Bash", summary: emoji } }),
    );
    expect(v.pendingLine?.summary).toBe(emoji);
  });

  it("escalated: names the reason and wins over paused", () => {
    const v = driveBannerView(pd({ escalatedReason: "needs prod credentials", paused: true }));
    expect(v.text).toBe("alpha escalated: needs prod credentials");
    expect(v.escalated).toBe(true);
  });

  it("forced-from: adds the take-over note", () => {
    expect(driveBannerView(pd({ forcedFrom: "charlie" })).note).toBe("Took over from charlie.");
  });
});
