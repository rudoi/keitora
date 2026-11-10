import { describe, expect, it } from "vitest";
import type { LaneModel, SummaryStats } from "./railGroups";
import {
  groupAriaLabel,
  headerView,
  laneHintBadges,
  lastActivityLabel,
  sentinelAbove,
} from "./railGroupView";

function stats(extra: Partial<SummaryStats> = {}): SummaryStats {
  return {
    live: 0,
    waiting: 0,
    busy: 0,
    building: 0,
    dots: [],
    focusedDot: null,
    overflow: 0,
    lastActivity: null,
    active: false,
    ...extra,
  };
}

function lane(name: string, status: LaneModel["status"] = "available"): LaneModel {
  return {
    name,
    path: `/w/${name}`,
    cli: "claude",
    status,
    terminated: false,
    dispatch: true,
    sessionState: undefined,
    attn: undefined,
    task: undefined,
    activity: undefined,
    lastHeartbeat: null,
    building: false,
    working: false,
    blockState: status === "available" ? "idle" : "offline",
  };
}

describe("headerView", () => {
  it("shows the dot strip only while collapsed and only with dots", () => {
    const s = stats({ dots: ["idle", "busy"] });
    expect(headerView(s, false).showDots).toBe(true);
    expect(headerView(s, true).showDots).toBe(false);
    expect(headerView(stats(), false).showDots).toBe(false);
  });

  it("flags waiting whenever a lane needs input, open or not", () => {
    expect(headerView(stats({ waiting: 1 }), false).waiting).toBe(true);
    expect(headerView(stats({ waiting: 2 }), true).waiting).toBe(true);
    expect(headerView(stats(), false).waiting).toBe(false);
  });

  it("prefers the building work dot over busy, and hides it when idle", () => {
    expect(headerView(stats({ building: 1, busy: 3 }), false).work).toBe("working");
    expect(headerView(stats({ busy: 1 }), false).work).toBe("busy");
    expect(headerView(stats(), false).work).toBeNull();
  });

  it("renders the live count, blank at zero", () => {
    expect(headerView(stats({ live: 7 }), false).count).toBe("7");
    expect(headerView(stats(), true).count).toBe("");
  });
});

describe("lastActivityLabel", () => {
  it("ages the latest heartbeat, or says active while a lane works", () => {
    const now = Date.parse("2026-10-16T12:00:00Z");
    const s = stats({ lastActivity: "2026-10-16 11:58:00" });
    expect(lastActivityLabel(s, now)).toBe("2m");
    expect(lastActivityLabel({ ...s, active: true }, now)).toBe("active");
    expect(lastActivityLabel(stats(), now)).toBeNull();
  });
});

describe("groupAriaLabel", () => {
  it("is just the project when nothing is going on", () => {
    expect(groupAriaLabel("my-game", stats())).toBe("my-game");
  });

  it("lists the summary signals in order", () => {
    expect(groupAriaLabel("my-game", stats({ waiting: 2, building: 1, busy: 1, live: 7 }))).toBe(
      "my-game, 2 need input, 1 building, 1 busy, 7 live sessions",
    );
  });

  it("uses the singular forms", () => {
    expect(groupAriaLabel("tools", stats({ waiting: 1, live: 1 }))).toBe(
      "tools, 1 needs input, 1 live session",
    );
  });
});

describe("sentinelAbove", () => {
  it("is stuck when the sentinel left through the top", () => {
    expect(sentinelAbove(false, -20, 100)).toBe(true);
  });

  it("is not stuck while the sentinel is visible", () => {
    expect(sentinelAbove(true, 120, 100)).toBe(false);
  });

  it("is not stuck when the sentinel left through the bottom", () => {
    expect(sentinelAbove(false, 900, 100)).toBe(false);
  });

  it("is not stuck without a root rect", () => {
    expect(sentinelAbove(false, -20, null)).toBe(false);
  });
});

describe("laneHintBadges", () => {
  it("numbers available lanes in order, skipping the rest", () => {
    const badges = laneHintBadges([lane("a"), lane("gone", "missing"), lane("b"), lane("c")]);
    expect([...badges]).toEqual([
      ["a", 1],
      ["b", 2],
      ["c", 3],
    ]);
  });
});
