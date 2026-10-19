// Ordering of the rail's project groups while a drag reorder is in flight. The rail shows
// a dropped order straight away (an "override") and keeps it until the registry's own
// order catches up, or drops it when the registry changes under it. Pure, keyed by
// registry key.

import { arrayMove } from "@dnd-kit/sortable";
import { isNoopSlot, landingIndex } from "./railDrop";

/** Whether `a` and `b` hold the same keys in the same order. */
export function sameOrder(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((k, i) => k === b[i]);
}

/** Whether `a` and `b` hold the same set of keys, in any order. */
export function sameKeys(a: Iterable<string>, b: Iterable<string>): boolean {
  const as = new Set(a);
  const bs = new Set(b);
  return as.size === bs.size && [...bs].every((k) => as.has(k));
}

/**
 * The order after dropping `activeId` into insertion `slot` (the gap before the key at
 * that index; `keys.length` is the end), or `null` when nothing moves (the slot right
 * before or after the key itself, out of range, or the key unknown).
 */
export function moveToSlot(
  keys: readonly string[],
  activeId: string,
  slot: number,
): string[] | null {
  const from = keys.indexOf(activeId);
  if (from === -1 || slot < 0 || slot > keys.length) return null;
  if (isNoopSlot(slot, from)) return null;
  return arrayMove([...keys], from, landingIndex(slot, from));
}

/**
 * The override still worth showing once the registry reports `incoming`: `null` when the
 * registry already has that order (it caught up) or a different set of projects (one was
 * added or removed, so the dropped order is stale); otherwise the override itself.
 */
export function settleOverride(
  override: readonly string[] | null,
  incoming: readonly string[],
): readonly string[] | null {
  if (override === null) return null;
  if (sameOrder(override, incoming) || !sameKeys(override, incoming)) return null;
  return override;
}

/**
 * `items` in `order` (by `keyOf`). Items missing from `order` keep their relative order
 * at the end; keys in `order` with no item are skipped.
 */
export function applyOrder<T>(
  items: readonly T[],
  order: readonly string[] | null,
  keyOf: (item: T) => string,
): T[] {
  if (order === null) return [...items];
  const byKey = new Map(items.map((it) => [keyOf(it), it]));
  const out: T[] = [];
  for (const k of order) {
    const it = byKey.get(k);
    if (it !== undefined) {
      out.push(it);
      byKey.delete(k);
    }
  }
  for (const it of items) if (byKey.has(keyOf(it))) out.push(it);
  return out;
}

/**
 * Wrap `persist` so calls run one at a time, in call order, and a call made while one is
 * in flight replaces any call still waiting: rapid drops land in order and the latest
 * one wins. A replaced call resolves without running; the others settle with their own
 * run's outcome.
 */
export function latestWins<A>(persist: (arg: A) => Promise<void>): (arg: A) => Promise<void> {
  type Waiting = { arg: A; resolve: () => void; reject: (e: unknown) => void };
  let busy = false;
  let waiting: Waiting | null = null;
  const run = async ({ arg, resolve, reject }: Waiting) => {
    busy = true;
    try {
      await persist(arg);
      resolve();
    } catch (e) {
      reject(e);
    }
    const next = waiting;
    waiting = null;
    if (next === null) busy = false;
    else void run(next);
  };
  return (arg) =>
    new Promise<void>((resolve, reject) => {
      if (!busy) {
        void run({ arg, resolve, reject });
        return;
      }
      waiting?.resolve();
      waiting = { arg, resolve, reject };
    });
}

/**
 * `latestWins` per key: calls with the same key run one at a time and the latest waiting
 * one wins; calls with different keys run independently.
 */
export function latestWinsBy<A>(
  persist: (key: string, arg: A) => Promise<void>,
): (key: string, arg: A) => Promise<void> {
  const queues = new Map<string, (arg: A) => Promise<void>>();
  return (key, arg) => {
    let queue = queues.get(key);
    if (queue === undefined) {
      queue = latestWins((a: A) => persist(key, a));
      queues.set(key, queue);
    }
    return queue(arg);
  };
}

// Worktree order within each project group, by the same override scheme: a dropped
// order shows at once, per group (registry key), until the group's reported lanes
// catch up.

/** Dropped worktree orders not yet reported back, by group key. */
export type LaneOverrides = ReadonlyMap<string, readonly string[]>;

/**
 * `groups` with each overridden group's lanes in its override order. A group with no
 * override is returned as the same object.
 */
export function applyLaneOrders<G extends { key: string; lanes: readonly { name: string }[] }>(
  groups: readonly G[],
  overrides: LaneOverrides,
): G[] {
  if (overrides.size === 0) return [...groups];
  return groups.map((g) => {
    const order = overrides.get(g.key);
    return order === undefined ? g : { ...g, lanes: applyOrder(g.lanes, order, (l) => l.name) };
  });
}

/** Each group's worktree names in lane order, by group key. */
export function laneNamesByGroup(
  groups: readonly { key: string; lanes: readonly { name: string }[] }[],
): Map<string, string[]> {
  return new Map(groups.map((g) => [g.key, g.lanes.map((l) => l.name)]));
}

/**
 * The overrides still worth showing once each group reports the worktree names in
 * `incoming`: `settleOverride` per group, and none for a group no longer reported. The
 * same map when nothing changes.
 */
export function settleLaneOverrides(
  overrides: LaneOverrides,
  incoming: ReadonlyMap<string, readonly string[]>,
): LaneOverrides {
  let changed = false;
  const next = new Map<string, readonly string[]>();
  for (const [key, order] of overrides) {
    const reported = incoming.get(key);
    const kept = reported === undefined ? null : settleOverride(order, reported);
    if (kept === null) changed = true;
    else next.set(key, kept);
  }
  return changed ? next : overrides;
}

/**
 * The drag id of worktree `name` in group `groupKey`. It starts with a NUL character, so
 * it differs from every group's id (its registry key, a folder or project name) that does
 * not, and separates the two parts with one, so ids for different pairs differ.
 */
export function laneDragId(groupKey: string, name: string): string {
  return `\0lane\0${groupKey}\0${name}`;
}
