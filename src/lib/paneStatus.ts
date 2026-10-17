// The terminal pane's status and how it reacts to daemon connectivity and herdr
// server recovery. Pure, so the reaction is unit-testable. Panes never restart
// themselves: the backend owns recovery and resumes sessions on its own.

import type { DaemonState } from "./ipc";
import type { PaneRecovery } from "./recovery";

/**
 * The per-pane state machine's status. DECIDED union — no other values are valid.
 */
export type Status =
  | "running" // streaming live output; input enabled
  | "attaching" // ptyAttach RPC in flight; input gated; term.open not yet called
  | "detached-alive" // daemon holds a live session; pane not yet streaming (transient)
  | "daemon-down" // daemon unreachable; pane frozen (NOT ended; do not call ptyKill)
  | "recovering" // herdr is recovering; session will resume automatically
  | "suspended" // recovery parked this session; it resumes when the pane is focused
  | "spawn-failed" // ptySpawn returned an error
  | "ended" // claude process exited normally (pty://exit fired)
  | "ended-while-away" // claude exited during detach; discovered on re-attach attempt
  | "lost" // session was held but daemon restarted with empty Welcome.sessions
  | "terminated"; // the user terminated the session; stays down until launched

export const ALL_STATUSES: readonly Status[] = [
  "running",
  "attaching",
  "detached-alive",
  "daemon-down",
  "recovering",
  "suspended",
  "spawn-failed",
  "ended",
  "ended-while-away",
  "lost",
  "terminated",
];

/** What the pane does next: its status, and whether to (re)attach. */
export interface PaneReaction {
  status: Status;
  attach: boolean;
}

const LIVE: readonly Status[] = ["running", "attaching", "detached-alive"];

/** Statuses a recovery state may replace: the pane's session is (or was) live. */
const RECOVERABLE: readonly Status[] = [...LIVE, "daemon-down", "recovering", "suspended"];

/** Statuses that re-attach once recovery lets go of the pane. */
const WAITING: readonly Status[] = ["daemon-down", "recovering", "suspended"];

/**
 * Statuses whose session can't take input right now: the pane is still attaching,
 * waiting on the daemon or recovery, or terminated. Keystrokes and scrolls are dropped.
 */
const INPUT_BLOCKED: readonly Status[] = ["attaching", "detached-alive", ...WAITING, "terminated"];

/** Whether a pane in `status` drops keystrokes and scrolls (see `INPUT_BLOCKED`). */
export function inputBlocked(status: Status): boolean {
  return INPUT_BLOCKED.includes(status);
}

/**
 * React to the daemon state and the pane's recovery state (`paneRecoveryState`).
 * A terminated pane stays terminated: nothing re-attaches it and recovery never
 * claims it (only a launch brings it back). A suspended session shows as suspended whatever the pane was doing; while
 * herdr recovers, a live or frozen pane goes quiet (`recovering`); ended and
 * failed panes keep their state. Once recovery lets go of the pane and the
 * daemon is connected, a waiting pane re-attaches (attach-first: a live session
 * is adopted, a missing one is spawned through the gate); a daemon that drops
 * while recovery is idle (herdr unavailable or stopped) freezes the pane. Nothing
 * here ever restarts a session.
 */
export function nextPaneStatus(
  status: Status,
  daemonState: DaemonState,
  rec: PaneRecovery,
): PaneReaction {
  if (status === "terminated") return { status, attach: false };
  if (rec === "suspended") return { status: "suspended", attach: false };
  if (rec === "recovering") {
    return RECOVERABLE.includes(status)
      ? { status: "recovering", attach: false }
      : { status, attach: false };
  }
  if (daemonState === "Connected" && WAITING.includes(status)) {
    return { status, attach: true };
  }
  if (daemonState === "Disconnected" && LIVE.includes(status)) {
    return { status: "daemon-down", attach: false };
  }
  return { status, attach: false };
}

/**
 * What a pane does after the backend refused it because herdr is recovering.
 * The refusal can have been decided before recovery let go of the pane: the
 * re-attach that recovery ending triggers may then already have been skipped
 * (an attach was in flight), so a pane recovery no longer holds tries again
 * at once instead of waiting for a change that already happened.
 */
export function afterRecoveringRefusal(daemonState: DaemonState, rec: PaneRecovery): PaneReaction {
  return nextPaneStatus("recovering", daemonState, rec);
}
