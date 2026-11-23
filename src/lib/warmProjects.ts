// Pure reducer module for the warm-project registry.
// No React, no Tauri imports — unit-testable and side-effect-free.

import type { View } from "../components/TopBar";
import type { ProjectSummary, ProjectWorktrees, WorktreeInfo } from "./ipc";

export interface WarmProject {
  summary: ProjectSummary;
  worktrees: WorktreeInfo[]; // RICH list with paths — captured at warm time
  selected: string | null; // remembered focused worktree (per-project)
  view: View; // remembered grid/focus view (per-project)
}

export interface WarmState {
  active: string | null; // active project name
  projects: Record<string, WarmProject>; // keyed by project name
}

export const emptyWarmState: WarmState = { active: null, projects: {} };

/**
 * Return the first worktree whose status is "available", or null.
 */
export function firstAvailable(worktrees: WorktreeInfo[]): string | null {
  for (const w of worktrees) {
    if (w.status === "available") return w.name;
  }
  return null;
}

/**
 * Content equality for two worktree lists (name + path + status + cli + terminated +
 * dispatch, in order). Used to skip a re-warm when a live `worktrees` reload hands back a fresh array
 * with identical contents (the 30s resync), avoiding a needless render cascade.
 */
export function sameWorktrees(a: WorktreeInfo[], b: WorktreeInfo[]): boolean {
  if (a === b) return true;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (
      a[i].name !== b[i].name ||
      a[i].path !== b[i].path ||
      a[i].status !== b[i].status ||
      a[i].cli !== b[i].cli ||
      a[i].terminated !== b[i].terminated ||
      (a[i].dispatch !== false) !== (b[i].dispatch !== false)
    ) {
      return false;
    }
  }
  return true;
}

/**
 * Add-or-promote a project and set it as active.
 *
 * - If the project is new: insert it with the provided (or defaulted) selected/view.
 * - If the project already exists (re-warm): keep its remembered selected/view but
 *   refresh summary + worktrees.
 * - Always sets state.active to this project's name.
 */
export function warmProject(
  state: WarmState,
  p: {
    summary: ProjectSummary;
    worktrees: WorktreeInfo[];
    selected?: string | null;
    view?: View;
  },
): WarmState {
  const name = p.summary.name;
  const existing = state.projects[name];

  const entry: WarmProject = existing
    ? // Re-warm: refresh summary + worktrees; keep remembered selected/view.
      {
        summary: p.summary,
        worktrees: p.worktrees,
        selected: existing.selected,
        view: existing.view,
      }
    : // New project: seed with defaults or caller-supplied selected/view.
      {
        summary: p.summary,
        worktrees: p.worktrees,
        selected: p.selected !== undefined ? p.selected : firstAvailable(p.worktrees),
        view: p.view ?? "grid",
      };

  return {
    active: name,
    projects: {
      ...state.projects,
      [name]: entry,
    },
  };
}

/**
 * Flip the active project to an already-warm project without touching its
 * stored selected/view. Pure visibility flip.
 */
export function setActive(state: WarmState, name: string): WarmState {
  return {
    ...state,
    active: name,
  };
}

/**
 * Update the remembered focused worktree for a warm project (immutable).
 */
export function rememberSelection(
  state: WarmState,
  name: string,
  selected: string | null,
): WarmState {
  const existing = state.projects[name];
  if (!existing) return state;
  return {
    ...state,
    projects: {
      ...state.projects,
      [name]: { ...existing, selected },
    },
  };
}

/**
 * Update the remembered view for a warm project (immutable).
 */
export function rememberView(state: WarmState, name: string, view: View): WarmState {
  const existing = state.projects[name];
  if (!existing) return state;
  return {
    ...state,
    projects: {
      ...state.projects,
      [name]: { ...existing, view },
    },
  };
}

/**
 * Return the active project's entry, or null if none is active.
 */
export function activeProject(state: WarmState): WarmProject | null {
  if (state.active === null) return null;
  return state.projects[state.active] ?? null;
}

/**
 * The tagged list's worktrees when it was read for `projectName`, else null. A list read
 * for another project (a stale response, or the registry's active project moved under
 * the app) must never be filed under `projectName`.
 */
export function worktreesFor(
  projectName: string | null,
  tagged: ProjectWorktrees | null,
): WorktreeInfo[] | null {
  if (projectName === null || tagged === null || tagged.project !== projectName) return null;
  return tagged.worktrees;
}

/**
 * What the warm-seeding effect does with the live worktree list:
 * - `skip` while a project switch is still awaiting the backend, or without both inputs;
 * - `warm` when the list was read for the project the app holds as active;
 * - `resync` when it was read for a different project: the app's active project and
 *   the backend's disagree, so both are re-read (the backend's registry is the truth).
 *   A list read for no project is skipped instead: that is usually a brief resolve
 *   failure (the project's config mid-write), and re-reading the project then would
 *   drop the app into onboarding. A real removal reseeds via `project://changed`.
 *
 * Under `KEITORA_PROJECT` both reads resolve the env project, so they always agree.
 */
export type WarmDecision =
  | { kind: "skip" }
  | { kind: "warm"; worktrees: WorktreeInfo[] }
  | { kind: "resync"; listProject: string };

export function warmDecision(
  projectName: string | null,
  tagged: ProjectWorktrees | null,
  switchInFlight: boolean,
): WarmDecision {
  if (switchInFlight || projectName === null || tagged === null || tagged.project === null) {
    return { kind: "skip" };
  }
  const worktrees = worktreesFor(projectName, tagged);
  return worktrees !== null
    ? { kind: "warm", worktrees }
    : { kind: "resync", listProject: tagged.project };
}
