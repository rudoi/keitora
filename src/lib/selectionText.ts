// Rebuilding a terminal selection as text, with soft-wrapped rows rejoined.
import type { IBuffer } from "@xterm/xterm";
import { bufferRowCells, wrapsWideGlyphAfter } from "./bufferText";

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

const isBlank = (s: string) => /^ *$/.test(s);

/** Box drawing and block elements: a row ending in one is a border, not wrapped text. */
function isBoxGlyph(chars: string): boolean {
  const cp = chars.codePointAt(0) ?? 0;
  return cp >= 0x2500 && cp <= 0x259f;
}

/**
 * Whether row `y` is filled to its last cell, so its text plausibly continues on the next
 * row. A wide glyph across the last two cells counts, as does an empty last cell left by a
 * wide glyph that wrapped. A trailing box-drawing glyph does not.
 */
function isFilled(buffer: IBuffer, y: number, cols: number): boolean {
  const line = buffer.getLine(y);
  if (!line || cols < 1) return false;
  const last = line.getCell(cols - 1);
  let chars = last?.getChars() ?? "";
  if (last?.getWidth() === 0) {
    chars = line.getCell(cols - 2)?.getChars() ?? "";
  } else if (!chars) {
    return wrapsWideGlyphAfter(buffer, y);
  }
  if (!chars || isBoxGlyph(chars)) return false;
  return chars !== " " && chars !== " ";
}

/**
 * The selected part of row `y`: the units whose glyph overlaps cells [from, to). The row is
 * read as joining the next, so a padding cell before a wrapped wide glyph is skipped; when
 * the row does not join, that cell would only have been a trailing space, which the logical
 * line's right-trim drops anyway.
 */
function rowSegment(buffer: IBuffer, y: number, cols: number, from: number, to: number): string {
  const { text, cellAt } = bufferRowCells(buffer, y, cols, true);
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
 * The text of `range` in `buffer`, one logical line per run of joined rows. Row `y` joins
 * the next when the next row's selected part is not blank (blank rows are kept exactly)
 * and the next row is marked wrapped or row `y` is filled. Each logical line is
 * right-trimmed of spaces; lines are separated by `eol`.
 */
export function selectionToText(
  buffer: IBuffer,
  cols: number,
  range: SelectionRange | null | undefined,
  eol: string,
): string {
  if (!range) return "";
  const { start, end } = range;
  const segmentOf = (y: number) =>
    rowSegment(buffer, y, cols, y === start.y ? start.x : 0, y === end.y ? end.x : cols);
  const lines: string[] = [];
  let current = "";
  // Each row is read once: the next row's segment decides the join, then is the next row's.
  let segment = segmentOf(start.y);
  for (let y = start.y; y <= end.y; y++) {
    let joins = false;
    let next = "";
    if (y < end.y) {
      next = segmentOf(y + 1);
      joins = !isBlank(next) && (!!buffer.getLine(y + 1)?.isWrapped || isFilled(buffer, y, cols));
    }
    current += segment;
    segment = next;
    if (joins) continue;
    lines.push(current.replace(/ +$/, ""));
    current = "";
  }
  return lines.join(eol);
}
