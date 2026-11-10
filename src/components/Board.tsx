// The task board: assignment lanes (Open | per-worktree | Done).
// A wrapping LaneLayout (full-height Open left, worktree lanes flowing into rows,
// Done trailing + collapsible) inside a DndContext for drag-to-dispatch.
//
// Each worktree lane is that worktree's queue. A drop on a pool lane assigns the
// card at once (it shows pending there with why); the backend sends a lane's front
// pending card when the lane is free, and keeps doing so on its own as sessions go
// idle. Every outcome — sent, pending, refused, failed — is toasted; nothing snaps
// back silently. Dispatch/release run through the shared useDispatchActions hook.

import {
  DndContext,
  type DragEndEvent,
  DragOverlay,
  type DragStartEvent,
  PointerSensor,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { Plus } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { cardStates, sentIds } from "../lib/cardState";
import type { DispatchFailure, Reservation } from "../lib/dispatch";
import type { WorktreeSession } from "../lib/sessions";
import { backlogTask, errorMessage, newTask, reopenTask, type TaskWithDeps } from "../lib/tasks";
import { useDispatchActions } from "../lib/useDispatchActions";
import { judgeLane, type LaneContext, useDispatchTargets } from "../lib/useDispatchTargets";
import { useLocalStorageState } from "../lib/useLocalStorageState";
import type { CardEnv } from "./board/cardHelpers";
import { LaneLayout } from "./board/LaneLayout";
import { useBoardLanes } from "./board/useBoardLanes";
import { TaskFormDialog } from "./TaskFormDialog";
import { useToast } from "./Toast";

/** Pure: failures in `now` not in `seen` (keyed by task, lane and time), so the
 *  board toasts each failed send once — including the auto-send's. */
export function newFailures(seen: Set<string>, now: DispatchFailure[]): DispatchFailure[] {
  return now.filter((f) => !seen.has(`${f.task_id}:${f.worktree}:${f.at}`));
}

// ── Board ─────────────────────────────────────────────────────────────────────

export function Board({
  tasks,
  backlog,
  failures,
  worktreeNames,
  pool,
  sessions,
  actor,
  selectedId,
  onSelect,
  reload,
  registerOpenCardMenu,
  registerToggleBacklog,
  registerAutoDispatch,
  dispatched,
}: {
  tasks: TaskWithDeps[];
  /** Backlog tasks (the Open lane shows them when its backlog toggle is on). */
  backlog: TaskWithDeps[];
  /** Standing failed sends, from the snapshot. */
  failures: DispatchFailure[];
  worktreeNames: string[];
  /** Worktrees in the dispatch pool (may receive tasks). */
  pool: string[];
  sessions: Map<string, WorktreeSession>;
  actor: string | null;
  selectedId: number | null;
  onSelect: (id: number) => void;
  reload: () => Promise<void>;
  /** Hand App a stable callback that toggles the DispatchMenu for the selected
   *  card. The keymap's `board.dispatch` binding calls it. */
  registerOpenCardMenu?: (fn: () => void) => void;
  /** Hand App a stable callback that backlogs / reopens the selected card
   *  (`board.backlog`). */
  registerToggleBacklog?: (fn: () => void) => void;
  /** Hand App a stable callback that auto-dispatches the selected card
   *  (`board.autoDispatch`, Shift+D) — the card's Dispatch button. */
  registerAutoDispatch?: (fn: () => void) => void;
  /** Reservations already sent — cards in this set show `sent ✓`. */
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
  // The Open lane's backlog toggle, persisted the same way; default OFF.
  const [showBacklog, setShowBacklog] = useLocalStorageState("keitora.board.showBacklog", false);
  const { showToast } = useToast();

  const allTasks = useMemo(
    () => (showBacklog ? [...tasks, ...backlog] : tasks),
    [tasks, backlog, showBacklog],
  );
  const boardLanes = useBoardLanes(allTasks, worktreeNames, sessions, showBacklog);
  const byId = useMemo(
    () => new Map([...tasks, ...backlog].map((t) => [t.id, t])),
    [tasks, backlog],
  );
  const poolSet = useMemo(() => new Set(pool), [pool]);
  const states = useMemo(
    () => cardStates(tasks, backlog, worktreeNames, sessions, dispatched, failures, poolSet),
    [tasks, backlog, worktreeNames, sessions, dispatched, failures, poolSet],
  );
  const laneCtx = useMemo<LaneContext>(
    () => ({ sessions, tasks, sent: sentIds(dispatched) }),
    [sessions, tasks, dispatched],
  );

  // Lanes for the drop rules: { key, isWorktree, inPool }.
  const lanesForTargets = useMemo(
    () =>
      boardLanes.map((l) => ({
        key: l.key,
        isWorktree: l.kind === "worktree",
        inPool: l.kind === "worktree" ? poolSet.has(l.key) : undefined,
      })),
    [boardLanes, poolSet],
  );

  const draggedCard = activeId !== null ? (byId.get(activeId) ?? null) : null;
  const decisions = useDispatchTargets(draggedCard, lanesForTargets, laneCtx);

  // Pointer only. dnd-kit's KeyboardSensor started a drag on Enter/Space on a focused
  // card — including an Enter pressed in the card's "Dispatch to…" menu, which
  // reaches the card through the React tree despite the portal — and the drag then
  // sat mid-animation with no lane to move to. The keyboard path to dispatch is the
  // menu (`d`) and Shift+D, not keyboard dragging.
  const sensors = useSensors(useSensor(PointerSensor, { activationConstraint: { distance: 6 } }));

  // Run a mutation, surface any store error (inline + toast), then refetch
  // immediately (the tasks://changed watcher will also fire, but this keeps the UI
  // snappy).
  const act = useCallback(
    (fn: () => Promise<unknown>) => {
      setErr(null);
      void fn()
        .then(() => reload())
        .catch((e) => {
          setErr(errorMessage(e));
          showToast(errorMessage(e));
        });
    },
    [reload, showToast],
  );

  const {
    runDispatch: dispatchTask,
    runAutoDispatch: autoDispatchTask,
    runRelease,
  } = useDispatchActions(reload, showToast, setErr);
  const runDispatch = useCallback(
    (worktree: string, taskId: number) => {
      dispatchTask(worktree, taskId, (wt) => setFlyIn({ id: taskId, worktree: wt }));
    },
    [dispatchTask],
  );
  const runAutoDispatch = useCallback(
    (taskId: number) => {
      autoDispatchTask(taskId, (wt) => setFlyIn({ id: taskId, worktree: wt }));
    },
    [autoDispatchTask],
  );
  const runBacklog = useCallback(
    (taskId: number) => {
      act(() => backlogTask(taskId).then(() => showToast(`Moved #${taskId} to the backlog`)));
    },
    [act, showToast],
  );
  const runReopen = useCallback(
    (taskId: number) => {
      act(() => reopenTask(taskId).then(() => showToast(`Reopened #${taskId}`)));
    },
    [act, showToast],
  );

  // Toast each failed send once, including those the backend's auto-send hit while
  // nobody was dragging. Failures already standing when the board mounts are shown on
  // their cards, not toasted again.
  const seenFailures = useRef<Set<string> | null>(null);
  useEffect(() => {
    const key = (f: DispatchFailure) => `${f.task_id}:${f.worktree}:${f.at}`;
    if (seenFailures.current === null) {
      seenFailures.current = new Set(failures.map(key));
      return;
    }
    for (const f of newFailures(seenFailures.current, failures)) {
      seenFailures.current.add(key(f));
      showToast(`Send failed: #${f.task_id} → @${f.worktree} — ${f.reason}`);
    }
  }, [failures, showToast]);

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

  // Keyboard paths for the selected card. The callbacks are stable across renders —
  // they read the live selection from refs — so App can hold onto one reference.
  const selectedIdRef = useRef(selectedId);
  selectedIdRef.current = selectedId;
  const byIdRef = useRef(byId);
  byIdRef.current = byId;
  const openCardMenu = useCallback(() => {
    const id = selectedIdRef.current;
    if (id === null) return;
    setMenuTaskId((cur) => (cur === id ? null : id));
  }, []);
  const toggleBacklog = useCallback(() => {
    const id = selectedIdRef.current;
    if (id === null) return;
    const card = byIdRef.current.get(id);
    if (!card || card.status === "done" || card.status === "cancelled") return;
    if (card.status === "backlog") runReopen(id);
    else runBacklog(id);
  }, [runBacklog, runReopen]);
  useEffect(() => {
    registerOpenCardMenu?.(openCardMenu);
  }, [registerOpenCardMenu, openCardMenu]);
  useEffect(() => {
    registerToggleBacklog?.(toggleBacklog);
  }, [registerToggleBacklog, toggleBacklog]);
  const autoDispatchSelected = useCallback(() => {
    const id = selectedIdRef.current;
    if (id === null) return;
    const card = byIdRef.current.get(id);
    if (!card) return;
    setMenuTaskId(null);
    // The backend refuses what can't go (blocked, backlog, active, …) and the hook
    // toasts the outcome either way.
    runAutoDispatch(id);
  }, [runAutoDispatch]);
  useEffect(() => {
    registerAutoDispatch?.(autoDispatchSelected);
  }, [registerAutoDispatch, autoDispatchSelected]);

  const handleDragStart = (e: DragStartEvent) => {
    setActiveId(Number(e.active.id));
  };

  const handleDragEnd = (e: DragEndEvent) => {
    const id = Number(e.active.id);
    setActiveId(null);
    const overKey = e.over ? String(e.over.id) : null;
    if (overKey === null) return; // dropped outside every lane: a plain cancel
    const card = byId.get(id);
    if (!card) return;
    const lane = lanesForTargets.find((l) => l.key === overKey);
    if (!lane) return;
    const { decision, reason } = judgeLane(card, lane, laneCtx);
    if (decision === "dispatch" || decision === "queue") {
      runDispatch(overKey, id);
    } else if (decision === "release") {
      runRelease(id);
    } else if (reason) {
      // A refused drop says why instead of silently snapping back.
      showToast(`Can't drop #${id} on ${lane.isWorktree ? `@${lane.key}` : lane.key}: ${reason}`);
    }
  };

  const env: CardEnv = {
    states,
    pool: poolSet,
    laneCtx,
    sessions,
    actor,
    selectedId,
    worktreeNames,
    flyIn,
    menuTaskId,
    onMenuOpenChange: (taskId, open) => setMenuTaskId(open ? taskId : null),
    onSelect,
    onAction: act,
    onDispatch: runDispatch,
    onAutoDispatch: runAutoDispatch,
    onRelease: runRelease,
    onBacklog: runBacklog,
    onReopen: runReopen,
  };

  const board = (
    <LaneLayout
      lanes={boardLanes}
      decisions={decisions}
      dragging={activeId !== null}
      env={env}
      showDone={showDone}
      backlog={{
        shown: showBacklog,
        count: backlog.length,
        onToggle: () => setShowBacklog(!showBacklog),
      }}
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
        onSubmit={async (v) => {
          await newTask(v.title, v.description, v.priority, v.tags);
          await reload();
        }}
      />
    </div>
  );
}
