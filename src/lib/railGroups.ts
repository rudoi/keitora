// The rail's per-project data model: one GroupModel per registered project, each with
// its lanes and a collapsed-header summary. Pure, so the rail components render it
// without branching on "is this the active project" for data.
//
// Two strings identify a project and they can differ: the registry key
// (`ProjectSummary.name`, `ProjectOverview.key`) and the resolved `[project].name`
// (`ProjectSummary.resolved_name`). Overview entries are matched by key; everything in
// the session/daemon namespace (activity, live counts, the active project) by resolved
// name.

import type { AttentionKind } from "./attention";
import { parseUtc } from "./format";
import type {
  CliKind,
  ProjectSummary,
  WorktreeActivity,
  WorktreeInfo,
  WorktreeStatus,
} from "./ipc";
import { blockStateCls, type DotState } from "./laneState";
import type { ProjectOverview } from "./projectsOverview";
import type { SessionState, WorktreeSession } from "./sessions";

export interface LaneModel {
  name: string;
  path: string;
  cli: CliKind;
  status: WorktreeStatus;
  /** The user terminated the session (only ever set on an available worktree). */
  terminated: boolean;
  /** In the project's dispatch pool (may receive dispatched tasks). */
  dispatch: boolean;
  sessionState: SessionState | undefined;
  /** Active group: from `attention`. Other groups: `waiting` iff the session is waiting. */
  attn: AttentionKind | undefined;
  /** The worktree's active task (the active group's from its task list, others' from the overview). */
  task: { id: number; title: string } | undefined;
  /** Only for available worktrees. */
  activity: WorktreeActivity | undefined;
  /** The session's last hook event (SQLite UTC `YYYY-MM-DD HH:MM:SS`); null without one. */
  lastHeartbeat: string | null;
  building: boolean;
  working: boolean;
  blockState: DotState;
}

export interface SummaryStats {
  /** Daemon sessions for the project (not derived from lanes). */
  live: number;
  waiting: number;
  /** Lanes doing work that is not a build. */
  busy: number;
  building: number;
  /** Block state of each available lane in lane order, cut to `maxDots`. */
  dots: DotState[];
  /** Index into `dots` of the selected lane; active group only. */
  focusedDot: number | null;
  /** Available lanes left out of `dots` (the "+N"). */
  overflow: number;
  /** The latest `lastHeartbeat` across the lanes; null when none has one. */
  lastActivity: string | null;
  /** Some lane is working right now: a busy session, background work, or a build. */
  active: boolean;
}

export interface GroupModel {
  key: string;
  resolvedName: string;
  /** The project's summary, as `list_projects` returned it (`name` is the key). */
  summary: ProjectSummary;
  isActive: boolean;
  /** Built from the overview, which has not loaded yet. */
  loading: boolean;
  lanes: LaneModel[];
  summaryStats: SummaryStats;
}

export interface BuildGroupsInput {
  /** Every registered project in display order, from `list_projects`. */
  projects: ProjectSummary[];
  activeResolvedName: string | null;
  /** The active project's rich sources below belong to it (the warm registry shows it). */
  activeReady: boolean;
  activeWorktrees: WorktreeInfo[];
  /** Active project's sessions, keyed by worktree. */
  sessions: Map<string, WorktreeSession>;
  attention: Map<string, AttentionKind>;
  activeTaskByWorktree: Map<string, { id: number; title: string }>;
  overview: ProjectOverview[] | null;
  /** Keyed `${resolvedName}/${worktree}`. */
  activity: Map<string, WorktreeActivity>;
  /** Daemon session count per resolved name. */
  liveCounts: Map<string, number>;
  /** The active project's selected worktree. */
  selected: string | null;
  maxDots: number;
}

/** One lane's model. `attention` applies only to an available worktree. */
export function buildLane(
  w: WorktreeInfo,
  resolvedName: string,
  session: WorktreeSession | undefined,
  attention: AttentionKind | undefined,
  task: { id: number; title: string } | undefined,
  activityMap: Map<string, WorktreeActivity>,
): LaneModel {
  const ok = w.status === "available";
  const activity = ok ? activityMap.get(`${resolvedName}/${w.name}`) : undefined;
  const building = activity?.build?.state === "running";
  const attn = ok ? attention : undefined;
  const terminated = ok && w.terminated;
  return {
    name: w.name,
    path: w.path,
    cli: w.cli,
    status: w.status,
    terminated,
    dispatch: w.dispatch !== false,
    sessionState: session?.state,
    attn,
    task,
    activity,
    lastHeartbeat: session?.last_heartbeat || null,
    building,
    working: activity?.working === true,
    blockState: blockStateCls(attn, session, ok, building, terminated),
  };
}

/**
 * The collapsed-header summary of a group's lanes. `focused` names the selected lane
 * (null outside the active group). When the dots overflow `maxDots` and the focused
 * lane would be cut, it takes the last visible slot so it stays visible.
 */
export function summarize(
  lanes: LaneModel[],
  live: number,
  focused: string | null,
  maxDots: number,
): SummaryStats {
  const available = lanes.filter((l) => l.status === "available");
  const all = available.map((l) => l.blockState);
  const focusIdx = focused === null ? -1 : available.findIndex((l) => l.name === focused);
  const max = Math.max(0, maxDots);

  let dots = all;
  let focusedDot: number | null = focusIdx >= 0 ? focusIdx : null;
  if (all.length > max) {
    dots = all.slice(0, max);
    if (focusIdx >= max) {
      if (max > 0) {
        dots[max - 1] = all[focusIdx];
        focusedDot = max - 1;
      } else {
        focusedDot = null;
      }
    }
  }

  // Compared as times: heartbeats may come zoneless (SQLite) or as zoned ISO strings.
  let lastActivity: string | null = null;
  let latest = -Infinity;
  for (const l of lanes) {
    const t = parseUtc(l.lastHeartbeat);
    if (t !== null && t > latest) {
      latest = t;
      lastActivity = l.lastHeartbeat;
    }
  }

  return {
    live,
    waiting: lanes.filter((l) => l.blockState === "waiting").length,
    busy: lanes.filter((l) => l.working && !l.building).length,
    building: lanes.filter((l) => l.building).length,
    dots,
    focusedDot,
    overflow: all.length - dots.length,
    lastActivity,
    active: lanes.some((l) => l.working || l.building || l.sessionState === "busy"),
  };
}

/** One GroupModel per project, in `projects` order. */
export function buildGroups(i: BuildGroupsInput): GroupModel[] {
  return i.projects.map((p) => {
    const key = p.name;
    const resolvedName = p.resolved_name;
    const isActive = i.activeResolvedName !== null && resolvedName === i.activeResolvedName;
    // The active group renders from the rich active-only sources once they belong to it;
    // until then (a cold switch) it falls back to the overview like every other group, so
    // it never shows the previous project's lanes.
    const rich = isActive && i.activeReady;

    let lanes: LaneModel[];
    if (rich) {
      lanes = i.activeWorktrees.map((w) =>
        buildLane(
          w,
          resolvedName,
          i.sessions.get(w.name),
          i.attention.get(w.name),
          i.activeTaskByWorktree.get(w.name),
          i.activity,
        ),
      );
    } else {
      const entry = i.overview?.find((o) => o.key === key);
      const byWorktree = new Map(entry?.sessions.map((s) => [s.worktree, s]) ?? []);
      const tasks = new Map(
        entry?.active_tasks.map((t) => [t.worktree, { id: t.id, title: t.title }]) ?? [],
      );
      lanes = (entry?.worktrees ?? []).map((w) => {
        const session = byWorktree.get(w.name);
        return buildLane(
          w,
          resolvedName,
          session,
          session?.state === "waiting" ? "waiting" : undefined,
          tasks.get(w.name),
          i.activity,
        );
      });
    }

    return {
      key,
      resolvedName,
      summary: p,
      isActive,
      loading: !rich && i.overview === null,
      lanes,
      summaryStats: summarize(
        lanes,
        i.liveCounts.get(resolvedName) ?? 0,
        rich ? i.selected : null,
        i.maxDots,
      ),
    };
  });
}

/**
 * Structural equality over plain data (primitives, arrays, plain objects), which is all a
 * GroupModel holds. A key set to `undefined` equals a missing key.
 */
export function sameData(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    const bs = b as unknown[];
    return a.length === bs.length && a.every((x, i) => sameData(x, bs[i]));
  }
  const ao = a as Record<string, unknown>;
  const bo = b as Record<string, unknown>;
  const keys = new Set([...Object.keys(ao), ...Object.keys(bo)]);
  for (const k of keys) if (!sameData(ao[k], bo[k])) return false;
  return true;
}

/** `next` when it differs from `prev`, else `prev` itself. */
function keep<T>(prev: T, next: T): T {
  return sameData(prev, next) ? prev : next;
}

/**
 * `next` with every part that equals its counterpart in `prev` replaced by the `prev`
 * object, so memoized rail components see unchanged groups and lanes as the same
 * references. Lanes are matched by name within a group, groups by key; a group whose
 * fields and lanes all match is `prev`'s group itself, and a list with nothing changed
 * is `prev` itself.
 */
export function shareGroups(prev: readonly GroupModel[] | null, next: GroupModel[]): GroupModel[] {
  if (prev === null) return next;
  const prevByKey = new Map(prev.map((g) => [g.key, g]));
  const out = next.map((g) => {
    const p = prevByKey.get(g.key);
    if (p === undefined) return g;
    const prevLanes = new Map(p.lanes.map((l) => [l.name, l]));
    const lanes = g.lanes.map((l) => {
      const pl = prevLanes.get(l.name);
      return pl === undefined ? l : keep(pl, l);
    });
    const sameLanes = lanes.length === p.lanes.length && lanes.every((l, i) => l === p.lanes[i]);
    const shared: GroupModel = {
      ...g,
      summary: keep(p.summary, g.summary),
      lanes: sameLanes ? p.lanes : lanes,
      summaryStats: keep(p.summaryStats, g.summaryStats),
    };
    const unchanged =
      sameLanes &&
      shared.summary === p.summary &&
      shared.summaryStats === p.summaryStats &&
      g.resolvedName === p.resolvedName &&
      g.isActive === p.isActive &&
      g.loading === p.loading;
    return unchanged ? p : shared;
  });
  return out.length === prev.length && out.every((g, i) => g === prev[i])
    ? (prev as GroupModel[])
    : out;
}
