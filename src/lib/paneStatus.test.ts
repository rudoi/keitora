import { describe, expect, it } from "vitest";
import type { DaemonState } from "./ipc";
import { ALL_STATUSES, afterRecoveringRefusal, inputBlocked, nextPaneStatus } from "./paneStatus";
import type { PaneRecovery } from "./recovery";

const DAEMON_STATES: DaemonState[] = ["NeverStarted", "Connected", "Disconnected"];
const RECS: PaneRecovery[] = [null, "recovering", "suspended"];

describe("nextPaneStatus", () => {
  it("parks a running pane while herdr is recovering", () => {
    expect(nextPaneStatus("running", "Connected", "recovering")).toEqual({
      status: "recovering",
      attach: false,
    });
    expect(nextPaneStatus("daemon-down", "Disconnected", "recovering").status).toBe("recovering");
  });

  it("shows a suspended session as suspended whatever the pane was doing", () => {
    for (const s of ALL_STATUSES.filter((s) => s !== "terminated")) {
      expect(nextPaneStatus(s, "Connected", "suspended")).toEqual({
        status: "suspended",
        attach: false,
      });
    }
  });

  it("a suspended pane goes back to recovering when it is resumed", () => {
    expect(nextPaneStatus("suspended", "Connected", "recovering").status).toBe("recovering");
  });

  it("re-attaches a waiting pane once recovery lets go and the daemon is connected", () => {
    for (const s of ["recovering", "suspended", "daemon-down"] as const) {
      expect(nextPaneStatus(s, "Connected", null)).toEqual({ status: s, attach: true });
    }
  });

  it("waits for the connection before re-attaching", () => {
    expect(nextPaneStatus("recovering", "Disconnected", null)).toEqual({
      status: "recovering",
      attach: false,
    });
  });

  it("freezes a live pane when the daemon drops while recovery is idle", () => {
    for (const s of ["running", "attaching", "detached-alive"] as const) {
      expect(nextPaneStatus(s, "Disconnected", null)).toEqual({
        status: "daemon-down",
        attach: false,
      });
    }
  });

  it("leaves ended and failed panes alone unless their session is suspended", () => {
    for (const s of ["ended", "ended-while-away", "lost", "spawn-failed"] as const) {
      for (const rec of [null, "recovering"] as const) {
        expect(nextPaneStatus(s, "Connected", rec)).toEqual({ status: s, attach: false });
      }
    }
  });

  it("keeps a terminated pane terminated: no attach, and recovery never claims it", () => {
    for (const d of DAEMON_STATES) {
      for (const rec of RECS) {
        expect(nextPaneStatus("terminated", d, rec)).toEqual({
          status: "terminated",
          attach: false,
        });
      }
    }
  });

  it("enumerates the terminated status", () => {
    expect(ALL_STATUSES).toContain("terminated");
    expect(new Set(ALL_STATUSES).size).toBe(ALL_STATUSES.length);
  });

  it("never restarts: every reaction is a known status plus an optional attach", () => {
    for (const s of ALL_STATUSES) {
      for (const d of DAEMON_STATES) {
        for (const rec of RECS) {
          const r = nextPaneStatus(s, d, rec);
          expect(Object.keys(r).sort()).toEqual(["attach", "status"]);
          expect(ALL_STATUSES).toContain(r.status);
          // An attach is only ever requested, never a respawn of a healthy pane.
          if (r.attach) expect(["daemon-down", "recovering", "suspended"]).toContain(s);
        }
      }
    }
  });
});

describe("afterRecoveringRefusal", () => {
  it("tries again when recovery already let go of the pane", () => {
    expect(afterRecoveringRefusal("Connected", null)).toEqual({
      status: "recovering",
      attach: true,
    });
  });

  it("waits while recovery still holds the pane or the daemon is away", () => {
    expect(afterRecoveringRefusal("Connected", "recovering")).toEqual({
      status: "recovering",
      attach: false,
    });
    expect(afterRecoveringRefusal("Connected", "suspended")).toEqual({
      status: "suspended",
      attach: false,
    });
    for (const d of DAEMON_STATES.filter((d) => d !== "Connected")) {
      expect(afterRecoveringRefusal(d, null).attach).toBe(false);
    }
  });
});

describe("inputBlocked", () => {
  it("blocks input exactly while the session can't take it", () => {
    const blocked = ALL_STATUSES.filter(inputBlocked);
    expect(blocked).toEqual([
      "attaching",
      "detached-alive",
      "daemon-down",
      "recovering",
      "suspended",
      "terminated",
    ]);
  });
});
