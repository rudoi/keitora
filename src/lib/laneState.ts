// Pure lane-state helpers shared by the rail's lane rows, its project-group summaries
// and the worktree context menu. No React imports, so the rail's data model can use
// them without depending on a component module.

import type { AttentionKind } from "./attention";
import type { CliKind, WorktreeStatus } from "./ipc";
import type { PendingAction } from "./pendingIntent";
import type { WorktreeSession } from "./sessions";

/** Every class {@link blockStateCls} can return: a lane block's / summary dot's colour. */
export type DotState = "idle" | "busy" | "waiting" | "offline" | "working" | "terminated";

/**
 * Colour class for a worktree's status block. A `waiting` attention (blocked on a permission
 * prompt) wins with its own colour; a `finished` worktree deliberately does NOT — it falls
 * through to its live `idle` session state (green/available), since the badge + notification
 * already carry the "finished — awaiting you" signal. Otherwise the live session state, then
 * the filesystem status for worktrees with no session yet. A terminated session wins over
 * everything: it has no agent, so nothing else about it is live.
 */
export function blockStateCls(
  attn: AttentionKind | undefined,
  session: WorktreeSession | undefined,
  available: boolean,
  building = false,
  terminated = false,
): DotState {
  if (terminated) return "terminated";
  if (attn === "waiting") return "waiting";
  // A build running for this worktree (including one launched outside Keitora, with no live
  // session) tints the block its "working" colour — it wins over the live session state but
  // never over a blocking `waiting` prompt, which still needs you.
  if (building) return "working";
  if (session) return session.state; // idle | busy | waiting | offline
  return available ? "idle" : "offline";
}

/**
 * Whether a worktree's status block animates (glow + pulse). Only a blocking,
 * ball-in-your-court state does — i.e. `waiting`. A `finished` worktree stays steady so it
 * reads as available, not as a warning.
 */
export function blockAnimates(attn: AttentionKind | undefined): boolean {
  return attn === "waiting";
}

/** One entry of the worktree context menu. */
export type WorktreeMenuItem =
  | "focus"
  | "restart"
  | "fresh"
  | "transfer"
  | "terminate"
  | "launch"
  | "launch-fresh";

/**
 * The context-menu entries for a worktree, in order. A terminated session can only be
 * launched (resuming its conversation, or fresh); restarting or transferring it would
 * bring it back behind the user's back. A lane of a project other than the active one
 * (`inactive`) leads with "focus", which switches to that project and selects the lane.
 */
export function worktreeMenuItems(
  terminated: boolean,
  opts?: { inactive?: boolean },
): WorktreeMenuItem[] {
  const items: WorktreeMenuItem[] = terminated
    ? ["launch", "launch-fresh"]
    : ["restart", "fresh", "transfer", "terminate"];
  return opts?.inactive ? ["focus", ...items] : items;
}

/**
 * The lane action a worktree-menu entry runs, with `transferTo` the CLI a transfer moves
 * the session to. `null` for "focus", which only selects the lane.
 */
export function laneMenuAction(item: WorktreeMenuItem, transferTo: CliKind): PendingAction | null {
  switch (item) {
    case "focus":
      return null;
    case "restart":
      return { kind: "restart", fresh: false };
    case "fresh":
      return { kind: "restart", fresh: true };
    case "transfer":
      return { kind: "transfer", to: transferTo };
    case "terminate":
      return { kind: "terminate" };
    case "launch":
      return { kind: "launch", fresh: false };
    case "launch-fresh":
      return { kind: "launch", fresh: true };
  }
}

/**
 * Why the worktree menu's transfer entry is disabled, or `null` when it is enabled. A
 * transfer already in flight wins over a missing Codex install.
 */
export function transferDisabledReason(o: { codexMissing: boolean; busy: boolean }): string | null {
  if (o.busy) return "A transfer is already in progress";
  if (o.codexMissing) return "Codex isn't installed";
  return null;
}

/**
 * Why a session-killing worktree-menu entry (restart, start fresh, terminate) is disabled,
 * or `null` when it is enabled: the pane ignores those signals while a transfer owns it.
 */
export function sessionItemDisabledReason(
  item: WorktreeMenuItem,
  transferBusy: boolean,
): string | null {
  const kills = item === "restart" || item === "fresh" || item === "terminate";
  return kills && transferBusy ? "A transfer is in progress" : null;
}

/**
 * Why the worktree menu's "Open folder" and "Open terminal here" are disabled, or `null`
 * when they are enabled: both need the worktree's directory to exist. A directory that
 * isn't a git worktree can still be opened.
 */
export function openItemDisabledReason(status: WorktreeStatus): string | null {
  if (status === "missing") return "The worktree's path was not found";
  if (status === "not_dir") return "The worktree's path is not a directory";
  return null;
}
