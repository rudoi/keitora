// Load a resource from the backend, then keep it live by refetching whenever a
// given Tauri event fires. Captures the load + subscribe + cancel-safe teardown
// pattern shared by tasks, sessions, and worktrees. Pass a STABLE `load` (a
// module-level function or a useCallback) so the subscription isn't torn down and
// recreated on every render.
//
// The lifecycle lives in the framework-agnostic `subscribeLive` so it can be unit
// tested without React/jsdom; `useLiveResource` is a thin wrapper over it.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";

export interface LiveResource<T> {
  data: T | null;
  error: string | null;
  reload: () => Promise<void>;
}

type ListenFn = typeof listen;

export interface SubscribeOptions {
  /** Periodic re-sync interval in ms. Default `DEFAULT_RESYNC_MS`. `<= 0` disables polling. */
  intervalMs?: number;
  /** Injectable listen, for tests; defaults to the real Tauri `listen`. */
  listen?: ListenFn;
  /** Optional cheap revision probe for the periodic tick. When provided, the tick
   *  fetches the revision first and skips the full `load()` when it is unchanged (a
   *  failed probe falls through to a full reload, degrading to the old blind poll).
   *  When omitted, the tick reloads blindly every interval — correct for consumers
   *  with no revision command (e.g. worktrees). Shares `lastRevision` with the
   *  event-gating below, so an event-driven reload also suppresses the next tick. */
  revision?: () => Promise<string>;
}

/**
 * A `listen` that attaches the same handler to `extra` events too, for a resource that
 * reloads on more than one event. Its unlisten detaches every listener; if any attach
 * fails, the ones that succeeded are detached and the failure is rethrown.
 */
export function listenAlso(extra: readonly string[], base: ListenFn = listen): ListenFn {
  const combined = async (...[event, handler, options]: Parameters<ListenFn>) => {
    const attached = await Promise.allSettled(
      [event, ...extra].map((name) => base(name, handler, options)),
    );
    const uns = attached.flatMap((r) => (r.status === "fulfilled" ? [r.value] : []));
    const failed = attached.find((r) => r.status === "rejected");
    if (failed !== undefined) {
      for (const u of uns) u();
      throw failed.reason;
    }
    return () => {
      for (const u of uns) u();
    };
  };
  return combined as ListenFn;
}

/** Default periodic re-sync interval — a safety net for any dropped change event. */
export const DEFAULT_RESYNC_MS = 30_000;

export type SubscribeResult<T> = { data: T } | { error: string };

/**
 * Orders overlapping async loads: call `begin()` as a load starts, then the returned
 * `settle()` once it has its result; `settle()` is true when the result should be
 * applied, false when a later-started load has already settled (so this one is stale).
 * A later load that is merely still in flight does not hold an earlier one back.
 */
export function newestSettledGate(): { begin: () => () => boolean } {
  let next = 0;
  let settled = -1;
  return {
    begin: () => {
      const ticket = next++;
      return () => {
        if (ticket < settled) return false;
        settled = ticket;
        return true;
      };
    },
  };
}

/** The teardown `subscribeLive` returns, plus a manual reload ordered with its loads. */
export type LiveSubscription = (() => void) & { reload: () => Promise<void> };

// Keep a backend resource live: attach the change listener FIRST, then run the
// initial load (so an event arriving during setup can't be missed), and re-sync on
// an interval as a fallback for dropped events. Returns a synchronous teardown that
// also carries `reload`, a manual refetch ordered with the subscription's own loads.
//
// Loads can overlap (an event lands while the tick's load is in flight, a manual
// reload races both) and a slow backend can settle them out of order. Each load takes
// an increasing ticket, and a result is applied only when no later-started load has
// already settled, so an older response can never overwrite a newer one. A later load
// that is merely still in flight does not hold an earlier result back.
export function subscribeLive<T>(
  load: () => Promise<T>,
  eventName: string,
  onResult: (r: SubscribeResult<T>) => void,
  options: SubscribeOptions = {},
): LiveSubscription {
  const intervalMs = options.intervalMs ?? DEFAULT_RESYNC_MS;
  const listenFn = options.listen ?? listen;
  const revisionFn = options.revision;

  let cancelled = false;
  let un: UnlistenFn | undefined;
  let timer: ReturnType<typeof setInterval> | undefined;
  // True while a periodic tick's probe/reload is still outstanding. The tick returns early
  // when set, so a slow backend (e.g. a multi-second scan on a large repo) can't stack up
  // overlapping probes/loads — the interval becomes "at most one in flight", not "one every
  // interval regardless". Event-driven and manual reloads are unaffected.
  let tickBusy = false;
  // The last change-revision we have reloaded for. Shared by the event callback and
  // the periodic tick so a redundant refetch is skipped when nothing changed. A
  // `null`/unit payload (no revision) never sets or gates on it.
  let lastRevision: string | undefined;
  // Drops a load's result when a later-started load has already settled.
  const gate = newestSettledGate();

  // Run the load and, on SUCCESS ONLY, commit `pendingRevision` into `lastRevision`.
  // Advancing the gate only after a successful reload is what keeps a transient load
  // failure from poisoning it: a failed load leaves `lastRevision` untouched, so the
  // next same-revision event/tick retries instead of being gated out (the safety-net
  // poll must self-heal). `undefined` pending (unit payload / blind tick) never gates.
  // A result superseded by a newer settled load is dropped without touching
  // `lastRevision`, so the revision it carried is still reloaded later.
  const run = async (pendingRevision?: string) => {
    const settle = gate.begin();
    let result: SubscribeResult<T>;
    try {
      result = { data: await load() };
    } catch (e) {
      result = { error: String(e) };
    }
    if (cancelled || !settle()) return;
    if ("data" in result && pendingRevision !== undefined) lastRevision = pendingRevision;
    onResult(result);
  };

  // Listen first, then load — any event after attach triggers a reload, and the
  // initial load captures current state, so there is no miss window. If attaching
  // the listener fails, surface the error but still run the initial load (so a
  // listen() failure can't leave the view permanently empty).
  void listenFn(eventName, (event) => {
    // Gate only when the payload carries a revision (tasks/sessions `{ revision }`);
    // a null/unit payload (worktrees) has no revision → must always run(). Skip only
    // when a revision is present AND unchanged since the last reload.
    const rev = (event.payload as { revision?: string } | null)?.revision ?? undefined;
    if (rev !== undefined && rev === lastRevision) return;
    void run(rev);
  })
    .then(
      (u) => {
        if (cancelled)
          u(); // unmounted while listen() was still resolving
        else un = u;
      },
      (e) => {
        if (!cancelled) onResult({ error: String(e) });
      },
    )
    .then(() => {
      if (!cancelled) void run();
    });

  if (intervalMs > 0) {
    // Revision-gated tick: probe the cheap revision first and skip the full reload
    // when nothing changed. Without a probe, reload blindly (old behavior). Either
    // way the poll is the safety net for a dropped event / dead watcher / quiet
    // reconnect, so it MUST stay non-zero.
    timer = setInterval(() => {
      // Skip this tick entirely if the previous one is still working — no overlap.
      if (tickBusy) return;
      tickBusy = true;
      void (async () => {
        try {
          if (!revisionFn) {
            await run();
            return;
          }
          let rev: string;
          try {
            rev = await revisionFn();
          } catch {
            // revision probe failed (e.g. the backend has no revision command) —
            // fall through to a full blind reload, exactly as the pre-gating poll
            // did (no gate advance).
            if (!cancelled) await run();
            return;
          }
          if (rev === lastRevision) return; // nothing changed since last reload
          // Pass the probed revision as pending: `run` commits it only if the reload
          // succeeds, so a failed reload is retried on the next tick.
          if (!cancelled) await run(rev);
        } finally {
          tickBusy = false;
        }
      })();
    }, intervalMs);
  }

  const teardown = () => {
    cancelled = true;
    if (timer !== undefined) clearInterval(timer);
    un?.();
  };
  return Object.assign(teardown, { reload: () => run() });
}

export function useLiveResource<T>(
  load: () => Promise<T>,
  eventName: string,
  options?: SubscribeOptions,
): LiveResource<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);

  const apply = useCallback((r: SubscribeResult<T>) => {
    if ("data" in r) {
      setError(null);
      setData(r.data);
    } else {
      setError(r.error);
    }
  }, []);

  // The live subscription, so a manual reload is ordered with its loads.
  const subRef = useRef<LiveSubscription | null>(null);

  // Manual refresh, retained for consumers that call it directly (e.g. reloadWorktrees).
  // Goes through the subscription so a slow manual load can't overwrite a newer event
  // or tick result (or vice versa); before the subscription exists it loads directly.
  const reload = useCallback(async () => {
    const sub = subRef.current;
    if (sub) return sub.reload();
    try {
      apply({ data: await load() });
    } catch (e) {
      apply({ error: String(e) });
    }
  }, [load, apply]);

  const intervalMs = options?.intervalMs;
  const listenOpt = options?.listen;
  const revisionOpt = options?.revision;
  useEffect(() => {
    const sub = subscribeLive(load, eventName, apply, {
      intervalMs,
      listen: listenOpt,
      revision: revisionOpt,
    });
    subRef.current = sub;
    return () => {
      if (subRef.current === sub) subRef.current = null;
      sub();
    };
  }, [load, eventName, apply, intervalMs, listenOpt, revisionOpt]);

  return { data, error, reload };
}
