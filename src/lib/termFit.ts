// Pure size/fit decision helpers for the embedded terminal panes.
// No xterm dependency — callers pass plain numbers so this stays unit-testable.

/** Minimum usable PTY geometry — claude's TUI corrupts below this. */
export const MIN_PTY = { rows: 4, cols: 10 } as const;

export interface Dims {
  rows: number;
  cols: number;
}

/** Clamp measured xterm dims up to the minimum the PTY/claude renders sanely. */
export function clampPtySize(d: Dims): Dims {
  return {
    rows: Math.max(MIN_PTY.rows, d.rows),
    cols: Math.max(MIN_PTY.cols, d.cols),
  };
}

/**
 * A fit proposal is usable only if both axes are positive, finite integers. Guards
 * against FitAddon.proposeDimensions() returning undefined (renderer cell metrics
 * still 0) or a degenerate 0/NaN measurement from a not-yet-laid-out host.
 */
export function dimsAreSane(d: Dims | undefined | null): d is Dims {
  return !!d && Number.isFinite(d.rows) && Number.isFinite(d.cols) && d.rows > 0 && d.cols > 0;
}

/** Two proposals describe the same geometry (stability check across frames). */
export function dimsEqual(a: Dims, b: Dims): boolean {
  return a.rows === b.rows && a.cols === b.cols;
}

/** Defensive row height when the terminal has not produced a usable measurement. */
export const FALLBACK_ROW_PX = 17;

export interface WheelLineResult {
  lines: number;
  remainder: number;
}

// WheelEvent.deltaMode values, spelled out so this module needs no DOM.
export const WHEEL_DELTA_PIXEL = 0;
export const WHEEL_DELTA_LINE = 1;
export const WHEEL_DELTA_PAGE = 2;

/** Upper bound on lines one wheel event can produce (malformed or extreme deltas). */
export const WHEEL_LINES_CAP = 100;

/**
 * Convert wheel movement to whole lines while carrying the sub-line remainder.
 * Pixel deltas are divided by the row height, line deltas pass through, and page
 * deltas are multiplied by the visible row count. Invalid measurements and inputs are
 * normalized so the result is never NaN or Infinity, and one event yields at most
 * WHEEL_LINES_CAP lines either way.
 */
export function accumulateWheelLines(
  delta: number,
  rowHeightPx: number,
  prevRemainder: number,
  deltaMode: number = WHEEL_DELTA_PIXEL,
  rows = 0,
): WheelLineResult {
  const safeDelta = Number.isFinite(delta) ? delta : 0;
  const safePrevRaw = Number.isFinite(prevRemainder) ? prevRemainder : 0;
  // Do not let carry from the prior direction delay a reversal.
  const safePrev =
    safePrevRaw !== 0 && safeDelta !== 0 && Math.sign(safePrevRaw) !== Math.sign(safeDelta)
      ? 0
      : safePrevRaw;

  let deltaLines: number;
  if (deltaMode === WHEEL_DELTA_LINE) {
    deltaLines = safeDelta;
  } else if (deltaMode === WHEEL_DELTA_PAGE) {
    deltaLines = safeDelta * (Number.isFinite(rows) && rows > 0 ? rows : 1);
  } else {
    const safeRow = Number.isFinite(rowHeightPx) && rowHeightPx > 0 ? rowHeightPx : FALLBACK_ROW_PX;
    deltaLines = safeDelta / safeRow;
  }

  const acc = safePrev + deltaLines;
  // Math.trunc keeps the sign symmetric; `|| 0` normalizes -0 → 0 for a canonical result.
  const whole = Math.trunc(acc) || 0;
  if (Math.abs(whole) > WHEEL_LINES_CAP) {
    // Clamp and drop the carry: an extreme event shouldn't bank movement for later.
    return { lines: Math.sign(whole) * WHEEL_LINES_CAP, remainder: 0 };
  }
  return { lines: whole, remainder: acc - whole };
}

export interface Cell {
  column: number;
  row: number;
}

/** Clamp a 0-based cell index to `0..count-1`; a non-finite index or empty axis is 0. */
function clampAxis(index: number, count: number): number {
  const max = Number.isFinite(count) && count > 0 ? Math.floor(count) - 1 : 0;
  if (!Number.isFinite(index)) return 0;
  return Math.min(max, Math.max(0, Math.floor(index)));
}

/**
 * Map a pointer offset (relative to the terminal screen's top-left, in CSS px) to the
 * 0-based cell under it, clamped to the grid. A missing cell measurement maps to 0.
 */
export function wheelCell(
  offsetX: number,
  offsetY: number,
  cellW: number,
  cellH: number,
  cols: number,
  rows: number,
): Cell {
  const axis = (offset: number, size: number, count: number) =>
    Number.isFinite(size) && size > 0 ? clampAxis(offset / size, count) : 0;
  return { column: axis(offsetX, cellW, cols), row: axis(offsetY, cellH, rows) };
}

/** The cursor's 0-based cell (`cursorX`,`cursorY` in xterm's buffer), clamped to the grid. */
export function cursorCell(cursorX: number, cursorY: number, cols: number, rows: number): Cell {
  return { column: clampAxis(cursorX, cols), row: clampAxis(cursorY, rows) };
}
