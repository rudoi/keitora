// How the window carries out a driver's `reset`, `wake` and `swap` requests
// (`drive://restart`, `drive://swap`): the checks every request passes first, the refusal texts the driver
// reads, and the handlers App subscribes. Pure apart from the injected callbacks, so the
// decisions are unit-testable without React or Tauri. The handlers never change the
// human's selection, view or active project: a driver works in the background.

import type {
  CliKind,
  DriveOutcome,
  DriveRequestEvent,
  DriveRestartEvent,
  DriveRestartMode,
  DriveSwapEvent,
} from "./ipc";
import type { Status } from "./paneStatus";
import { cliTitle } from "./providers";
import type { RestartKind } from "./resumeNotice";
import { type SpawnResult, type TransferMap, transferKey } from "./transferPhase";
import type { WarmState } from "./warmProjects";

/** Refusal: the project has no panes in this window. */
export const PROJECT_NOT_OPEN = "project not open in Keitora";
/** Refusal: the human cancelled the swap's confirm dialog. */
export const SWAP_CANCELLED = "swap cancelled in Keitora";
/** Refusal: another transfer started while the swap's dialog was open. */
export const TRANSFER_ALREADY_RUNNING = "a transfer is already in progress";
/** Toast: the swap was confirmed after the runner had already given up on it. */
export const SWAP_EXPIRED_TOAST = "That swap request already expired.";

/** Refusal: the worktree has no live pane in its (open) project. */
export const noPaneReason = (worktree: string): string => `@${worktree} has no pane in Keitora`;
/** Refusal: a transfer owns the worktree's pane. */
export const transferInProgressReason = (worktree: string): string =>
  `a transfer or switch is in progress for @${worktree}`;

/**
 * Why a driver request for `worktree` in `project` cannot run in this window, or null
 * when it can: the project must be open, the worktree must have a pane there, and no
 * transfer may own that pane.
 */
export function drivePrecheck(
  warm: WarmState,
  transfers: TransferMap,
  project: string,
  worktree: string,
): string | null {
  const wp = warm.projects[project];
  if (wp === undefined) return PROJECT_NOT_OPEN;
  if (!wp.worktrees.some((w) => w.name === worktree && w.status === "available")) {
    return noPaneReason(worktree);
  }
  if (transfers.has(transferKey(project, worktree))) return transferInProgressReason(worktree);
  return null;
}

/** The line a driven swap's confirm body starts with, naming who asked for it. */
export function driveSwapConfirmPrefix(
  driver: string,
  worktree: string,
  project: string,
  to: CliKind,
): string {
  return `${driver} asked to transfer ${worktree} in ${project} to ${cliTitle(to)}.\n\n`;
}

/** How a driven reset's spawn is reported back to the runner. */
export function driveRestartOutcome(r: SpawnResult): { outcome: DriveOutcome; reason: string } {
  return r.ok ? { outcome: "applied", reason: "" } : { outcome: "refused", reason: r.error };
}

/** What a pane does for a driver's wake, by its status:
 *  - `launch`: the session is terminated; bring it back the way the rail's Launch does
 *    (which clears the terminated mark);
 *  - `spawn`: the agent exited or never started; spawn it again;
 *  - `resume`: recovery parked the session; ask recovery to bring it back;
 *  - `restart`: recovery parked the session but the wake needs a new one (fresh, or on
 *    another provider); restart it instead of resuming the old one;
 *  - `awake`: a session is running or on its way (attaching, recovering): nothing to do;
 *  - `retry`: as `awake`, but the wake needs a new session and the pane's one is live or
 *    settling, which a wake must not kill; refuse so the driver retries;
 *  - `unavailable`: herdr is unreachable, so nothing can start. */
export type WakeStep =
  | "launch"
  | "spawn"
  | "resume"
  | "restart"
  | "awake"
  | "retry"
  | "unavailable";

/** Pure: the {@link WakeStep} for a pane in `status` (`terminated` is the prop). `replace`
 *  when the wake must start a new session: keeping or resuming the pane's one would
 *  report success while the old conversation or provider runs on. */
export function wakeStep(status: Status, terminated: boolean, replace = false): WakeStep {
  if (terminated || status === "terminated") return "launch";
  switch (status) {
    case "running":
    case "attaching":
    case "detached-alive":
    case "recovering":
      return replace ? "retry" : "awake";
    case "suspended":
      return replace ? "restart" : "resume";
    case "daemon-down":
      return "unavailable";
    default:
      return "spawn";
  }
}

/** Refusal for a {@link WakeStep} `retry`: the pane's session is live or settling. */
export function wakeRetryReason(worktree: string, status: Status): string {
  const what =
    status === "detached-alive" ? "reattaching" : status === "running" ? "already running" : status;
  return `@${worktree} is ${what}; retry the wake`;
}

/** The pane restart kind a driver's `drive://restart` maps to. */
export function driveRestartKind(launch: {
  mode: DriveRestartMode;
  replace: boolean;
}): RestartKind {
  if (launch.mode !== "wake") return "drive-reset";
  return launch.replace ? "drive-wake-replace" : "drive-wake";
}

/** Refusal: herdr is down, so a wake can't start anything. */
export const WAKE_UNAVAILABLE = "herdr is unreachable; the session can't be started";

/** The projects a window has open, as one stable string (sorted, newline-joined), so an
 *  effect can depend on the set rather than on the warm registry object. */
export function openProjectsKey(warm: WarmState): string {
  return Object.keys(warm.projects).sort().join("\n");
}

/** The project names back out of {@link openProjectsKey}. */
export function openProjectsFromKey(key: string): string[] {
  return key === "" ? [] : key.split("\n");
}

export interface DriveEventDeps {
  /** The live warm registry (read at event time, never a stale closure). */
  warm: () => WarmState;
  /** The live transfer map. */
  transfers: () => TransferMap;
  resolve: (
    project: string,
    requestId: number,
    outcome: DriveOutcome,
    reason?: string,
  ) => Promise<void>;
  /** Restart (`reset`) or wake the pane on behalf of `requestId`, resuming unless
   *  `fresh` (a `replace` wake must start a new session); the pane reports the spawn. */
  restart: (
    project: string,
    worktree: string,
    requestId: number,
    launch: { mode: DriveRestartMode; fresh: boolean; replace: boolean },
  ) => void;
  /** Run the transfer flow in driven mode; it reports its own outcome. */
  swap: (
    project: string,
    worktree: string,
    to: CliKind,
    drive: { requestId: number; driver: string },
  ) => void;
}

/** The `drive://restart` and `drive://swap` handlers over `deps`. */
export function createDriveEventHandlers(deps: DriveEventDeps): {
  onRestart: (e: DriveRestartEvent) => void;
  onSwap: (e: DriveSwapEvent) => void;
} {
  /** Refuse the request when it cannot run here; true when it may proceed. */
  const admit = (e: DriveRequestEvent): boolean => {
    const reason = drivePrecheck(deps.warm(), deps.transfers(), e.project, e.worktree);
    if (reason === null) return true;
    // A rejected resolve means the runner already gave up on the request.
    void deps.resolve(e.project, e.request_id, "refused", reason).catch(() => {});
    return false;
  };
  return {
    onRestart: (e) => {
      if (!admit(e)) return;
      deps.restart(e.project, e.worktree, e.request_id, {
        mode: e.mode,
        fresh: e.fresh,
        replace: e.replace,
      });
    },
    onSwap: (e) => {
      if (admit(e)) {
        deps.swap(e.project, e.worktree, e.to, { requestId: e.request_id, driver: e.driver });
      }
    },
  };
}
