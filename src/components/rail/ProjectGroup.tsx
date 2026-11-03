// One project in the rail: a sticky header that toggles the group open/closed, and a
// collapsible body with the project's worktree lanes. Collapsed, the header still
// summarises the lanes (a dot per worktree, a needs-input ring, a work dot, the live
// session count). The active project's header keeps a "selected" look whether or not it
// is open. Opening a group is only a peek; a lane click is what switches project.
//
// Groups are draggable: the whole header is the pointer drag handle (past a small
// threshold, so a click still toggles), and the grip button is the keyboard one (Space
// picks up, arrows move, Space drops, Escape cancels). While a drag runs, the group stays
// in place, dimmed, and a lifted copy of its header (ProjectGroupOverlay) follows the
// pointer; RailProjects draws the line where it will land. Worktree lanes inside an open
// group drag the same way, by mouse only, within their group.

import { useDraggable } from "@dnd-kit/core";
import { ChevronRight, Ellipsis, GripVertical, Plus } from "lucide-react";
import {
  type CSSProperties,
  memo,
  type Ref,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import { shortPath, tailPath } from "../../lib/format";
import { projectHue, projectMonogram } from "../../lib/projectHue";
import type { GroupModel } from "../../lib/railGroups";
import {
  groupAriaLabel,
  headerView,
  laneHintBadges,
  lastActivityLabel,
  sentinelAbove,
} from "../../lib/railGroupView";
import { Lane } from "./Lane";

function groupClass(group: GroupModel, open: boolean): string {
  return [
    "rail-group",
    open ? "open" : "",
    group.isActive ? "active" : "",
    group.summaryStats.waiting > 0 ? "has-waiting" : "",
  ]
    .filter(Boolean)
    .join(" ");
}

/** The project's identity hue, for the card, monogram tile and lane spine. */
function hueStyle(key: string): CSSProperties {
  return { "--proj-h": projectHue(key) } as CSSProperties;
}

/**
 * The header's content: the monogram tile (which turns into the chevron on hover or
 * focus) beside up to three rows: the name with its needs-input ring, then the work dot
 * and live count at the far end; the subtitle (the project's path and last activity); and,
 * collapsed, the dot strip. Shared by the header's toggle and the drag overlay, which
 * passes no subtitle (and whose CSS drops the dot row), so it is a one-row chip.
 */
function HeaderSummary({
  group,
  open,
  reduceMotion,
  subtitle,
}: {
  group: GroupModel;
  open: boolean;
  reduceMotion: boolean;
  subtitle: { path: string; ago: string | null } | null;
}) {
  const { key, summaryStats: stats } = group;
  const view = headerView(stats, open);
  const workCls = ["lane-work-dot", view.work ?? "off", view.work && !reduceMotion ? "live" : ""]
    .filter(Boolean)
    .join(" ");
  return (
    <>
      <span className="rail-group-tile" aria-hidden="true">
        <span className="rail-group-mono">{projectMonogram(key)}</span>
        <ChevronRight className="rail-group-chev" size={13} strokeWidth={2.5} />
      </span>
      <span className="rail-group-text">
        <span className="rail-group-title">
          <span className="rail-group-name" title={key}>
            {key}
          </span>
          {view.waiting && (
            <span
              className={reduceMotion ? "rail-group-wait" : "rail-group-wait pulse"}
              title="needs input"
              aria-hidden="true"
            />
          )}
          <span className="rail-group-meta" aria-hidden="true">
            <span
              className={workCls}
              title={
                view.work === null ? undefined : view.work === "working" ? "building" : "agent busy"
              }
            />
            <span
              className="rail-group-count"
              title={
                view.count ? `${view.count} live session${stats.live === 1 ? "" : "s"}` : undefined
              }
            >
              {view.count}
            </span>
          </span>
        </span>
        {subtitle && (
          <span className="rail-group-sub">
            <span className="rail-group-path" title={group.summary.root}>
              <bdi>{subtitle.path}</bdi>
            </span>
            {subtitle.ago && (
              <span
                className={subtitle.ago === "active" ? "rail-group-ago active" : "rail-group-ago"}
                title={subtitle.ago === "active" ? "working now" : "last activity"}
              >
                {subtitle.ago}
              </span>
            )}
          </span>
        )}
        {view.showDots && (
          <span className="rail-dots" aria-hidden="true">
            {stats.dots.map((dot, i) => (
              <span
                // biome-ignore lint/suspicious/noArrayIndexKey: dots have no identity beyond their slot
                key={i}
                className={i === stats.focusedDot ? `rail-dot ${dot} focused` : `rail-dot ${dot}`}
              />
            ))}
            {stats.overflow > 0 && <span className="rail-dots-more">+{stats.overflow}</span>}
          </span>
        )}
      </span>
    </>
  );
}

/**
 * The lifted copy of a group's header that follows the pointer (or, unseen, the arrow
 * keys) while the group is dragged: a one-row chip (name, work dot, count). It is centred
 * in the overlay box, which is sized to the grabbed header, so the chip's centre (what
 * RailProjects measures) starts where the header's was.
 */
export function ProjectGroupOverlay({
  ref,
  group,
  hidden,
  reduceMotion,
}: {
  /** The copy's element, which RailProjects measures to place the drop line. */
  ref?: Ref<HTMLDivElement>;
  group: GroupModel;
  /** Invisible but still laid out (and measured): a keyboard drag shows only the line. */
  hidden: boolean;
  reduceMotion: boolean;
}) {
  return (
    <div
      ref={ref}
      className={`${groupClass(group, false)} rail-group-overlay${hidden ? " is-hidden" : ""}`}
      style={hueStyle(group.key)}
      aria-hidden="true"
    >
      <div className="rail-group-head">
        <div className="rail-group-toggle">
          <HeaderSummary group={group} open={false} reduceMotion={reduceMotion} subtitle={null} />
        </div>
      </div>
    </div>
  );
}

// Memoized: the handlers are shared by every group and take the group, which each binds
// itself, so a group whose props did not change skips re-rendering.
export const ProjectGroup = memo(function ProjectGroup({
  group,
  open,
  menuOpen,
  selected,
  hintVisible,
  reduceMotion,
  home,
  now,
  onToggle,
  onSelectLane,
  onOpenDiff,
  onLaneContextMenu,
  onNewWorktree,
  onOpenMenu,
}: {
  group: GroupModel;
  open: boolean;
  /** The project menu is open for this group: the header stays highlighted. */
  menuOpen: boolean;
  /** The active project's selected worktree; only the active group marks it. */
  selected: string | null;
  hintVisible: boolean;
  reduceMotion: boolean;
  /** The home directory, shown as `~` in the project path; null until known. */
  home: string | null;
  /** The minute clock that ages the last-activity label. */
  now: number;
  onToggle: (key: string) => void;
  onSelectLane: (g: GroupModel, worktree: string) => void;
  onOpenDiff: (g: GroupModel, worktree: string) => void;
  onLaneContextMenu: (g: GroupModel, worktree: string, x: number, y: number) => void;
  onNewWorktree: (g: GroupModel) => void;
  /** Open the project menu at viewport coordinates; `from` gets focus back on close. */
  onOpenMenu: (g: GroupModel, x: number, y: number, from: HTMLElement | null) => void;
}) {
  const bodyId = useId();
  // The header is the draggable node, so the lifted copy starts from it. Nothing gets a
  // transform: the group never moves mid-drag (and a transform would break sticky).
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, isDragging } = useDraggable({
    id: group.key,
    data: { kind: "group" },
  });
  const sentinel = useRef<HTMLDivElement>(null);
  // Whether the sticky header is pinned to the top of the rail's scroll area: its 1px
  // sentinel has scrolled out above the scroll container.
  const [pinned, setPinned] = useState(false);
  useEffect(() => {
    const el = sentinel.current;
    if (!el) return;
    const root = el.closest(".rail-lanes");
    const io = new IntersectionObserver(
      (entries) => {
        const e = entries[entries.length - 1];
        if (e) {
          setPinned(
            sentinelAbove(e.isIntersecting, e.boundingClientRect.top, e.rootBounds?.top ?? null),
          );
        }
      },
      { root, threshold: 0 },
    );
    io.observe(el);
    return () => io.disconnect();
  }, []);

  // Lane handlers bound to this group, shared by its lanes. They read the group through a
  // ref, so a change elsewhere in the group does not hand every lane new handlers.
  const groupRef = useRef(group);
  groupRef.current = group;
  const selectLane = useCallback((w: string) => onSelectLane(groupRef.current, w), [onSelectLane]);
  const openDiff = useCallback((w: string) => onOpenDiff(groupRef.current, w), [onOpenDiff]);
  const laneContextMenu = useCallback(
    (w: string, x: number, y: number) => onLaneContextMenu(groupRef.current, w, x, y),
    [onLaneContextMenu],
  );

  const { key, isActive, lanes, summaryStats: stats } = group;
  // A collapsed group's header has nothing to pin over, so only an open one reads as stuck.
  const stuck = open && pinned;
  const badges = isActive ? laneHintBadges(lanes) : null;

  const groupCls = isDragging ? `${groupClass(group, open)} is-dragging` : groupClass(group, open);
  const subtitle = {
    path: tailPath(shortPath(group.summary.root, home)),
    ago: lastActivityLabel(stats, now),
  };

  return (
    <div className={groupCls} style={hueStyle(key)} data-group={key}>
      <div ref={sentinel} className="rail-group-sentinel" aria-hidden="true" />
      <div
        ref={setNodeRef}
        className="rail-group-head"
        data-stuck={stuck ? "true" : "false"}
        data-menu={menuOpen ? "open" : undefined}
        {...listeners}
        onContextMenu={(e) => {
          e.preventDefault();
          onOpenMenu(group, e.clientX, e.clientY, null);
        }}
      >
        <button
          type="button"
          className="rail-group-toggle"
          aria-expanded={open}
          aria-controls={bodyId}
          aria-current={isActive ? "true" : undefined}
          aria-label={groupAriaLabel(key, stats)}
          onClick={() => onToggle(key)}
        >
          <HeaderSummary
            group={group}
            open={open}
            reduceMotion={reduceMotion}
            subtitle={subtitle}
          />
        </button>
        <span className="rail-group-actions">
          <button
            type="button"
            ref={setActivatorNodeRef}
            className="rail-group-act rail-group-grip"
            title="Drag to reorder"
            {...attributes}
            aria-label={`Reorder ${key}`}
          >
            <GripVertical size={12} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="rail-group-act rail-group-add"
            title="New worktree"
            aria-label={`New worktree in ${key}`}
            onClick={() => onNewWorktree(group)}
          >
            <Plus size={13} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="rail-group-act rail-group-more"
            title="Project actions"
            aria-label={`${key} actions`}
            aria-haspopup="menu"
            aria-expanded={menuOpen}
            onClick={(e) => {
              const r = e.currentTarget.getBoundingClientRect();
              onOpenMenu(group, r.left, r.bottom, e.currentTarget);
            }}
          >
            <Ellipsis size={13} aria-hidden="true" />
          </button>
        </span>
      </div>
      <div className="rail-group-body" id={bodyId} role="group" aria-label={`${key} worktrees`}>
        <div className="rail-group-body-inner">
          <div className="rail-group-lanes">
            {lanes.length === 0 ? (
              <div className="rail-group-empty">{group.loading ? "Loading…" : "No worktrees"}</div>
            ) : (
              lanes.map((lane) => (
                <Lane
                  key={lane.name}
                  groupKey={key}
                  lane={lane}
                  isSelected={isActive && lane.name === selected}
                  hintBadge={badges?.get(lane.name)}
                  hintVisible={hintVisible}
                  onSelect={selectLane}
                  onOpenDiff={openDiff}
                  onContextMenu={laneContextMenu}
                />
              ))
            )}
          </div>
        </div>
      </div>
    </div>
  );
});
