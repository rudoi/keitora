// Reading an xterm buffer row back as text, with a map from the text to cell columns.
import type { IBuffer, IBufferCell } from "@xterm/xterm";

/** Whether `text` is only spaces (including the no-break spaces some TUIs paint). */
export const isBlank = (text: string) => /^[  ]*$/.test(text);

/** A buffer row's text with the cell column of each UTF-16 unit, plus one entry for its end. */
export interface RowCells {
  text: string;
  cellAt: number[];
}

/**
 * Whether the row after `row` starts with a wide glyph, so an empty last cell of `row` is
 * the padding xterm leaves when a wide glyph wraps to the next row.
 */
export function wrapsWideGlyphAfter(buffer: IBuffer, row: number): boolean {
  return (
    buffer
      .getLine(row + 1)
      ?.getCell(0)
      ?.getWidth() === 2
  );
}

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
export function isFilled(buffer: IBuffer, y: number, cols: number): boolean {
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
 * Walk the painted cells of `row`. A wide glyph's zero-width tail is skipped and an empty
 * cell reads as " ". With `joinsNext` (the next row continues this one), an empty last
 * cell before a wrapped wide glyph is padding and is skipped too. Fills `cellAt` when given.
 */
function readRow(
  buffer: IBuffer,
  row: number,
  cols: number,
  joinsNext: boolean,
  cellAt: number[] | null,
): string {
  const line = buffer.getLine(row);
  let text = "";
  if (line) {
    const reuse = buffer.getNullCell();
    for (let col = 0; col < cols; col++) {
      const cell: IBufferCell | undefined = line.getCell(col, reuse);
      if (!cell || cell.getWidth() === 0) continue;
      const chars = cell.getChars();
      if (!chars && joinsNext && col === cols - 1 && wrapsWideGlyphAfter(buffer, row)) continue;
      const value = chars || " ";
      text += value;
      if (cellAt) for (let i = 0; i < value.length; i++) cellAt.push(col);
    }
  }
  cellAt?.push(cols);
  return text;
}

/** The text of buffer row `row` (see readRow for how cells read). */
export function bufferRowText(buffer: IBuffer, row: number, cols: number, joinsNext = false) {
  return readRow(buffer, row, cols, joinsNext, null);
}

/** The text of buffer row `row` with the map back to its cell columns. */
export function bufferRowCells(
  buffer: IBuffer,
  row: number,
  cols: number,
  joinsNext = false,
): RowCells {
  const cellAt: number[] = [];
  const text = readRow(buffer, row, cols, joinsNext, cellAt);
  return { text, cellAt };
}

/** The cells a hit at `col`/`len` of a row's text covers. */
export function cellSpan(row: RowCells, col: number, len: number): { x: number; width: number } {
  const last = row.cellAt.length - 1;
  const x = row.cellAt[Math.min(col, last)];
  const end = row.cellAt[Math.min(col + len, last)];
  return { x, width: Math.max(1, end - x) };
}
