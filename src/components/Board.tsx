// The task board: assignment lanes (Unassigned | per-worktree | Done).
// A wrapping LaneLayout (full-height Unassigned left, worktree lanes
// flowing into rows, Done trailing + collapsible) inside a DndContext for
// drag-to-dispatch. Dispatch/release run through the shared useDispatchActions hook.

import {
  DndContext,
  type DragEndEvent,
  DragOverlay,
  type DragStartEvent,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { Plus } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Reservation } from "../lib/dispatch";
import type { WorktreeSession } from "../lib/sessions";
import { newTask, type TaskWithDeps } from "../lib/tasks";
import { useDispatchActions } from "../lib/useDispatchActions";
import { decideLane, useDispatchTargets } from "../lib/useDispatchTargets";
import { useLocalStorageState } from "../lib/useLocalStorageState";
import { LaneLayout } from "./board/LaneLayout";
import { availableTags } from "./board/tagFilter";
import { useBoardLanes } from "./board/useBoardLanes";
import { TaskFormDialog } from "./TaskFormDialog";
import { useToast } from "./Toast";

const PRIORITY_RANK: Record<TaskWithDeps["priority"], number> = {
  urgent: 0,
  high: 1,
  normal: 2,
  low: 3,
};

/** Per worktree, the id of the *front* reserved (open+assigned) card — highest
 *  priority, then lowest id. Only that card flips to a live `Dispatch ↵` when its
 *  worktree is idle; the rest stay `queued`. */
function frontReservedByWorktree(lanes: ReturnType<typeof useBoardLanes>): Map<string, number> {
  const out = new Map<string, number>();
  for (const lane of lanes) {
    if (lane.kind !== "worktree") continue;
    const reserved = lane.tasks.filter((t) => t.status === "open" && t.assigned_to === lane.key);
    if (reserved.length === 0) continue;
    const front = reserved.reduce((best, t) => {
      const r = PRIORITY_RANK[t.priority] - PRIORITY_RANK[best.priority];
      if (r < 0) return t;
      if (r === 0 && t.id < best.id) return t;
      return best;
    });
    out.set(lane.key, front.id);
  }
  return out;
}

// ── Board ─────────────────────────────────────────────────────────────────────

export function Board({
  tasks,
  worktreeNames,
  sessions,
  actor,
  selectedId,
  onSelect,
  reload,
  registerOpenCardMenu,
  dispatched,
}: {
  tasks: TaskWithDeps[];
  worktreeNames: string[];
  sessions: Map<string, WorktreeSession>;
  actor: string | null;
  selectedId: number | null;
  onSelect: (id: number) => void;
  reload: () => Promise<void>;
  /** Hand App a stable callback that toggles the DispatchMenu for the selected
   *  card. The keymap's `board.dispatch` binding calls it. */
  registerOpenCardMenu?: (fn: () => void) => void;
  /** Reservations already injected — cards in this set show `sent ✓`. */
  dispatched: Reservation[];
}) {
  const [err, setErr] = useState<string | null>(null);
  const [newOpen, setNewOpen] = useState(false);
  const [activeId, setActiveId] = useState<number | null>(null);
  const [menuTaskId, setMenuTaskId] = useState<number | null>(null);
  // One-shot fly-in: the task that just dispatched and its target worktree.
  const [flyIn, setFlyIn] = useState<{ id: number; worktree: string } | null>(null);
  // Show-done toggle: persisted in localStorage (guarded helper); default OFF.
  const [showDone, setShowDone] = useLocalStorageState("keitora.board.showDone", false);
  const { showToast } = useToast();

  // Derive lanes via the extracted pure hook.
  const boardLanes = useBoardLanes(tasks, worktreeNames, sessions);
  const frontReserved = useMemo(() => frontReservedByWorktree(boardLanes), [boardLanes]);
  const byId = useMemo(() => new Map(tasks.map((t) => [t.id, t])), [tasks]);
  // Every tag in use on the project's tasks, suggested by the New task form.
  const projectTags = useMemo(() => availableTags(tasks), [tasks]);

  // Stable lookup: "taskId:worktree" → already dispatched.
  const dispatchedSet = useMemo(
    () => new Set(dispatched.map((r) => `${r.task_id}:${r.worktree}`)),
    [dispatched],
  );

  // Lanes for useDispatchTargets: { key, isWorktree }
  const lanesForTargets = useMemo(
    () => boardLanes.map((l) => ({ key: l.key, isWorktree: l.kind === "worktree" })),
    [boardLanes],
  );

  const draggedCard = activeId !== null ? (byId.get(activeId) ?? null) : null;
  const decisions = useDispatchTargets(draggedCard, lanesForTargets, sessions);

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor),
  );

  // Run a mutation, surface any store error, then refetch immediately (the
  // tasks://changed watcher will also fire, but this keeps the UI snappy).
  const act = useCallback(
    (fn: () => Promise<unknown>) => {
      setErr(null);
      void fn()
        .then(() => reload())
        .catch((e) => setErr(String(e)));
    },
    [reload],
  );

  // Apply a dispatch / release via the shared hook. The backend decides
  // inject-vs-queue; the fly-in animates the fresh-inject happy path inline and
  // a toast covers the deferred (queued) case.
  const { runDispatch: dispatchTask, runRelease } = useDispatchActions(reload, showToast, setErr);
  const runDispatch = useCallback(
    (worktree: string, taskId: number) => {
      setErr(null);
      dispatchTask(worktree, taskId, (wt) => setFlyIn({ id: taskId, worktree: wt }));
    },
    [dispatchTask],
  );

  // Clear the inline fly-in once the worktree's own Claude claims the task (it
  // flips open → active), or after a short window if it never does.
  useEffect(() => {
    if (!flyIn) return;
    const card = byId.get(flyIn.id);
    if (card && card.status !== "open") {
      setFlyIn(null);
      return;
    }
    const timer = setTimeout(() => setFlyIn(null), 4000);
    return () => clearTimeout(timer);
  }, [flyIn, byId]);

  // Toggle the "Dispatch to…" menu for the selected card (keyboard-first path,
  // bound to `d` in the central keymap). The callback is stable across renders —
  // it reads the live selection from a ref — so App can hold onto one reference.
  const selectedIdRef = useRef(selectedId);
  selectedIdRef.current = selectedId;
  const openCardMenu = useCallback(() => {
    const id = selectedIdRef.current;
    if (id === null) return;
    setMenuTaskId((cur) => (cur === id ? null : id));
  }, []);
  useEffect(() => {
    registerOpenCardMenu?.(openCardMenu);
  }, [registerOpenCardMenu, openCardMenu]);

  const handleDragStart = (e: DragStartEvent) => {
    setActiveId(Number(e.active.id));
  };

  const handleDragEnd = (e: DragEndEvent) => {
    const id = Number(e.active.id);
    setActiveId(null);
    const overKey = e.over ? String(e.over.id) : null;
    if (overKey === null) return;
    const card = byId.get(id);
    if (!card) return;
    const lane = lanesForTargets.find((l) => l.key === overKey);
    if (!lane) return;
    const decision = decideLane(card, lane, sessions);
    if (decision === "dispatch" || decision === "queue") {
      runDispatch(overKey, id);
    } else if (decision === "release") {
      runRelease(id);
    }
    // invalid → ignored; the card snaps back.
  };

  // The lane layout is identical on both surfaces; only the drag wrapper differs.
  const board = (
    <LaneLayout
      lanes={boardLanes}
      decisions={decisions}
      dragging={activeId !== null}
      flyIn={flyIn}
      sessions={sessions}
      actor={actor}
      selectedId={selectedId}
      worktreeNames={worktreeNames}
      dispatchedSet={dispatchedSet}
      frontReserved={frontReserved}
      menuTaskId={menuTaskId}
      onMenuOpenChange={(taskId, open) => setMenuTaskId(open ? taskId : null)}
      onSelect={onSelect}
      onAction={act}
      onDispatch={runDispatch}
      onRelease={runRelease}
      showDone={showDone}
    />
  );

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col bg-bg-0">
      <div className="flex items-center gap-3 border-b border-line px-3.5 py-2.5">
        <span className="text-[12px] font-semibold text-tx-1">Board</span>
        {err && <span className="truncate text-[11px] text-urgent">{err}</span>}
        {/* Show done toggle — persistence handled by useLocalStorageState. */}
        <label className="ml-auto flex cursor-pointer items-center gap-1.5 text-[11px] text-tx-2 select-none">
          <input
            type="checkbox"
            checked={showDone}
            onChange={(e) => setShowDone(e.target.checked)}
            className="accent-[var(--color-accent)] h-3.5 w-3.5 cursor-pointer rounded"
          />
          show done
        </label>
        <button
          onClick={() => setNewOpen(true)}
          className="flex items-center gap-1.5 rounded-chip border border-accent/40 bg-accent-dim px-2.5 py-1 text-[11.5px] text-accent-text"
        >
          <Plus size={13} /> New
        </button>
      </div>

      <DndContext
        sensors={sensors}
        onDragStart={handleDragStart}
        onDragEnd={handleDragEnd}
        onDragCancel={() => setActiveId(null)}
      >
        {board}

        <DragOverlay>
          {draggedCard && (
            <div className="rotate-[1.5deg] scale-[1.02] rounded-chip border border-accent/40 bg-bg-2 px-2.5 py-2.5 shadow-[0_8px_24px_rgba(0,0,0,.45)] ring-1 ring-accent/40">
              <div className="flex items-center gap-2 pl-2">
                <span className="font-mono text-[11px] text-tx-3">#{draggedCard.id}</span>
              </div>
              <div className="mt-1.5 pl-2 text-[12.5px] leading-snug text-tx-1">
                {draggedCard.title}
              </div>
            </div>
          )}
        </DragOverlay>
      </DndContext>

      <TaskFormDialog
        open={newOpen}
        onOpenChange={setNewOpen}
        mode="new"
        tagOptions={projectTags}
        onSubmit={async (v) => {
          await newTask(v.title, v.description, v.priority, v.tags);
          await reload();
        }}
      />
    </div>
  );
}
