import { describe, expect, it } from "vitest";
import type { PerfMachine, PerfRow, PerfRowKind } from "./ipc";
import {
  buildPerfView,
  DEFAULT_PERF_SORT,
  ioOf,
  memFraction,
  nameOf,
  nextSort,
  type PerfSort,
  sparklinePoints,
  toggleInSet,
} from "./perf";

function row(
  kind: PerfRowKind,
  project: string | null,
  worktree: string | null,
  o: Partial<PerfRow> = {},
): PerfRow {
  const key =
    kind === "project"
      ? `project:${project}`
      : kind === "worktree"
        ? `worktree:${project}/${worktree}`
        : kind;
  return {
    key,
    kind,
    project,
    worktree,
    resolving: false,
    cpu_percent: 0,
    mem_bytes: 0,
    disk_read_bps: 0,
    disk_write_bps: 0,
    proc_count: 0,
    inferred_count: 0,
    history: { cpu: [], mem: [] },
    top_procs: [],
    ...o,
  };
}

const proj = (name: string, o: Partial<PerfRow> = {}) => row("project", name, null, o);
const wt = (p: string, w: string, o: Partial<PerfRow> = {}) => row("worktree", p, w, o);
const names = (rows: PerfRow[]) => rows.map((r) => r.worktree ?? r.project);

describe("nextSort", () => {
  it("flips direction on the same key", () => {
    expect(nextSort({ key: "cpu", dir: "desc" }, "cpu")).toEqual({ key: "cpu", dir: "asc" });
    expect(nextSort({ key: "cpu", dir: "asc" }, "cpu")).toEqual({ key: "cpu", dir: "desc" });
  });

  it("starts a new key at its natural direction", () => {
    expect(nextSort(DEFAULT_PERF_SORT, "mem")).toEqual({ key: "mem", dir: "desc" });
    expect(nextSort(DEFAULT_PERF_SORT, "io")).toEqual({ key: "io", dir: "desc" });
    expect(nextSort(DEFAULT_PERF_SORT, "name")).toEqual({ key: "name", dir: "asc" });
    expect(nextSort({ key: "name", dir: "desc" }, "cpu")).toEqual({ key: "cpu", dir: "desc" });
  });
});

describe("ioOf", () => {
  it("sums read and write", () => {
    expect(ioOf(wt("p", "a", { disk_read_bps: 3, disk_write_bps: 4 }))).toBe(7);
  });
});

describe("nameOf", () => {
  it("uses the worktree name for worktrees and the project name otherwise", () => {
    expect(nameOf(wt("p", "a"))).toBe("a");
    expect(nameOf(proj("p"))).toBe("p");
  });
});

describe("memFraction", () => {
  it("is the share of total memory, or 0 when the total is unknown", () => {
    expect(memFraction(25, { mem_total: 100 } as PerfMachine)).toBe(0.25);
    expect(memFraction(25, { mem_total: 0 } as PerfMachine)).toBe(0);
  });
});

describe("buildPerfView ordering", () => {
  const rows = [
    wt("p", "a", { cpu_percent: 1, mem_bytes: 300, disk_read_bps: 5 }),
    wt("p", "b", { cpu_percent: 9, mem_bytes: 100, disk_write_bps: 20 }),
    wt("p", "c", { cpu_percent: 5, mem_bytes: 200, disk_read_bps: 1 }),
    proj("p"),
  ];
  const order = (sort: PerfSort) => names(buildPerfView(rows, sort).groups[0].worktrees);

  it("sorts by each key and direction", () => {
    expect(order({ key: "cpu", dir: "desc" })).toEqual(["b", "c", "a"]);
    expect(order({ key: "cpu", dir: "asc" })).toEqual(["a", "c", "b"]);
    expect(order({ key: "mem", dir: "desc" })).toEqual(["a", "c", "b"]);
    expect(order({ key: "mem", dir: "asc" })).toEqual(["b", "c", "a"]);
    expect(order({ key: "io", dir: "desc" })).toEqual(["b", "a", "c"]);
    expect(order({ key: "io", dir: "asc" })).toEqual(["c", "a", "b"]);
    expect(order({ key: "name", dir: "asc" })).toEqual(["a", "b", "c"]);
    expect(order({ key: "name", dir: "desc" })).toEqual(["c", "b", "a"]);
  });

  it("breaks ties by name ascending regardless of direction", () => {
    const tied = [wt("p", "z", { cpu_percent: 2 }), wt("p", "m", { cpu_percent: 2 }), proj("p")];
    for (const dir of ["asc", "desc"] as const) {
      expect(names(buildPerfView(tied, { key: "cpu", dir }).groups[0].worktrees)).toEqual([
        "m",
        "z",
      ]);
    }
  });
});

describe("buildPerfView grouping", () => {
  it("groups worktrees under their project and orders projects by the sort", () => {
    const rows = [
      wt("p1", "a", { cpu_percent: 1 }),
      wt("p2", "b", { cpu_percent: 8 }),
      wt("p2", "c", { cpu_percent: 2 }),
      proj("p1", { cpu_percent: 1 }),
      proj("p2", { cpu_percent: 10 }),
    ];
    const { groups } = buildPerfView(rows, DEFAULT_PERF_SORT);
    expect(groups.map((g) => g.project.project)).toEqual(["p2", "p1"]);
    expect(names(groups[0].worktrees)).toEqual(["b", "c"]);
    expect(names(groups[1].worktrees)).toEqual(["a"]);
  });

  it("pins keitora, herdr, other in order and never sorts them", () => {
    const rows = [
      row("other", null, null, { cpu_percent: 90 }),
      row("herdr", null, null, { cpu_percent: 50 }),
      row("keitora", null, null, { cpu_percent: 1 }),
      proj("p"),
    ];
    for (const dir of ["asc", "desc"] as const) {
      const { pinned } = buildPerfView(rows, { key: "cpu", dir });
      expect(pinned.map((p) => p.row.kind)).toEqual(["keitora", "herdr", "other"]);
      expect(pinned.map((p) => p.label)).toEqual(["Keitora", "herdr", "Other"]);
    }
  });
});

describe("buildPerfView attributedShare", () => {
  it("excludes other and project rows from the attributed sum", () => {
    const rows = [
      proj("p", { cpu_percent: 6 }),
      wt("p", "a", { cpu_percent: 6 }),
      row("keitora", null, null, { cpu_percent: 2 }),
      row("herdr", null, null, { cpu_percent: 0 }),
      row("other", null, null, { cpu_percent: 2 }),
    ];
    expect(buildPerfView(rows, DEFAULT_PERF_SORT).attributedShare).toBeCloseTo(0.8);
  });

  it("is null when the total is below 0.05", () => {
    const rows = [
      wt("p", "a", { cpu_percent: 0.01 }),
      row("other", null, null, { cpu_percent: 0.02 }),
    ];
    expect(buildPerfView(rows, DEFAULT_PERF_SORT).attributedShare).toBeNull();
    expect(buildPerfView([], DEFAULT_PERF_SORT).attributedShare).toBeNull();
  });
});

describe("toggleInSet", () => {
  it("adds and removes without mutating the input", () => {
    const a = new Set(["x"]);
    const b = toggleInSet(a, "y");
    expect([...b].sort()).toEqual(["x", "y"]);
    expect([...a]).toEqual(["x"]);
    expect([...toggleInSet(b, "x")]).toEqual(["y"]);
  });
});

describe("sparklinePoints", () => {
  it("is empty for no samples", () => {
    expect(sparklinePoints([], 100, 20, 10)).toBe("");
  });

  it("right-aligns a single sample", () => {
    expect(sparklinePoints([5], 149, 20, 10)).toBe("149.00,10.00");
  });

  it("spans the full width for a full ring", () => {
    const pts = sparklinePoints(
      Array.from({ length: 150 }, () => 10),
      149,
      20,
      10,
    ).split(" ");
    expect(pts).toHaveLength(150);
    expect(pts[0]).toBe("0.00,0.00");
    expect(pts[149]).toBe("149.00,0.00");
  });

  it("handles a flat zero series without dividing by zero", () => {
    expect(sparklinePoints([0, 0], 149, 20, 0)).toBe("148.00,20.00 149.00,20.00");
  });
});
