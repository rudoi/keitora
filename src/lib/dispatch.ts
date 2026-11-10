// Typed bridge to the dispatcher backend (src-tauri/src/dispatch.rs via Tauri
// commands). Mirrors lib/tasks.ts and lib/ipc.ts — snake_case field names match
// the Rust serde output; camelCase invoke args match Tauri's arg conversion.

import { getTransport } from "./transport";

// Route through the active transport (verbatim Tauri).
function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return getTransport().invoke<T>(cmd, args);
}

export interface DispatchOutcome {
  /** The prompt reached the session (fresh, partial, or sent earlier). */
  dispatched: boolean;
  /** sent | already_sent | pending (assigned, waiting on its lane) | failed (assigned,
   *  but the send errored; it stays pending). */
  state: "sent" | "already_sent" | "pending" | "failed";
  /** The lane the task is on (the picked one, for an auto-pick). */
  worktree: string;
  reason: string | null;
}

/** Assign the task to `worktree`'s lane at once, and send the lane's front card if
 *  the lane is free. The backend is authoritative on send-vs-pending. */
export function dispatchApply(worktree: string, taskId: number): Promise<DispatchOutcome> {
  return invoke<DispatchOutcome>("dispatch_apply", { worktree, taskId });
}

/** One-click Dispatch: the backend auto-picks a pool worktree (an idle one first,
 *  least recently dispatched; else the shortest queue) and applies as above. */
export function dispatchAuto(taskId: number): Promise<DispatchOutcome> {
  return invoke<DispatchOutcome>("dispatch_auto", { taskId });
}

/** A pending card whose latest send to its lane failed (from `board_snapshot`). */
export interface DispatchFailure {
  task_id: number;
  worktree: string;
  reason: string;
  at: string;
}

/** A task already dispatched and still reserved to a worktree. Serialized from the
 *  Rust `Reservation` struct as an object (NOT a positional array). The board shows
 *  these as a non-clickable `sent ✓`. Fetched as part of `board_snapshot` (see
 *  `useTasks`), not via a standalone command. */
export interface Reservation {
  task_id: number;
  worktree: string;
}

/** Admin force-unclaim a task regardless of current owner. */
export function releaseTask(id: number): Promise<void> {
  return invoke("task_release", { id });
}
