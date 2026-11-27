// Live drive bindings for the active project and the per-pane banner model built from
// them, plus the drive actions (take over, pause/resume, release) the rail menu and the
// pane banner share.

import { useCallback, useMemo, useRef } from "react";
import { type DriveMenuItem, type PaneDrive, paneDrive } from "./drive";
import { plural } from "./format";
import {
  type DriveView,
  drivePause,
  driveRelease,
  drivesList,
  driveTakeOver,
  type WorktreeActivity,
} from "./ipc";
import { sameData } from "./railGroups";
import type { WorktreeSession } from "./sessions";
import { liveListen } from "./transport";
import { useLiveResource } from "./useLiveResource";

/** One `drives_list` answer, stamped with the project it was asked for. */
export interface DrivesSnapshot {
  project: string | null;
  views: DriveView[];
}

/** The drives of `project` by driven worktree; empty when `snap` answered another project
 *  (a response that landed after a switch must not show the old project's drives). */
export function drivesFor(
  snap: DrivesSnapshot | null,
  project: string | null,
): Map<string, DriveView> {
  if (snap === null || project === null || snap.project !== project) return new Map();
  return new Map(snap.views.map((v) => [v.driven, v]));
}

/**
 * The active project's drives by driven worktree, refetched on every `sessions://changed`
 * (each prompt decision bumps the session revision). Empty for `null`.
 */
export function useDrives(project: string | null): Map<string, DriveView> {
  const load = useCallback(
    async (): Promise<DrivesSnapshot> => ({
      project,
      views: project === null ? [] : await drivesList(project),
    }),
    [project],
  );
  const { data } = useLiveResource(load, "sessions://changed", { listen: liveListen });
  return useMemo(() => drivesFor(data, project), [data, project]);
}

/**
 * Banner models for every driven pane, keyed `project/worktree`. Activity covers every
 * project; the session rows and drive views are the active project's only, so other
 * projects' (hidden) panes fall back to the activity fields. An entry equal to the one in
 * `prev` is handed back as the same object so the pane props stay stable.
 */
export function buildPaneDrives(
  prev: ReadonlyMap<string, PaneDrive> | null,
  activity: ReadonlyMap<string, WorktreeActivity>,
  activeProject: string | null,
  sessions: ReadonlyMap<string, WorktreeSession>,
  views: ReadonlyMap<string, DriveView>,
): Map<string, PaneDrive> {
  const out = new Map<string, PaneDrive>();
  const put = (key: string, d: PaneDrive | null) => {
    if (d === null) return;
    const old = prev?.get(key);
    out.set(key, old !== undefined && sameData(old, d) ? old : d);
  };
  for (const [key, a] of activity) {
    const active = activeProject !== null && a.project === activeProject;
    put(
      key,
      paneDrive(
        a,
        active ? sessions.get(a.worktree) : undefined,
        active ? views.get(a.worktree) : undefined,
      ),
    );
  }
  if (activeProject !== null) {
    // Active-project sessions the activity poll has not reported yet.
    for (const [wt, s] of sessions) {
      const key = `${activeProject}/${wt}`;
      if (!activity.has(key)) put(key, paneDrive(undefined, s, views.get(wt)));
    }
  }
  return out;
}

/** {@link buildPaneDrives} behind a memo that keeps unchanged entries' identity. */
export function usePaneDrives(
  activity: ReadonlyMap<string, WorktreeActivity>,
  activeProject: string | null,
  sessions: ReadonlyMap<string, WorktreeSession>,
  views: ReadonlyMap<string, DriveView>,
): Map<string, PaneDrive> {
  const prev = useRef<Map<string, PaneDrive> | null>(null);
  return useMemo(() => {
    const next = buildPaneDrives(prev.current, activity, activeProject, sessions, views);
    prev.current = next;
    return next;
  }, [activity, activeProject, sessions, views]);
}

/** The drive commands an action runs; injectable for tests. */
export interface DriveActionApi {
  takeOver: (project: string, driven: string) => Promise<number>;
  pause: (project: string, driven: string, paused: boolean) => Promise<void>;
  release: (project: string, driven: string) => Promise<void>;
}

const DRIVE_API: DriveActionApi = {
  takeOver: driveTakeOver,
  pause: drivePause,
  release: driveRelease,
};

/**
 * Run a drive action on `driven` in `project` (its resolved name, so it never needs the
 * project to be active) and toast the result or the error.
 */
export async function runDriveAction(
  project: string,
  driven: string,
  item: DriveMenuItem,
  driver: string,
  toast: (message: string) => void,
  api: DriveActionApi = DRIVE_API,
): Promise<void> {
  try {
    switch (item) {
      case "take-over": {
        const n = await api.takeOver(project, driven);
        toast(
          n === 0
            ? "No pending prompts to take over."
            : `Took over ${n} ${plural(n, "prompt")} in ${driven}.`,
        );
        break;
      }
      case "pause-driver":
        await api.pause(project, driven, true);
        toast(`Paused ${driver}'s drive of ${driven}.`);
        break;
      case "resume-driver":
        await api.pause(project, driven, false);
        toast(`Resumed ${driver}'s drive of ${driven}.`);
        break;
      case "release":
        await api.release(project, driven);
        toast(`Released ${driven}.`);
        break;
    }
  } catch (e) {
    toast(String(e));
  }
}
