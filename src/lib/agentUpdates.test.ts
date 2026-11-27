import { describe, expect, it } from "vitest";
import {
  bannerRows,
  closeInterruptsRun,
  entryStatusLine,
  installKindLabel,
  interruptRunCopy,
  parseCheckHours,
  pendingRestartAction,
  restartNoticeCopy,
  runOutcomeCopy,
  sessionKey,
  sessionKeys,
  shouldAskToStop,
  stoppedForUpdateCopy,
  stopSessionsLabel,
  updateDisabledReason,
  updatePromptCliOf,
  updatePromptHint,
  withCliKeys,
  withoutCliKey,
} from "./agentUpdates";
import type { AgentUpdateEntry, AgentUpdateState, AgentUpdatesSnapshot, CliKind } from "./ipc";

function entry(cli: CliKind, state: AgentUpdateState, over: Partial<AgentUpdateEntry> = {}) {
  return {
    cli,
    state,
    installed: null,
    latest: null,
    exe: `/Users/me/.local/bin/${cli}`,
    install_kind: "npm",
    command: null,
    guidance: null,
    ...over,
  } satisfies AgentUpdateEntry;
}

function snap(
  codex: AgentUpdateEntry,
  claude: AgentUpdateEntry,
  antigravity: AgentUpdateEntry = entry("antigravity", { type: "unchecked" }),
): AgentUpdatesSnapshot {
  return {
    agents: { claude, codex, antigravity },
    checked_at: "2026-11-18T00:00:00Z",
    stopped: [],
    running: null,
  };
}

const codexAvail = entry("codex", {
  type: "available",
  installed: "0.160.0",
  latest: "0.163.0",
  command: "npm install -g @openai/codex@latest",
});
const claudeAvail = entry("claude", {
  type: "available",
  installed: "2.1.300",
  latest: "2.1.328",
  command: "claude update",
});

describe("bannerRows", () => {
  it("shows nothing without a snapshot", () => {
    expect(bannerRows(null, {})).toEqual([]);
  });

  it.each<AgentUpdateState>([
    { type: "unchecked" },
    { type: "not_installed" },
    { type: "unknown" },
    { type: "up_to_date" },
    { type: "skipped", latest: "0.163.0" },
  ])("shows no row for state %o", (state) => {
    expect(bannerRows(snap(entry("codex", state), entry("claude", state)), {})).toEqual([]);
  });

  it("orders claude, codex, antigravity and words the antigravity row", () => {
    const agyAvail = entry("antigravity", {
      type: "available",
      installed: "1.5.17",
      latest: "1.5.18",
      command: "agy update",
    });
    const rows = bannerRows(snap(codexAvail, claudeAvail, agyAvail), {});
    expect(rows.map((r) => r.cli)).toEqual(["claude", "codex", "antigravity"]);
    expect(rows[2].text).toBe("Antigravity 1.5.18 is available (you have 1.5.17)");
  });

  it("shows one row per available CLI, in provider order", () => {
    const rows = bannerRows(snap(codexAvail, claudeAvail), {});
    expect(rows.map((r) => r.cli)).toEqual(["claude", "codex"]);
    expect(rows[1]).toEqual({
      cli: "codex",
      latest: "0.163.0",
      text: "Codex 0.163.0 is available (you have 0.160.0)",
      canRun: true,
      guidance: null,
      guidanceNote: null,
    });
    expect(rows[0].text).toBe("Claude 2.1.328 is available (you have 2.1.300)");
  });

  it("hides a CLI put off until later, but shows a newer release again", () => {
    const s = snap(codexAvail, claudeAvail);
    expect(bannerRows(s, { codex: "0.163.0" }).map((r) => r.cli)).toEqual(["claude"]);
    expect(bannerRows(s, { codex: "0.162.0" }).map((r) => r.cli)).toEqual(["claude", "codex"]);
  });

  it("offers guidance instead of a run when there is no command", () => {
    const native = entry(
      "codex",
      { type: "available", installed: "0.160.0", latest: "0.163.0", command: null },
      { install_kind: "native", guidance: "codex update" },
    );
    const [row] = bannerRows(snap(native, entry("claude", { type: "up_to_date" })), {});
    expect(row.canRun).toBe(false);
    expect(row.guidance).toBe("codex update");
    expect(row.guidanceNote).toBe("Run this in your own terminal:");

    const unknown = { ...native, install_kind: "unknown" as const, guidance: "npm i -g x" };
    const [u] = bannerRows(snap(unknown, entry("claude", { type: "up_to_date" })), {});
    expect(u.guidanceNote).toContain("couldn't tell how Codex was installed");
  });
});

describe("updateDisabledReason", () => {
  const [row] = bannerRows(snap(codexAvail, entry("claude", { type: "up_to_date" })), {});

  it("runs only while recovery is healthy", () => {
    expect(updateDisabledReason(row, "Healthy")).toBeNull();
    expect(updateDisabledReason(row, "Resuming")).toBe("Wait for session recovery to finish");
  });

  it("is disabled without a command whatever recovery says", () => {
    expect(updateDisabledReason({ ...row, canRun: false }, "Healthy")).toBe(
      "Keitora can't update this Codex install itself",
    );
  });
});

describe("runOutcomeCopy", () => {
  it("offers a restart after an update", () => {
    expect(runOutcomeCopy("codex", { type: "updated", from: "0.160.0", to: "0.163.0" })).toEqual({
      text: "Codex updated to 0.163.0 — running sessions keep the old version until restarted",
      tone: "info",
      canRestart: true,
    });
  });

  it("explains an unchanged version", () => {
    const c = runOutcomeCopy("claude", { type: "unchanged", version: "2.1.300" });
    expect(c.text).toBe("The update finished but Claude is still 2.1.300");
    expect(c.canRestart).toBe(false);
    expect(runOutcomeCopy("claude", { type: "unchanged", version: null }).text).toContain(
      "didn't change",
    );
  });

  it("reports failures with the hint when there is one", () => {
    expect(
      runOutcomeCopy("codex", {
        type: "failed",
        code: 1,
        hint: "close running Codex sessions first",
      }),
    ).toEqual({
      text: "The update failed (exit 1): close running Codex sessions first",
      tone: "error",
      canRestart: false,
    });
    expect(runOutcomeCopy("codex", { type: "failed", code: 243, hint: null }).text).toBe(
      "The update failed (exit 243)",
    );
    expect(runOutcomeCopy("codex", { type: "timed_out" }).tone).toBe("error");
  });
});

describe("shouldAskToStop", () => {
  const live = [
    { project: "proj", worktree: "alpha" },
    { project: "proj", worktree: "bravo" },
  ];

  it("asks on Windows when sessions run", () => {
    expect(shouldAskToStop(true, live)).toBe(true);
  });

  it("runs straight away with no sessions, or off Windows", () => {
    expect(shouldAskToStop(true, [])).toBe(false);
    expect(shouldAskToStop(false, live)).toBe(false);
  });

  it("labels the stop button by count", () => {
    expect(stopSessionsLabel(1)).toBe("stop 1 session and update");
    expect(stopSessionsLabel(3)).toBe("stop 3 sessions and update");
  });
});

describe("pane copy", () => {
  it("hints at a pane parked on the update screen", () => {
    expect(updatePromptHint("codex")).toBe(
      "Codex is asking to update — answer in the terminal, or use keitora's updater",
    );
  });

  it("names the update a pane was stopped for", () => {
    expect(stoppedForUpdateCopy("codex")).toBe("stopped for the Codex update");
  });

  it("reads the CLI from the update-prompt token", () => {
    expect(updatePromptCliOf("AgentUpdatePrompt: codex")).toBe("codex");
    expect(updatePromptCliOf("AgentUpdatePrompt: claude")).toBe("claude");
    expect(updatePromptCliOf("AgentUpdatePrompt: antigravity")).toBe("antigravity");
    expect(updatePromptCliOf("AgentUpdatePrompt: nope")).toBeNull();
    expect(updatePromptCliOf("AgentUpdatePrompt: other")).toBeNull();
    expect(updatePromptCliOf("boom")).toBeNull();
    expect(updatePromptCliOf("codex did not start: x")).toBeNull();
    expect(updatePromptCliOf("agentupdateprompt: codex")).toBeNull();
    expect(updatePromptCliOf(null)).toBeNull();
  });
});

describe("settings helpers", () => {
  it("summarises each state", () => {
    expect(entryStatusLine(codexAvail)).toBe("0.163.0 available");
    expect(entryStatusLine(entry("codex", { type: "skipped", latest: "0.163.0" }))).toBe(
      "0.163.0 skipped",
    );
    expect(entryStatusLine(entry("codex", { type: "not_installed" }))).toBe("not installed");
    expect(entryStatusLine(entry("codex", { type: "up_to_date" }))).toBe("up to date");
  });

  it("labels each install kind", () => {
    expect(installKindLabel("brew")).toBe("homebrew");
    expect(installKindLabel("native")).toBe("native installer");
    expect(installKindLabel(null)).toBe("—");
  });

  it("parses the interval", () => {
    expect(parseCheckHours("6")).toBe(6);
    expect(parseCheckHours(" 0 ")).toBe(0);
    expect(parseCheckHours("168")).toBe(168);
    expect(parseCheckHours("169")).toBeNull();
    expect(parseCheckHours("-1")).toBeNull();
    expect(parseCheckHours("1.5")).toBeNull();
    expect(parseCheckHours("")).toBeNull();
  });
});

describe("key sets", () => {
  it("keys sessions by project and worktree", () => {
    const keys = sessionKeys([{ project: "proj", worktree: "alpha" }]);
    expect(keys).toEqual([sessionKey("proj", "alpha")]);
    expect(keys).toEqual(["proj/alpha"]);
  });

  it("tracks which CLI a key belongs to", () => {
    const m = withCliKeys(new Map(), ["proj/alpha"], "codex");
    expect(m.get("proj/alpha")).toBe("codex");
    expect(withCliKeys(m, ["proj/alpha"], "codex")).toBe(m);
    expect(withoutCliKey(m, "proj/alpha").size).toBe(0);
    expect(withoutCliKey(m, "proj/bravo")).toBe(m);
  });
});

describe("pendingRestartAction", () => {
  const act = (
    over: Partial<Parameters<typeof pendingRestartAction>[0]> = {},
  ): ReturnType<typeof pendingRestartAction> =>
    pendingRestartAction({
      pending: true,
      markedBeforeMount: false,
      status: "running",
      transferring: false,
      ...over,
    });

  it("restarts a pane that was mounted when the user asked, once it settles", () => {
    expect(act()).toBe("restart");
    for (const status of ["spawn-failed", "ended", "ended-while-away", "lost"] as const) {
      expect(act({ status })).toBe("restart");
    }
    for (const status of ["attaching", "detached-alive", "recovering", "suspended"] as const) {
      expect(act({ status })).toBe("none");
    }
  });

  it("never auto-kills a session found by a pane mounted after the mark", () => {
    expect(act({ markedBeforeMount: true })).toBe("notice");
    for (const status of [
      "attaching",
      "ended",
      "ended-while-away",
      "spawn-failed",
      "lost",
    ] as const) {
      expect(act({ markedBeforeMount: true, status })).toBe("none");
    }
  });

  it("drops the mark of a terminated session and ignores panes without one", () => {
    expect(act({ status: "terminated" })).toBe("drop");
    expect(act({ status: "terminated", markedBeforeMount: true })).toBe("drop");
    expect(act({ pending: false })).toBe("none");
    expect(act({ pending: false, markedBeforeMount: true })).toBe("none");
    expect(act({ transferring: true })).toBe("none");
  });

  it("words the notice with the new version when known", () => {
    expect(restartNoticeCopy("codex", "0.163.0")).toBe("restart to use Codex 0.163.0");
    expect(restartNoticeCopy("claude", null)).toBe("restart to use the updated Claude");
  });
});

describe("closeInterruptsRun", () => {
  it("asks before closing only while a run has no outcome", () => {
    expect(closeInterruptsRun("running", false)).toBe(true);
    expect(closeInterruptsRun("running", true)).toBe(false);
    for (const step of ["preparing", "ask", "stopping", "error"]) {
      expect(closeInterruptsRun(step, false)).toBe(false);
    }
    expect(interruptRunCopy("codex")).toBe(
      "The Codex update is still running — closing will interrupt it.",
    );
  });
});
