// Static class maps for task priority/status → token colors. Enumerated (not
// interpolated) so Tailwind's compiler detects every class name.

import type { SessionState } from "./sessions";
import type { Priority, TaskStatus } from "./tasks";

// Session-state → dot color. Enumerated so Tailwind detects each class.
export const SESSION_DOT: Record<SessionState, string> = {
  idle: "bg-idle",
  busy: "bg-busy",
  waiting: "bg-waiting",
  offline: "bg-offline",
};

/** Dot classes for a session state (color + a gentle pulse while busy). */
export function sessionDotCls(state: SessionState): string {
  return `${SESSION_DOT[state]} ${state === "busy" ? "animate-pulse" : ""}`;
}

/** Order dispatch targets so the actionable ones lead: idle → busy → waiting →
 *  offline. Used by the DispatchMenu. */
export const STATE_ORDER: Record<SessionState, number> = {
  idle: 0,
  busy: 1,
  waiting: 2,
  offline: 3,
};

export const PRIORITY_BAR: Record<Priority, string> = {
  urgent: "bg-urgent",
  high: "bg-high",
  normal: "bg-normal",
  low: "bg-low",
};

export const PRIORITY_PILL: Record<Priority, string> = {
  urgent: "bg-urgent/15 text-urgent",
  high: "bg-high/15 text-high",
  normal: "bg-normal/15 text-normal",
  low: "bg-low/15 text-low",
};

// Session/worktree status dot. (Live idle/busy/waiting arrives in Phase 3; for
// now active work shows the accent, terminal states are muted.)
export const STATUS_DOT: Record<TaskStatus, string> = {
  open: "bg-tx-3",
  active: "bg-accent",
  review: "bg-waiting",
  backlog: "bg-offline",
  done: "bg-idle",
  cancelled: "bg-offline",
};

/** Compact `MM-DD HH:MM` from a SQLite `YYYY-MM-DD HH:MM:SS` timestamp. */
export function shortTime(ts: string): string {
  // ts is "2026-06-20 14:02:31"; slice without constructing a Date (UTC-safe).
  const m = /^\d{4}-(\d{2}-\d{2}) (\d{2}:\d{2})/.exec(ts);
  return m ? `${m[1]} ${m[2]}` : ts;
}
