// LaneHeader — plain-language status line + count chip +
// optional collapse toggle. Used by Lane.tsx for all lane kinds.

import type { ReactNode } from "react";
import type { Decision } from "../../lib/useDispatchTargets";
import type { Lane } from "./useBoardLanes";

// ── Status-line helpers ───────────────────────────────────────────────────────

/** Queue depth for a worktree: assigned, not-yet-claimed (pending or sent) tasks. */
function queuedCount(lane: Lane): number {
  return lane.tasks.filter((t) => t.status === "open" && t.assigned_to === lane.key).length;
}

/**
 * Plain-language status string for a lane header.
 *
 * - Unassigned → "unassigned · drag to a worktree"
 * - Done       → "completed"
 * - Worktree   → state-derived ("idle · ready", "busy · N queued", "waiting · N queued", "offline")
 */
export function laneStatusText(lane: Lane): string {
  if (lane.kind === "unassigned") return "unassigned · drag to a worktree";
  if (lane.kind === "done") return "completed";

  const state = lane.session?.state ?? "offline";
  const q = queuedCount(lane);
  switch (state) {
    case "idle":
      return q > 0 ? `idle · ${q} queued` : "idle · ready";
    case "busy":
      return q > 0 ? `busy · ${q} queued` : "busy";
    case "waiting":
      return q > 0 ? `waiting · ${q} queued` : "waiting";
    default:
      return q > 0 ? `offline · ${q} pending` : "offline";
  }
}

// ── Decision chip (shown while a card is being dragged) ──────────────────────

function DecisionChip({ decision }: { decision: Decision | undefined }) {
  switch (decision) {
    case "dispatch":
      return (
        <span className="rounded-full bg-accent/20 px-1.5 py-px text-[9.5px] text-accent-text">
          Dispatch ↵
        </span>
      );
    case "queue":
      return (
        <span className="rounded-full border border-accent/30 px-1.5 py-px text-[9.5px] text-accent-text/70">
          Pending
        </span>
      );
    case "release":
      return (
        <span className="rounded-full border border-line px-1.5 py-px text-[9.5px] text-tx-2">
          Release
        </span>
      );
    case "invalid":
      return (
        <span className="rounded-full border border-line px-1.5 py-px text-[9.5px] text-tx-3">
          —
        </span>
      );
    default:
      return null;
  }
}

// ── Component ─────────────────────────────────────────────────────────────────

export function LaneHeader({
  lane,
  dragging,
  decision,
  headerFlash,
  collapsed,
  onToggleCollapse,
  filterSlot,
  filteredCount,
}: {
  lane: Lane;
  /** True while any card is being dragged (shows decision chip). */
  dragging: boolean;
  /** Current decideLane result for this lane (undefined = no drag in progress). */
  decision?: Decision;
  /** One-shot header flash after a successful dispatch to this lane. */
  headerFlash: boolean;
  /** Whether this lane is currently collapsed to a chip-only view. */
  collapsed: boolean;
  /** Called when the user clicks the collapse toggle (Done + empty-offline lanes). */
  onToggleCollapse?: () => void;
  /** Optional control rendered just before the count chip (the Open lane's tag filter). */
  filterSlot?: ReactNode;
  /** When set, the count chip shows this instead of `lane.count` (filtered view). */
  filteredCount?: number;
}) {
  const statusText = laneStatusText(lane);
  const canCollapse =
    lane.kind === "done" || (lane.isEmptyOffline && onToggleCollapse !== undefined);

  return (
    <div
      className={`flex items-center gap-2 rounded-chip px-1 pb-2 pt-1 transition-colors ${
        headerFlash ? "bg-accent-dim" : ""
      }`}
    >
      {/* Label + status line */}
      <div className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-[12px] font-semibold leading-tight text-tx-1">
          {lane.label}
        </span>
        {!collapsed && (
          <span className="truncate text-[10px] leading-tight text-tx-3">{statusText}</span>
        )}
      </div>

      {/* Decision chip (while dragging) */}
      {dragging && decision !== undefined && <DecisionChip decision={decision} />}

      {/* Optional filter control (Open lane) */}
      {filterSlot}

      {/* Count chip (right-aligned) — filtered count when a filter is active. */}
      <span className="rounded-full border border-line bg-bg-3 px-2 py-px text-[10px] text-tx-3">
        {filteredCount ?? lane.count}
      </span>

      {/* Collapse toggle for Done and empty-offline lanes */}
      {canCollapse && onToggleCollapse && (
        <button
          onClick={onToggleCollapse}
          className="flex h-5 w-5 flex-none items-center justify-center rounded text-tx-3 hover:text-tx-1 transition-colors"
          aria-label={collapsed ? "Expand lane" : "Collapse lane"}
        >
          {collapsed ? "›" : "‹"}
        </button>
      )}
    </div>
  );
}
