import { describe, expect, it } from "vitest";
import { switchConfirmCopy } from "./agentSwitch";

describe("switchConfirmCopy", () => {
  it("warns that the Claude conversation is not carried over to Antigravity", () => {
    expect(switchConfirmCopy("claude", "antigravity")).toEqual({
      title: "Switch to Antigravity?",
      body:
        "Antigravity will start a new conversation. The current Claude conversation won't be " +
        "carried over; it stays on disk, and switching back later starts fresh too.",
      confirmLabel: "Switch to Antigravity",
    });
  });

  it("names both agents when switching from Antigravity to Codex", () => {
    const copy = switchConfirmCopy("antigravity", "codex");
    expect(copy.title).toBe("Switch to Codex?");
    expect(copy.confirmLabel).toBe("Switch to Codex");
    expect(copy.body).toContain("Codex will start a new conversation.");
    expect(copy.body).toContain("The current Antigravity conversation won't be carried over");
    expect(copy.body).not.toContain("middle of a turn");
  });

  it("appends the mid-turn note for the source agent when it is mid-turn", () => {
    const calm = switchConfirmCopy("antigravity", "claude");
    const busy = switchConfirmCopy("antigravity", "claude", true);
    expect(busy.body).toBe(
      `${calm.body}\n\nAntigravity is in the middle of a turn. That in-flight turn will not be ` +
        "carried over.",
    );
    expect(busy.title).toBe(calm.title);
    expect(busy.confirmLabel).toBe(calm.confirmLabel);
  });
});
