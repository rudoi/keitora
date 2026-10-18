import type { IBuffer } from "@xterm/xterm";
import { describe, expect, it } from "vitest";
import { bufferRowCells, bufferRowText, cellSpan, wrapsWideGlyphAfter } from "./bufferText";

type Cell = { chars: string; width: number };
const cell = (chars: string, width = 1): Cell => ({ chars, width });

/** A buffer of the given rows, each a list of painted cells. */
function fakeBuffer(rows: Cell[][]): IBuffer {
  const toCell = (c: Cell) => ({ getChars: () => c.chars, getWidth: () => c.width });
  return {
    getNullCell: () => ({}),
    getLine: (y: number) =>
      rows[y] && {
        isWrapped: y > 0,
        getCell: (x: number) => (rows[y][x] ? toCell(rows[y][x]) : undefined),
      },
  } as unknown as IBuffer;
}

describe("bufferRowCells / cellSpan", () => {
  it("maps text back to cell columns, padding empty cells", () => {
    const buf = fakeBuffer([[cell("a"), cell(""), cell("b")]]);
    const row = bufferRowCells(buf, 0, 3);
    expect(row.text).toBe("a b");
    expect(bufferRowText(buf, 0, 3)).toBe("a b");
    expect(cellSpan(row, 2, 1)).toEqual({ x: 2, width: 1 });
  });

  it("accounts for wide glyphs", () => {
    // "你" takes cells 0-1 (cell 1 is its zero-width tail), then "ok".
    const row = bufferRowCells(
      fakeBuffer([[cell("你", 2), cell("", 0), cell("o"), cell("k")]]),
      0,
      4,
    );
    expect(row.text).toBe("你ok");
    expect(cellSpan(row, 0, 1)).toEqual({ x: 0, width: 2 });
    expect(cellSpan(row, 1, 2)).toEqual({ x: 2, width: 2 });
    expect(cellSpan(row, 0, 3)).toEqual({ x: 0, width: 4 });
  });

  it("handles surrogate pairs in one cell", () => {
    const row = bufferRowCells(fakeBuffer([[cell("😀", 2), cell("", 0), cell("x")]]), 0, 3);
    expect(row.text).toBe("😀x");
    expect(cellSpan(row, 2, 1)).toEqual({ x: 2, width: 1 });
    expect(cellSpan(row, 0, 2)).toEqual({ x: 0, width: 2 });
  });

  it("drops the padding before a wide glyph wrapped onto the next row only when joined", () => {
    const buf = fakeBuffer([
      [cell("a"), cell("")],
      [cell("你", 2), cell("", 0)],
    ]);
    expect(bufferRowText(buf, 0, 2)).toBe("a ");
    expect(bufferRowCells(buf, 0, 2, true)).toEqual({ text: "a", cellAt: [0, 2] });
  });

  it("reads a missing row as empty", () => {
    expect(bufferRowCells(fakeBuffer([]), 0, 4)).toEqual({ text: "", cellAt: [4] });
  });
});

describe("wrapsWideGlyphAfter", () => {
  it("is true only when the next row starts with a wide glyph", () => {
    const buf = fakeBuffer([
      [cell("a"), cell("")],
      [cell("你", 2), cell("", 0)],
      [cell("b"), cell("")],
    ]);
    expect(wrapsWideGlyphAfter(buf, 0)).toBe(true);
    expect(wrapsWideGlyphAfter(buf, 1)).toBe(false);
    // No next row.
    expect(wrapsWideGlyphAfter(buf, 2)).toBe(false);
  });
});
