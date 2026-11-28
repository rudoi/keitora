// Owns the health monitor's findings for the whole app: seeds them from `health_list`
// and follows `health://changed`. The rail glyphs and the Settings Health section read
// them through `useHealth()` / `useHealthFindings()`. The findings live in a store
// outside React so a scoped reader re-renders only when its own scope changes.

import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  useSyncExternalStore,
} from "react";
import {
  createHealthStore,
  type HealthFinding,
  type HealthStore,
  scopedSelector,
  subscribeHealth,
} from "../lib/health";

const Ctx = createContext<HealthStore>(createHealthStore());

/** Every finding (active and recently cleared). */
export function useHealthFindings(): HealthFinding[] {
  const store = useContext(Ctx);
  return useSyncExternalStore(store.subscribe, store.get, store.get);
}

/** The active findings of one worktree, or of a whole project when `worktree` is
 *  omitted (its worktrees' and its own); the same array while they are unchanged. */
export function useHealth(project: string, worktree?: string): HealthFinding[] {
  const store = useContext(Ctx);
  const select = useMemo(() => scopedSelector(project, worktree), [project, worktree]);
  const snapshot = useCallback(() => select(store.get()), [select, store]);
  return useSyncExternalStore(store.subscribe, snapshot, snapshot);
}

export function HealthProvider({ children }: { children: ReactNode }) {
  const [store] = useState(() => createHealthStore());
  useEffect(() => subscribeHealth(store.set), [store]);
  return <Ctx.Provider value={store}>{children}</Ctx.Provider>;
}
