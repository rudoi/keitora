// Typed bridge to the task backend (keitora_tasks::Store via Tauri commands) plus a
// useTasks hook for live board data. Mirrors lib/ipc.ts. Field names are
// snake_case to match the Rust models' serde output.

import type { UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useRef } from "react";
import type { DispatchFailure, Reservation } from "./dispatch";
import { shouldSeedFromCache } from "./projectScope";
import { getTransport, liveListen } from "./transport";
import { useLiveResource } from "./useLiveResource";

// Route through the active transport (verbatim Tauri).
function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return getTransport().invoke<T>(cmd, args);
}

export type TaskStatus = "open" | "active" | "review" | "backlog" | "done" | "cancelled";
export type Priority = "low" | "normal" | "high" | "urgent";

export interface Task {
  id: number;
  title: string;
  description: string;
  status: TaskStatus;
  priority: Priority;
  assigned_to: string | null;
  created_by: string | null;
  tags: string[];
  branch: string | null;
  pr_url: string | null;
  created_at: string;
  updated_at: string;
}

// The Rust `TaskWithDeps` uses `#[serde(flatten)]` on its `task` field, so it
// serializes flat: every Task field at the top level plus the dep edges.
export interface TaskWithDeps extends Task {
  blocked_by: number[];
  blocks: number[];
}

export interface DepNode {
  id: number;
  depth: number;
  title: string;
  status: TaskStatus;
}

export interface LogEntry {
  created_at: string;
  worktree: string;
  action: string;
  detail: string;
}

export interface TaskDetail {
  task: TaskWithDeps;
  deps: DepNode[];
  logs: LogEntry[];
}

export interface DoneResult {
  task: Task;
  unblocked: number[];
}

/** Result of `task_claim`. `outcome` distinguishes a fresh claim from an
 * idempotent re-claim, and (for future eventually-consistent backends) flags a
 * submitted-but-unverified claim. */
export interface ClaimResult {
  outcome: "claimed" | "already_yours" | "unverified";
  task: Task;
}

/** Structured error surfaced by the task commands (mirrors the Rust `IpcError`).
 * `kind` is a machine-readable variant tag; `remediation` / `retry_after_secs`
 * carry the typed backend error's hints when present. */
export interface IpcError {
  kind: string;
  message: string;
  remediation: string | null;
  retry_after_secs: number | null;
}

/** Render a task-command rejection for display. Task Tauri commands reject with
 * a serialized {@link IpcError} object, not a string; `String(e)` on that yields
 * the useless `"[object Object]"`. This unwraps the structured error: it returns
 * the `message`, appending the `remediation` and a `retry in Ns` hint when the
 * backend supplied them. Non-object rejections (string-rejecting commands, thrown
 * `Error`s) fall through to `String(e)`. */
export function errorMessage(e: unknown): string {
  if (e && typeof e === "object" && typeof (e as { message?: unknown }).message === "string") {
    const ipc = e as Partial<IpcError>;
    let msg = ipc.message as string;
    if (typeof ipc.remediation === "string" && ipc.remediation) {
      msg += ` — ${ipc.remediation}`;
    }
    if (typeof ipc.retry_after_secs === "number") {
      msg += ` (retry in ${ipc.retry_after_secs}s)`;
    }
    return msg;
  }
  return String(e);
}

export interface TaskUpdatePatch {
  title?: string;
  description?: string;
  priority?: Priority;
  status?: TaskStatus;
  branch?: string;
  pr_url?: string;
}

// ---- Commands ----

/** The worktree this app acts as (the assignee for claim/done/unclaim). */
export function getActor(): Promise<string> {
  return invoke<string>("task_actor");
}

/** The board model plus the active project it was resolved from. The
 *  `project` stamp lets `useTasks` drop a late response from a project the user has
 *  already switched away from. The Tauri command serializes it directly. */
export interface TaskBoardResponse {
  project: string;
  tasks: TaskWithDeps[];
}

/** All non-archived, non-backlog tasks with their dep edges (the board model),
 *  stamped with the active project they belong to. */
export function listTasks(): Promise<TaskBoardResponse> {
  return invoke<TaskBoardResponse>("tasks_list", { filter: null });
}

/** The board fetched in a single backend call: tasks + dispatched reservations,
 *  stamped with the active project (superset of {@link TaskBoardResponse}). */
export interface BoardSnapshotResponse extends TaskBoardResponse {
  reservations: Reservation[];
  /** Backlog tasks (kept out of `tasks`); the Open lane shows them on its toggle. */
  backlog?: TaskWithDeps[];
  /** Pending cards whose latest send failed, with the reason. */
  failures?: DispatchFailure[];
}

/** Fetch the whole board — tasks + `sent ✓` reservations — in **one** backend call
 *  (`board_snapshot`), replacing the previously co-fired `tasks_list` +
 *  `dispatched_reservations`. */
export function loadBoard(): Promise<BoardSnapshotResponse> {
  return invoke<BoardSnapshotResponse>("board_snapshot", { filter: null });
}

/**
 * The tasks for `activeProject`, or `null` when the response describes a *different*
 * project — a superseded / late-arriving load from a project the user switched away
 * from. Returning `null` (rather than the foreign tasks) is what prevents the
 * stale-overwrite that put another project's cards on the active board.
 */
export function tasksForProject(
  resp: TaskBoardResponse | null,
  activeProject: string | null,
): TaskWithDeps[] | null {
  if (!resp || !activeProject) return null;
  return resp.project === activeProject ? resp.tasks : null;
}

/**
 * Per-project last-known board. Cross-project isolated; process-lifetime (a handful
 * of projects, no eviction needed) — the task-board analogue of `SessionCache`.
 */
export class TaskCache {
  private readonly _store = new Map<string, TaskWithDeps[]>();

  get(project: string): TaskWithDeps[] | undefined {
    return this._store.get(project);
  }

  /** Stores an independent copy so later mutation of the source can't corrupt the cache. */
  set(project: string, tasks: TaskWithDeps[]): void {
    this._store.set(project, [...tasks]);
  }
}

/** Full inspector detail (task + dependency tree + activity log), or null if gone. */
export function getTask(id: number): Promise<TaskDetail | null> {
  return invoke<TaskDetail | null>("task_get", { id });
}

export function claimTask(id: number, actor: string): Promise<ClaimResult> {
  return invoke<ClaimResult>("task_claim", { id, actor });
}

export function doneTask(id: number, actor: string): Promise<DoneResult> {
  return invoke<DoneResult>("task_done", { id, actor });
}

export function unclaimTask(id: number, actor: string): Promise<void> {
  return invoke("task_unclaim", { id, actor });
}

/** Move a task to the backlog (hidden from the board by default, never dispatched). */
export function backlogTask(id: number): Promise<void> {
  return invoke("task_backlog", { id });
}

/** Bring a backlog task back to open. */
export function reopenTask(id: number): Promise<void> {
  return invoke("task_reopen", { id });
}

export function updateTask(id: number, patch: TaskUpdatePatch): Promise<void> {
  return invoke("task_update", { id, patch });
}

/// Permanently delete a task (hard-delete on SQLite, tombstone on GitHub). Rejects
/// with a `Conflict` when other tasks depend on it.
export function deleteTask(id: number): Promise<void> {
  return invoke("task_delete", { id });
}

export function newTask(
  title: string,
  description: string,
  priority: Priority,
  tags: string[],
): Promise<Task> {
  return invoke<Task>("task_new", { title, description, priority, tags });
}

/** Fires when the task DB changes (this app, the CLI, or another worktree). */
export function onTasksChanged(cb: () => void): Promise<UnlistenFn> {
  return getTransport().listen("tasks://changed", () => cb());
}

/** Cheap revision probe for the gated periodic tick — returns the backend's current
 *  `revision()` token without fetching the whole board. Module-level so it is a
 *  stable reference across renders (safe to pass into `useLiveResource` options). */
export function fetchTasksRevision(): Promise<string> {
  return invoke<string>("tasks_revision");
}

/** Safety-net resync interval by backend kind. sqlite leans on the fs watcher (the
 *  poll is a pure net → 30 s); github's native push is dead for external changes so
 *  the gated poll is the *primary* freshness path, but each probe is ~1 GraphQL
 *  query so 5 min is affordable. Unknown/absent backend → sqlite tier. */
export function resyncIntervalForBackend(backend: string | null | undefined): number {
  return backend === "github" ? 300_000 : 30_000;
}

// ---- Hook ----

export interface UseTasks {
  tasks: TaskWithDeps[] | null;
  /** Dispatched-and-still-reserved (taskId, worktree) pairs the board paints as
   *  `sent ✓`. Fetched in the same `board_snapshot` call as `tasks`, so a board
   *  refresh is one backend call. Scoped to the active project like `tasks`. */
  reservations: Reservation[];
  /** Backlog tasks of the active project (empty until a matching response lands). */
  backlog: TaskWithDeps[];
  /** Standing failed sends of the active project. */
  failures: DispatchFailure[];
  error: string | null;
  reload: () => Promise<void>;
}

/**
 * Load the board, then keep it live by refetching on `tasks://changed`. Scoped to
 * `activeProject`: a response stamped with a different project (a late load
 * from a project the user already switched away from) is dropped rather than shown,
 * and the board is seeded from a per-project cache for an instant correct view on
 * switch-back. Mirrors `useSessions`.
 */
export function useTasks(activeProject: string | null, backend?: string | null): UseTasks {
  // Re-subscribe when the active project changes so `subscribeLive`'s revision gate
  // resets: its `lastRevision` is per-subscription, and carrying project A's token
  // into project B could gate out B's safety-net reload on a revision collision.
  // `loadBoard` is itself project-agnostic (it resolves the active project
  // backend-side); keying the callback on `activeProject` just forces a fresh
  // subscription + initial load on switch.
  // biome-ignore lint/correctness/useExhaustiveDependencies: `activeProject` is an intentional re-subscribe key, not a value `loadBoard` reads — changing it must recreate the subscription (resetting the revision gate) even though loadBoard resolves the project backend-side.
  const load = useCallback(() => loadBoard(), [activeProject]);
  const { data, error, reload } = useLiveResource(load, "tasks://changed", {
    listen: liveListen,
    // Backend-tiered safety-net poll, gated on the cheap revision probe so an
    // unchanged board is not refetched (see `fetchTasksRevision`).
    intervalMs: resyncIntervalForBackend(backend),
    revision: fetchTasksRevision,
  });

  // Per-project last-known board — survives re-renders, never replaced.
  const cacheRef = useRef<TaskCache>(new TaskCache());
  // Per-project last-known reservations, cached + seeded in lockstep with the task
  // board so a switch-back paints `sent ✓` chips instantly instead of briefly
  // showing a live Dispatch button (which invites a redundant dispatch click).
  const reservationsCacheRef = useRef<Map<string, Reservation[]>>(new Map());
  // The project whose real data we have observed this session. Until it matches
  // `activeProject`, a null `live` means "not loaded yet" (seed from cache).
  const loadedProjectRef = useRef<string | null>(null);

  // The live tasks for the active project, or null if the latest response is for
  // another project (superseded). Unlike sessions, the explicit project stamp makes
  // a matching EMPTY response a definitive "loaded empty", so we mark the project
  // loaded even when the list is empty (below).
  const live = useMemo(() => tasksForProject(data, activeProject), [data, activeProject]);

  // Cache + mark loaded whenever a project-matching response lands (including []).
  useEffect(() => {
    if (activeProject && live !== null) {
      cacheRef.current.set(activeProject, live);
      loadedProjectRef.current = activeProject;
    }
  }, [activeProject, live]);

  // Display: before the active project's first matching response lands, seed from the
  // per-project cache for an instant board on revisit. Once `live` is a real array
  // (even []), show it verbatim — a seed can never mask a genuine empty board. We gate
  // on `live === null`, so the size arg to the shared predicate is a literal 0 and the
  // operative guard is `loadedProject !== activeProject`.
  const tasks = useMemo(() => {
    if (live === null && shouldSeedFromCache(0, loadedProjectRef.current, activeProject)) {
      // biome-ignore lint/style/noNonNullAssertion: activeProject is non-null: guarded by shouldSeedFromCache
      return cacheRef.current.get(activeProject!) ?? null;
    }
    return live;
  }, [activeProject, live]);

  // Reservations ride the same response as tasks, scoped to the active project the
  // same way (a late response from a project the user switched away from is dropped
  // rather than painting stale `sent ✓` chips). `null` = no matching response yet.
  const liveReservations = useMemo<Reservation[] | null>(
    () => (data && activeProject && data.project === activeProject ? data.reservations : null),
    [data, activeProject],
  );

  // Cache reservations in lockstep with the task board (same matching-response gate).
  useEffect(() => {
    if (activeProject && liveReservations !== null) {
      reservationsCacheRef.current.set(activeProject, [...liveReservations]);
    }
  }, [activeProject, liveReservations]);

  // Seed reservations from cache on switch-back exactly like `tasks` (same predicate),
  // so the two stay consistent — a cached board never shows dispatched cards without
  // their `sent ✓` chip. Falls back to `[]` (not null) — the board wants an array.
  const reservations = useMemo<Reservation[]>(() => {
    if (
      liveReservations === null &&
      shouldSeedFromCache(0, loadedProjectRef.current, activeProject)
    ) {
      // biome-ignore lint/style/noNonNullAssertion: activeProject is non-null: guarded by shouldSeedFromCache
      return reservationsCacheRef.current.get(activeProject!) ?? [];
    }
    return liveReservations ?? [];
  }, [activeProject, liveReservations]);

  // Backlog + failures ride the same response; shown only for a matching project.
  const matching = data && activeProject && data.project === activeProject ? data : null;
  const backlog = useMemo(() => matching?.backlog ?? [], [matching]);
  const failures = useMemo(() => matching?.failures ?? [], [matching]);

  return { tasks, reservations, backlog, failures, error, reload };
}
