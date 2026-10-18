// Pure view-model for the performance overlay: sorting, grouping, and sparkline geometry.
// No React, no Tauri.

import type { PerfMachine, PerfRow } from "./ipc";

export type PerfSortKey = "cpu" | "mem" | "io" | "name";

export interface PerfSort {
  key: PerfSortKey;
  dir: "asc" | "desc";
}

export const DEFAULT_PERF_SORT: PerfSort = { key: "cpu", dir: "desc" };

/** The next sort after clicking a column: same key flips direction, a new key starts at its
 *  natural direction (descending for numbers, ascending for names). */
export function nextSort(current: PerfSort, key: PerfSortKey): PerfSort {
  if (current.key === key) return { key, dir: current.dir === "asc" ? "desc" : "asc" };
  return { key, dir: key === "name" ? "asc" : "desc" };
}

/** Total disk throughput of a row, bytes per second. */
export function ioOf(row: PerfRow): number {
  return row.disk_read_bps + row.disk_write_bps;
}

export interface PerfGroup {
  project: PerfRow;
  worktrees: PerfRow[];
}

/** A pinned system row with its display label. */
export interface PerfPinned {
  row: PerfRow;
  label: string;
}

export interface PerfView {
  groups: PerfGroup[];
  /** `keitora`, `herdr`, `other` in that order; never sorted. */
  pinned: PerfPinned[];
  /** Share of CPU attributed to a known owner, or null when there is too little load. */
  attributedShare: number | null;
}

/** The pinned rows in display order, with their labels. */
const PINNED_ROWS = [
  { kind: "keitora", label: "Keitora" },
  { kind: "herdr", label: "herdr" },
  { kind: "other", label: "Other" },
] as const;

const nameCollator = new Intl.Collator();

/** Display name of a project or worktree row. */
export function nameOf(row: PerfRow): string {
  return row.kind === "worktree" ? (row.worktree ?? row.key) : (row.project ?? row.key);
}

/** `bytes` as a share of the machine's total memory; 0 when the total is unknown. */
export function memFraction(bytes: number, machine: PerfMachine): number {
  return machine.mem_total > 0 ? bytes / machine.mem_total : 0;
}

function metric(row: PerfRow, key: Exclude<PerfSortKey, "name">): number {
  if (key === "cpu") return row.cpu_percent;
  if (key === "mem") return row.mem_bytes;
  return ioOf(row);
}

function comparator(sort: PerfSort): (a: PerfRow, b: PerfRow) => number {
  const sign = sort.dir === "asc" ? 1 : -1;
  return (a, b) => {
    const byName = nameCollator.compare(nameOf(a), nameOf(b));
    if (sort.key === "name") return sign * byName;
    const d = metric(a, sort.key) - metric(b, sort.key);
    return d !== 0 ? sign * d : byName;
  };
}

export function buildPerfView(rows: PerfRow[], sort: PerfSort): PerfView {
  const cmp = comparator(sort);
  const projects = rows.filter((r) => r.kind === "project").sort(cmp);
  const byProject = new Map<string | null, PerfRow[]>();
  for (const r of rows) {
    if (r.kind !== "worktree") continue;
    const list = byProject.get(r.project);
    if (list) list.push(r);
    else byProject.set(r.project, [r]);
  }
  const groups = projects.map((project) => ({
    project,
    worktrees: (byProject.get(project.project) ?? []).sort(cmp),
  }));
  const pinned = PINNED_ROWS.flatMap(({ kind, label }) => {
    const row = rows.find((r) => r.kind === kind);
    return row ? [{ row, label }] : [];
  });

  let attributed = 0;
  let total = 0;
  for (const r of rows) {
    if (r.kind === "project") continue;
    total += r.cpu_percent;
    if (r.kind !== "other") attributed += r.cpu_percent;
  }
  return { groups, pinned, attributedShare: total < 0.05 ? null : attributed / total };
}

/** A copy of `set` with `key` added, or removed when already present. */
export function toggleInSet(set: Set<string>, key: string): Set<string> {
  const next = new Set(set);
  if (!next.delete(key)) next.add(key);
  return next;
}

/** SVG `points` for a right-aligned sparkline over `slots` positions: sample `i` of `n` sits at
 *  `x = (slots - n + i) / (slots - 1) * width`. `max` is clamped to at least 1e-9. */
export function sparklinePoints(
  values: number[],
  width: number,
  height: number,
  max: number,
  slots = 150,
): string {
  const n = values.length;
  const top = Math.max(max, 1e-9);
  return values
    .map((v, i) => {
      const x = ((slots - n + i) / (slots - 1)) * width;
      const y = height - (v / top) * height;
      return `${x.toFixed(2)},${y.toFixed(2)}`;
    })
    .join(" ");
}
