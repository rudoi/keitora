// Pure view derivations for a rail project group's header: what the collapsed summary
// shows, the toggle's accessible label, when a sticky header counts as "stuck", and the
// Cmd+N jump badges of the active group's lanes. Kept out of the component so they can be
// unit-tested without rendering.

import { formatAgo } from "./format";
import type { LaneModel, SummaryStats } from "./railGroups";

export interface HeaderView {
  /** The per-worktree dot strip; collapsed groups only (open groups show the lanes). */
  showDots: boolean;
  /** Some worktree needs input: the ring dot after the name and the waiting edge. */
  waiting: boolean;
  /** The work dot's modifier: a running build wins over a busy agent; null hides it. */
  work: "working" | "busy" | null;
  /** The live-session count as shown; empty at 0 so the slot keeps its width. */
  count: string;
  /** Lanes driven by another worktree; the header shows `N driven` when non-zero. */
  driven: number;
}

export function headerView(stats: SummaryStats, open: boolean): HeaderView {
  return {
    showDots: !open && stats.dots.length > 0,
    waiting: stats.waiting > 0,
    work: stats.building > 0 ? "working" : stats.busy > 0 ? "busy" : null,
    count: stats.live > 0 ? String(stats.live) : "",
    driven: stats.driven,
  };
}

/**
 * The header subtitle's last activity: `active` while a lane is working (the heartbeat
 * lags a long turn), else the time since the latest heartbeat; null if there is none.
 */
export function lastActivityLabel(stats: SummaryStats, nowMs: number): string | null {
  return stats.active ? "active" : formatAgo(stats.lastActivity, nowMs);
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/**
 * The toggle button's accessible name: the project, then whatever the collapsed summary
 * signals, so a screen reader hears the same "what's going on" the dots convey.
 */
export function groupAriaLabel(key: string, stats: SummaryStats): string {
  const parts = [key];
  if (stats.waiting > 0) parts.push(plural(stats.waiting, "needs input", "need input"));
  if (stats.building > 0) parts.push(`${stats.building} building`);
  if (stats.busy > 0) parts.push(`${stats.busy} busy`);
  if (stats.driven > 0) parts.push(`${stats.driven} driven`);
  if (stats.live > 0) parts.push(plural(stats.live, "live session", "live sessions"));
  return parts.join(", ");
}

/**
 * Whether a group's sentinel (a 1px marker at the top of the group) has scrolled out of
 * the rail above its top edge, which is when the sticky header is pinned. A sentinel that
 * left through the bottom edge, or a missing root rect, is not stuck.
 */
export function sentinelAbove(
  isIntersecting: boolean,
  sentinelTop: number,
  rootTop: number | null,
): boolean {
  return !isIntersecting && rootTop !== null && sentinelTop < rootTop;
}

/** 1-based index of each available lane, in order: the Cmd+N jump badge numbers. */
export function laneHintBadges(lanes: readonly LaneModel[]): Map<string, number> {
  const badges = new Map<string, number>();
  let n = 0;
  for (const lane of lanes) {
    if (lane.status === "available") badges.set(lane.name, ++n);
  }
  return badges;
}
