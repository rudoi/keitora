// Shared dispatch/release actions, lifted out of Board. `runDispatch` assigns a card
// to a worktree lane (the backend sends it now if the lane is free, else it waits
// there as pending); `runAutoDispatch` lets the backend pick the worktree from the
// dispatch pool; `runRelease` force-unclaims a reserved task. Every outcome is
// toasted — a pending card says why it waits, a failure says what failed — and the
// board refetches, so nothing silently snaps back.

import { useCallback, useRef } from "react";
import { type DispatchOutcome, dispatchApply, dispatchAuto, releaseTask } from "./dispatch";
import { errorMessage } from "./tasks";

/** What a dispatch outcome means for the UI. Pure + exported so the branching is
 *  unit-testable without a React renderer. */
export type DispatchEffect =
  | { kind: "injected"; toast: string } // fresh send — caller may animate the fly-in
  | { kind: "pending"; toast: string } // assigned; waits on its lane
  | { kind: "failed"; toast: string } // assigned, but the send failed
  | { kind: "noop"; toast: string }; // already sent there

export function classifyDispatch(outcome: DispatchOutcome, taskId: number): DispatchEffect {
  const wt = outcome.worktree;
  const why = outcome.reason ? ` — ${outcome.reason.replace(/^pending: /, "")}` : "";
  switch (outcome.state) {
    case "sent":
      return { kind: "injected", toast: `Sent #${taskId} to @${wt}` };
    case "already_sent":
      return { kind: "noop", toast: `#${taskId} was already sent to @${wt}` };
    case "failed":
      return { kind: "failed", toast: `Send failed: #${taskId} → @${wt}${why}` };
    default:
      return { kind: "pending", toast: `Assigned #${taskId} to @${wt} (pending${why})` };
  }
}

export interface DispatchActions {
  /** Assign + maybe send. `onDispatched(worktree)` fires only on a fresh send —
   *  Board uses it for the fly-in. */
  runDispatch: (
    worktree: string,
    taskId: number,
    onDispatched?: (worktree: string) => void,
  ) => void;
  /** One-click Dispatch: the backend picks the worktree. */
  runAutoDispatch: (taskId: number, onDispatched?: (worktree: string) => void) => void;
  runRelease: (taskId: number) => void;
}

export function useDispatchActions(
  reload: () => Promise<void>,
  showToast: (msg: string) => void,
  /** Inline-error setter; called with `null` at the start of each action to
   *  clear a stale error, and with the message on failure. */
  setError?: (msg: string | null) => void,
): DispatchActions {
  // Task ids with a dispatch in flight. A ref (not state) so the re-entrancy check
  // is synchronous: a rapid double-click is rejected before it can issue a second
  // IPC call.
  const pendingDispatch = useRef<Set<number>>(new Set());

  const run = useCallback(
    (
      taskId: number,
      call: () => Promise<DispatchOutcome>,
      onDispatched?: (worktree: string) => void,
    ) => {
      if (pendingDispatch.current.has(taskId)) return;
      pendingDispatch.current.add(taskId);
      setError?.(null);
      void call()
        .then((outcome) => {
          const effect = classifyDispatch(outcome, taskId);
          if (effect.kind === "injected") onDispatched?.(outcome.worktree);
          if (effect.kind === "failed") setError?.(effect.toast);
          showToast(effect.toast);
          return reload();
        })
        .catch((e) => {
          // A refusal (excluded worktree, blocked, backlog, …): nothing changed.
          const msg = `Dispatch refused: ${errorMessage(e)}`;
          showToast(msg);
          setError?.(msg);
          return reload();
        })
        .finally(() => {
          pendingDispatch.current.delete(taskId);
        });
    },
    [reload, showToast, setError],
  );

  const runDispatch = useCallback(
    (worktree: string, taskId: number, onDispatched?: (worktree: string) => void) =>
      run(taskId, () => dispatchApply(worktree, taskId), onDispatched),
    [run],
  );

  const runAutoDispatch = useCallback(
    (taskId: number, onDispatched?: (worktree: string) => void) =>
      run(taskId, () => dispatchAuto(taskId), onDispatched),
    [run],
  );

  const runRelease = useCallback(
    (taskId: number) => {
      setError?.(null);
      // `releaseTask` calls the `task_release` command, which rejects with a
      // structured IpcError — unwrap it (a raw String(e) would show "[object
      // Object]").
      void releaseTask(taskId)
        .then(() => reload())
        .catch((e) => {
          showToast(errorMessage(e));
          setError?.(errorMessage(e));
        });
    },
    [reload, showToast, setError],
  );

  return { runDispatch, runAutoDispatch, runRelease };
}
