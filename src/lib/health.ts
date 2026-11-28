// The app's self-diagnostics as the window shows them: ordering, the rail glyph's
// tone and tooltip, the Settings grouping, the copy-as-JSON payload, and the
// subscription that keeps a list current. Pure apart from `subscribeHealth` and the
// open-Settings request.

import { type HealthFinding, type HealthSeverity, healthList, onHealthChanged } from "./ipc";

export type { HealthFinding, HealthSeverity };

const RANK: Record<HealthSeverity, number> = { info: 0, warn: 1, error: 2 };

/** Severity descending, then newest `lastSeen` first. */
export function sortFindings(findings: readonly HealthFinding[]): HealthFinding[] {
  return [...findings].sort(
    (a, b) => RANK[b.severity] - RANK[a.severity] || b.lastSeen - a.lastSeen,
  );
}

/** Active findings (not cleared). */
export function activeFindings(findings: readonly HealthFinding[]): HealthFinding[] {
  return findings.filter((f) => f.clearedAt === null);
}

/**
 * The active findings in a scope: one worktree's (`worktree` given), or a whole
 * project's (its worktrees' and its own, `worktree` omitted).
 */
export function scopeFindings(
  findings: readonly HealthFinding[],
  project: string,
  worktree?: string,
): HealthFinding[] {
  return activeFindings(findings).filter(
    (f) => f.project === project && (worktree === undefined || f.worktree === worktree),
  );
}

/**
 * A `scopeFindings` for one scope that hands back its previous array while the
 * scope's findings are unchanged in content, so a consumer keeps a stable reference
 * (and skips re-rendering) when only other scopes' findings moved.
 */
export function scopedSelector(
  project: string,
  worktree?: string,
): (findings: readonly HealthFinding[]) => HealthFinding[] {
  let last: HealthFinding[] | null = null;
  let lastSig = "";
  return (findings) => {
    const next = scopeFindings(findings, project, worktree);
    const sig = JSON.stringify(next);
    if (last !== null && sig === lastSig) return last;
    last = next;
    lastSig = sig;
    return next;
  };
}

/** The findings list and its listeners, read with `useSyncExternalStore`. */
export interface HealthStore {
  get(): HealthFinding[];
  set(findings: HealthFinding[]): void;
  subscribe(listener: () => void): () => void;
}

export function createHealthStore(initial: HealthFinding[] = []): HealthStore {
  let findings = initial;
  const listeners = new Set<() => void>();
  return {
    get: () => findings,
    set(next) {
      findings = next;
      for (const l of listeners) l();
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}

/** What the rail glyph shows: `warn`/`error` findings only, in glyph order. */
export function glyphFindings(findings: readonly HealthFinding[]): HealthFinding[] {
  return sortFindings(activeFindings(findings).filter((f) => RANK[f.severity] >= RANK.warn));
}

/** What the rail glyph shows, from one pass over the findings; null: nothing. */
export interface GlyphView {
  /** The highest severity. */
  tone: "warn" | "error";
  /** The first finding's message, then `(+n more)`. */
  title: string;
  /** How many findings it stands for. */
  count: number;
}

export function glyphView(findings: readonly HealthFinding[]): GlyphView | null {
  const shown = glyphFindings(findings);
  const top = shown[0];
  if (!top) return null;
  const more = shown.length - 1;
  return {
    tone: top.severity as "warn" | "error",
    title: more > 0 ? `${top.message}\n(+${more} more)` : top.message,
    count: shown.length,
  };
}

/** The glyph's colour: the highest severity, or null (nothing to show). */
export function glyphTone(findings: readonly HealthFinding[]): "warn" | "error" | null {
  return glyphView(findings)?.tone ?? null;
}

/** The glyph's tooltip: the first finding's message, then `(+n more)`. */
export function glyphTitle(findings: readonly HealthFinding[]): string {
  return glyphView(findings)?.title ?? "";
}

/** The glyph's accessible name. */
export function glyphLabel(n: number): string {
  return `Health: ${n} issue${n === 1 ? "" : "s"}`;
}

export interface WorktreeGroup {
  /** `""` for project-level findings. */
  worktree: string;
  findings: HealthFinding[];
}

export interface ProjectGroup {
  /** `""` for app-level findings. */
  project: string;
  worktrees: WorktreeGroup[];
}

/** Group by project (app-level first, then by name), then worktree (project-level
 *  first, then by name); each group in glyph order. */
export function groupFindings(findings: readonly HealthFinding[]): ProjectGroup[] {
  const byProject = new Map<string, Map<string, HealthFinding[]>>();
  for (const f of findings) {
    let wts = byProject.get(f.project);
    if (!wts) {
      wts = new Map();
      byProject.set(f.project, wts);
    }
    const list = wts.get(f.worktree) ?? [];
    list.push(f);
    wts.set(f.worktree, list);
  }
  const byName = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
  return [...byProject.keys()].sort(byName).map((project) => {
    const wts = byProject.get(project) ?? new Map<string, HealthFinding[]>();
    return {
      project,
      worktrees: [...wts.keys()].sort(byName).map((worktree) => ({
        worktree,
        findings: sortFindings(wts.get(worktree) ?? []),
      })),
    };
  });
}

/** A group's heading: `app`, the project name, `project-level` or `@worktree`. */
export function projectLabel(project: string): string {
  return project === "" ? "app" : project;
}
export function worktreeLabel(worktree: string): string {
  return worktree === "" ? "project" : `@${worktree}`;
}

/** RFC 3339 UTC, whole seconds. */
function rfc3339(ms: number): string {
  return new Date(Math.floor(ms / 1000) * 1000).toISOString().replace(".000Z", "Z");
}

/**
 * The `findings` array `keitora status --json` prints, for the active findings, as
 * pretty JSON (`{ "findings": [...] }`), so it pastes into a report as is. The keys
 * and their order mirror the CLI's `finding_json` (crates/keitora-cli/src/status.rs);
 * change both together (each side's tests pin the shape).
 */
export function statusJson(findings: readonly HealthFinding[]): string {
  const rows = sortFindings(activeFindings(findings)).map((f) => ({
    project: f.project,
    worktree: f.worktree,
    check_id: f.checkId,
    severity: f.severity,
    message: f.message,
    first_seen: rfc3339(f.firstSeen),
    last_seen: rfc3339(f.lastSeen),
    occurrences: f.occurrences,
    remedy: f.remedy,
  }));
  return JSON.stringify({ findings: rows }, null, 2);
}

/** Local `HH:MM:SS`. */
export function clockText(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** `{s}s` under two minutes, `{m}m` under two hours, else `{h}h`. */
export function ageText(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 120) return `${s}s`;
  if (s < 7200) return `${Math.floor(s / 60)}m`;
  return `${Math.floor(s / 3600)}h`;
}

/**
 * Keep `set` current with the monitor's list: register for `health://changed` first,
 * then read the snapshot, so an event landing meanwhile is not lost. A snapshot that
 * answers after an event is older than it and is dropped. Returns the unsubscribe.
 */
export function subscribeHealth(set: (findings: HealthFinding[]) => void): () => void {
  let cancelled = false;
  let heard = false;
  let unlisten: (() => void) | null = null;
  void onHealthChanged((findings) => {
    if (cancelled) return;
    heard = true;
    set(findings);
  })
    .then(
      (fn) => {
        if (cancelled) fn();
        else unlisten = fn;
      },
      // No events: the snapshot is still worth reading.
      () => {},
    )
    .then(() => (cancelled ? undefined : healthList()))
    .then((findings) => {
      if (findings && !cancelled && !heard) set(findings);
    })
    .catch(() => {});
  return () => {
    cancelled = true;
    unlisten?.();
  };
}

// ── Open Settings at the Health section ─────────────────────────────────────

/** The window event that asks the app to open Settings at the Health section. */
export const OPEN_HEALTH_EVENT = "keitora:open-health";

let focusPending = false;

/** Ask for Settings, scrolled to the Health section (the rail glyph's click). */
export function requestHealthSettings(target: EventTarget = window): void {
  focusPending = true;
  target.dispatchEvent(new Event(OPEN_HEALTH_EVENT));
}

/** Whether a Health focus was asked for and not yet served (clears it). */
export function takeHealthFocus(): boolean {
  const was = focusPending;
  focusPending = false;
  return was;
}
