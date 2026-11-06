import { describe, expect, it } from "vitest";
import {
  dimLine,
  nextRestartSignal,
  restartConfirmCopy,
  resumeNoticeLine,
  terminateConfirmCopy,
} from "./resumeNotice";

describe("restartConfirmCopy", () => {
  it("resume mode warns about lost output and confirms a kill", () => {
    expect(restartConfirmCopy("resume")).toEqual({
      message:
        "This will terminate the running session and resume its most recent conversation. Output since your last visit will be lost.",
      confirmLabel: "Confirm kill",
    });
  });

  it("fresh mode says the old conversation stays on disk", () => {
    expect(restartConfirmCopy("fresh")).toEqual({
      message:
        "This will end the running session and start a new, empty conversation. The previous conversation stays on disk.",
      confirmLabel: "Confirm fresh start",
    });
  });
});

describe("terminateConfirmCopy", () => {
  it("says the session stays down until launched", () => {
    expect(terminateConfirmCopy()).toEqual({
      message:
        "This will terminate the running session. It stays down, and is not resumed, until you launch it again.",
      confirmLabel: "Confirm terminate",
    });
  });
});

describe("resumeNoticeLine", () => {
  it("names a resumed or continued conversation", () => {
    expect(resumeNoticeLine({ outcome: "resumed", fell_back: false })).toBe(
      "— resumed previous conversation —",
    );
    expect(resumeNoticeLine({ outcome: "continued", fell_back: false })).toBe(
      "— continued most recent conversation —",
    );
  });

  it("prefixes a fallback", () => {
    expect(resumeNoticeLine({ outcome: "continued", fell_back: true })).toBe(
      "— could not resume that session; continued most recent conversation —",
    );
    expect(resumeNoticeLine({ outcome: "fresh", fell_back: true })).toBe(
      "— could not resume that session; started a new conversation —",
    );
  });

  it("says nothing for a plain fresh start or an unknown outcome", () => {
    expect(resumeNoticeLine({ outcome: "fresh", fell_back: false })).toBeNull();
    expect(resumeNoticeLine({ outcome: "teleported", fell_back: true })).toBeNull();
  });

  it("dims the line", () => {
    expect(dimLine("x")).toBe("\x1b[2mx\x1b[m");
  });
});

describe("nextRestartSignal", () => {
  it("bumps the counter and carries the latest flavour", () => {
    const first = nextRestartSignal(undefined, true);
    expect(first).toEqual({ n: 1, fresh: true, transfer: false });
    // Each bump carries only its own flavour: a resume right after a fresh start is not fresh.
    expect(nextRestartSignal(first, false)).toEqual({ n: 2, fresh: false, transfer: false });
  });

  it("carries transfer when asked", () => {
    const first = nextRestartSignal(undefined, false, true);
    expect(first).toEqual({ n: 1, fresh: false, transfer: true });
    // A following plain bump is not a transfer restart.
    expect(nextRestartSignal(first, false)).toEqual({ n: 2, fresh: false, transfer: false });
  });
});
