// A lane action or focus request aimed at a project that is not active yet. Clicking a
// lane of another project switches to it first; the follow-up (select the lane, open its
// diff, restart its session, ...) waits in a single pending slot until the switch has
// settled, because the lane actions resolve the backend's active project and the target's
// panes must be mounted to receive them. Pure — no React, no Tauri.

import type { CliKind } from "./ipc";

/** What to run on a lane once its project is active. */
export type PendingAction =
  | { kind: "restart"; fresh: boolean }
  | { kind: "transfer"; to: CliKind }
  | { kind: "switch"; to: CliKind }
  | { kind: "terminate" }
  | { kind: "launch"; fresh: boolean }
  | { kind: "diff" }
  | { kind: "new-worktree" };

export interface PendingIntent {
  /** Resolved name of the switch target. */
  project: string;
  /** Lane to focus on arrival; null for "new-worktree" and plain project switches. */
  worktree: string | null;
  action: PendingAction | null;
}

/** The App state a pending intent waits on, all keyed by resolved name. */
export interface SettleState {
  activeProjectName: string | null;
  switchingTo: string | null;
  warmActive: string | null;
  /** Whether the warm registry holds the intent's project. */
  warmHas: boolean;
}

/**
 * Whether the switch to `i.project` has settled: the project is active, no switch is in
 * flight, and the warm registry has seeded it and made it the visible one. The active
 * project flips optimistically, so `activeProjectName` alone is not enough.
 */
export function canConsume(i: PendingIntent, s: SettleState): boolean {
  return (
    s.activeProjectName === i.project &&
    s.switchingTo === null &&
    s.warmActive === i.project &&
    s.warmHas
  );
}

/** Whether the switch ended somewhere else (failed or superseded), so the intent is stale. */
export function shouldDrop(i: PendingIntent, s: SettleState): boolean {
  return s.switchingTo === null && s.activeProjectName !== i.project;
}

/** Options for a project switch started from a lane or a menu. */
export interface SwitchOpts {
  /** Worktree to focus on arrival (implies the focus view). */
  select?: string | null;
  /** Skip the "Switched to …" toast. */
  silent?: boolean;
  /** Run once the switch settles. */
  action?: PendingAction | null;
}

/**
 * The intent a switch to `resolvedName` must park until it settles, or null when there is
 * nothing to do afterwards. An action always waits. A selection waits unless the caller
 * has already applied it (`selectedUpFront`): a warm target shows its own worktrees
 * immediately, so its selection is applied with the visibility flip, while a cold one
 * still shows the previous project's worktrees, which would reset an early selection. An
 * already-active target parks its selection too; the consumer applies it at once when
 * that project is settled.
 */
export function intentForSwitch(
  resolvedName: string,
  opts: SwitchOpts | undefined,
  selectedUpFront: boolean,
): PendingIntent | null {
  const action = opts?.action ?? null;
  const worktree = opts?.select ?? null;
  if (action === null && (worktree === null || selectedUpFront)) return null;
  return { project: resolvedName, worktree, action };
}
