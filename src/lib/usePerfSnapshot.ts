// Subscription hook for the performance overlay: keeps the backend sampling lease alive
// while mounted and surfaces the latest snapshot.

import { useEffect } from "react";
import { type PerfSnapshot, perfSnapshot } from "./ipc";
import { releasePerfLease, subscribePerfLease } from "./perfLease";
import { DAEMON_EVENTS_ONLY } from "./sessions";
import { useLiveResource } from "./useLiveResource";

/** Renew well inside the backend's 10 s lease. */
const LEASE_RENEW_MS = 4000;

/**
 * The latest performance snapshot (`null` before the first sample). Holds the sampling lease
 * while mounted: subscribes immediately, renews every 4 s, and releases on unmount. Lease calls
 * reach the backend in issue order, so a release from one mount never overtakes the next
 * mount's subscribe. Updates arrive as `perf://snapshot` events (payload-less; each one re-pulls
 * the cached snapshot).
 */
export function usePerfSnapshot(): PerfSnapshot | null {
  const { data } = useLiveResource(perfSnapshot, "perf://snapshot", DAEMON_EVENTS_ONLY);

  useEffect(() => {
    const renew = () => {
      void subscribePerfLease();
    };
    renew();
    const id = setInterval(renew, LEASE_RENEW_MS);
    return () => {
      clearInterval(id);
      void releasePerfLease();
    };
  }, []);

  return data ?? null;
}
