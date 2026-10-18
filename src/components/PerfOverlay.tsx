// Performance overlay: a fixed panel over the stage that breaks CPU, memory and disk IO down
// by project → worktree → process, live from the backend sampler while it is mounted.
// Mounted only while open, so sort and expansion state reset each time it is closed.

import { Activity, ChevronRight, X } from "lucide-react";
import { Fragment, memo, useMemo, useState } from "react";
import { formatBytes, formatCpu, formatRate } from "../lib/format";
import type { PerfMachine, PerfProc, PerfRow } from "../lib/ipc";
import {
  buildPerfView,
  DEFAULT_PERF_SORT,
  memFraction,
  nameOf,
  nextSort,
  type PerfSort,
  type PerfSortKey,
  sparklinePoints,
  toggleInSet,
} from "../lib/perf";
import { usePerfSnapshot } from "../lib/usePerfSnapshot";

const SPARK_W = 64;
const SPARK_H = 18;
/** CPU sparklines never scale below 5 %, so an idle row reads as flat rather than noisy. */
const CPU_SPARK_FLOOR = 5;

const INFERRED_TITLE = "attributed by path, not by pane";

interface SparkData {
  values: number[];
  /** Lower bound for the scale, so a near-idle series reads as flat rather than noisy. */
  floor?: number;
}

const Sparkline = memo(function Sparkline({ values, floor = 0 }: SparkData) {
  const max = values.reduce((m, v) => Math.max(m, v), floor);
  return (
    <svg
      className="perf-spark"
      width={SPARK_W}
      height={SPARK_H}
      viewBox={`0 0 ${SPARK_W} ${SPARK_H}`}
      aria-hidden="true"
    >
      <polyline
        points={sparklinePoints(values, SPARK_W, SPARK_H, max)}
        fill="none"
        stroke="currentColor"
        strokeWidth={1.5}
        strokeLinejoin="round"
        strokeLinecap="round"
      />
    </svg>
  );
});

/** A value with a proportional bar under it and, for rows with history, a sparkline. */
function MetricCell({
  kind,
  text,
  fraction,
  spark,
}: {
  kind: "cpu" | "mem";
  text: string;
  fraction: number;
  spark?: SparkData;
}) {
  const pct = Math.max(0, Math.min(1, Number.isFinite(fraction) ? fraction : 0)) * 100;
  return (
    <td className={`perf-metric perf-${kind}`}>
      {spark ? (
        <Sparkline values={spark.values} floor={spark.floor} />
      ) : (
        <span className="perf-spark" />
      )}
      <span className="perf-num">{text}</span>
      <span className="perf-bar" aria-hidden="true">
        <span className="perf-bar-fill" style={{ width: `${pct}%` }} />
      </span>
    </td>
  );
}

function IoCell({ read, write }: { read: number; write: number }) {
  return (
    <td
      className="perf-num perf-io"
      title={`read ${formatRate(read)} · write ${formatRate(write)}`}
    >
      {read + write > 0 ? formatRate(read + write) : "—"}
    </td>
  );
}

/** An expand/collapse toggle at the start of a name cell. */
function Chevron({ open, label, onClick }: { open: boolean; label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      className="perf-chevron"
      data-open={open}
      aria-expanded={open}
      aria-label={label}
      onClick={onClick}
    >
      <ChevronRight size={12} />
    </button>
  );
}

function RowMetrics({ row, machine }: { row: PerfRow; machine: PerfMachine }) {
  return (
    <>
      <MetricCell
        kind="cpu"
        text={formatCpu(row.cpu_percent)}
        fraction={row.cpu_percent / 100}
        spark={{ values: row.history.cpu, floor: CPU_SPARK_FLOOR }}
      />
      <MetricCell
        kind="mem"
        text={formatBytes(row.mem_bytes)}
        fraction={memFraction(row.mem_bytes, machine)}
        spark={{ values: row.history.mem }}
      />
      <IoCell read={row.disk_read_bps} write={row.disk_write_bps} />
    </>
  );
}

function ProcRows({ procs, machine }: { procs: PerfProc[]; machine: PerfMachine }) {
  if (procs.length === 0) {
    return (
      <tr className="perf-row perf-proc perf-proc-empty">
        <td colSpan={4}>no processes</td>
      </tr>
    );
  }
  return (
    <>
      {procs.map((p) => (
        <tr key={p.pid} className="perf-row perf-proc">
          <td className="perf-name" title={`${p.name} (pid ${p.pid})`}>
            <span className="perf-name-text mono">{p.name}</span>
            <span className="perf-pid">{p.pid}</span>
            {p.inferred && (
              <span className="perf-badge" title={INFERRED_TITLE}>
                inferred
              </span>
            )}
          </td>
          <MetricCell kind="cpu" text={formatCpu(p.cpu_percent)} fraction={p.cpu_percent / 100} />
          <MetricCell
            kind="mem"
            text={formatBytes(p.mem_bytes)}
            fraction={memFraction(p.mem_bytes, machine)}
          />
          <IoCell read={p.disk_read_bps} write={p.disk_write_bps} />
        </tr>
      ))}
    </>
  );
}

function rowTitle(row: PerfRow): string {
  const procs = `${row.proc_count} process${row.proc_count === 1 ? "" : "es"}`;
  return row.inferred_count > 0 ? `${procs}, ${row.inferred_count} attributed by path` : procs;
}

const COLUMNS: { key: PerfSortKey; label: string }[] = [
  { key: "name", label: "Name" },
  { key: "cpu", label: "CPU" },
  { key: "mem", label: "Mem" },
  { key: "io", label: "IO" },
];

function SortHeader({ sort, onSort }: { sort: PerfSort; onSort: (k: PerfSortKey) => void }) {
  return (
    <thead>
      <tr className="perf-row perf-colhead">
        {COLUMNS.map((c) => {
          const active = sort.key === c.key;
          return (
            <th
              key={c.key}
              scope="col"
              className={`perf-col-${c.key}`}
              aria-sort={active ? (sort.dir === "asc" ? "ascending" : "descending") : "none"}
            >
              <button
                type="button"
                className="perf-sort"
                data-active={active}
                onClick={() => onSort(c.key)}
              >
                {c.label}
                <span className="perf-sort-arrow" aria-hidden="true">
                  {active ? (sort.dir === "asc" ? "▲" : "▼") : ""}
                </span>
              </button>
            </th>
          );
        })}
      </tr>
    </thead>
  );
}

export function PerfOverlay({
  onClose,
  onFocusSession,
}: {
  onClose: () => void;
  onFocusSession: (project: string, worktree: string) => void;
}) {
  const snapshot = usePerfSnapshot();
  const [sort, setSort] = useState<PerfSort>(DEFAULT_PERF_SORT);
  const [collapsedProjects, setCollapsedProjects] = useState<Set<string>>(() => new Set());
  const [expandedProcs, setExpandedProcs] = useState<Set<string>>(() => new Set());

  const view = useMemo(
    () => (snapshot ? buildPerfView(snapshot.rows, sort) : null),
    [snapshot, sort],
  );
  const machine = snapshot?.machine;

  const toggleProcs = (key: string) => setExpandedProcs((s) => toggleInSet(s, key));

  return (
    <section className="perf-overlay" aria-label="Performance">
      <header className="perf-head">
        <div className="perf-title-row">
          <Activity size={14} className="perf-title-icon" />
          <span className="perf-title">Performance</span>
          <button type="button" className="perf-close" onClick={onClose} aria-label="Close">
            <X size={14} />
          </button>
        </div>
        {machine && (
          <div className="perf-machine">
            <span className="perf-stat">
              <span className="perf-stat-label">CPU</span>
              <span className="perf-stat-value">{formatCpu(machine.cpu_percent)}</span>
            </span>
            <span className="perf-stat">
              <span className="perf-stat-label">Mem</span>
              <span className="perf-stat-value">
                {formatBytes(machine.mem_used)}
                <span className="perf-stat-dim"> / {formatBytes(machine.mem_total)}</span>
              </span>
            </span>
            <span className="perf-stat">
              <span className="perf-stat-label">Disk</span>
              <span className="perf-stat-value">
                ↓ {formatRate(machine.disk_read_bps)} ↑ {formatRate(machine.disk_write_bps)}
              </span>
            </span>
            {view?.attributedShare != null && (
              <span
                className="perf-stat perf-stat-share"
                title="Share of CPU attributed to a known owner (not Other)"
              >
                <span className="perf-stat-label">Attributed</span>
                <span className="perf-stat-value">{Math.round(view.attributedShare * 100)}%</span>
              </span>
            )}
          </div>
        )}
      </header>

      {!view || !machine ? (
        <div className="perf-empty">Sampling…</div>
      ) : (
        <table className="perf-table">
          <SortHeader sort={sort} onSort={(k) => setSort((s) => nextSort(s, k))} />
          {view.groups.length === 0 && (
            <tbody>
              <tr className="perf-row perf-proc-empty">
                <td colSpan={4}>No sessions running</td>
              </tr>
            </tbody>
          )}
          {view.groups.map(({ project, worktrees }) => {
            const name = nameOf(project);
            const open = !collapsedProjects.has(project.key);
            return (
              <tbody key={project.key} className="perf-group">
                <tr className="perf-row perf-project" title={rowTitle(project)}>
                  <td className="perf-name">
                    <Chevron
                      open={open}
                      label={`${open ? "Collapse" : "Expand"} ${name}`}
                      onClick={() => setCollapsedProjects((s) => toggleInSet(s, project.key))}
                    />
                    <span className="perf-name-text">{name}</span>
                    {!open && <span className="perf-count">{worktrees.length}</span>}
                  </td>
                  <RowMetrics row={project} machine={machine} />
                </tr>
                {open &&
                  worktrees.map((w) => {
                    const wname = nameOf(w);
                    const procsOpen = expandedProcs.has(w.key);
                    return (
                      <Fragment key={w.key}>
                        <tr className="perf-row perf-worktree" title={rowTitle(w)}>
                          <td className="perf-name">
                            <Chevron
                              open={procsOpen}
                              label={`${procsOpen ? "Hide" : "Show"} processes for ${wname}`}
                              onClick={() => toggleProcs(w.key)}
                            />
                            <button
                              type="button"
                              className="perf-name-text perf-focus"
                              title={`Focus ${name} / ${wname}`}
                              onClick={() => w.project && onFocusSession(w.project, wname)}
                            >
                              {wname}
                            </button>
                            {w.resolving && <span className="perf-resolving">resolving…</span>}
                          </td>
                          <RowMetrics row={w} machine={machine} />
                        </tr>
                        {procsOpen && <ProcRows procs={w.top_procs} machine={machine} />}
                      </Fragment>
                    );
                  })}
              </tbody>
            );
          })}
          {view.pinned.length > 0 && (
            <tbody className="perf-group perf-pinned">
              {view.pinned.map(({ row: r, label }) => {
                const expandable = r.kind === "other";
                const procsOpen = expandable && expandedProcs.has(r.key);
                return (
                  <Fragment key={r.key}>
                    <tr
                      className={`perf-row perf-system perf-system-${r.kind}`}
                      title={rowTitle(r)}
                    >
                      <td className="perf-name">
                        {expandable ? (
                          <Chevron
                            open={procsOpen}
                            label={`${procsOpen ? "Hide" : "Show"} top processes`}
                            onClick={() => toggleProcs(r.key)}
                          />
                        ) : (
                          <span className="perf-chevron-spacer" />
                        )}
                        <span className="perf-name-text">{label}</span>
                      </td>
                      <RowMetrics row={r} machine={machine} />
                    </tr>
                    {procsOpen && <ProcRows procs={r.top_procs} machine={machine} />}
                  </Fragment>
                );
              })}
            </tbody>
          )}
        </table>
      )}
    </section>
  );
}
