// LaneLayout — three-region board layout.
//
// Open (unassigned) full-height left column | center flex-1 scrollable
// column of WorktreeRows | Done full-height right column (only when showDone).

import type { Decision } from "../../lib/useDispatchTargets";
import type { CardEnv } from "./cardHelpers";
import { Lane } from "./Lane";
import type { Lane as LaneData } from "./useBoardLanes";
import { WorktreeRow } from "./WorktreeRow";

interface LaneLayoutProps {
  lanes: LaneData[];
  decisions: Map<string, Decision>;
  dragging: boolean;
  env: CardEnv;
  /** When false (default), the Done column is not rendered. */
  showDone: boolean;
  /** The Open lane's backlog toggle. */
  backlog: { shown: boolean; count: number; onToggle: () => void };
}

export function LaneLayout({
  lanes,
  decisions,
  dragging,
  env,
  showDone,
  backlog,
}: LaneLayoutProps) {
  const flyIn = env.flyIn;
  const unassignedLane = lanes.find((l) => l.kind === "unassigned");
  const worktreeLanes = lanes.filter((l) => l.kind === "worktree");
  const doneLane = lanes.find((l) => l.kind === "done");

  // Shared props for Lane (side columns) and WorktreeRow.
  const sharedProps = { dragging, env };

  // Three full-height regions.
  return (
    <div className="flex min-h-0 flex-1 gap-3.5 overflow-hidden p-3.5">
      {/* Open (unassigned) — full-height left column */}
      {unassignedLane && (
        <div className="flex-none self-stretch">
          <Lane
            lane={unassignedLane}
            decision={decisions.get(unassignedLane.key)}
            headerFlash={flyIn?.worktree === unassignedLane.key}
            backlog={backlog}
            {...sharedProps}
          />
        </div>
      )}

      {/* Center: worktree rows in their own bounded, scrollable panel */}
      <div className="flex min-w-0 flex-1 flex-col overflow-y-auto rounded-xl border border-line">
        {worktreeLanes.map((lane) => (
          <WorktreeRow
            key={lane.key}
            lane={lane}
            decision={decisions.get(lane.key)}
            headerFlash={flyIn?.worktree === lane.key}
            {...sharedProps}
          />
        ))}
      </div>

      {/* Done — full-height right column, only when showDone */}
      {showDone && doneLane && (
        <div className="flex-none self-stretch">
          <Lane
            lane={doneLane}
            decision={decisions.get(doneLane.key)}
            headerFlash={false}
            {...sharedProps}
          />
        </div>
      )}
    </div>
  );
}
