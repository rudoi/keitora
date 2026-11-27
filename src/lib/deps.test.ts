import { describe, expect, it } from "vitest";
import {
  cardModel,
  depsReducer,
  depsStepVisible,
  dialogModel,
  failureCopy,
  INITIAL_DEPS,
  isBelowFloor,
  isNewerProtocol,
  needsDepsStep,
  overwriteInstall,
  runLabel,
  runView,
  settingsModel,
  stageLabel,
  stepModel,
  upgradeConfirmCopy,
} from "./deps";
import type {
  DepsFailureKind,
  DepsProgressEvent,
  DepsRunState,
  DepsSnapshot,
  DepsStage,
  DoctorRow,
} from "./ipc";

const MANUAL = {
  reason: null,
  commands: [{ shell: "Terminal", command: "curl -fsSL https://herdr.dev/install.sh | sh" }],
  link: "https://herdr.dev/docs/install/",
};

function snap(over: Partial<DepsSnapshot> = {}): DepsSnapshot {
  return {
    herdr_installed: true,
    floor: { type: "ok" },
    pinned_bin: null,
    platform: "macos-aarch64",
    run: { type: "idle" },
    prompt: null,
    live_sessions: 0,
    route: { type: "direct" },
    overwrite_target: null,
    manual: MANUAL,
    ...over,
  };
}

const running = (stage: DepsStage = "checking", run_id = 1, upgrade = false): DepsRunState => ({
  type: "running",
  run_id,
  tier: "direct",
  stage,
  upgrade,
});

const failed = (
  next: "try_direct" | "manual",
  kind: DepsFailureKind = "network",
): DepsRunState => ({
  type: "failed",
  run_id: 1,
  kind,
  message: "connection reset",
  next,
});

function progress(over: Partial<DepsProgressEvent> = {}): DepsProgressEvent {
  return { run_id: 1, stage: "downloading", downloaded: 0, total: null, line: null, ...over };
}

const ALL_STAGES: DepsStage[] = [
  "checking",
  "downloading",
  "installing",
  "verifying",
  "restarting",
  "done",
  "failed",
];

const ALL_KINDS: DepsFailureKind[] = [
  "offline",
  "network",
  "policy_blocked",
  "checksum",
  "blocked_by_av",
  "timeout",
  "incomplete",
  "too_old_after_install",
  "no_compatible_release",
  "other",
];

describe("depsReducer", () => {
  it("replaces the snapshot on status and marks loaded", () => {
    const a = depsReducer(INITIAL_DEPS, { type: "status", snapshot: snap() });
    const b = depsReducer(a, {
      type: "status",
      snapshot: snap({ herdr_installed: false }),
    });
    expect(a.loaded).toBe(true);
    expect(b.snapshot?.herdr_installed).toBe(false);
  });

  it("marks loaded when the first status fails", () => {
    expect(depsReducer(INITIAL_DEPS, { type: "load_failed" }).loaded).toBe(true);
  });

  it("folds progress into the running run", () => {
    const s0 = depsReducer(INITIAL_DEPS, { type: "status", snapshot: snap({ run: running() }) });
    const s1 = depsReducer(s0, {
      type: "progress",
      event: progress({ downloaded: 50, total: 100, line: "fetching" }),
    });
    expect(s1.snapshot?.run).toMatchObject({ type: "running", stage: "downloading" });
    expect(s1.progress?.downloaded).toBe(50);
    expect(s1.progress?.line).toBe("fetching");
  });

  it("ignores progress from another run", () => {
    const s0 = depsReducer(INITIAL_DEPS, {
      type: "status",
      snapshot: snap({ run: running("checking", 2) }),
    });
    expect(depsReducer(s0, { type: "progress", event: progress({ run_id: 1 }) })).toBe(s0);
  });

  it("keeps the stage on a terminal progress event until the status arrives", () => {
    const s0 = depsReducer(INITIAL_DEPS, {
      type: "status",
      snapshot: snap({ run: running("verifying") }),
    });
    const s1 = depsReducer(s0, { type: "progress", event: progress({ stage: "done" }) });
    expect(s1.snapshot?.run).toMatchObject({ stage: "verifying" });
  });

  it("drops progress once the run ends", () => {
    let s = depsReducer(INITIAL_DEPS, { type: "status", snapshot: snap({ run: running() }) });
    s = depsReducer(s, { type: "progress", event: progress({ downloaded: 5 }) });
    const kept = depsReducer(s, { type: "status", snapshot: snap({ run: running("installing") }) });
    expect(kept.progress).not.toBeNull();
    const done = depsReducer(s, {
      type: "status",
      snapshot: snap({ run: { type: "done", run_id: 1, path: "/x", restart_pending: false } }),
    });
    expect(done.progress).toBeNull();
    const fail = depsReducer(s, { type: "status", snapshot: snap({ run: failed("manual") }) });
    expect(fail.progress).toBeNull();
  });

  it("holds progress that arrives before any snapshot", () => {
    const s = depsReducer(INITIAL_DEPS, { type: "progress", event: progress() });
    expect(s.progress).not.toBeNull();
    expect(s.snapshot).toBeNull();
  });
});

describe("stageLabel / failureCopy / runLabel / runView", () => {
  it("labels every stage", () => {
    const labels = ALL_STAGES.map(stageLabel);
    for (const l of labels) expect(l.length).toBeGreaterThan(0);
    expect(new Set(labels).size).toBe(ALL_STAGES.length);
    expect(stageLabel("restarting")).toBe("Restarting herdr…");
  });

  it("has copy for every failure kind", () => {
    const copy = ALL_KINDS.map(failureCopy);
    for (const c of copy) expect(c).toMatch(/\.$/);
    expect(new Set(copy).size).toBe(ALL_KINDS.length);
    expect(failureCopy("offline")).toContain("offline");
  });

  it("shows a percentage while downloading", () => {
    const run = running("downloading") as Extract<DepsRunState, { type: "running" }>;
    const p = progress({ downloaded: 25, total: 100 });
    expect(runLabel(run, p)).toBe("Downloading 25%…");
    expect(runLabel(run, null)).toBe("Downloading…");
    expect(runLabel(run, { ...p, run_id: 2 })).toBe("Downloading…");
    expect(runView(snap({ run }), p).running).toBe("Downloading 25%…");
    expect(runView(snap(), p)).toEqual({ running: null, failure: null });
    expect(runView(null, null)).toEqual({ running: null, failure: null });
  });

  it("describes a failed run", () => {
    expect(runView(snap({ run: failed("manual", "checksum") }), null)).toEqual({
      running: null,
      failure: { text: failureCopy("checksum"), message: "connection reset", tryDirect: false },
    });
  });

  it("is below the floor only when too old or mismatched", () => {
    expect(isBelowFloor({ type: "too_old", found: "0.9.1" })).toBe(true);
    expect(isBelowFloor({ type: "protocol_mismatch", found: 21, want: 22, newer: false })).toBe(
      true,
    );
    expect(isBelowFloor({ type: "ok" })).toBe(false);
    expect(isBelowFloor({ type: "unknown" })).toBe(false);
    expect(isBelowFloor({ type: "restart_to_finish", binary: "0.9.6", server: "0.9.1" })).toBe(
      false,
    );
  });
});

describe("cardModel", () => {
  it("is null before a snapshot and when herdr is fine", () => {
    expect(cardModel(null)).toBeNull();
    expect(cardModel(snap())).toBeNull();
    expect(cardModel(snap({ floor: { type: "unknown" } }))).toBeNull();
  });

  it("offers Install when herdr is missing", () => {
    const m = cardModel(snap({ herdr_installed: false }));
    expect(m?.action).toEqual({ kind: "install", label: "Install herdr" });
    expect(m?.tone).toBe("error");
  });

  it("has no Install on an unsupported platform", () => {
    expect(cardModel(snap({ herdr_installed: false, platform: null }))?.action).toBeNull();
  });

  it("offers Upgrade when too old", () => {
    const m = cardModel(snap({ floor: { type: "too_old", found: "0.9.1" } }));
    expect(m?.text).toContain("0.9.1");
    expect(m?.text).toContain("0.9.5+");
    expect(m?.action?.kind).toBe("upgrade");
    expect(m?.tone).toBe("warn");
  });

  it("offers Upgrade on a protocol mismatch", () => {
    const m = cardModel(
      snap({ floor: { type: "protocol_mismatch", found: 21, want: 22, newer: false } }),
    );
    expect(m?.text).toContain("21");
    expect(m?.text).toContain("22");
    expect(m?.action?.kind).toBe("upgrade");
    expect(m?.tone).toBe("error");
  });

  it("offers Restart to finish", () => {
    const m = cardModel(
      snap({ floor: { type: "restart_to_finish", binary: "0.9.6", server: "0.9.1" } }),
    );
    expect(m?.action).toEqual({ kind: "restart_to_finish", label: "Restart to finish" });
  });

  it("shows progress while installing", () => {
    const m = cardModel(
      snap({ herdr_installed: false, run: running("downloading") }),
      progress({ downloaded: 10, total: 20 }),
    );
    expect(m?.busy).toBe(true);
    expect(m?.action).toBeNull();
    expect(m?.text).toBe("Installing herdr: Downloading 50%…");
    expect(cardModel(snap({ run: running("installing", 1, true) }))?.text).toBe(
      "Upgrading herdr: Installing…",
    );
  });

  it("offers Try direct download after a failed script run", () => {
    const m = cardModel(snap({ herdr_installed: false, run: failed("try_direct") }));
    expect(m?.action).toEqual({ kind: "try_direct", label: "Try direct download" });
    expect(m?.text).toBe(failureCopy("network"));
  });

  it("offers Retry after a manual-route failure only while herdr is missing", () => {
    expect(cardModel(snap({ herdr_installed: false, run: failed("manual") }))?.action?.kind).toBe(
      "install",
    );
    expect(cardModel(snap({ run: failed("manual") }))?.action).toBeNull();
  });

  it("notes a pending restart after a done run", () => {
    const m = cardModel(
      snap({ run: { type: "done", run_id: 1, path: "/x", restart_pending: true } }),
    );
    expect(m?.text).toContain("recovery");
    expect(
      cardModel(snap({ run: { type: "done", run_id: 1, path: "/x", restart_pending: false } })),
    ).toBeNull();
  });
});

describe("dialogModel", () => {
  const tooOld = { type: "too_old", found: "0.9.1" } as const;

  it("is closed without a prompt unless forced", () => {
    expect(dialogModel(null)).toBeNull();
    expect(dialogModel(snap({ floor: tooOld }))).toBeNull();
    expect(dialogModel(snap({ floor: tooOld }), true)?.hard).toBe(false);
    expect(dialogModel(snap(), true)).toBeNull();
  });

  it("builds a soft prompt with Later", () => {
    const m = dialogModel(
      snap({ floor: tooOld, prompt: { kind: "too_old", found: "0.9.1", hard: false } }),
    );
    expect(m?.hard).toBe(false);
    expect(m?.title).toBe("Upgrade herdr?");
    expect(m?.body).toBe("herdr 0.9.1 is older than Keitora needs (0.9.5+). Upgrade now?");
    expect(m?.primary).toEqual({ label: "Upgrade", tier: null });
    expect(m?.external).toBeNull();
  });

  it("builds a hard prompt for a protocol mismatch", () => {
    const floor = { type: "protocol_mismatch", found: 21, want: 22, newer: false } as const;
    const m = dialogModel(
      snap({ floor, prompt: { kind: "protocol_mismatch", found: "21", hard: true } }),
    );
    expect(m?.hard).toBe(true);
    expect(m?.body).toContain("Keitora needs 22");
    expect(dialogModel(snap({ floor }), true)?.hard).toBe(true);
  });

  it("lays out an External install with a managed copy instead", () => {
    const m = dialogModel(
      snap({
        floor: tooOld,
        prompt: { kind: "too_old", found: "0.9.1", hard: false },
        route: { type: "external", instruction: "brew upgrade herdr" },
      }),
    );
    expect(m?.external).toBe("brew upgrade herdr");
    expect(m?.primary).toEqual({ label: "Install a Keitora-managed copy instead", tier: "direct" });
  });

  it("is hidden while the upgrade only needs a restart", () => {
    expect(
      dialogModel(
        snap({
          floor: { type: "restart_to_finish", binary: "0.9.6", server: "0.9.1" },
          prompt: { kind: "too_old", found: "0.9.1", hard: false },
        }),
        true,
      ),
    ).toBeNull();
  });

  it("carries live sessions, progress and failure", () => {
    const prompt = { kind: "too_old", found: "0.9.1", hard: false } as const;
    const busy = dialogModel(
      snap({ floor: tooOld, prompt, live_sessions: 3, run: running("verifying") }),
    );
    expect(busy?.liveSessions).toBe(3);
    expect(busy?.running).toBe("Verifying…");
    const bad = dialogModel(snap({ floor: tooOld, prompt, run: failed("try_direct", "timeout") }));
    expect(bad?.failure).toEqual({
      text: failureCopy("timeout"),
      message: "connection reset",
      tryDirect: true,
    });
  });

  it("phrases the live-session confirmation", () => {
    expect(upgradeConfirmCopy(1).description).toContain("1 running session ");
    expect(upgradeConfirmCopy(2).description).toContain("2 running sessions");
    expect(upgradeConfirmCopy(2).confirmLabel).toBe("Upgrade");
  });
});

describe("stepModel / needsDepsStep", () => {
  it("needs the step when herdr is missing, too old, mismatched or installing", () => {
    expect(needsDepsStep(null)).toBe(false);
    expect(needsDepsStep(snap())).toBe(false);
    expect(needsDepsStep(snap({ herdr_installed: false }))).toBe(true);
    expect(needsDepsStep(snap({ floor: { type: "too_old", found: "0.9.1" } }))).toBe(true);
    expect(
      needsDepsStep(
        snap({ floor: { type: "protocol_mismatch", found: 1, want: 22, newer: false } }),
      ),
    ).toBe(true);
    expect(needsDepsStep(snap({ run: running() }))).toBe(true);
  });

  it("offers an install, or the manual commands where Keitora can't install", () => {
    const m = stepModel(snap({ herdr_installed: false }));
    expect(m).toMatchObject({ kind: "offer", title: "Install herdr", canInstall: true });
    const manual = stepModel(snap({ herdr_installed: false, platform: null }));
    expect(manual).toMatchObject({ kind: "offer", canInstall: false });
    const old = stepModel(snap({ floor: { type: "too_old", found: "0.9.1" } }));
    expect(old).toMatchObject({ kind: "offer", title: "Upgrade herdr" });
  });

  it("shows progress, failure and done", () => {
    expect(
      stepModel(
        snap({ herdr_installed: false, run: running("downloading") }),
        progress({ line: "got 1 MB" }),
      ),
    ).toMatchObject({ kind: "running", label: "Downloading…", line: "got 1 MB" });
    expect(stepModel(snap({ herdr_installed: false, run: failed("try_direct") }))).toMatchObject({
      kind: "failed",
      tryDirect: true,
      message: "connection reset",
    });
    expect(
      stepModel(snap({ run: { type: "done", run_id: 1, path: "/x", restart_pending: false } })),
    ).toMatchObject({ kind: "done" });
    expect(stepModel(snap())).toEqual({ kind: "hidden" });
  });
});

describe("settingsModel", () => {
  const rows: DoctorRow[] = [
    {
      tool: "herdr",
      path: "/Users/me/.herdr/bin/herdr",
      version: "0.9.1",
      floor: "0.9.5",
      status: "too_old",
      action: "upgrade",
      hint: null,
    },
    {
      tool: "git",
      path: null,
      version: "2.43.0",
      floor: null,
      status: "ok",
      action: null,
      hint: null,
    },
    {
      tool: "claude",
      path: null,
      version: null,
      floor: null,
      status: "missing",
      action: null,
      hint: null,
    },
    {
      tool: "codex",
      path: "/Users/me/bin/codex",
      version: null,
      floor: null,
      status: "unknown",
      action: null,
      hint: null,
    },
  ];

  it("maps doctor rows to details and actions", () => {
    const m = settingsModel(snap({ pinned_bin: "/Users/me/.herdr/bin/herdr" }), rows);
    expect(m.rows.map((r) => r.detail)).toEqual([
      "0.9.1, needs 0.9.5+",
      "2.43.0",
      "not found",
      "version unknown",
    ]);
    expect(m.rows[0].action).toEqual({ kind: "upgrade", label: "Upgrade" });
    expect(m.rows[0].tone).toBe("warn");
    expect(m.rows[2].tone).toBe("muted");
    expect(m.pinned).toBe("/Users/me/.herdr/bin/herdr");
    expect(m.running).toBeNull();
    expect(m.failure).toBeNull();
    expect(m.manual).toBeNull();
  });

  it("carries the install hint on a missing antigravity row", () => {
    const hint = "curl -fsSL https://example.com/install.sh | bash";
    const agy: DoctorRow = {
      tool: "antigravity",
      path: null,
      version: null,
      floor: null,
      status: "missing",
      action: null,
      hint,
    };
    const m = settingsModel(snap(), [...rows, agy]);
    expect(m.rows[4]).toMatchObject({
      tool: "antigravity",
      detail: "not found",
      tone: "muted",
      hint,
    });
    expect(m.rows[2].hint).toBeNull();
  });

  it("shows Restart to finish and the protocol on the herdr row", () => {
    const restart = settingsModel(
      snap({ floor: { type: "restart_to_finish", binary: "0.9.6", server: "0.9.1" } }),
      rows,
    );
    expect(restart.rows[0].action?.kind).toBe("restart_to_finish");
    const mismatch = settingsModel(
      snap({ floor: { type: "protocol_mismatch", found: 21, want: 22, newer: false } }),
      rows,
    );
    expect(mismatch.rows[0].detail).toBe("protocol 21, needs 22");
  });

  it("swaps Install for Browse on an unsupported platform and shows manual commands", () => {
    const missing: DoctorRow = { ...rows[0], status: "missing", action: "install", version: null };
    const m = settingsModel(snap({ platform: null, herdr_installed: false }), [missing]);
    expect(m.rows[0].action).toEqual({ kind: "browse", label: "Browse…" });
    expect(m.rows[0].tone).toBe("error");
    expect(m.manual).toEqual(MANUAL);
    const ok = settingsModel(snap({ herdr_installed: false }), [missing]);
    expect(ok.rows[0].action?.kind).toBe("install");
  });

  it("carries the running run and a failure", () => {
    expect(settingsModel(snap({ run: running("installing") }), rows).running).toBe("Installing…");
    expect(settingsModel(snap({ run: failed("try_direct") }), rows).failure?.tryDirect).toBe(true);
    expect(settingsModel(null, []).rows).toEqual([]);
  });
});

describe("a herdr newer than Keitora", () => {
  const newer = { type: "protocol_mismatch", found: 23, want: 22, newer: true } as const;

  it("is told apart from an old protocol", () => {
    expect(isNewerProtocol(newer)).toBe(true);
    expect(isNewerProtocol({ ...newer, found: 21, newer: false })).toBe(false);
    expect(isNewerProtocol({ type: "too_old", found: "0.9.1" })).toBe(false);
  });

  it("never asks to upgrade herdr", () => {
    const card = cardModel(snap({ floor: newer }));
    expect(card?.text).toContain("newer than this Keitora (22)");
    expect(card?.action).toEqual({ kind: "upgrade", label: "Resolve" });

    const route = { type: "external", instruction: "brew upgrade herdr" } as const;
    const dialog = dialogModel(
      snap({ floor: newer, route, prompt: { kind: "protocol_mismatch", found: "23", hard: true } }),
    );
    expect(dialog?.title).toBe("herdr is newer than Keitora");
    expect(dialog?.body).toContain("Update Keitora, or install a Keitora-managed herdr");
    expect(dialog?.body).not.toContain("upgrade");
    expect(dialog?.external).toBeNull();
    expect(dialog?.primary).toEqual({ label: "Install a compatible herdr", tier: "direct" });
    expect(dialog?.hard).toBe(true);

    const step = stepModel(snap({ floor: newer }));
    expect(step).toMatchObject({ kind: "offer", title: "Install a compatible herdr" });
    expect(step.kind === "offer" && step.body).toContain("Update Keitora");

    const settings = settingsModel(snap({ floor: newer }), [
      {
        tool: "herdr",
        path: "/opt/homebrew/bin/herdr",
        version: "0.12.0",
        floor: "0.9.5",
        status: "ok",
        action: null,
        hint: null,
      },
    ]);
    expect(settings.rows[0]).toMatchObject({
      detail: "protocol 23, newer than Keitora's 22",
      action: { kind: "upgrade", label: "Resolve" },
    });
  });
});

describe("overwriteInstall", () => {
  it("installs only for Replace or Keep; a dismissed dialog installs nothing", () => {
    expect(overwriteInstall("replace", null)).toEqual({ tier: null, confirmOverwrite: true });
    expect(overwriteInstall("keep", null)).toEqual({ tier: "direct", confirmOverwrite: false });
    expect(overwriteInstall("cancel", null)).toBeNull();
    expect(overwriteInstall("cancel", "script")).toBeNull();
  });
});

describe("depsStepVisible", () => {
  const base = { loaded: true, needs: false, seen: false, dismissed: false };

  it("waits for the first snapshot", () => {
    expect(depsStepVisible({ ...base, loaded: false, needs: true })).toBeNull();
  });

  it("turns on for a later snapshot that needs it", () => {
    // The first snapshot's floor is still unknown: no step yet.
    expect(depsStepVisible(base)).toBe(false);
    // The probe then finds herdr too old.
    expect(depsStepVisible({ ...base, needs: true })).toBe(true);
  });

  it("stays on once seen, until the user moves past it", () => {
    expect(depsStepVisible({ ...base, seen: true })).toBe(true);
    expect(depsStepVisible({ ...base, seen: true, needs: true, dismissed: true })).toBe(false);
    expect(depsStepVisible({ ...base, needs: true, dismissed: true })).toBe(false);
  });
});
