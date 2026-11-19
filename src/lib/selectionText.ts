// Rebuilding a terminal selection as text, with soft-wrapped rows rejoined.
import type { IBuffer } from "@xterm/xterm";
import { bufferRowCells, isBlank, isFilled, type RowCells } from "./bufferText";

/** A selection's bounds, 0-based and ordered (start first); `end.x` is exclusive (as xterm reports it). */
export interface SelectionRange {
  start: { x: number; y: number };
  end: { x: number; y: number };
}

/** The line separator copied text uses: CRLF on Windows, LF elsewhere (as xterm does). */
export function copyEol(isWindows: boolean): string {
  return isWindows ? "\r\n" : "\n";
}

/**
 * Whether a selection is a rectangle after a mouse press, given whether it was one before.
 * A plain left press starts a new selection, a rectangle when Alt is held; a Shift+click
 * extends the current one and any other button (a right click) leaves it alone.
 */
export function rectSelectionAfterPress(
  rect: boolean,
  press: Pick<MouseEvent, "button" | "altKey" | "shiftKey">,
): boolean {
  return press.button === 0 && !press.shiftKey ? press.altKey : rect;
}

/**
 * The selected part of a row: the units whose glyph overlaps cells [from, to). The row is
 * read as joining the next, so a padding cell before a wrapped wide glyph is skipped; when
 * the row does not join, that cell would only have been a trailing space, which the logical
 * line's right-trim drops anyway.
 */
function rowSegment({ text, cellAt }: RowCells, from: number, to: number): string {
  let out = "";
  for (let i = 0; i < text.length; i++) {
    const x = cellAt[i];
    let j = i + 1;
    while (cellAt[j] <= x) j++;
    // A wide glyph is kept when either of its cells is in range.
    if (x < to && cellAt[j] > from) out += text[i];
  }
  return out.replace(/ /g, " ");
}

/**
 * One row as copying needs it, so a row can still be copied after it has left the screen
 * (xterm keeps no scrollback here): its cells, whether it continues the row above
 * (wrapped), and whether it is filled to its last cell.
 */
export interface RowSnapshot {
  cells: RowCells;
  wrapped: boolean;
  filled: boolean;
}

export function snapshotRow(buffer: IBuffer, y: number, cols: number): RowSnapshot {
  return {
    cells: bufferRowCells(buffer, y, cols, true),
    wrapped: !!buffer.getLine(y)?.isWrapped,
    filled: isFilled(buffer, y, cols),
  };
}

/**
 * The text of `range` over rows from `rowAt`, one logical line per run of joined rows. Row
 * `y` joins the next when the next row's selected part is not blank (blank rows are kept
 * exactly) and the next row is marked wrapped or row `y` is filled. Each logical line is
 * right-trimmed of spaces; lines are separated by `eol`. A row `rowAt` lacks reads as blank.
 */
export function rowsToText(
  rowAt: (y: number) => RowSnapshot | undefined,
  cols: number,
  range: SelectionRange | null | undefined,
  eol: string,
): string {
  if (!range) return "";
  const { start, end } = range;
  const segmentOf = (y: number) => {
    const row = rowAt(y);
    return row
      ? rowSegment(row.cells, y === start.y ? start.x : 0, y === end.y ? end.x : cols)
      : "";
  };
  const lines: string[] = [];
  let current = "";
  // Each row is read once: the next row's segment decides the join, then is the next row's.
  let segment = segmentOf(start.y);
  for (let y = start.y; y <= end.y; y++) {
    let joins = false;
    let next = "";
    if (y < end.y) {
      next = segmentOf(y + 1);
      joins = !isBlank(next) && (!!rowAt(y + 1)?.wrapped || !!rowAt(y)?.filled);
    }
    current += segment;
    segment = next;
    if (joins) continue;
    lines.push(current.replace(/ +$/, ""));
    current = "";
  }
  return lines.join(eol);
}

/** The text of `range` in `buffer`; see {@link rowsToText}. */
export function selectionToText(
  buffer: IBuffer,
  cols: number,
  range: SelectionRange | null | undefined,
  eol: string,
): string {
  return rowsToText((y) => snapshotRow(buffer, y, cols), cols, range, eol);
}

/**
 * A selection moved `delta` rows down (negative: up) along with the text under it, clipped
 * to a `rows`×`cols` viewport. Null when none of it is left on screen. Used when the pane's
 * history scrolls under a selection: xterm keeps no scrollback of its own, so it would
 * otherwise leave the highlight on the same cells while different text moves beneath it.
 */
export function shiftSelection(
  range: SelectionRange,
  delta: number,
  rows: number,
  cols: number,
): SelectionRange | null {
  let start = { x: range.start.x, y: range.start.y + delta };
  let end = { x: range.end.x, y: range.end.y + delta };
  if (end.y < 0 || start.y >= rows) return null;
  if (start.y < 0) start = { x: 0, y: 0 };
  if (end.y >= rows) end = { x: cols, y: rows - 1 };
  if (end.y === start.y && end.x <= start.x) return null;
  return { start, end };
}

/** The `length` xterm's `select(column, row, length)` takes for a range: cells, row-major. */
export function selectionLength(range: SelectionRange, cols: number): number {
  return (range.end.y - range.start.y) * cols + range.end.x - range.start.x;
}

/** Whether two selection ranges cover the same cells (both absent counts as equal). */
export function sameSelection(
  a: SelectionRange | undefined,
  b: SelectionRange | undefined,
): boolean {
  if (!a || !b) return a === b;
  return (
    a.start.x === b.start.x && a.start.y === b.start.y && a.end.x === b.end.x && a.end.y === b.end.y
  );
}

/**
 * How many rows the screen's content moved between two snapshots of its rows (positive:
 * down, as when scrolling up into history), found by lining the rows up. Null when the two
 * share too little to tell, e.g. the content moved by a whole screen or more.
 *
 * Every shift is tried; the one matching the most non-blank rows wins, and `expected` (the
 * lines asked for since `before`) breaks ties, so repeated rows such as blank lines or
 * borders cannot pull the answer off the real movement.
 */
export function alignShift(before: string[], after: string[], expected = 0): number | null {
  const n = Math.min(before.length, after.length);
  let best: { d: number; score: number; overlap: number } | null = null;
  for (let d = -(n - 1); d <= n - 1; d++) {
    let score = 0;
    let overlap = 0;
    for (let i = Math.max(0, -d); i < Math.min(n, n - d); i++) {
      if (before[i].trim() === "") continue;
      overlap++;
      if (after[i + d] === before[i]) score++;
    }
    if (
      !best ||
      score > best.score ||
      (score === best.score && Math.abs(d - expected) < Math.abs(best.d - expected))
    ) {
      best = { d, score, overlap };
    }
  }
  if (!best || best.score === 0) return null;
  // A lone matching row is chance unless it is all there was to match.
  if (best.score < 2 && best.overlap > 1) return null;
  if (best.score * 2 < best.overlap) return null;
  return best.d;
}

/** A cell boundary in screen coordinates: `x` runs 0..cols (between cells), `y` is a row. */
export interface CellPoint {
  x: number;
  y: number;
}

/** The selection between two cell boundaries, whichever comes first. */
export function rangeBetween(a: CellPoint, b: CellPoint): SelectionRange {
  const aFirst = a.y < b.y || (a.y === b.y && a.x <= b.x);
  const [start, end] = aFirst ? [a, b] : [b, a];
  return { start: { ...start }, end: { ...end } };
}

/**
 * The cell boundary under a pointer, as xterm picks one for a selection: the nearest gap
 * between cells across, the row it is in down. Clamped to the screen.
 */
export function boundaryAt(
  clientX: number,
  clientY: number,
  screen: Pick<DOMRect, "left" | "top" | "width" | "height">,
  cols: number,
  rows: number,
): CellPoint {
  const cellW = screen.width / cols;
  const cellH = screen.height / rows;
  const x = Math.round((clientX - screen.left) / cellW);
  const y = Math.floor((clientY - screen.top) / cellH);
  return {
    x: Math.min(cols, Math.max(0, x)),
    y: Math.min(rows - 1, Math.max(0, y)),
  };
}
