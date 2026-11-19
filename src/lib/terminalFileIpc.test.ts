import { beforeEach, describe, expect, it, vi } from "vitest";

const transport = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("./transport", () => ({
  getTransport: () => transport,
}));

import { openTerminalFile } from "./ipc";

describe("openTerminalFile", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
  });

  it("passes only project, worktree, and literal decoded path", async () => {
    await openTerminalFile("keitora", "epsilon", "src/literal%20file.ts");
    expect(transport.invoke).toHaveBeenCalledWith("open_terminal_file", {
      project: "keitora",
      worktree: "epsilon",
      path: "src/literal%20file.ts",
    });
  });

  it("preserves native failure feedback", async () => {
    transport.invoke.mockRejectedValue(new Error("File does not exist"));
    await expect(openTerminalFile("keitora", "epsilon", "../file.ts")).rejects.toThrow(
      "File does not exist",
    );
  });
});
