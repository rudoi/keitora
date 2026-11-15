import { describe, expect, it, vi } from "vitest";
import type { UpdateSnapshot, UpdateStatus, UpdateStatusEvent } from "./ipc";
import {
  bannerOf,
  formatProgress,
  INITIAL_UPDATES,
  installLabel,
  installUpdate,
  statusLine,
  toastFor,
  type UpdatesState,
  updatesReducer,
} from "./updates";

const AVAILABLE: UpdateStatus = {
  type: "available",
  version: "0.4.1",
  notes: "- Faster boot",
  release_url: "https://example.com/releases/v0.4.1",
  published_at: "2026-11-01T00:00:00Z",
};

function ev(status: UpdateStatus, manual = false): UpdateStatusEvent {
  return { ...status, manual };
}

function st(over: Partial<UpdatesState> = {}): UpdatesState {
  return { ...INITIAL_UPDATES, currentVersion: "0.4.0", ...over };
}

describe("updatesReducer", () => {
  it("seeds status, phase, progress and version from the snapshot", () => {
    const snapshot: UpdateSnapshot = {
      status: AVAILABLE,
      phase: "downloading",
      progress: { downloaded: 5, total: 10 },
      current_version: "0.4.0",
    };
    const s = updatesReducer(INITIAL_UPDATES, { type: "seed", snapshot });
    expect(s.status).toEqual(AVAILABLE);
    expect(s.phase).toBe("downloading");
    expect(s.progress).toEqual({ downloaded: 5, total: 10 });
    expect(s.currentVersion).toBe("0.4.0");
  });

  it("does not let a late snapshot overwrite a fresher event", () => {
    const live = updatesReducer(INITIAL_UPDATES, { type: "status", event: ev(AVAILABLE) });
    const snapshot: UpdateSnapshot = {
      status: { type: "unchecked" },
      phase: "idle",
      progress: null,
      current_version: "0.4.0",
    };
    const s = updatesReducer(live, { type: "seed", snapshot });
    expect(s.status.type).toBe("available");
    expect(s.currentVersion).toBe("0.4.0");
  });

  it("keeps the snapshot's status when only progress events arrived first", () => {
    const live = updatesReducer(INITIAL_UPDATES, {
      type: "progress",
      event: { phase: "downloading", downloaded: 7, total: 10 },
    });
    const snapshot: UpdateSnapshot = {
      status: AVAILABLE,
      phase: "downloading",
      progress: { downloaded: 5, total: 10 },
      current_version: "0.4.0",
    };
    const s = updatesReducer(live, { type: "seed", snapshot });
    expect(s.status).toEqual(AVAILABLE);
    expect(s.progress).toEqual({ downloaded: 7, total: 10 });
  });

  it("stores the status without the manual flag", () => {
    const s = updatesReducer(INITIAL_UPDATES, { type: "status", event: ev(AVAILABLE, true) });
    expect(s.status).toEqual(AVAILABLE);
    expect("manual" in s.status).toBe(false);
  });

  it("tracks progress and clears it when idle", () => {
    let s = updatesReducer(st(), {
      type: "progress",
      event: { phase: "downloading", downloaded: 3, total: null },
    });
    expect(s.phase).toBe("downloading");
    expect(s.progress).toEqual({ downloaded: 3, total: null });
    s = updatesReducer(s, {
      type: "progress",
      event: { phase: "idle", downloaded: 0, total: null },
    });
    expect(s.phase).toBe("idle");
    expect(s.progress).toBeNull();
  });

  it("returns the same state for a progress event that changes nothing", () => {
    const event = { phase: "downloading" as const, downloaded: 3, total: 9 };
    const s = updatesReducer(st(), { type: "progress", event });
    expect(updatesReducer(s, { type: "progress", event: { ...event } })).toBe(s);
    expect(
      updatesReducer(INITIAL_UPDATES, {
        type: "progress",
        event: { phase: "idle", downloaded: 0, total: null },
      }),
    ).toBe(INITIAL_UPDATES);
  });

  it("dismisses the available version only", () => {
    let s = updatesReducer(st({ status: AVAILABLE }), { type: "dismiss" });
    expect(s.dismissedVersion).toBe("0.4.1");
    expect(bannerOf(s)).toBeNull();
    // A newer version shows again.
    s = updatesReducer(s, {
      type: "status",
      event: ev({ ...AVAILABLE, version: "0.4.2" } as UpdateStatus),
    });
    expect(bannerOf(s)?.version).toBe("0.4.2");
  });

  it("ignores dismiss when nothing is available", () => {
    const s = st();
    expect(updatesReducer(s, { type: "dismiss" })).toBe(s);
  });

  it("clears a dismissal on a manual available event but not a background one", () => {
    const dismissed = st({ status: AVAILABLE, dismissedVersion: "0.4.1" });
    expect(
      updatesReducer(dismissed, { type: "status", event: ev(AVAILABLE, false) }).dismissedVersion,
    ).toBe("0.4.1");
    expect(
      updatesReducer(dismissed, { type: "status", event: ev(AVAILABLE, true) }).dismissedVersion,
    ).toBeNull();
  });

  it("runs the check and install lifecycles", () => {
    let s = updatesReducer(st(), { type: "check_start" });
    expect(s.checking).toBe(true);
    s = updatesReducer(s, { type: "check_end" });
    expect(s.checking).toBe(false);

    s = updatesReducer(st({ error: "old" }), { type: "install_start" });
    expect(s.installing).toBe(true);
    expect(s.error).toBeNull();
    s = updatesReducer(s, { type: "install_end", error: "boom" });
    expect(s.installing).toBe(false);
    expect(s.error).toBe("boom");
  });
});

describe("bannerOf", () => {
  it("is null unless an update is available", () => {
    for (const status of [
      { type: "unchecked" },
      { type: "up_to_date" },
      { type: "failed", message: "x" },
      { type: "unsupported" },
    ] as UpdateStatus[]) {
      expect(bannerOf(st({ status }))).toBeNull();
    }
  });

  it("is null once that version is dismissed", () => {
    expect(bannerOf(st({ status: AVAILABLE, dismissedVersion: "0.4.1" }))).toBeNull();
  });

  it("announces an available update", () => {
    expect(bannerOf(st({ status: AVAILABLE }))).toMatchObject({
      version: "0.4.1",
      text: "Keitora 0.4.1 is available. Your sessions keep running.",
      tone: "info",
      buttonLabel: "Install & restart",
      busy: false,
      canDismiss: true,
      releaseUrl: "https://example.com/releases/v0.4.1",
    });
  });

  it("shows progress while downloading and blocks dismissing", () => {
    const b = bannerOf(
      st({ status: AVAILABLE, phase: "downloading", progress: { downloaded: 42, total: 100 } }),
    );
    expect(b).toMatchObject({ buttonLabel: "Downloading 42%…", busy: true, canDismiss: false });
  });

  it("shows restarting once applying", () => {
    expect(bannerOf(st({ status: AVAILABLE, phase: "applying" }))?.buttonLabel).toBe("Restarting…");
  });

  it("surfaces an install error with a retry button", () => {
    expect(bannerOf(st({ status: AVAILABLE, error: "signature mismatch" }))).toMatchObject({
      tone: "error",
      text: "Update failed: signature mismatch",
      buttonLabel: "Retry",
      busy: false,
    });
  });
});

describe("after a failed apply", () => {
  it("shows the error and a retry while the download stays ready", () => {
    expect(
      bannerOf(
        st({
          status: AVAILABLE,
          phase: "ready",
          error: "Move Keitora to your Applications folder, then try again.",
        }),
      ),
    ).toMatchObject({ tone: "error", buttonLabel: "Retry", busy: false, canDismiss: true });
  });

  it("says restarting while the ready update is being applied", () => {
    expect(installLabel(st({ status: AVAILABLE, phase: "ready", installing: true }))).toBe(
      "Restarting…",
    );
  });
});

describe("installLabel", () => {
  it("says downloading before the first progress event", () => {
    expect(installLabel(st({ installing: true }))).toBe("Downloading…");
  });
});

describe("toastFor", () => {
  it("toasts manual results only", () => {
    expect(toastFor(ev({ type: "up_to_date" }, true))).toBe("Keitora is up to date.");
    expect(toastFor(ev({ type: "failed", message: "offline" }, true))).toBe(
      "Update check failed: offline",
    );
    expect(toastFor(ev(AVAILABLE, true))).toBeNull();
    expect(toastFor(ev({ type: "unsupported" }, true))).toBe(
      "Updates aren't available on this platform.",
    );
  });

  it("stays quiet for background checks", () => {
    expect(toastFor(ev({ type: "up_to_date" }, false))).toBeNull();
    expect(toastFor(ev({ type: "failed", message: "offline" }, false))).toBeNull();
  });
});

describe("statusLine", () => {
  it("describes every status", () => {
    expect(statusLine({ type: "unchecked" })).toBe("Not checked yet");
    expect(statusLine({ type: "up_to_date" })).toBe("Up to date");
    expect(statusLine(AVAILABLE)).toBe("Version 0.4.1 is available");
    expect(statusLine({ type: "failed", message: "offline" })).toBe("Last check failed: offline");
    expect(statusLine({ type: "unsupported" })).toBe("Updates aren't available on this platform.");
  });
});

describe("formatProgress", () => {
  it("shows a percentage when the total is known", () => {
    expect(formatProgress(42, 100)).toBe("Downloading 42%…");
    expect(formatProgress(500, 100)).toBe("Downloading 100%…");
  });

  it("shows bytes when the total is unknown or zero", () => {
    expect(formatProgress(3_250_000, null)).toBe("Downloading 3.1 MB…");
    expect(formatProgress(256_000, 0)).toBe("Downloading 250 KB…");
  });
});

describe("installUpdate", () => {
  it("downloads then applies", async () => {
    const calls: string[] = [];
    const r = await installUpdate({
      download: async () => void calls.push("download"),
      apply: async () => void calls.push("apply"),
    });
    expect(r).toEqual({ ok: true });
    expect(calls).toEqual(["download", "apply"]);
  });

  it("skips apply when the download fails", async () => {
    const apply = vi.fn();
    const r = await installUpdate({ download: () => Promise.reject("no space"), apply });
    expect(r).toEqual({ ok: false, error: "no space" });
    expect(apply).not.toHaveBeenCalled();
  });

  it("surfaces an apply error", async () => {
    const r = await installUpdate({
      download: async () => null,
      apply: () => Promise.reject(new Error("not in /Applications")),
    });
    expect(r).toEqual({ ok: false, error: "not in /Applications" });
  });
});
