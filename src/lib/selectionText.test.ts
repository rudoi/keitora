import type { IBuffer } from "@xterm/xterm";
import { describe, expect, it } from "vitest";
import {
  alignShift,
  boundaryAt,
  copyEol,
  rangeBetween,
  rectSelectionAfterPress,
  rowsToText,
  type SelectionRange,
  sameSelection,
  selectionLength,
  selectionToText,
  shiftSelection,
  snapshotRow,
} from "./selectionText";

type Cell = { chars: string; width: number };
type Row = { cells: Cell[]; wrapped?: boolean };

const COLS = 6;
const empty = (): Cell => ({ chars: "", width: 1 });

/** A row of `text`, one cell per character, padded to `COLS` with empty cells. */
function row(text: string, wrapped = false): Row {
  const cells: Cell[] = [...text].map((chars) => ({ chars, width: 1 }));
  while (cells.length < COLS) cells.push(empty());
  return { cells, wrapped };
}

/** A row of literal cells, as given (wide glyphs and their zero-width tails included). */
function cellsRow(cells: Cell[], wrapped = false): Row {
  return { cells, wrapped };
}

/** A buffer of `rows`; only rows marked `wrapped` report `isWrapped`. */
function fakeBuffer(rows: Row[]): IBuffer {
  const toCell = (c: Cell) => ({ getChars: () => c.chars, getWidth: () => c.width });
  return {
    getNullCell: () => ({}),
    getLine: (y: number) =>
      rows[y] && {
        isWrapped: !!rows[y].wrapped,
        getCell: (x: number) => (rows[y].cells[x] ? toCell(rows[y].cells[x]) : undefined),
      },
  } as unknown as IBuffer;
}

/** Whole rows `first..last`. */
const rows = (first: number, last: number): SelectionRange => ({
  start: { x: 0, y: first },
  end: { x: COLS, y: last },
});

const copy = (buf: Row[], range: SelectionRange | null | undefined = rows(0, buf.length - 1)) =>
  selectionToText(fakeBuffer(buf), COLS, range, "\n");

describe("selectionToText", () => {
  it("joins rows marked wrapped and keeps the space at the boundary", () => {
    expect(copy([row("hello "), row("world", true)])).toBe("hello world");
  });

  it("joins a full-width row onto the next and breaks after a short row", () => {
    expect(copy([row("abcdef"), row("gh"), row("ij")])).toBe("abcdefgh\nij");
  });

  it("never joins onto a blank row and keeps blank rows exactly", () => {
    expect(copy([row("abcdef"), row(""), row("b")])).toBe("abcdef\n\nb");
    expect(copy([row("abcdef"), row(""), row("   "), row(""), row("b")])).toBe("abcdef\n\n\n\nb");
    // A wrapped mark does not override the blank rule either.
    expect(copy([row("abcdef"), row("", true), row("b")])).toBe("abcdef\n\nb");
  });

  it("keeps leading and trailing blank rows", () => {
    expect(copy([row(""), row("a"), row("")])).toBe("\na\n");
  });

  it("does not treat a trailing box-drawing or block glyph as filled", () => {
    expect(copy([row("──────"), row("text")])).toBe("──────\ntext");
    expect(copy([row("ab████"), row("text")])).toBe("ab████\ntext");
    expect(copy([row("│ ab │"), row("│ cd │")])).toBe("│ ab │\n│ cd │");
    // An explicit wrap mark still joins.
    expect(copy([row("──────"), row("──", true)])).toBe("────────");
  });

  it("joins a row whose last two cells hold a wide glyph", () => {
    const wide = cellsRow([
      { chars: "a", width: 1 },
      { chars: "b", width: 1 },
      { chars: "c", width: 1 },
      { chars: "d", width: 1 },
      { chars: "你", width: 2 },
      { chars: "", width: 0 },
    ]);
    expect(copy([wide, row("ok")])).toBe("abcd你ok");
  });

  it("joins across a wide glyph wrapped to the next row without the padding cell", () => {
    const first = row("abcde"); // last cell empty: the wide glyph did not fit
    const next = cellsRow([
      { chars: "你", width: 2 },
      { chars: "", width: 0 },
      { chars: "x", width: 1 },
      empty(),
      empty(),
      empty(),
    ]);
    expect(copy([first, next])).toBe("abcde你x");
    // Without the wide glyph the empty last cell is just a short row.
    expect(copy([first, row("x")])).toBe("abcde\nx");
  });

  it("trims trailing spaces per logical line and turns no-break spaces into spaces", () => {
    expect(copy([row("ab   "), row("c")])).toBe("ab\nc");
    expect(copy([row("a b ")])).toBe("a b");
    // A row of literal spaces to the last cell is not filled.
    expect(copy([row("ab    "), row("c")])).toBe("ab\nc");
  });

  it("cuts partial first and last rows by cell column", () => {
    const buf = [row("abcdef"), row("ghijkl"), row("mn")];
    const range = { start: { x: 2, y: 0 }, end: { x: 3, y: 2 } };
    expect(copy(buf, range)).toBe("cdefghijklmn");
    // end.x is exclusive.
    expect(copy([row("abcdef"), row("gh")], { start: { x: 4, y: 0 }, end: { x: 1, y: 1 } })).toBe(
      "efg",
    );
  });

  it("gives an empty last segment when the selection ends at column 0", () => {
    expect(copy([row("ab"), row("cd")], { start: { x: 0, y: 0 }, end: { x: 0, y: 1 } })).toBe(
      "ab\n",
    );
  });

  it("does not join onto a next row whose selected part is blank", () => {
    // Row 1's text is past end.x, so the full row 0 has nothing to join onto.
    expect(copy([row("abcdef"), row("  xy")], { start: { x: 0, y: 0 }, end: { x: 2, y: 1 } })).toBe(
      "abcdef\n",
    );
  });

  it("selects a single row between both bounds", () => {
    expect(copy([row("abcdef")], { start: { x: 1, y: 0 }, end: { x: 4, y: 0 } })).toBe("bcd");
    expect(copy([row("ab  ef")], { start: { x: 1, y: 0 }, end: { x: 4, y: 0 } })).toBe("b");
  });

  it("includes a wide glyph when either of its cells is in range", () => {
    const buf = [
      cellsRow([
        { chars: "a", width: 1 },
        { chars: "你", width: 2 },
        { chars: "", width: 0 },
        { chars: "b", width: 1 },
        empty(),
        empty(),
      ]),
    ];
    // Ends on the glyph's first cell.
    expect(copy(buf, { start: { x: 0, y: 0 }, end: { x: 2, y: 0 } })).toBe("a你");
    // Starts on the glyph's tail.
    expect(copy(buf, { start: { x: 2, y: 0 }, end: { x: 4, y: 0 } })).toBe("你b");
    // Misses it entirely.
    expect(copy(buf, { start: { x: 3, y: 0 }, end: { x: 4, y: 0 } })).toBe("b");
  });

  it("keeps a surrogate pair whole", () => {
    const buf = [
      cellsRow([
        { chars: "😀", width: 2 },
        { chars: "", width: 0 },
        { chars: "x", width: 1 },
        empty(),
        empty(),
        empty(),
      ]),
    ];
    expect(copy(buf, { start: { x: 1, y: 0 }, end: { x: 3, y: 0 } })).toBe("😀x");
  });

  it("never pulls in rows outside the selection", () => {
    const buf = [row("abcdef"), row("ghijkl"), row("more")];
    expect(copy(buf, rows(0, 1))).toBe("abcdefghijkl");
    expect(copy(buf, rows(1, 2))).toBe("ghijklmore");
  });

  it("reads a missing row as empty", () => {
    expect(copy([row("ab")], rows(0, 2))).toBe("ab\n\n");
  });

  it("separates lines with the given eol", () => {
    const buf = fakeBuffer([row("a"), row("b")]);
    expect(selectionToText(buf, COLS, rows(0, 1), "\r\n")).toBe("a\r\nb");
    expect(selectionToText(buf, COLS, rows(0, 1), "\n")).toBe("a\nb");
  });

  it("returns empty text without a range", () => {
    expect(copy([row("a")], null)).toBe("");
    expect(selectionToText(fakeBuffer([row("a")]), COLS, undefined, "\n")).toBe("");
  });
});

describe("copyEol", () => {
  it("is CRLF on Windows and LF elsewhere", () => {
    expect(copyEol(true)).toBe("\r\n");
    expect(copyEol(false)).toBe("\n");
  });
});

describe("rectSelectionAfterPress", () => {
  const press = (button: number, altKey = false, shiftKey = false) => ({
    button,
    altKey,
    shiftKey,
  });

  it("starts a rectangle on an Alt left press and plain text on a plain one", () => {
    expect(rectSelectionAfterPress(false, press(0, true))).toBe(true);
    expect(rectSelectionAfterPress(true, press(0))).toBe(false);
  });

  it("keeps the mode on a Shift+click extending the selection", () => {
    expect(rectSelectionAfterPress(true, press(0, false, true))).toBe(true);
    expect(rectSelectionAfterPress(false, press(0, true, true))).toBe(false);
  });

  it("keeps the mode on a right click", () => {
    expect(rectSelectionAfterPress(true, press(2))).toBe(true);
    expect(rectSelectionAfterPress(false, press(2, true))).toBe(false);
  });
});

describe("shiftSelection", () => {
  const range = (sx: number, sy: number, ex: number, ey: number): SelectionRange => ({
    start: { x: sx, y: sy },
    end: { x: ex, y: ey },
  });

  it("moves the selection with the text when history scrolls", () => {
    // Scrolling up 3 lines pushes the on-screen text 3 rows down.
    expect(shiftSelection(range(2, 4, 7, 5), 3, 24, 80)).toEqual(range(2, 7, 7, 8));
    expect(shiftSelection(range(2, 4, 7, 5), -2, 24, 80)).toEqual(range(2, 2, 7, 3));
  });

  it("leaves an unmoved selection as it is", () => {
    expect(shiftSelection(range(0, 0, 5, 0), 0, 24, 80)).toEqual(range(0, 0, 5, 0));
  });

  it("clips a selection partly pushed off the top or bottom", () => {
    expect(shiftSelection(range(4, 1, 6, 3), -2, 24, 80)).toEqual(range(0, 0, 6, 1));
    expect(shiftSelection(range(4, 20, 6, 22), 3, 24, 80)).toEqual(range(4, 23, 80, 23));
  });

  it("drops a selection pushed entirely off screen", () => {
    expect(shiftSelection(range(0, 1, 5, 2), -3, 24, 80)).toBeNull();
    expect(shiftSelection(range(0, 20, 5, 22), 4, 24, 80)).toBeNull();
  });

  it("drops a selection whose only visible part is empty", () => {
    // Ends at column 0 of the first row left on screen: nothing selected there.
    expect(shiftSelection(range(5, 0, 0, 2), -2, 24, 80)).toBeNull();
  });
});

describe("selectionLength", () => {
  it("counts cells row-major across rows", () => {
    expect(selectionLength({ start: { x: 2, y: 1 }, end: { x: 7, y: 1 } }, 80)).toBe(5);
    expect(selectionLength({ start: { x: 78, y: 1 }, end: { x: 3, y: 2 } }, 80)).toBe(5);
  });
});

describe("sameSelection", () => {
  it("compares every bound, and treats two absent selections as equal", () => {
    const a = { start: { x: 1, y: 2 }, end: { x: 3, y: 4 } };
    expect(sameSelection(a, { start: { x: 1, y: 2 }, end: { x: 3, y: 4 } })).toBe(true);
    expect(sameSelection(a, { start: { x: 1, y: 2 }, end: { x: 3, y: 5 } })).toBe(false);
    expect(sameSelection(a, undefined)).toBe(false);
    expect(sameSelection(undefined, undefined)).toBe(true);
  });
});

describe("alignShift", () => {
  const screen = (...rows: string[]) => rows;

  it("finds content that moved down (scrolled up into history)", () => {
    const before = screen("c", "d", "e", "f", "g");
    const after = screen("a", "b", "c", "d", "e");
    expect(alignShift(before, after, 2)).toBe(2);
  });

  it("finds content that moved up (scrolled back down)", () => {
    const before = screen("a", "b", "c", "d", "e");
    const after = screen("d", "e", "f", "g", "h");
    expect(alignShift(before, after, -3)).toBe(-3);
  });

  it("reports no movement for an unchanged screen", () => {
    const rows = screen("a", "b", "c");
    expect(alignShift(rows, rows, 3)).toBe(0);
  });

  it("finds a smaller move than asked for when history ran out", () => {
    const before = screen("b", "c", "d", "e");
    const after = screen("a", "b", "c", "d");
    expect(alignShift(before, after, 3)).toBe(1);
  });

  it("ignores blank rows and uses the expected move to break ties", () => {
    const before = screen("", "x", "", "x", "");
    const after = screen("x", "", "x", "", "x");
    // Shifts of -1 and +1 match equally; the expected direction decides.
    expect(alignShift(before, after, 1)).toBe(1);
    expect(alignShift(before, after, -1)).toBe(-1);
  });

  it("gives up when the screens share nothing", () => {
    expect(alignShift(screen("a", "b", "c"), screen("x", "y", "z"), 3)).toBeNull();
    expect(alignShift(screen("", ""), screen("", ""), 0)).toBeNull();
  });

  it("does not trust a single chance match among many rows", () => {
    const before = screen("a", "b", "c", "d", "e");
    const after = screen("v", "w", "x", "c", "z");
    expect(alignShift(before, after, 0)).toBeNull();
  });
});

describe("rangeBetween", () => {
  it("orders the two points, whichever way the drag went", () => {
    const down = rangeBetween({ x: 4, y: 2 }, { x: 1, y: 5 });
    const up = rangeBetween({ x: 1, y: 5 }, { x: 4, y: 2 });
    expect(down).toEqual({ start: { x: 4, y: 2 }, end: { x: 1, y: 5 } });
    expect(up).toEqual(down);
  });

  it("orders by column on the same row", () => {
    expect(rangeBetween({ x: 9, y: 3 }, { x: 2, y: 3 })).toEqual({
      start: { x: 2, y: 3 },
      end: { x: 9, y: 3 },
    });
  });
});

describe("boundaryAt", () => {
  const screen = { left: 100, top: 50, width: 800, height: 240 }; // 80x24 of 10x10 cells

  it("picks the nearest gap across and the row down", () => {
    expect(boundaryAt(100 + 34, 50 + 25, screen, 80, 24)).toEqual({ x: 3, y: 2 });
    expect(boundaryAt(100 + 36, 50 + 25, screen, 80, 24)).toEqual({ x: 4, y: 2 });
  });

  it("clamps a pointer outside the screen to its edges", () => {
    expect(boundaryAt(0, 0, screen, 80, 24)).toEqual({ x: 0, y: 0 });
    expect(boundaryAt(5000, 5000, screen, 80, 24)).toEqual({ x: 80, y: 23 });
  });
});

describe("rowsToText", () => {
  // Rows kept as they scrolled past, keyed by where they are now: -2 and -1 are above the
  // screen, 0 and 1 are on it.
  const kept = (buf: Row[], firstY: number) => {
    const b = fakeBuffer(buf);
    const map = new Map(buf.map((_, i) => [firstY + i, snapshotRow(b, i, COLS)]));
    return (y: number) => map.get(y);
  };

  it("copies rows that have scrolled off the screen", () => {
    const rowAt = kept([row("one"), row("two"), row("three"), row("four")], -2);
    expect(rowsToText(rowAt, COLS, rows(-2, 1), "\n")).toBe("one\ntwo\nthree\nfour");
  });

  it("keeps partial first and last rows and joins wrapped rows", () => {
    const rowAt = kept([row("abcdef"), row("gh", true), row("ijk")], -1);
    const range = { start: { x: 2, y: -1 }, end: { x: 2, y: 1 } };
    expect(rowsToText(rowAt, COLS, range, "\n")).toBe("cdefgh\nij");
  });

  it("reads a row it never saw as blank", () => {
    const rowAt = kept([row("top")], -3);
    expect(rowsToText(rowAt, COLS, rows(-3, -2), "\n")).toBe("top\n");
  });
});
