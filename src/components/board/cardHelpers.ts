// Shared board helpers used by both the side-column Lane (Open/Done) and the
// center WorktreeRow, so the drag-over affordance and the per-card wiring stay
// defined in exactly one place.

import type { CardState } from "../../lib/cardState";
import type { WorktreeSession } from "../../lib/sessions";
import type { TaskWithDeps } from "../../lib/tasks";
import type { Decision, LaneContext } from "../../lib/useDispatchTargets";

/** Body tint + active-over emphasis for a lane's live drop decision. */
export function decisionClasses(decision: Decision | undefined, isOver: boolean): string {
  switch (decision) {
    case "dispatch":
      return isOver ? "border-accent bg-accent/[.18]" : "border-accent/60 bg-accent-dim";
    case "queue":
      return isOver
        ? "border-dashed border-accent/60 bg-bg-2"
        : "border-dashed border-accent/35 bg-bg-2";
    case "release":
      return isOver ? "border-dashed border-line bg-bg-2" : "border-dashed border-line";
    case "invalid":
      return "border-transparent opacity-90";
    default:
      return "border-transparent";
  }
}

/** Everything a card needs from the board, threaded through the lanes as one prop. */
export interface CardEnv {
  states: Map<number, CardState>;
  /** Worktrees in the dispatch pool. */
  pool: Set<string>;
  laneCtx: LaneContext;
  sessions: Map<string, WorktreeSession>;
  actor: string | null;
  selectedId: number | null;
  worktreeNames: string[];
  flyIn: { id: number; worktree: string } | null;
  menuTaskId: number | null;
  onMenuOpenChange: (taskId: number, open: boolean) => void;
  onSelect: (id: number) => void;
  onAction: (fn: () => Promise<unknown>) => void;
  onDispatch: (worktree: string, taskId: number) => void;
  onAutoDispatch: (taskId: number) => void;
  onRelease: (taskId: number) => void;
  onBacklog: (taskId: number) => void;
  onReopen: (taskId: number) => void;
}

/** A card's state, falling back to `open` for a card the board hasn't judged. */
export function stateOf(env: CardEnv, t: TaskWithDeps): CardState {
  return env.states.get(t.id) ?? { kind: "open" };
}
