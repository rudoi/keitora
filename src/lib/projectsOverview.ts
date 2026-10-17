// Worktrees and session states for every registered project, so the rail can show real
// status for projects other than the active one. The wrapper lives here rather than in
// `ipc.ts` because `WorktreeSession` is defined in `sessions.ts`, which imports from
// `ipc.ts` (the same reason `listSessions` lives in `sessions.ts`).

import { useEffect, useMemo, useRef } from "react";
import { trailingDebounce } from "./debounce";
import { onProjectChanged, onWorktreesChanged, type WorktreeInfo } from "./ipc";
import type { WorktreeSession } from "./sessions";
import { getTransport, liveListen } from "./transport";
import { useLiveResource } from "./useLiveResource";

/** A worktree's active (claimed) task. */
export interface ActiveTask {
  worktree: string;
  id: number;
  title: string;
}

export interface ProjectOverview {
  key: string; // registry key (what set_active_project takes)
  resolved_name: string; // [project].name; the session/activity namespace
  worktrees: WorktreeInfo[];
  sessions: WorktreeSession[]; // `project` stamped to `resolved_name`
  /** Each worktree's active task (lowest id if several), from the project's task DB. */
  active_tasks: ActiveTask[];
}

/** Every registered project's worktrees and session states, in display order. */
export function projectsOverview(): Promise<ProjectOverview[]> {
  return getTransport().invoke<ProjectOverview[]>("projects_overview");
}

/** A value with its JSON signature, for `keepStable`. */
export interface Stable<T> {
  sig: string;
  value: T;
}

/**
 * `next` with its signature, unless it serializes the same as `prev`, in which case `prev`
 * itself, so an unchanged poll result keeps its old reference and memos downstream hold.
 */
export function keepStable<T>(prev: Stable<T> | null, next: T): Stable<T> {
  const sig = JSON.stringify(next);
  return prev !== null && prev.sig === sig ? prev : { sig, value: next };
}

/** Trailing window that coalesces a burst of worktree/project change events. */
const CHANGE_DEBOUNCE_MS = 150;

/**
 * Live overview of every project; `null` until the first load. Polls every 3 s like
 * `useSessions` (a session running background work emits no event, so the interval is
 * what surfaces idle→busy), refetches on `sessions://changed`, and also on worktree and
 * project changes through a short trailing debounce (a project switch fires several of
 * those in a burst). A reload that returns the same data keeps the previous reference.
 */
export function useProjectsOverview(): ProjectOverview[] | null {
  const { data, reload } = useLiveResource(projectsOverview, "sessions://changed", {
    intervalMs: 3_000,
    listen: liveListen,
  });

  useEffect(() => {
    const debounced = trailingDebounce(() => void reload(), CHANGE_DEBOUNCE_MS);
    let cancelled = false;
    const uns: (() => void)[] = [];
    for (const subscribe of [onWorktreesChanged, onProjectChanged]) {
      void subscribe(debounced.call).then((u) => {
        if (cancelled) u();
        else uns.push(u);
      });
    }
    return () => {
      cancelled = true;
      debounced.cancel();
      for (const u of uns) u();
    };
  }, [reload]);

  // Most polls return what is already shown; keep the old reference for those.
  const stable = useRef<Stable<ProjectOverview[]> | null>(null);
  return useMemo(() => {
    if (data === null) return null;
    stable.current = keepStable(stable.current, data);
    return stable.current.value;
  }, [data]);
}
