// The rail's list of project groups, in registry order: one ProjectGroup per GroupModel,
// reorderable by drag (pointer on the header, keyboard from the grip). A drag never moves
// or collapses anything: the groups stay where they are, a lifted copy of the header
// follows the pointer, and a line marks the gap it will drop into (worked out from the
// copy's position against each group's header, see lib/railDrop). A drop shows its new
// order at once and persists it through `onReorder`; the shown order holds until the
// registry reports it (or reverts, with a toast, when the save fails).

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
import { applyOrder, moveToSlot, settleOverride } from "../../lib/railOrder";
import { useHomeDir, useMinuteNow } from "../../lib/useRailClock";
import { useToast } from "../Toast";
import type { LaneOpenTarget } from "./Lane";
import { ProjectGroup, ProjectGroupOverlay } from "./ProjectGroup";

/**
 * The group being dragged: its key, its index in the current order, the slot it would drop
 * into, and whether the keyboard is driving it.
 */
interface DragState {
  key: string;
  from: number;
  slot: number;
  keyboard: boolean;
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
  onOpenLane,
  onLaneContextMenu,
  onNewWorktree,
  onOpenMenu,
  onReorder,
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
  onOpenLane: (g: GroupModel, worktree: string, target: LaneOpenTarget) => void;
  onLaneContextMenu: (g: GroupModel, worktree: string, x: number, y: number) => void;
  onNewWorktree: (g: GroupModel) => void;
  onOpenMenu: (g: GroupModel, x: number, y: number, from: HTMLElement | null) => void;
  onReorder: (keys: string[]) => Promise<void>;
}) {
  const { showToast } = useToast();
  const home = useHomeDir();
  const now = useMinuteNow();
  // A dropped order not yet reported back by the registry.
  const [override, setOverride] = useState<readonly string[] | null>(null);
  // The key of the group being dragged, and whether the keyboard is driving it (the copy
  // is then hidden: the arrows move the line, and the dimmed group shows what moves).
  const [dragKey, setDragKey] = useState<string | null>(null);
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

  const shown = useMemo(() => applyOrder(groups, override, (g) => g.key), [groups, override]);
  const keys = useMemo(() => shown.map((g) => g.key), [shown]);
  // The current order, for the drag callbacks: the list can change under a running drag.
  const keysRef = useRef(keys);
  keysRef.current = keys;
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
    const from = keysRef.current.indexOf(d.key);
    if (from !== -1) d.from = from;
    if (!d.keyboard) {
      const y = overlayCentre();
      if (y === null) return;
      d.slot = insertionIndex(m.heads, y);
    }
    const top = isNoopSlot(d.slot, d.from) ? null : lineOffset(m.cards, d.slot, m.listTop);
    setLineTop(
      top === null
        ? null
        : Math.min(Math.max(top, m.view.top - m.listTop + 2), m.view.bottom - m.listTop - 2),
    );
  }, [measure, overlayCentre]);

  // The list also moves under a still pointer when the rail auto-scrolls.
  useEffect(() => {
    if (dragKey === null) return;
    document.addEventListener("scroll", track, { capture: true, passive: true });
    return () => document.removeEventListener("scroll", track, { capture: true });
  }, [dragKey, track]);

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
  const announcements = useMemo<Announcements>(() => {
    const n = () => keysRef.current.length;
    return {
      onDragStart: ({ active }) => {
        spoken.current = null;
        return `Picked up ${active.id}, position ${keysRef.current.indexOf(String(active.id)) + 1} of ${n()}.`;
      },
      onDragMove: ({ active }) => {
        const d = drag.current;
        if (!d || spoken.current === landing(d)) return undefined;
        spoken.current = landing(d);
        return `${active.id} will move to position ${landing(d)} of ${n()}.`;
      },
      // No droppables, so there is never an `over` to speak; onDragMove says where it lands.
      onDragOver: () => undefined,
      onDragEnd: ({ active }) => {
        const d = drag.current;
        return d ? `${active.id} dropped at position ${landing(d)} of ${n()}.` : undefined;
      },
      onDragCancel: ({ active }) => `Reorder cancelled. ${active.id} is back where it was.`,
    };
  }, []);

  const onDragStart = (e: DragStartEvent) => {
    const key = String(e.active.id);
    const from = keys.indexOf(key);
    const keyboard = e.activatorEvent instanceof KeyboardEvent;
    drag.current = { key, from, slot: from, keyboard };
    setByKeyboard(keyboard);
    setDragKey(key);
  };
  const stop = () => {
    setDragKey(null);
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
      <div ref={listRef} className={dragKey === null ? "rail-projects" : "rail-projects dragging"}>
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
            onOpenLane={onOpenLane}
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
      </DragOverlay>
    </DndContext>
  );
}
