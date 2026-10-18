// Reading an xterm buffer row back as text, with a map from the text to cell columns.
import type { IBuffer, IBufferCell } from "@xterm/xterm";

/** A buffer row's text with the cell column of each UTF-16 unit, plus one entry for its end. */
export interface RowCells {
  text: string;
  cellAt: number[];
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
      if (
        !chars &&
        joinsNext &&
        col === cols - 1 &&
        buffer
          .getLine(row + 1)
          ?.getCell(0)
          ?.getWidth() === 2
      )
        continue;
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
