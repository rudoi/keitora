// Typed bridge to the dispatcher backend (src-tauri/src/dispatch.rs via Tauri
// commands). Mirrors lib/tasks.ts and lib/ipc.ts — snake_case field names match
// the Rust serde output; camelCase invoke args match Tauri's arg conversion.

import { getTransport } from "./transport";

// Route through the active transport (verbatim Tauri).
function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return getTransport().invoke<T>(cmd, args);
}

export interface DispatchOutcome {
  dispatched: boolean;
  reason: string | null;
}

/** Reserve and (if the worktree is idle + live) inject the task into its PTY.
 *  The backend is authoritative on the idle-vs-queue decision. */
export function dispatchApply(worktree: string, taskId: number): Promise<DispatchOutcome> {
  return invoke<DispatchOutcome>("dispatch_apply", { worktree, taskId });
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
