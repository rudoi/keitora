// Copy for switching a worktree to another agent. A switch starts the new agent fresh: the
// conversation is not carried over, so the user is told before anything happens. Pure.

import type { CliKind } from "./ipc";
import { cliTitle } from "./providers";
import { midTurnNote } from "./transferPhase";

/** The confirm dialog shown before switching a worktree from `from` to `to`. `midTurn`: the
 *  `from` agent is mid-turn, so the body also warns that the in-flight turn is lost. */
export function switchConfirmCopy(
  from: CliKind,
  to: CliKind,
  midTurn = false,
): { title: string; body: string; confirmLabel: string } {
  const target = cliTitle(to);
  return {
    title: `Switch to ${target}?`,
    body:
      `${target} will start a new conversation. The current ${cliTitle(from)} conversation ` +
      `won't be carried over; it stays on disk, and switching back later starts fresh too.` +
      (midTurn ? `\n\n${midTurnNote(from)}` : ""),
    confirmLabel: `Switch to ${target}`,
  };
}
