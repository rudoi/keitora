// Wording and small decisions for restarting a session: the Kill & Restart /
// Start fresh / Terminate confirm copy, the dim line a pane prints when the backend resumed
// (or could not resume) a conversation, and the per-worktree restart signal App
// hands each pane. Pure, so the decisions are unit-testable.

/** Which flavour of restart the user asked for. */
export type RestartMode = "resume" | "fresh";

/** Payload of the backend's `pty://resume/<project>/<worktree>` event. */
export interface ResumeNotice {
  outcome: string;
  fell_back: boolean;
}

/**
 * The per-worktree restart signal: a bump counter plus the flavour of the latest bump.
 * `fresh` starts a new, empty conversation; `transfer` marks the restart that completes a
 * CLI transfer (already confirmed, so the pane skips its confirm-kill prompt).
 */
export interface RestartSignal {
  n: number;
  fresh: boolean;
  transfer: boolean;
}

/** The confirm surface's message and confirm-button label for `mode`. */
export function restartConfirmCopy(mode: RestartMode): { message: string; confirmLabel: string } {
  if (mode === "fresh") {
    return {
      message:
        "This will end the running session and start a new, empty conversation. The previous conversation stays on disk.",
      confirmLabel: "Confirm fresh start",
    };
  }
  return {
    message:
      "This will terminate the running session and resume its most recent conversation. Output since your last visit will be lost.",
    confirmLabel: "Confirm kill",
  };
}

/** The confirm surface's message and confirm-button label for terminating a session. */
export function terminateConfirmCopy(): { message: string; confirmLabel: string } {
  return {
    message:
      "This will terminate the running session. It stays down, and is not resumed, until you launch it again.",
    confirmLabel: "Confirm terminate",
  };
}

/** What the agent came back as, in words (no surrounding dashes). */
function outcomeWords(outcome: string): string | null {
  switch (outcome) {
    case "resumed":
      return "resumed previous conversation";
    case "continued":
      return "continued most recent conversation";
    case "fresh":
      return "started a new conversation";
    default:
      return null;
  }
}

/**
 * The notice line for a resume event, or `null` when there is nothing to say (a
 * plain fresh start, or an outcome this build does not know).
 */
export function resumeNoticeLine(n: ResumeNotice): string | null {
  const words = outcomeWords(n.outcome);
  if (words === null) return null;
  if (n.fell_back) return `— could not resume that session; ${words} —`;
  if (n.outcome === "fresh") return null;
  return `— ${words} —`;
}

/** `line` rendered dim for the terminal. */
export function dimLine(line: string): string {
  return `\x1b[2m${line}\x1b[m`;
}

/** The next restart signal after `prev` for a restart of flavour `fresh` (and `transfer`). */
export function nextRestartSignal(
  prev: RestartSignal | undefined,
  fresh: boolean,
  transfer = false,
): RestartSignal {
  return { n: (prev?.n ?? 0) + 1, fresh, transfer };
}
