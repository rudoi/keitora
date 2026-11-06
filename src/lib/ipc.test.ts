import { beforeEach, describe, expect, it, vi } from "vitest";

const transport = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn(), channel: vi.fn() }));
vi.mock("./transport", () => ({
  getTransport: () => transport,
}));

import {
  cancelTransfer,
  daemonStatus,
  injectHandoffSummary,
  type OutputMsg,
  onPtyPasteRefused,
  onPtyResume,
  onTransferProgress,
  previewClaudeTransfer,
  ptyAttach,
  ptyDetach,
  ptyPaste,
  ptyScroll,
  ptyScrollOffset,
  ptySpawn,
  ptyWrite,
  sessionLaunch,
  sessionTerminate,
  type TransferOutcome,
  type TransferProgress,
  transferDiagnostics,
  transferSettled,
  transferWorktree,
} from "./ipc";

describe("daemonStatus", () => {
  it("passes the unavailable reason through", async () => {
    transport.invoke.mockReset().mockResolvedValue({
      state: "Disconnected",
      log_path: "",
      reason: "herdr is unavailable: not found. Fix this, then relaunch Keitora.",
    });
    const info = await daemonStatus();
    expect(info.state).toBe("Disconnected");
    expect(info.reason).toBe("herdr is unavailable: not found. Fix this, then relaunch Keitora.");
  });

  it("leaves the reason undefined when the payload has none", async () => {
    transport.invoke.mockReset().mockResolvedValue({ state: "Connected", pid: 7 });
    const info = await daemonStatus();
    expect(info.reason).toBeUndefined();
    expect(info.pid).toBe(7);
  });

  it("carries restartable through and treats a missing field as false", async () => {
    transport.invoke
      .mockReset()
      .mockResolvedValue({ state: "Disconnected", log_path: "", restartable: true });
    expect((await daemonStatus()).restartable).toBe(true);
    transport.invoke.mockReset().mockResolvedValue({ state: "Disconnected", log_path: "" });
    expect((await daemonStatus()).restartable).toBe(false);
  });
});

describe("ptySpawn", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(1);
  });

  it("defaults fresh to false", async () => {
    await ptySpawn("keitora", "epsilon", "/wt", { rows: 24, cols: 80 });
    expect(transport.invoke).toHaveBeenCalledWith("pty_spawn", {
      project: "keitora",
      id: "epsilon",
      cwd: "/wt",
      rows: 24,
      cols: 80,
      command: null,
      fresh: false,
    });
  });

  it("sends null sizes through when the pane is unmeasured", async () => {
    await ptySpawn("keitora", "epsilon", "/wt", null);
    expect(transport.invoke).toHaveBeenCalledWith(
      "pty_spawn",
      expect.objectContaining({ rows: null, cols: null }),
    );
  });

  it("forwards fresh", async () => {
    await ptySpawn("keitora", "epsilon", "/wt", { rows: 24, cols: 80 }, undefined, true);
    expect(transport.invoke).toHaveBeenCalledWith(
      "pty_spawn",
      expect.objectContaining({ command: null, fresh: true }),
    );
  });
  it("omits the transfer key unless true", async () => {
    const size = { rows: 24, cols: 80 };
    await ptySpawn("proj", "alpha", "/wt", size, undefined, false, false);
    await ptySpawn("proj", "alpha", "/wt", size, undefined, false, undefined);
    await ptySpawn("proj", "alpha", "/wt", size);
    expect(transport.invoke).toHaveBeenCalledTimes(3);
    for (const [, payload] of transport.invoke.mock.calls) {
      expect(payload).not.toHaveProperty("transfer");
    }
  });

  it("forwards transfer when true", async () => {
    await ptySpawn("proj", "alpha", "/wt", { rows: 24, cols: 80 }, undefined, false, true);
    expect(transport.invoke).toHaveBeenCalledWith("pty_spawn", {
      project: "proj",
      id: "alpha",
      cwd: "/wt",
      rows: 24,
      cols: 80,
      command: null,
      fresh: false,
      transfer: true,
    });
  });
});

describe("transfer wrappers", () => {
  beforeEach(() => {
    transport.invoke.mockReset();
  });

  it("pass project then worktree", async () => {
    transport.invoke.mockResolvedValue(undefined);
    await previewClaudeTransfer("proj", "alpha");
    expect(transport.invoke).toHaveBeenLastCalledWith("preview_claude_transfer", {
      project: "proj",
      worktree: "alpha",
    });
    await injectHandoffSummary("proj", "alpha");
    expect(transport.invoke).toHaveBeenLastCalledWith("inject_handoff_summary", {
      project: "proj",
      worktree: "alpha",
    });
    await cancelTransfer("proj", "alpha");
    expect(transport.invoke).toHaveBeenLastCalledWith("cancel_transfer", {
      project: "proj",
      worktree: "alpha",
    });
    await transferSettled("other-proj", "bravo");
    expect(transport.invoke).toHaveBeenLastCalledWith("transfer_settled", {
      project: "other-proj",
      worktree: "bravo",
    });
  });

  it("transferWorktree sends targetCli and resolves the outcome unchanged", async () => {
    const outcome: TransferOutcome = {
      kind: "fresh",
      reason: { kind: "skipped", reason: "continued_in", message: "m", stderrTail: "" },
      reasonText: "x",
    };
    transport.invoke.mockResolvedValue(outcome);
    await expect(transferWorktree("proj", "alpha", "codex")).resolves.toBe(outcome);
    expect(transport.invoke).toHaveBeenCalledWith("transfer_worktree", {
      project: "proj",
      worktree: "alpha",
      targetCli: "codex",
    });
    // Without a confirmed transcript the key is left out entirely.
    const calls = transport.invoke.mock.calls;
    expect(Object.keys(calls[calls.length - 1]?.[1] ?? {})).not.toContain("jsonl");
  });

  it("transferWorktree sends the confirmed transcript as jsonl", async () => {
    transport.invoke.mockResolvedValue({ kind: "imported", threadId: "t1" });
    await transferWorktree("proj", "alpha", "codex", "/home/u/.claude/projects/enc/s-1.jsonl");
    expect(transport.invoke).toHaveBeenCalledWith("transfer_worktree", {
      project: "proj",
      worktree: "alpha",
      targetCli: "codex",
      jsonl: "/home/u/.claude/projects/enc/s-1.jsonl",
    });
  });

  it("transferDiagnostics resolves null unchanged", async () => {
    transport.invoke.mockResolvedValue(null);
    await expect(transferDiagnostics("proj", "alpha")).resolves.toBeNull();
    expect(transport.invoke).toHaveBeenCalledWith("transfer_diagnostics", {
      project: "proj",
      worktree: "alpha",
    });
  });
});

describe("onTransferProgress", () => {
  beforeEach(() => {
    transport.listen.mockReset();
  });

  it("listens on transfer://progress and hands the payload to the callback", async () => {
    const payload: TransferProgress = {
      project: "proj",
      worktree: "alpha",
      phase: "importing",
      message: null,
      elapsedMs: 10,
      deadlineMs: 150_000,
    };
    const un = vi.fn();
    transport.listen.mockImplementation((_channel: string, cb: (p: unknown) => void) => {
      cb(payload);
      return Promise.resolve(un);
    });
    const cb = vi.fn();
    await expect(onTransferProgress(cb)).resolves.toBe(un);
    expect(transport.listen).toHaveBeenCalledWith("transfer://progress", cb);
    expect(cb).toHaveBeenCalledWith(payload);
  });
});

describe("sessionTerminate", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
  });

  it("forwards the generation", async () => {
    await sessionTerminate("keitora", "epsilon", 4);
    expect(transport.invoke).toHaveBeenCalledWith("session_terminate", {
      project: "keitora",
      id: "epsilon",
      gen: 4,
    });
  });

  it("sends a null generation when none is known", async () => {
    await sessionTerminate("keitora", "epsilon");
    expect(transport.invoke).toHaveBeenCalledWith("session_terminate", {
      project: "keitora",
      id: "epsilon",
      gen: null,
    });
  });
});

describe("sessionLaunch", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(2);
  });

  it("defaults fresh to false and returns the generation", async () => {
    await expect(sessionLaunch("keitora", "epsilon", "/wt", { rows: 24, cols: 80 })).resolves.toBe(
      2,
    );
    expect(transport.invoke).toHaveBeenCalledWith("session_launch", {
      project: "keitora",
      id: "epsilon",
      cwd: "/wt",
      rows: 24,
      cols: 80,
      fresh: false,
    });
  });

  it("sends null sizes through when the pane is unmeasured", async () => {
    await sessionLaunch("keitora", "epsilon", "/wt", null);
    expect(transport.invoke).toHaveBeenCalledWith(
      "session_launch",
      expect.objectContaining({ rows: null, cols: null }),
    );
  });

  it("forwards fresh", async () => {
    await sessionLaunch("keitora", "epsilon", "/wt", { rows: 24, cols: 80 }, true);
    expect(transport.invoke).toHaveBeenCalledWith(
      "session_launch",
      expect.objectContaining({ fresh: true }),
    );
  });
});

describe("ptyScroll", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
  });

  it("invokes pty_scroll with the direction, source, lines and cell", async () => {
    await ptyScroll("keitora", "epsilon", "up", "wheel", 3, 12, 7);
    expect(transport.invoke).toHaveBeenCalledWith("pty_scroll", {
      project: "keitora",
      id: "epsilon",
      direction: "up",
      source: "wheel",
      lines: 3,
      column: 12,
      row: 7,
    });
    await ptyScroll("keitora", "epsilon", "down", "page_key", 23, 0, 0);
    expect(transport.invoke).toHaveBeenLastCalledWith(
      "pty_scroll",
      expect.objectContaining({ direction: "down", source: "page_key", lines: 23 }),
    );
  });
});

describe("ptyScrollOffset", () => {
  beforeEach(() => {
    transport.invoke.mockReset();
  });

  it("invokes pty_scroll_offset and returns the offset", async () => {
    transport.invoke.mockResolvedValue(42);
    await expect(ptyScrollOffset("keitora", "epsilon")).resolves.toBe(42);
    expect(transport.invoke).toHaveBeenCalledWith("pty_scroll_offset", {
      project: "keitora",
      id: "epsilon",
    });
  });

  it("passes through null when the backend cannot tell", async () => {
    transport.invoke.mockResolvedValue(null);
    await expect(ptyScrollOffset("keitora", "epsilon")).resolves.toBeNull();
  });
});

describe("ptyWrite", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
  });

  it("omits t0 when undefined", async () => {
    await ptyWrite("keitora", "epsilon", "x");
    expect(transport.invoke).toHaveBeenCalledWith("pty_write", {
      project: "keitora",
      id: "epsilon",
      data: "x",
    });
  });

  it("forwards t0 when set", async () => {
    await ptyWrite("keitora", "epsilon", "x", 123.5);
    expect(transport.invoke).toHaveBeenCalledWith("pty_write", {
      project: "keitora",
      id: "epsilon",
      data: "x",
      t0: 123.5,
    });
  });
});

describe("ptyPaste", () => {
  it("invokes pty_paste with the project, id and text", async () => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
    await ptyPaste("keitora", "epsilon", "a\rb");
    expect(transport.invoke).toHaveBeenCalledWith("pty_paste", {
      project: "keitora",
      id: "epsilon",
      text: "a\rb",
    });
  });

  it("passes a size refusal through as the rejection", async () => {
    transport.invoke.mockReset().mockRejectedValue("PasteTooLarge: 1048577 > 1048576");
    await expect(ptyPaste("keitora", "epsilon", "x")).rejects.toBe(
      "PasteTooLarge: 1048577 > 1048576",
    );
  });
});

describe("ptyAttach", () => {
  beforeEach(() => {
    transport.invoke.mockReset();
    transport.channel.mockReset();
  });

  it("passes a channel wired to the handler and returns the attach id", async () => {
    // The mocked channel is just its handler, so the test can play messages into it.
    transport.channel.mockImplementation((onMessage: (m: OutputMsg) => void) => ({ onMessage }));
    transport.invoke.mockResolvedValue({
      gen: 3,
      scrollback: "",
      attach_id: 9,
      repaints: true,
    });
    const got: OutputMsg[] = [];
    const result = await ptyAttach("keitora", "epsilon", { rows: 24, cols: 80 }, (m) =>
      got.push(m),
    );
    expect(result.attach_id).toBe(9);
    expect(result.repaints).toBe(true);

    const [cmd, args] = transport.invoke.mock.calls[0];
    expect(cmd).toBe("pty_attach");
    expect(args).toMatchObject({ project: "keitora", id: "epsilon", rows: 24, cols: 80 });
    args.onOutput.onMessage({ type: "frame", data: "aGk=", full: true });
    args.onOutput.onMessage({ type: "exit" });
    expect(got).toEqual([{ type: "frame", data: "aGk=", full: true }, { type: "exit" }]);
  });

  it("sends null sizes through when the pane is unmeasured", async () => {
    transport.channel.mockImplementation((onMessage: (m: OutputMsg) => void) => ({ onMessage }));
    transport.invoke.mockResolvedValue({ gen: 3, scrollback: "", attach_id: 9, repaints: true });
    await ptyAttach("keitora", "epsilon", null, () => {});
    expect(transport.invoke.mock.calls[0][1]).toMatchObject({ rows: null, cols: null });
  });
});

describe("ptyDetach", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
  });

  it("scopes the detach to an attach id", async () => {
    await ptyDetach("keitora", "epsilon", 9);
    expect(transport.invoke).toHaveBeenCalledWith("pty_detach", {
      project: "keitora",
      id: "epsilon",
      attachId: 9,
    });
  });

  it("sends a null attach id when unscoped", async () => {
    await ptyDetach("keitora", "epsilon");
    expect(transport.invoke).toHaveBeenCalledWith("pty_detach", {
      project: "keitora",
      id: "epsilon",
      attachId: null,
    });
  });
});

describe("onPtyResume", () => {
  beforeEach(() => {
    transport.listen.mockReset();
  });

  it("listens on the session's resume channel", async () => {
    const un = vi.fn();
    transport.listen.mockResolvedValue(un);
    const cb = vi.fn();
    await expect(onPtyResume("keitora", "epsilon", cb)).resolves.toBe(un);
    expect(transport.listen).toHaveBeenCalledWith("pty://resume/keitora/epsilon", cb);
  });

  it("resolves to a no-op when the transport cannot listen", async () => {
    transport.listen.mockRejectedValue(new Error("unsupported"));
    const un = await onPtyResume("keitora", "epsilon", () => {});
    expect(() => un()).not.toThrow();
    transport.listen.mockImplementation(() => {
      throw new Error("unsupported");
    });
    const un2 = await onPtyResume("keitora", "epsilon", () => {});
    expect(() => un2()).not.toThrow();
  });
});

describe("onPtyPasteRefused", () => {
  beforeEach(() => {
    transport.listen.mockReset();
  });

  it("listens on the session's paste-refused channel", async () => {
    const un = vi.fn();
    transport.listen.mockResolvedValue(un);
    const cb = vi.fn();
    await expect(onPtyPasteRefused("keitora", "epsilon", cb)).resolves.toBe(un);
    expect(transport.listen).toHaveBeenCalledWith("pty://paste-refused/keitora/epsilon", cb);
  });

  it("resolves to a no-op when the transport cannot listen", async () => {
    transport.listen.mockRejectedValue(new Error("unsupported"));
    const un = await onPtyPasteRefused("keitora", "epsilon", () => {});
    expect(() => un()).not.toThrow();
    transport.listen.mockImplementation(() => {
      throw new Error("unsupported");
    });
    const un2 = await onPtyPasteRefused("keitora", "epsilon", () => {});
    expect(() => un2()).not.toThrow();
  });
});
