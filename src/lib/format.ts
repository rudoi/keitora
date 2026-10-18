// Shared formatting utilities.

/**
 * Format a duration given as milliseconds into a human-readable uptime string.
 * Pass `startedAt * 1000` (unix epoch seconds → ms) at the call site when the
 * source is a `DaemonInfo.started_at` field (which is in seconds).
 */
export function formatUptime(ms: number): string {
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h ${m % 60}m`;
}

/**
 * Format a byte count as a compact human-readable size (`84 MB`, `1.2 GB`). Uses binary
 * units (1024) with a space before the unit. Values ≥ 10 in a unit show no decimal; below
 * 10 show one, so the width stays stable as the number ticks. `KB` and up round; bytes are
 * exact.
 */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  // Decide decimals from the value AFTER one-decimal rounding, so a value like 9.95 (which
  // rounds to "10.0") is shown as "10" — keeping the "≥10 ⇒ no decimal" width invariant.
  const rounded1 = Math.round(value * 10) / 10;
  return rounded1 >= 10
    ? `${Math.round(value)} ${units[unit]}`
    : `${rounded1.toFixed(1)} ${units[unit]}`;
}

/** Format a bytes-per-second rate (`1.2 MB/s`); `—` for the inputs `formatBytes` rejects. */
export function formatRate(bytesPerSec: number): string {
  const b = formatBytes(bytesPerSec);
  return b === "—" ? b : `${b}/s`;
}

/**
 * Format a CPU percentage with one decimal place always (`1.0%`, `12.3%`), so the field
 * width doesn't jump as the value ticks in the rail card.
 */
export function formatCpu(percent: number): string {
  if (!Number.isFinite(percent) || percent < 0) return "—";
  return `${percent.toFixed(1)}%`;
}

/**
 * Epoch ms for `utc`: an ISO string with a zone, or SQLite's zoneless
 * `YYYY-MM-DD HH:MM:SS` (which is UTC). `null` for an empty or unparseable time.
 */
export function parseUtc(utc: string | null | undefined): number | null {
  if (!utc) return null;
  const zoned = /(?:[zZ]|[+-]\d\d:?\d\d)$/.test(utc);
  const t = Date.parse(zoned ? utc : `${utc.replace(" ", "T")}Z`);
  return Number.isNaN(t) ? null : t;
}

/**
 * Compact time since `utc` at `nowMs`: `now` under a minute (or in the future), then
 * `5m`, `3h`, `2d`. `utc` is either an ISO string with a zone or SQLite's zoneless
 * `YYYY-MM-DD HH:MM:SS`, which is UTC. `null` for an empty or unparseable time.
 */
export function formatAgo(utc: string | null | undefined, nowMs: number): string | null {
  const t = parseUtc(utc);
  if (t === null) return null;
  const m = Math.floor((nowMs - t) / 60_000);
  if (m < 1) return "now";
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

/** Forward slashes, minus a Windows verbatim prefix (`//?/C:/…` → `C:/…`, `//?/UNC/…` → `//…`). */
function displaySlashes(path: string): string {
  const p = path.replace(/\\/g, "/");
  if (p.startsWith("//?/UNC/")) return `//${p.slice(8)}`;
  if (p.startsWith("//?/")) return p.slice(4);
  return p;
}

/**
 * `path` for display: forward slashes (no Windows verbatim prefix), and a leading `home`
 * directory shown as `~`. The home prefix matches only at a path boundary, and ignores
 * case only for a Windows (drive-letter) path.
 */
export function shortPath(path: string, home: string | null): string {
  const p = displaySlashes(path);
  if (!home) return p;
  const h = displaySlashes(home).replace(/\/+$/, "");
  if (h === "") return p;
  const head = p.slice(0, h.length);
  const next = p.charAt(h.length);
  const same = /^[A-Za-z]:/.test(p) ? head.toLowerCase() === h.toLowerCase() : head === h;
  if (same && (next === "" || next === "/")) {
    return `~${p.slice(h.length)}`;
  }
  return p;
}

/**
 * A display path (as `shortPath` gives it) cut to its last `levels` folders (at least one)
 * when it has more than that below its root (`~`, a drive, a `//server/share`, or `/`);
 * shallower paths come back whole. Trailing slashes are dropped, except a bare drive's.
 */
export function tailPath(path: string, levels = 2): string {
  const keep = Math.max(1, Math.floor(levels));
  let p = path.length > 1 ? path.replace(/\/+$/, "") : path;
  if (/^[A-Za-z]:$/.test(p) && path.length > p.length) p = `${p}/`;

  let rest: string[];
  if (p.startsWith("//")) {
    rest = p.slice(2).split("/").slice(2);
  } else if (p.startsWith("/")) {
    rest = p.slice(1).split("/");
  } else {
    const parts = p.split("/");
    const rooted = parts[0] === "~" || /^[A-Za-z]:$/.test(parts[0]);
    rest = rooted ? parts.slice(1) : parts;
  }
  rest = rest.filter(Boolean);
  return rest.length > keep ? rest.slice(-keep).join("/") : p;
}
