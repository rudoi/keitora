// The rail's list of project groups, in registry order: one ProjectGroup per GroupModel,
// reorderable by drag (pointer on the header, keyboard from the grip). A drag never moves
// or collapses anything: the groups stay where they are, a lifted copy of the header
// follows the pointer, and a line marks the gap it will drop into (worked out from the
// copy's position against each group's header, see lib/railDrop). A drop shows its new
// order at once and persists it through `onReorder`; the shown order holds until the
// registry reports it (or reverts, with a toast, when the save fails).
//
// Worktree lanes reorder the same way within their own group, by mouse only: the line
// marks a gap between that group's lanes, and the dropped order shows at once as a
// per-group override until the group's reported lanes catch up.

import {
  type Announcements,
  DndContext,
  DragOverlay,
  type DragStartEvent,
  type KeyboardCoordinateGetter,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  insertionIndex,
  isNoopSlot,
  landingIndex,
  lineOffset,
  slotTargetY,
  stepSlot,
  type VSpan,
} from "../../lib/railDrop";
import type { GroupModel } from "../../lib/railGroups";
import {
  applyLaneOrders,
  applyOrder,
  type LaneOverrides,
  laneNamesByGroup,
  moveToSlot,
  settleLaneOverrides,
  settleOverride,
} from "../../lib/railOrder";
import { useHomeDir, useMinuteNow } from "../../lib/useRailClock";
import { useToast } from "../Toast";
import { type LaneDragData, LaneOverlay } from "./Lane";
import { ProjectGroup, ProjectGroupOverlay } from "./ProjectGroup";

/**
 * The item being dragged: a project group (`group` null) or a worktree lane of the group
 * `group`. `key` is the group's registry key or the lane's worktree name; `from` its index
 * in the current order (of groups, or of that group's lanes); `slot` the slot it would
 * drop into; and whether the keyboard is driving it (groups only).
 */
interface DragState {
  key: string;
  group: string | null;
  from: number;
  slot: number;
  keyboard: boolean;
}

function laneData(data: unknown): LaneDragData | null {
  return (data as { kind?: string } | undefined)?.kind === "lane" ? (data as LaneDragData) : null;
}

const span = (r: DOMRect): VSpan => ({ top: r.top, bottom: r.bottom });

/** 1-based position the dragged group ends up at if dropped now. */
const landing = (d: DragState) => landingIndex(d.slot, d.from) + 1;

export function RailProjects({
  groups,
  expanded,
  menuKey,
  selected,
  hintVisible,
  reduceMotion,
  onToggle,
  onSelectLane,
  onOpenDiff,
  onLaneContextMenu,
  onNewWorktree,
  onOpenMenu,
  onReorder,
  onReorderWorktrees,
}: {
  groups: GroupModel[];
  /** Registry keys of the open groups. */
  expanded: ReadonlySet<string>;
  /** Registry key of the project whose menu is open. */
  menuKey: string | null;
  selected: string | null;
  hintVisible: boolean;
  reduceMotion: boolean;
  onToggle: (key: string) => void;
  onSelectLane: (g: GroupModel, worktree: string) => void;
  onOpenDiff: (g: GroupModel, worktree: string) => void;
  onLaneContextMenu: (g: GroupModel, worktree: string, x: number, y: number) => void;
  onNewWorktree: (g: GroupModel) => void;
  onOpenMenu: (g: GroupModel, x: number, y: number, from: HTMLElement | null) => void;
  onReorder: (keys: string[]) => Promise<void>;
  onReorderWorktrees: (project: string, order: string[]) => Promise<void>;
}) {
  const { showToast } = useToast();
  const home = useHomeDir();
  const now = useMinuteNow();
  // A dropped order not yet reported back by the registry.
  const [override, setOverride] = useState<readonly string[] | null>(null);
  // Dropped worktree orders not yet reported back, by group key.
  const [laneOverrides, setLaneOverrides] = useState<LaneOverrides>(() => new Map());
  // The key of the group being dragged, and whether the keyboard is driving it (the copy
  // is then hidden: the arrows move the line, and the dimmed group shows what moves).
  const [dragKey, setDragKey] = useState<string | null>(null);
  // The worktree lane being dragged, for its lifted copy.
  const [dragLane, setDragLane] = useState<LaneDragData | null>(null);
  const [byKeyboard, setByKeyboard] = useState(false);
  // The drop line's offset from the top of the list; null hides it.
  const [lineTop, setLineTop] = useState<number | null>(null);
  // The live drag, read by tracking, the keyboard getter and announcements. It outlives
  // the drop so the drop's announcement can still read it.
  const drag = useRef<DragState | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const overlayRef = useRef<HTMLDivElement>(null);

  const incomingSig = groups.map((g) => g.key).join("\n");
  useEffect(() => {
    const incoming = incomingSig === "" ? [] : incomingSig.split("\n");
    setOverride((cur) => settleOverride(cur, incoming));
  }, [incomingSig]);

  // Settle the worktree overrides against each group's reported lanes; with none pending
  // there is nothing to compare.
  useEffect(() => {
    setLaneOverrides((cur) =>
      cur.size === 0 ? cur : settleLaneOverrides(cur, laneNamesByGroup(groups)),
    );
  }, [groups]);

  const shown = useMemo(
    () =>
      applyLaneOrders(
        applyOrder(groups, override, (g) => g.key),
        laneOverrides,
      ),
    [groups, override, laneOverrides],
  );
  const keys = useMemo(() => shown.map((g) => g.key), [shown]);
  // The current order, for the drag callbacks: the list can change under a running drag.
  const keysRef = useRef(keys);
  keysRef.current = keys;
  // The shown groups, for the same reason; their lane order is read only during a drag.
  const shownRef = useRef(shown);
  shownRef.current = shown;
  const laneNames = useCallback(
    (group: string): string[] =>
      shownRef.current.find((g) => g.key === group)?.lanes.map((l) => l.name) ?? [],
    [],
  );
  // The order a drag reorders: the groups, or the lanes of the dragged lane's group.
  const orderOf = useCallback(
    (d: DragState): readonly string[] => (d.group === null ? keysRef.current : laneNames(d.group)),
    [laneNames],
  );
  const dragGroup = dragKey === null ? null : (shown.find((g) => g.key === dragKey) ?? null);

  // Viewport rects of every group card and header in list order, the list's top, and the
  // visible part of the rail's scroll area.
  const measure = useCallback(() => {
    const list = listRef.current;
    if (!list) return null;
    const cards: VSpan[] = [];
    const heads: VSpan[] = [];
    for (const g of list.querySelectorAll<HTMLElement>(":scope > .rail-group")) {
      const head = g.querySelector(".rail-group-head");
      if (!head) continue;
      cards.push(span(g.getBoundingClientRect()));
      heads.push(span(head.getBoundingClientRect()));
    }
    const scroller = list.closest(".rail-lanes") ?? list;
    return {
      cards,
      heads,
      listTop: list.getBoundingClientRect().top,
      view: span(scroller.getBoundingClientRect()),
    };
  }, []);

  // Viewport rects of the lanes of the group `group`, in order.
  const measureLanes = useCallback((group: string): VSpan[] => {
    const list = listRef.current;
    if (!list) return [];
    const rows = list.querySelectorAll<HTMLElement>(
      `:scope > .rail-group[data-group="${CSS.escape(group)}"] .rail-group-lanes > .lane-row`,
    );
    return [...rows].map((r) => span(r.getBoundingClientRect()));
  }, []);

  // The lifted copy's vertical centre on screen. Read from the DOM rather than derived
  // from the drag delta, so it stays right while the rail auto-scrolls under it.
  const overlayCentre = useCallback(() => {
    const r = overlayRef.current?.getBoundingClientRect();
    return r ? (r.top + r.bottom) / 2 : null;
  }, []);

  // Bring the drag up to date and redraw the line. A pointer drag re-derives its slot from
  // where the copy is; a keyboard drag's slot is whatever the arrows chose. The line is
  // kept inside the visible rail, so a gap below a tall open group still shows.
  const track = useCallback(() => {
    const d = drag.current;
    const m = measure();
    if (!d || !m) return;
    const from = orderOf(d).indexOf(d.key);
    if (from !== -1) d.from = from;
    // A lane's slot and line are among its own group's lanes.
    const rows = d.group === null ? null : measureLanes(d.group);
    // With the group's lanes not in the DOM, keep the last slot and line.
    if (rows !== null && rows.length === 0) return;
    if (!d.keyboard) {
      const y = overlayCentre();
      if (y === null) return;
      d.slot = insertionIndex(rows ?? m.heads, y);
    }
    const top = isNoopSlot(d.slot, d.from) ? null : lineOffset(rows ?? m.cards, d.slot, m.listTop);
    setLineTop(
      top === null
        ? null
        : Math.min(Math.max(top, m.view.top - m.listTop + 2), m.view.bottom - m.listTop - 2),
    );
  }, [measure, measureLanes, orderOf, overlayCentre]);

  // The list also moves under a still pointer when the rail auto-scrolls.
  const dragging = dragKey !== null || dragLane !== null;
  useEffect(() => {
    if (!dragging) return;
    document.addEventListener("scroll", track, { capture: true, passive: true });
    return () => document.removeEventListener("scroll", track, { capture: true });
  }, [dragging, track]);

  // Arrow keys step the slot to the next one that changes the order. The (hidden) copy is
  // moved there too, which is what makes dnd-kit report the move and speak it.
  const keyboardCoordinates = useCallback<KeyboardCoordinateGetter>(
    (event, { currentCoordinates }) => {
      const dir = event.code === "ArrowDown" ? 1 : event.code === "ArrowUp" ? -1 : 0;
      const d = drag.current;
      const m = measure();
      const y = overlayCentre();
      if (dir === 0 || !d || !m || y === null) return undefined;
      event.preventDefault();
      const from = keysRef.current.indexOf(d.key);
      if (from !== -1) d.from = from;
      const slot = stepSlot(d.slot, dir, d.from, m.heads.length);
      const target = slotTargetY(m.heads, slot);
      if (slot === d.slot || target === null) return undefined;
      d.slot = slot;
      return { x: currentCoordinates.x, y: currentCoordinates.y + (target - y) };
    },
    [measure, overlayCentre],
  );

  const sensors = useSensors(
    // The threshold keeps a click on the header a toggle.
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: keyboardCoordinates }),
  );

  // Spoken updates by project name and position; a move is spoken once per new slot. The
  // drag is tracked in onDragMove before dnd-kit speaks, so the slot is current.
  const spoken = useRef<number | null>(null);
  // A lane is spoken by its worktree name and its position among its group's lanes.
  const announcements = useMemo<Announcements>(() => {
    const nameOf = (active: { id: unknown; data: { current?: unknown } }) =>
      laneData(active.data.current)?.worktree ?? String(active.id);
    const n = () => {
      const d = drag.current;
      return d ? orderOf(d).length : keysRef.current.length;
    };
    return {
      onDragStart: ({ active }) => {
        spoken.current = null;
        const d = drag.current;
        const position = d ? d.from + 1 : keysRef.current.indexOf(String(active.id)) + 1;
        return `Picked up ${nameOf(active)}, position ${position} of ${n()}.`;
      },
      onDragMove: ({ active }) => {
        const d = drag.current;
        if (!d || spoken.current === landing(d)) return undefined;
        spoken.current = landing(d);
        return `${nameOf(active)} will move to position ${landing(d)} of ${n()}.`;
      },
      // No droppables, so there is never an `over` to speak; onDragMove says where it lands.
      onDragOver: () => undefined,
      onDragEnd: ({ active }) => {
        const d = drag.current;
        return d ? `${nameOf(active)} dropped at position ${landing(d)} of ${n()}.` : undefined;
      },
      onDragCancel: ({ active }) => `Reorder cancelled. ${nameOf(active)} is back where it was.`,
    };
  }, [orderOf]);

  const onDragStart = (e: DragStartEvent) => {
    const lane = laneData(e.active.data.current);
    if (lane) {
      const from = laneNames(lane.group).indexOf(lane.worktree);
      drag.current = { key: lane.worktree, group: lane.group, from, slot: from, keyboard: false };
      setByKeyboard(false);
      setDragLane(lane);
      return;
    }
    const key = String(e.active.id);
    const from = keys.indexOf(key);
    const keyboard = e.activatorEvent instanceof KeyboardEvent;
    drag.current = { key, group: null, from, slot: from, keyboard };
    setByKeyboard(keyboard);
    setDragKey(key);
  };
  const stop = () => {
    setDragKey(null);
    setDragLane(null);
    setLineTop(null);
  };
  const onDragCancel = () => {
    if (drag.current) drag.current.slot = drag.current.from;
    stop();
  };
  const onDragEnd = () => {
    // One last read, so a pointer drop lands where the line was at that moment.
    track();
    const d = drag.current;
    stop();
    if (!d) return;
    if (d.group !== null) {
      const group = d.group;
      const next = moveToSlot(orderOf(d), d.key, d.slot);
      if (next === null) return;
      setLaneOverrides((cur) => new Map(cur).set(group, next));
      onReorderWorktrees(group, next).catch((err: unknown) => {
        // Revert only if no later drop in this group has replaced this one.
        setLaneOverrides((cur) => {
          if (cur.get(group) !== next) return cur;
          const out = new Map(cur);
          out.delete(group);
          return out;
        });
        showToast(String(err));
      });
      return;
    }
    const next = moveToSlot(keys, d.key, d.slot);
    if (next === null) return;
    setOverride(next);
    onReorder(next).catch((err: unknown) => {
      // Revert only if no later drop has replaced this one.
      setOverride((cur) => (cur === next ? null : cur));
      showToast(String(err));
    });
  };

  return (
    <DndContext
      sensors={sensors}
      accessibility={{ announcements }}
      onDragStart={onDragStart}
      onDragMove={track}
      onDragEnd={onDragEnd}
      onDragCancel={onDragCancel}
    >
      <div ref={listRef} className={dragging ? "rail-projects dragging" : "rail-projects"}>
        {shown.map((g) => (
          <ProjectGroup
            key={g.key}
            group={g}
            open={expanded.has(g.key)}
            menuOpen={g.key === menuKey}
            selected={g.isActive ? selected : null}
            hintVisible={hintVisible}
            reduceMotion={reduceMotion}
            home={home}
            now={now}
            onToggle={onToggle}
            onSelectLane={onSelectLane}
            onOpenDiff={onOpenDiff}
            onLaneContextMenu={onLaneContextMenu}
            onNewWorktree={onNewWorktree}
            onOpenMenu={onOpenMenu}
          />
        ))}
        {lineTop !== null && (
          <div className="rail-drop-line" style={{ top: lineTop }} aria-hidden="true" />
        )}
      </div>
      {/* No drop animation: the group appears in its new place as the copy goes. */}
      <DragOverlay dropAnimation={null}>
        {dragGroup && (
          <ProjectGroupOverlay
            ref={overlayRef}
            group={dragGroup}
            hidden={byKeyboard}
            reduceMotion={reduceMotion}
          />
        )}
        {dragLane && <LaneOverlay ref={overlayRef} name={dragLane.worktree} />}
      </DragOverlay>
    </DndContext>
  );
}
