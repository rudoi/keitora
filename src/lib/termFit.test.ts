import { describe, expect, it } from "vitest";
import {
  accumulateWheelLines,
  clampPtySize,
  cursorCell,
  dimsAreSane,
  dimsEqual,
  FALLBACK_ROW_PX,
  MIN_PTY,
  WHEEL_DELTA_LINE,
  WHEEL_DELTA_PAGE,
  WHEEL_DELTA_PIXEL,
  WHEEL_LINES_CAP,
  wheelCell,
} from "./termFit";

// Unit tests for the pure terminal size/fit decision helpers.

describe("clampPtySize", () => {
  it("floors rows below the minimum", () => {
    expect(clampPtySize({ rows: 1, cols: 80 })).toEqual({ rows: MIN_PTY.rows, cols: 80 });
  });

  it("floors cols below the minimum", () => {
    expect(clampPtySize({ rows: 24, cols: 3 })).toEqual({ rows: 24, cols: MIN_PTY.cols });
  });

  it("floors both axes together", () => {
    expect(clampPtySize({ rows: 0, cols: 0 })).toEqual({
      rows: MIN_PTY.rows,
      cols: MIN_PTY.cols,
    });
  });

  it("passes through values above the floor unchanged", () => {
    expect(clampPtySize({ rows: 40, cols: 120 })).toEqual({ rows: 40, cols: 120 });
  });
});

describe("dimsAreSane", () => {
  it("rejects undefined and null", () => {
    expect(dimsAreSane(undefined)).toBe(false);
    expect(dimsAreSane(null)).toBe(false);
  });

  it("rejects a zero axis", () => {
    expect(dimsAreSane({ rows: 0, cols: 80 })).toBe(false);
    expect(dimsAreSane({ rows: 24, cols: 0 })).toBe(false);
  });

  it("rejects negatives", () => {
    expect(dimsAreSane({ rows: -1, cols: 80 })).toBe(false);
  });

  it("rejects non-finite measurements", () => {
    expect(dimsAreSane({ rows: NaN, cols: 80 })).toBe(false);
    expect(dimsAreSane({ rows: 24, cols: Infinity })).toBe(false);
  });

  it("accepts a positive, finite proposal", () => {
    expect(dimsAreSane({ rows: 24, cols: 80 })).toBe(true);
  });
});

describe("dimsEqual", () => {
  it("is true for identical geometry", () => {
    expect(dimsEqual({ rows: 24, cols: 80 }, { rows: 24, cols: 80 })).toBe(true);
  });

  it("is false when rows differ", () => {
    expect(dimsEqual({ rows: 24, cols: 80 }, { rows: 25, cols: 80 })).toBe(false);
  });

  it("is false when cols differ", () => {
    expect(dimsEqual({ rows: 24, cols: 80 }, { rows: 24, cols: 81 })).toBe(false);
  });
});

describe("accumulateWheelLines", () => {
  it("accumulates a sub-line delta without emitting a line", () => {
    const { lines, remainder } = accumulateWheelLines(10, 17, 0);
    expect(lines).toBe(0);
    expect(remainder).toBeCloseTo(10 / 17, 5);
  });

  it("carries the remainder across calls to cross a line boundary", () => {
    const first = accumulateWheelLines(10, 17, 0);
    const second = accumulateWheelLines(10, 17, first.remainder);
    expect(second.lines).toBe(1);
    expect(second.remainder).toBeCloseTo(20 / 17 - 1, 5);
  });

  it("emits exactly one line for one row of pixels with no remainder", () => {
    expect(accumulateWheelLines(17, 17, 0)).toEqual({ lines: 1, remainder: 0 });
  });

  it("emits multiple lines for a large delta", () => {
    expect(accumulateWheelLines(170, 17, 0).lines).toBe(10);
  });

  it("preserves sign for upward scrolling", () => {
    expect(accumulateWheelLines(-17, 17, 0)).toEqual({ lines: -1, remainder: 0 });
    const { lines, remainder } = accumulateWheelLines(-10, 17, 0);
    expect(lines).toBe(0);
    expect(remainder).toBeCloseTo(-10 / 17, 5);
  });

  it("never returns NaN/Infinity for a zero or non-finite row height", () => {
    for (const badRow of [0, -5, NaN, Infinity, -Infinity]) {
      const { lines, remainder } = accumulateWheelLines(FALLBACK_ROW_PX, badRow, 0);
      expect(Number.isFinite(lines)).toBe(true);
      expect(Number.isFinite(remainder)).toBe(true);
      expect(lines).toBe(1);
    }
  });

  it("treats a non-finite delta as no movement", () => {
    expect(accumulateWheelLines(NaN, 17, 0)).toEqual({ lines: 0, remainder: 0 });
    expect(accumulateWheelLines(Infinity, 17, 0)).toEqual({ lines: 0, remainder: 0 });
  });

  it("treats a non-finite prior remainder as zero", () => {
    expect(accumulateWheelLines(17, 17, NaN).lines).toBe(1);
  });

  it("drops an opposite-sign remainder on direction reversal", () => {
    const { lines, remainder } = accumulateWheelLines(-5, 17, 0.9);
    expect(lines).toBe(0);
    expect(remainder).toBeLessThan(0);
    expect(remainder).toBeCloseTo(-5 / 17, 5);
  });

  it("keeps a same-sign remainder (no reset)", () => {
    const { remainder } = accumulateWheelLines(5, 17, 0.5);
    expect(remainder).toBeCloseTo(0.5 + 5 / 17, 5);
  });
});

describe("accumulateWheelLines — delta modes", () => {
  it("defaults to pixel mode", () => {
    expect(accumulateWheelLines(34, 17, 0)).toEqual(
      accumulateWheelLines(34, 17, 0, WHEEL_DELTA_PIXEL, 40),
    );
    expect(accumulateWheelLines(34, 17, 0).lines).toBe(2);
  });

  it("passes line deltas through regardless of row height", () => {
    expect(accumulateWheelLines(3, 17, 0, WHEEL_DELTA_LINE, 40)).toEqual({
      lines: 3,
      remainder: 0,
    });
    expect(accumulateWheelLines(-3, 0, 0, WHEEL_DELTA_LINE, 40)).toEqual({
      lines: -3,
      remainder: 0,
    });
  });

  it("carries fractional line deltas", () => {
    const first = accumulateWheelLines(0.5, 17, 0, WHEEL_DELTA_LINE, 40);
    expect(first.lines).toBe(0);
    const second = accumulateWheelLines(0.75, 17, first.remainder, WHEEL_DELTA_LINE, 40);
    expect(second.lines).toBe(1);
    expect(second.remainder).toBeCloseTo(0.25, 5);
  });

  it("multiplies page deltas by the row count", () => {
    expect(accumulateWheelLines(1, 17, 0, WHEEL_DELTA_PAGE, 40)).toEqual({
      lines: 40,
      remainder: 0,
    });
    expect(accumulateWheelLines(-1, 17, 0, WHEEL_DELTA_PAGE, 30).lines).toBe(-30);
  });

  it("treats a page as one line when the row count is unusable", () => {
    for (const badRows of [0, -3, NaN]) {
      expect(accumulateWheelLines(1, 17, 0, WHEEL_DELTA_PAGE, badRows).lines).toBe(1);
    }
  });

  it("drops an opposite-sign remainder in line mode too", () => {
    const { lines, remainder } = accumulateWheelLines(-0.5, 17, 0.9, WHEEL_DELTA_LINE, 40);
    expect(lines).toBe(0);
    expect(remainder).toBeCloseTo(-0.5, 5);
  });
});

describe("accumulateWheelLines — extreme-delta guard", () => {
  it("caps a huge pixel delta and drops the carry", () => {
    expect(accumulateWheelLines(1e9, 17, 0)).toEqual({ lines: WHEEL_LINES_CAP, remainder: 0 });
    expect(accumulateWheelLines(-1e9, 17, 0)).toEqual({
      lines: -WHEEL_LINES_CAP,
      remainder: 0,
    });
  });

  it("caps line and page modes", () => {
    expect(accumulateWheelLines(500, 17, 0, WHEEL_DELTA_LINE, 40).lines).toBe(WHEEL_LINES_CAP);
    expect(accumulateWheelLines(10, 17, 0, WHEEL_DELTA_PAGE, 40).lines).toBe(WHEEL_LINES_CAP);
  });

  it("does not cap a delta exactly at the limit", () => {
    expect(accumulateWheelLines(WHEEL_LINES_CAP, 17, 0, WHEEL_DELTA_LINE, 40)).toEqual({
      lines: WHEEL_LINES_CAP,
      remainder: 0,
    });
  });
});

describe("wheelCell", () => {
  it("maps an interior point to its cell", () => {
    expect(wheelCell(25, 40, 10, 20, 80, 24)).toEqual({ column: 2, row: 2 });
  });

  it("maps the top-left corner to 0,0", () => {
    expect(wheelCell(0, 0, 10, 20, 80, 24)).toEqual({ column: 0, row: 0 });
  });

  it("maps the last pixel of the grid to the last cell", () => {
    expect(wheelCell(799.9, 479.9, 10, 20, 80, 24)).toEqual({ column: 79, row: 23 });
  });

  it("clamps points past the right/bottom edge", () => {
    expect(wheelCell(800, 480, 10, 20, 80, 24)).toEqual({ column: 79, row: 23 });
    expect(wheelCell(5000, 5000, 10, 20, 80, 24)).toEqual({ column: 79, row: 23 });
  });

  it("clamps points before the left/top edge", () => {
    expect(wheelCell(-5, -1, 10, 20, 80, 24)).toEqual({ column: 0, row: 0 });
  });

  it("maps to 0 when the cell size or grid is unusable", () => {
    expect(wheelCell(25, 40, 0, NaN, 80, 24)).toEqual({ column: 0, row: 0 });
    expect(wheelCell(25, 40, 10, 20, 0, 0)).toEqual({ column: 0, row: 0 });
    expect(wheelCell(NaN, Infinity, 10, 20, 80, 24)).toEqual({ column: 0, row: 0 });
  });
});

describe("cursorCell", () => {
  it("passes an in-grid cursor through", () => {
    expect(cursorCell(12, 7, 80, 24)).toEqual({ column: 12, row: 7 });
  });

  it("clamps a cursor past the grid's edges", () => {
    expect(cursorCell(80, 24, 80, 24)).toEqual({ column: 79, row: 23 });
    expect(cursorCell(-1, -3, 80, 24)).toEqual({ column: 0, row: 0 });
  });

  it("maps to 0 for an empty grid or a non-finite cursor", () => {
    expect(cursorCell(5, 5, 0, 0)).toEqual({ column: 0, row: 0 });
    expect(cursorCell(NaN, Infinity, 80, 24)).toEqual({ column: 0, row: 0 });
  });
});
