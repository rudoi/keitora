// The user registry's `[rail]` view (worktree sort, emoji filter, the emoji switch), live
// on `rail://changed`. An update shows at once and holds until the registry reports
// back; a failed save drops it again and rethrows.

import { useCallback, useEffect, useMemo, useState } from "react";
import { getRailView, type RailView, setRailView } from "./ipc";
import { useLiveResource } from "./useLiveResource";
import { DEFAULT_RAIL_VIEW } from "./worktreeOrder";

export function useRailView(): {
  view: RailView;
  update: (patch: Partial<RailView>) => Promise<void>;
} {
  const { data } = useLiveResource(getRailView, "rail://changed");
  const [pending, setPending] = useState<Partial<RailView> | null>(null);
  // biome-ignore lint/correctness/useExhaustiveDependencies: a fresh read settles the pending patch
  useEffect(() => setPending(null), [data]);
  const view = useMemo(() => ({ ...(data ?? DEFAULT_RAIL_VIEW), ...pending }), [data, pending]);
  const update = useCallback(async (patch: Partial<RailView>) => {
    setPending((cur) => ({ ...cur, ...patch }));
    try {
      await setRailView(patch);
    } catch (e) {
      setPending(null);
      throw e;
    }
  }, []);
  return { view, update };
}
