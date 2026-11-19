import { beforeEach, describe, expect, it, vi } from "vitest";

const transport = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("./transport", () => ({
  getTransport: () => transport,
}));

import { notifySession, onNotificationActivated } from "./ipc";

describe("session notification ipc", () => {
  beforeEach(() => {
    transport.invoke.mockReset().mockResolvedValue(undefined);
    transport.listen.mockReset().mockResolvedValue(() => {});
  });

  it("sends the click target alongside the title and body", async () => {
    await notifySession({ project: "proj", worktree: "alpha" }, "Keitora — alpha", "Fix the build");
    expect(transport.invoke).toHaveBeenCalledWith("notify_session", {
      project: "proj",
      worktree: "alpha",
      title: "Keitora — alpha",
      body: "Fix the build",
    });
  });

  it("hands a clicked notification's target to the callback", async () => {
    const cb = vi.fn();
    await onNotificationActivated(cb);
    const [channel, handler] = transport.listen.mock.calls[0];
    expect(channel).toBe("notification://activated");
    handler({ project: "proj", worktree: "bravo" });
    expect(cb).toHaveBeenCalledWith({ project: "proj", worktree: "bravo" });
  });
});
