// Find-in-history for a terminal pane. herdr owns the pane's history, so search runs
// over the text it returns (one line per screen row, oldest first) and a hit is reached
// by scrolling herdr's viewport to it. A scroll offset counts lines above the live tail:
// at offset `o` the screen shows lines [total - rows - o, total - o).

export interface SearchFlags {
  caseSensitive: boolean;
  regex: boolean;
}

/** One hit: `line` indexes the searched lines, `col`/`len` are UTF-16 units in it. */
export interface Match {
  line: number;
  col: number;
  len: number;
}

export interface MatchResult {
  /** Every hit, in reading order (oldest line first, left to right). */
  matches: Match[];
  /** How many lines were searched. */
  lineCount: number;
  error?: "invalid-regex";
}

/**
 * Split history text into lines, one per screen row. Blank rows are kept, including
 * the live screen's unused rows at the bottom: herdr's scroll offset counts from the
 * bottom of the full screen, so the line count must too. Only the empty piece after a
 * final newline is dropped, since it is not a row.
 */
export function historyLines(text: string): string[] {
  const lines = text.split(/\r?\n/);
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/**
 * Whether the app in the pane, not herdr's history, receives a scroll: on the alternate
 * screen herdr turns it into arrow keys, and with mouse tracking on it forwards the
 * wheel to the app as mouse reports. A search must not scroll then.
 */
export function appOwnsScroll(bufferType: string, mouseTrackingMode: string): boolean {
  return bufferType !== "normal" || mouseTrackingMode !== "none";
}

function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** All non-overlapping hits of `query` in each line. Zero-length regex hits are skipped. */
export function findInLines(lines: string[], query: string, flags: SearchFlags): MatchResult {
  const lineCount = lines.length;
  if (query === "") return { matches: [], lineCount };
  let re: RegExp;
  try {
    re = new RegExp(flags.regex ? query : escapeRegExp(query), flags.caseSensitive ? "g" : "gi");
  } catch {
    return { matches: [], lineCount, error: "invalid-regex" };
  }
  const matches: Match[] = [];
  for (let line = 0; line < lines.length; line++) {
    const text = lines[line];
    re.lastIndex = 0;
    for (let m = re.exec(text); m !== null; m = re.exec(text)) {
      if (m[0].length === 0) {
        // An empty hit would match forever at the same index: step past it.
        re.lastIndex = m.index + 1;
        if (re.lastIndex > text.length) break;
        continue;
      }
      matches.push({ line, col: m.index, len: m[0].length });
    }
  }
  return { matches, lineCount };
}

/**
 * The scroll offset that puts `matchLine` in the middle of the screen, clamped to the
 * history that exists. 0 (the live tail) when the line is already on the live screen.
 */
export function targetOffset(matchLine: number, totalLines: number, viewportRows: number): number {
  const maxOffset = Math.max(0, totalLines - viewportRows);
  if (matchLine >= maxOffset) return 0;
  const centred = maxOffset + Math.floor(viewportRows / 2) - matchLine;
  return Math.min(maxOffset, Math.max(0, centred));
}

/** The screen row `matchLine` shows on at scroll `offset`, or null when off screen. */
export function rowInViewport(
  matchLine: number,
  totalLines: number,
  viewportRows: number,
  offset: number,
): number | null {
  const row = matchLine - (totalLines - viewportRows - offset);
  return row >= 0 && row < viewportRows ? row : null;
}

/** The hit closest to `row` (then to `col`), or null when there are none. */
export function nearestMatch(matches: Match[], row: number, col: number): Match | null {
  let best: Match | null = null;
  for (const m of matches) {
    if (
      best === null ||
      Math.abs(m.line - row) < Math.abs(best.line - row) ||
      (Math.abs(m.line - row) === Math.abs(best.line - row) &&
        Math.abs(m.col - col) < Math.abs(best.col - col))
    ) {
      best = m;
    }
  }
  return best;
}

/** Move `index` by `delta` through `count` hits, wrapping at both ends. */
export function stepIndex(index: number, count: number, delta: number): number {
  if (count <= 0) return 0;
  return (((index + delta) % count) + count) % count;
}

/**
 * Where the hit at `index` (in a search over `oldTotal` lines) sits in a fresh search over
 * `newTotal` lines. History grows (and is cut at the top) from the live end, so a hit is
 * identified by its distance from the end: the same line and column if it is still there,
 * otherwise the nearest one. -1 when there are no hits.
 */
export function reanchorIndex(
  old: Match,
  oldTotal: number,
  fresh: Match[],
  newTotal: number,
): number {
  const fromEnd = oldTotal - old.line;
  let best = -1;
  let bestDist = Number.POSITIVE_INFINITY;
  for (let i = 0; i < fresh.length; i++) {
    const m = fresh[i];
    const dist = Math.abs(newTotal - m.line - fromEnd) * 1e6 + Math.abs(m.col - old.col);
    if (dist < bestDist) {
      best = i;
      bestDist = dist;
    }
  }
  return best;
}
