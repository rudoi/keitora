// One lane row of the rail: a worktree's status block, name, task, stage, branch and build
// bar. Available lanes are selectable; others render muted with their status.

import { useDraggable } from "@dnd-kit/core";
import { FileDiff } from "lucide-react";
import type { CSSProperties, PointerEventHandler, Ref } from "react";
import { memo, useEffect, useMemo, useRef, useState } from "react";
import {
  formatActions,
  formatEta,
  showBuildBar,
  stageChipText,
  stageChipTone,
} from "../../lib/activity";
import type { WorkflowState, WorktreeActivity, WorktreeStatus } from "../../lib/ipc";
import { blockAnimates } from "../../lib/laneState";
import type { LaneModel } from "../../lib/railGroups";
import { laneDragId } from "../../lib/railOrder";
import { usePrefersReducedMotion } from "../../lib/usePrefersReducedMotion";
import { ProviderGlyph } from "../ProviderGlyph";

const STATUS_LABEL: Record<WorktreeStatus, string> = {
  available: "",
  missing: "path not found",
  not_dir: "not a directory",
  not_git: "not a git worktree",
};

// Tooltip text when the rail surfaces an attention. Only `waiting` is surfaced on the rail
// (colour, pulse, and this tooltip); a `finished` worktree reads as plain idle here — its
// signal lives in the dock badge + notification.
const WAITING_LABEL = "needs input";

// Tooltip text for a worktree whose session the user terminated.
const TERMINATED_LABEL = "Terminated";

// ── Marquee ────────────────────────────────────────────────────────────────────
// A single line of text clipped to the narrow rail card. When it's wider than the card we
// render it twice and run a continuous wrap-around marquee; the loop is seamless because the
// track scrolls left by exactly one copy + gap. Text that fits sits still. Shared by the
// active-task title and the git-branch label — both overflow the same way, so both scroll on
// hover identically. `outerClassName` selects the per-row sizing/typography wrapper
// (`lane-task` vs `lane-branch`); the inner track/copy classes drive the (shared) animation.
const MARQUEE_GAP = 32; // px of empty space between the repeated copies

function Marquee({ text, outerClassName }: { text: string; outerClassName: string }) {
  const outer = useRef<HTMLSpanElement>(null);
  const copy = useRef<HTMLSpanElement>(null);
  const reduceMotion = usePrefersReducedMotion();
  const [shift, setShift] = useState(0); // px to travel per loop; 0 = static (no overflow)
  // biome-ignore lint/correctness/useExhaustiveDependencies: text is an outer scope prop; biome flags it as invalid dep but it is used in the ResizeObserver measure callback
  useEffect(() => {
    if (reduceMotion) {
      setShift(0);
      return;
    }
    const measure = () => {
      const o = outer.current;
      const c = copy.current;
      if (!o || !c) return;
      // scrollWidth is the intrinsic text width in both the static (block) and ticker
      // (inline-flex) states; offsetWidth would just report the card width when static.
      const textWidth = c.scrollWidth;
      // One copy wider than the card → scroll it; travel = copy width + the gap.
      setShift(textWidth > o.clientWidth ? textWidth + MARQUEE_GAP : 0);
    };
    measure();
    const ro = new ResizeObserver(measure);
    if (outer.current) ro.observe(outer.current);
    return () => ro.disconnect();
  }, [text, reduceMotion]);
  const animate = shift > 0;
  // Scroll at a steady ~40px/s (the 14% settle is on top), so every line reads at the
  // same pace regardless of length. Only runs on hover (gated in CSS).
  const style = animate
    ? ({
        "--lane-task-shift": `${shift}px`,
        "--lane-task-dur": `${Math.max(4, shift / 40 / 0.86).toFixed(1)}s`,
      } as CSSProperties)
    : undefined;
  return (
    <span ref={outer} className={outerClassName}>
      <span className={animate ? "lane-task-track ticker" : "lane-task-track"} style={style}>
        <span
          ref={copy}
          className="lane-task-copy"
          style={animate ? { marginRight: MARQUEE_GAP } : undefined}
        >
          {text}
        </span>
        {animate && (
          <span className="lane-task-copy" aria-hidden="true">
            {text}
          </span>
        )}
      </span>
    </span>
  );
}

// ── Lane activity rows (stage chip · branch, and the build bar) ───────────────────
// Both read only from `WorktreeActivity` (normalized in lib/ipc); pure formatting lives in
// lib/activity.ts. Each sits on its own lane row (name → stage → task title → branch), so a
// long name/branch is never crushed. "Stale" is a muted display hint, never "idle".

function StageChip({ workflow }: { workflow: WorkflowState }) {
  const tone = stageChipTone(workflow);
  const toneCls =
    tone === "warn" || tone === "stale" || tone === "gate" || tone === "done" ? tone : "";
  const title = workflow.unreadable
    ? "state file unreadable — showing last good value"
    : (workflow.stageDetail ?? undefined);
  return (
    <span className={toneCls ? `lane-stage ${toneCls}` : "lane-stage"} title={title}>
      {stageChipText(workflow)}
    </span>
  );
}

function LaneBuild({ build }: { build: NonNullable<WorktreeActivity["build"]> }) {
  const eta = formatEta(build.etaSecs);
  const fillCls =
    build.state === "failed"
      ? "lane-build-fill failed"
      : build.state === "succeeded"
        ? "lane-build-fill succeeded"
        : "lane-build-fill";
  return (
    <div className={build.state === "failed" ? "lane-build failed" : "lane-build"}>
      <div className="lane-build-top">
        <span className="lane-build-label">{build.label}</span>
        <span className="lane-build-pct">{build.percent}%</span>
      </div>
      <div className="lane-build-track">
        <div className={fillCls} style={{ width: `${build.percent}%` }} />
      </div>
      <div className="lane-build-bot">
        <span>{formatActions(build.done, build.total)}</span>
        <span>{eta ? `${eta} left` : ""}</span>
      </div>
    </div>
  );
}

// ── Lane ───────────────────────────────────────────────────────────────────────

/** What a lane's drag reports to RailProjects: the group it belongs to and its worktree. */
export interface LaneDragData {
  kind: "lane";
  group: string;
  worktree: string;
}

/**
 * The lifted copy of a lane that follows the pointer while it is dragged: a one-row chip
 * with the worktree name, centred in the overlay box (sized to the grabbed lane).
 */
export function LaneOverlay({ ref, name }: { ref?: Ref<HTMLDivElement>; name: string }) {
  return (
    <div ref={ref} className="rail-lane-overlay" aria-hidden="true">
      <span className="lane-name">{name}</span>
    </div>
  );
}

// Memoized: the handlers take the worktree name, so the parent passes the same ones to
// every lane and an unchanged lane skips re-rendering.
//
// The whole row is a mouse drag handle (past the rail's 4 px threshold, so a click still
// selects). Only the pointer listener is attached: no keyboard activation, role or tab
// stop, so Enter on the diff button never starts a drag.
export const Lane = memo(function Lane({
  groupKey,
  lane,
  isSelected,
  hintBadge,
  hintVisible,
  onSelect,
  onOpenDiff,
  onContextMenu,
}: {
  /** Registry key of the lane's project group. */
  groupKey: string;
  lane: LaneModel;
  isSelected: boolean;
  hintBadge?: number;
  hintVisible: boolean;
  onSelect: (worktree: string) => void;
  onOpenDiff: (worktree: string) => void;
  onContextMenu: (worktree: string, x: number, y: number) => void;
}) {
  const reduceMotion = usePrefersReducedMotion();
  const data = useMemo<LaneDragData>(
    () => ({ kind: "lane", group: groupKey, worktree: lane.name }),
    [groupKey, lane.name],
  );
  const { listeners, setNodeRef, isDragging } = useDraggable({
    id: laneDragId(groupKey, lane.name),
    data,
  });
  const { activity: act, task, building, attn, terminated } = lane;
  const ok = lane.status === "available";
  const badge = hintBadge;
  // Path/state on hovering the name only — not the whole row — so the tooltip
  // never sits over the task line's marquee.
  const nameTitle = terminated
    ? `${lane.name} — ${TERMINATED_LABEL}`
    : lane.sessionState
      ? `${lane.name} — ${attn === "waiting" ? WAITING_LABEL : lane.sessionState}`
      : ok
        ? lane.path
        : `${lane.name} — ${STATUS_LABEL[lane.status]}`;
  const rowCls = [
    "lane-row",
    ok ? "" : "disabled",
    terminated ? "terminated" : "",
    isSelected ? "sel" : "",
    isDragging ? "is-dragging" : "",
  ]
    .filter(Boolean)
    .join(" ");
  // The status block carries the colour; it glows + pulses only for a blocking
  // attention (waiting), except on the selected lane (you're already looking at it)
  // or when motion is reduced. A finished worktree stays steady green.
  const animates = !terminated && blockAnimates(attn);
  const blockCls = [
    "wt-block",
    lane.blockState,
    animates ? "attn" : "",
    animates && !isSelected && !reduceMotion ? "pulse" : "",
  ]
    .filter(Boolean)
    .join(" ");
  // Small live dot in the name row when the worktree is doing something now (a build
  // or a busy tracked agent): green/teal while building, amber for a busy agent.
  const workDotCls = ["lane-work-dot", building ? "working" : "busy", reduceMotion ? "" : "live"]
    .filter(Boolean)
    .join(" ");
  return (
    <div
      ref={setNodeRef}
      className={rowCls}
      onPointerDown={listeners?.onPointerDown as PointerEventHandler<HTMLDivElement> | undefined}
      onClick={ok ? () => onSelect(lane.name) : undefined}
      onContextMenu={
        ok
          ? (e) => {
              // Only available worktrees have a mounted TerminalPane to restart —
              // mirrors the onClick gate above. Let the native menu through for the
              // rest (missing/not_dir/not_git rows have nothing to restart).
              e.preventDefault();
              onContextMenu(lane.name, e.clientX, e.clientY);
            }
          : undefined
      }
    >
      {hintVisible && badge !== undefined && badge <= 9 && (
        <span className="wt-hint-badge">{badge}</span>
      )}
      <span className={blockCls} title={terminated ? TERMINATED_LABEL : undefined} />
      <div className="lane">
        <div className="lane-head">
          <ProviderGlyph cli={lane.cli} />
          <span className="lane-name" title={nameTitle}>
            {lane.name}
          </span>
          {task && <span className="lane-status mono">#{task.id}</span>}
          {!lane.sessionState && !ok && (
            <span className="lane-status">{STATUS_LABEL[lane.status]}</span>
          )}
          {act?.working && (
            <span
              className={workDotCls}
              title={building ? "building" : "agent busy"}
              aria-hidden="true"
            />
          )}
          {ok && (
            <button
              type="button"
              className="lane-diff-btn"
              title="View diff"
              aria-label={`View diff for ${lane.name}`}
              onClick={(e) => {
                // Distinct from the lane's click-to-select — open the diff instead.
                e.stopPropagation();
                onOpenDiff(lane.name);
              }}
            >
              <FileDiff size={13} aria-hidden="true" />
            </button>
          )}
        </div>
        {ok && act?.workflow && <StageChip workflow={act.workflow} />}
        {task && <Marquee text={task.title} outerClassName="lane-task" />}
        {ok && act?.branch && <Marquee text={`⎇ ${act.branch}`} outerClassName="lane-branch" />}
        {ok && showBuildBar(act) && act?.build && <LaneBuild build={act.build} />}
      </div>
    </div>
  );
});
