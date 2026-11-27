// Pure lane-state helpers shared by the rail's lane rows, its project-group summaries
// and the worktree context menu. No React imports, so the rail's data model can use
// them without depending on a component module.

import type { AttentionKind } from "./attention";
import { type DriveMenuItem, driveMenuItems, isQuietWaiting, type LaneDrive } from "./drive";
import type { CliKind } from "./ipc";
import type { PendingAction } from "./pendingIntent";
import { cliTitle, switchTargets, transferTargets } from "./providers";
import type { WorktreeSession } from "./sessions";

/** Every class {@link blockStateCls} can return: a lane block's / summary dot's colour. */
export type DotState =
  | "idle"
  | "busy"
  | "waiting"
  | "brokered"
  | "offline"
  | "working"
  | "terminated";

/**
 * Colour class for a worktree's status block. A `waiting` attention (blocked on a permission
 * prompt) wins with its own colour; a `finished` worktree deliberately does NOT — it falls
 * through to its live `idle` session state (green/available), since the badge + notification
 * already carry the "finished — awaiting you" signal. Otherwise the live session state, then
 * the filesystem status for worktrees with no session yet. A terminated session wins over
 * everything: it has no agent, so nothing else about it is live. A driven session waiting
 * while its driver decides (`brokered`) gets its own quiet colour: it needs nobody yet.
 */
export function blockStateCls(
  attn: AttentionKind | undefined,
  session: WorktreeSession | undefined,
  available: boolean,
  building = false,
  terminated = false,
): DotState {
  if (terminated) return "terminated";
  if (isQuietWaiting(session)) return "brokered";
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

/** One entry of the worktree context menu. Transfer and switch entries carry their target. */
export type WorktreeMenuItem =
  | { kind: "focus" | "restart" | "fresh" | "terminate" | "launch" | "launch-fresh" }
  | { kind: "transfer" | "switch"; to: CliKind }
  | { kind: DriveMenuItem };

/** A stable React key for a menu entry: `${kind}:${to}` for transfer/switch, else `kind`. */
export function menuItemKey(item: WorktreeMenuItem): string {
  return "to" in item ? `${item.kind}:${item.to}` : item.kind;
}

/**
 * The context-menu entries for a worktree running `cli`, in order. A terminated session can
 * only be launched (resuming its conversation, or fresh); restarting, transferring or
 * switching it would bring it back behind the user's back. A live session offers a transfer
 * to each provider its conversation can be carried to, then a switch to every other
 * provider. A lane of a project other than the active one (`inactive`) leads with "focus",
 * which switches to that project and selects the lane. A driven lane ends with its drive
 * entries (take over, pause/resume, release), also when terminated, so a drive can always
 * be released.
 */
export function worktreeMenuItems(
  cli: CliKind,
  terminated: boolean,
  opts?: { inactive?: boolean; drive?: LaneDrive | null },
): WorktreeMenuItem[] {
  const items: WorktreeMenuItem[] = terminated
    ? [{ kind: "launch" }, { kind: "launch-fresh" }]
    : [
        { kind: "restart" },
        { kind: "fresh" },
        ...transferTargets(cli).map((to): WorktreeMenuItem => ({ kind: "transfer", to })),
        ...switchTargets(cli).map((to): WorktreeMenuItem => ({ kind: "switch", to })),
        { kind: "terminate" },
      ];
  const drive = driveMenuItems(opts?.drive ?? null).map((kind): WorktreeMenuItem => ({ kind }));
  return [...(opts?.inactive ? [{ kind: "focus" } as const] : []), ...items, ...drive];
}

/**
 * The lane action a worktree-menu entry runs. `null` for "focus", which only selects the
 * lane, and for the drive entries, which write the task store without touching the pane.
 */
export function laneMenuAction(item: WorktreeMenuItem): PendingAction | null {
  switch (item.kind) {
    case "focus":
    case "take-over":
    case "pause-driver":
    case "resume-driver":
    case "release":
      return null;
    case "restart":
      return { kind: "restart", fresh: false };
    case "fresh":
      return { kind: "restart", fresh: true };
    case "transfer":
      return { kind: "transfer", to: item.to };
    case "switch":
      return { kind: "switch", to: item.to };
    case "terminate":
      return { kind: "terminate" };
    case "launch":
      return { kind: "launch", fresh: false };
    case "launch-fresh":
      return { kind: "launch", fresh: true };
  }
}

/** A worktree-menu action that moves the worktree to another provider. */
export type LaneAction = "transfer" | "switch";

/**
 * Why a transfer or switch entry targeting `to` is disabled, or `null` when it is enabled. A
 * transfer or switch already in flight wins over a missing target install.
 */
export function actionDisabledReason(
  action: LaneAction,
  to: CliKind,
  available: Record<CliKind, boolean>,
  busy: boolean,
): string | null {
  if (busy) {
    return action === "transfer"
      ? "A transfer is already in progress"
      : "A switch is already in progress";
  }
  if (!available[to]) return `${cliTitle(to)} isn't installed`;
  return null;
}

/**
 * Why a session-killing worktree-menu entry (restart, start fresh, terminate) is disabled,
 * or `null` when it is enabled: the pane ignores those signals while a transfer or switch
 * owns it.
 */
export function sessionItemDisabledReason(
  item: WorktreeMenuItem,
  transferBusy: boolean,
): string | null {
  const kills = item.kind === "restart" || item.kind === "fresh" || item.kind === "terminate";
  return kills && transferBusy ? "A transfer or switch is in progress" : null;
}
