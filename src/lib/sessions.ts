// Live worktree session state (idle/busy/waiting/offline), recorded by per-worktree
// Claude Code hooks and surfaced by the backend. Mirrors lib/tasks.ts.

// `listen` is still used directly by the desktop-only `useDaemonSessions` (it only
// runs when the daemon is Connected). Session reads/subs go through the transport seam.
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  type DaemonInfo,
  type DaemonSessionHint,
  type DaemonStats,
  daemonListSessions,
  daemonStats,
  daemonStatus,
  HEALTHY_RECOVERY,
  type RecoveryInfo,
  recoveryStatus,
  type WorktreeActivity,
  worktreeActivity,
} from "./ipc";
import { shouldSeedFromCache } from "./projectScope";
import { getTransport, liveListen } from "./transport";
import { type SubscribeOptions, useLiveResource } from "./useLiveResource";

// Route through the active transport (verbatim Tauri).
function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return getTransport().invoke<T>(cmd, args);
}

export type SessionState = "idle" | "busy" | "waiting" | "offline";

export interface WorktreeSession {
  project: string; // daemon namespace; stamped by sessions_list
  worktree: string;
  state: SessionState;
  current_task_id: number | null;
  pid: number | null;
  last_heartbeat: string;
  /** The worktree driving this one; absent/null when it is not driven. */
  driven_by?: string | null;
  /** Waiting on a brokered prompt the driver is deciding (absent means false). */
  brokered?: boolean;
  /** While waiting on a prompt escalated to the human, why it was escalated. */
  escalated_reason?: string | null;
  /** Agent presence from the session engine; absent/null when no engine view was overlaid. */
  agent?: "present" | "starting" | "absent" | null;
  /** herdr's own agent status for the pane, when an engine view was overlaid. */
  herdr_status?: string | null;
  /** The state could not be confirmed against a fresh herdr snapshot (absent means false). */
  stale?: boolean;
}

/** The agent is still launching: its `busy` is not a turn, so a later `idle` is no finished turn. */
export function isStarting(s: Pick<WorktreeSession, "agent">): boolean {
  return s.agent === "starting";
}

/** All known worktree session states (PTY-ownership reconciled by the backend). */
export function listSessions(): Promise<WorktreeSession[]> {
  return invoke<WorktreeSession[]>("sessions_list");
}

/** Fires when any worktree's session *state* changes (heartbeats are deduped). */
export function onSessionsChanged(cb: () => void): Promise<UnlistenFn> {
  return getTransport().listen("sessions://changed", () => cb());
}

// ---------------------------------------------------------------------------
// Pure helpers — no React imports, fully unit-testable.
// ---------------------------------------------------------------------------

/**
 * Filter session rows to only those belonging to the given project (A2).
 * Returns [] for a null/empty project, or when no rows match.
 */
export function rowsForProject(
  rows: WorktreeSession[],
  project: string | null | undefined,
): WorktreeSession[] {
  if (!project) return [];
  return rows.filter((r) => r.project === project);
}

/**
 * Build a worktree-keyed map from a flat list of session rows.
 * Last entry wins when the same worktree name appears more than once.
 */
export function sessionsByWorktree(rows: WorktreeSession[]): Map<string, WorktreeSession> {
  return new Map(rows.map((s) => [s.worktree, s]));
}

/**
 * Per-project last-known sessions cache (C).
 * Backed by Map<project, Map<worktree, WorktreeSession>>; cross-project isolated.
 * No eviction needed (handful of projects; process-lifetime).
 */
export class SessionCache {
  private readonly _store = new Map<string, Map<string, WorktreeSession>>();

  get(project: string): Map<string, WorktreeSession> | undefined {
    return this._store.get(project);
  }

  /** Stores an immutable snapshot (new Map copy) so later mutation can't corrupt the cache. */
  set(project: string, map: Map<string, WorktreeSession>): void {
    this._store.set(project, new Map(map));
  }
}

// `shouldSeedFromCache` now lives in `./projectScope` (shared with the task board).

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export interface UseSessionsResult {
  /** Display map: cache-seeded for instant lights on project revisit (C). */
  sessions: Map<string, WorktreeSession>;
  /**
   * The uncached, project-filtered live map. Drives notification state
   * (`useAttention`) so a cache seed can never manufacture a false busy→idle
   * "finished" transition on switch-back. Empty until the
   * active project's real data lands.
   */
  liveSessions: Map<string, WorktreeSession>;
  reload: () => Promise<void>;
}

/**
 * Load + live-update worktree sessions, keyed by worktree name for easy lookup.
 * Scoped to `activeProject`: rows from other projects are dropped (A2), so a
 * stale pre-switch response can never corrupt the active project's lights.
 *
 * On project change the map is seeded from a per-project last-known cache (C)
 * for instant lights on revisit, then updated once the reload lands.
 *
 * NOTE: `sessions_list` returns active-project rows only (each stamped
 * with the active project name in `WorktreeSession.project`), so the bare worktree
 * name is unambiguous within this map. If a future change surfaces sessions from
 * more than one project at once, key this map by `project/worktree` instead.
 */
export function useSessions(activeProject: string | null): UseSessionsResult {
  // Poll every 3s (vs the 30s default): a session running a background task is
  // detected by the daemon on-demand per `sessions_list` and emits no
  // `sessions://changed` event, so this interval — not the event — is what
  // surfaces the idle→busy flip.
  // `listen: liveListen` routes the subscription through the transport seam.
  const { data, reload } = useLiveResource(listSessions, "sessions://changed", {
    intervalMs: 3_000,
    listen: liveListen,
  });

  // Per-project last-known cache — survives re-renders, never replaced.
  const cacheRef = useRef<SessionCache>(new SessionCache());
  // The project whose real (non-empty) data we have observed this session. Until
  // this matches `activeProject`, an empty filtered map means "not loaded yet"
  // (seed from cache); once it matches, an empty map is a genuine empty state.
  const loadedProjectRef = useRef<string | null>(null);

  // Derive the filtered, project-scoped map from raw rows (A2). This is the
  // LIVE map — uncached — and is what drives notification state.
  const filteredMap = useMemo(() => {
    const filtered = rowsForProject(data ?? [], activeProject);
    return sessionsByWorktree(filtered);
  }, [data, activeProject]);

  // Write non-empty maps into the cache and record that this project has loaded.
  useEffect(() => {
    if (activeProject && filteredMap.size > 0) {
      cacheRef.current.set(activeProject, filteredMap);
      loadedProjectRef.current = activeProject;
    }
  }, [activeProject, filteredMap]);

  // Display map: before the active project's first real load, seed from the
  // per-project cache for instant lights on revisit (A1+C). After it has loaded,
  // show the live map verbatim — including a genuine empty.
  const sessions = useMemo(() => {
    if (shouldSeedFromCache(filteredMap.size, loadedProjectRef.current, activeProject)) {
      // biome-ignore lint/style/noNonNullAssertion: activeProject is non-null: guarded by shouldSeedFromCache check
      return cacheRef.current.get(activeProject!) ?? filteredMap;
    }
    return filteredMap;
  }, [activeProject, filteredMap]);

  return { sessions, liveSessions: filteredMap, reload };
}

/** The default DaemonInfo returned before the first status is known. */
const DEFAULT_DAEMON_INFO: DaemonInfo = {
  state: "NeverStarted",
  pid: null,
  started_at: null,
  session_count: 0,
  log_path: null,
  daemon_version: null,
  restartable: false,
};

/**
 * Daemon status and stats refresh only on their events, with no resync timer: the
 * backend's status loop pushes `daemon://status` on every change plus a 30 s
 * heartbeat (the safety net for a dropped event), and `daemon://stats` on every
 * sample. Reloads are cheap reads of the backend's cache.
 */
export const DAEMON_EVENTS_ONLY: SubscribeOptions = { intervalMs: 0 };

/**
 * Live daemon connection status. Follows the same subscribe-then-snapshot
 * pattern as `useSessions` via `useLiveResource`, refreshed by `daemon://status`
 * events only (see {@link DAEMON_EVENTS_ONLY}).
 *
 * Returns DEFAULT_DAEMON_INFO (state: "NeverStarted") before the first
 * `daemonStatus()` call resolves or before any `daemon://status` event arrives.
 */
export function useDaemonStatus(): DaemonInfo {
  const { data } = useLiveResource(daemonStatus, "daemon://status", DAEMON_EVENTS_ONLY);
  return data ?? DEFAULT_DAEMON_INFO;
}

/**
 * Live herdr server recovery state (`daemon://recovery`), same pattern as
 * `useDaemonStatus`. Healthy until the first snapshot arrives.
 */
export function useRecovery(): RecoveryInfo {
  const { data } = useLiveResource(recoveryStatus, "daemon://recovery");
  return data ?? HEALTHY_RECOVERY;
}

/** The stats loader while disabled: never asks the backend. Module-level, so stable. */
const loadNoStats = (): Promise<DaemonStats | null> => Promise.resolve(null);

/**
 * What {@link useDaemonStats} loads: the backend's cached stats while `enabled`, else
 * nothing. Both are stable function identities, so the subscription only changes when
 * `enabled` does.
 */
export function daemonStatsLoader(enabled: boolean): () => Promise<DaemonStats | null> {
  return enabled ? daemonStats : loadNoStats;
}

/**
 * The daemon's live process stats (cpu/memory/uptime/version) while `enabled`. The
 * initial value is the backend's cached sample; updates arrive as `daemon://stats`
 * events (no polling). Returns `null` until the first sample resolves (and always while
 * disabled), so the rail card can render `—` placeholders without a layout shift. A
 * failed read keeps the last sample.
 *
 * Mount this hook only while the daemon is Connected (e.g. inside a Connected-only child),
 * so the subscription is naturally scoped and ends the moment the daemon drops.
 */
export function useDaemonStats(enabled: boolean): DaemonStats | null {
  const { data } = useLiveResource(
    daemonStatsLoader(enabled),
    "daemon://stats",
    DAEMON_EVENTS_ONLY,
  );
  return enabled ? (data ?? null) : null;
}

/**
 * Poll per-worktree activity (workflow stage, git branch, UBT build progress, working flag)
 * for EVERY project on a stable interval, keyed `` `${project}/${worktree}` ``. Plain interval
 * (a cheap sampled snapshot, no backend event).
 *
 * Deliberately NOT scoped to the active project and NEVER reset on a project switch: the map
 * stays warm so switching projects renders the new lanes instantly from the last sample
 * instead of blanking out until the next tick. A transient fetch error keeps the last sample
 * (stale-while-error) rather than clearing the lanes.
 */
export function useWorktreeActivity(intervalMs = 2500): Map<string, WorktreeActivity> {
  const [map, setMap] = useState<Map<string, WorktreeActivity>>(() => new Map());

  useEffect(() => {
    let cancelled = false;
    const tick = () => {
      void worktreeActivity()
        .then((rows) => {
          if (!cancelled) setMap(new Map(rows.map((r) => [`${r.project}/${r.worktree}`, r])));
        })
        .catch(() => {
          // Transient (e.g. the command raced a project-config reload): keep the last
          // sample so the lanes don't flicker empty, and retry next tick.
        });
    };
    tick(); // fetch immediately so lanes fill on mount instead of after the first interval
    const id = window.setInterval(tick, intervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(id);
    };
  }, [intervalMs]);

  return map;
}

/**
 * Live list of all daemon sessions across all projects. Refreshes whenever
 * any session changes OR whenever the daemon connection state changes (#4).
 *
 * Guards against RPCing while not Connected (#6): returns [] when the daemon
 * is not Connected, avoiding repeated `NotConnected` round-trips and log noise
 * while stopped/disconnected. The list is cleared immediately on disconnect/stop
 * and repopulated when the daemon reconnects.
 */
export function useDaemonSessions(): DaemonSessionHint[] {
  const { state } = useDaemonStatus();
  const [data, setData] = useState<DaemonSessionHint[]>([]);

  // On every daemon state change, either clear the list (not Connected) or
  // fetch the current list (Connected). This covers restart/stop transitions
  // that may never emit a sessions://changed DB event (#4).
  useEffect(() => {
    if (state !== "Connected") {
      setData([]);
      return;
    }
    let cancelled = false;
    void daemonListSessions()
      .then((sessions) => {
        if (!cancelled) setData(sessions);
      })
      .catch(() => {
        // Race: daemon became disconnected before response arrived — leave list empty.
      });
    return () => {
      cancelled = true;
    };
  }, [state]);

  // Subscribe to sessions://changed for DB-driven updates while Connected.
  // The subscription is torn down and re-established on state changes so it
  // is always live when Connected and never fires when not Connected.
  useEffect(() => {
    if (state !== "Connected") return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listen("sessions://changed", () => {
      if (cancelled) return;
      void daemonListSessions()
        .then((sessions) => {
          if (!cancelled) setData(sessions);
        })
        .catch(() => {});
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [state]);

  return data;
}

// Re-export for convenience — consumers can import from sessions.ts or ipc.ts.
export type { DaemonInfo, DaemonSessionHint } from "./ipc";
