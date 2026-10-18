import { describe, expect, it } from "vitest";
import {
  appOwnsScroll,
  findInLines,
  historyLines,
  nearestMatch,
  reanchorIndex,
  rowInViewport,
  type SearchFlags,
  stepIndex,
  targetOffset,
} from "./historySearch";

const plain: SearchFlags = { caseSensitive: false, regex: false };
/** Search history text as the pane does: split it, then find. */
const find = (text: string, query: string, flags: SearchFlags) =>
  findInLines(historyLines(text), query, flags);

describe("historyLines", () => {
  it("splits on LF and CRLF and drops only the piece after a final newline", () => {
    expect(historyLines("a\r\nb\n")).toEqual(["a", "b"]);
    expect(historyLines("a\nb")).toEqual(["a", "b"]);
  });

  it("keeps trailing blank rows, which herdr's scroll offset counts", () => {
    expect(historyLines("a\r\nb\n\n  \n")).toEqual(["a", "b", "", "  "]);
    expect(historyLines("a\n\n\n")).toEqual(["a", "", ""]);
  });

  it("keeps blank lines between content", () => {
    expect(historyLines("a\n\nb")).toEqual(["a", "", "b"]);
  });

  it("is empty for empty text and keeps all-blank rows", () => {
    expect(historyLines("")).toEqual([]);
    expect(historyLines("\n \n")).toEqual(["", " "]);
  });
});

describe("appOwnsScroll", () => {
  it("leaves the scroll to history on the normal screen without mouse tracking", () => {
    expect(appOwnsScroll("normal", "none")).toBe(false);
  });

  it("gives the scroll to the app on the alternate screen", () => {
    expect(appOwnsScroll("alternate", "none")).toBe(true);
  });

  it("gives the scroll to the app when it tracks the mouse", () => {
    for (const mode of ["x10", "vt200", "drag", "any"]) {
      expect(appOwnsScroll("normal", mode)).toBe(true);
    }
  });
});

describe("findInLines", () => {
  it("is case-insensitive by default", () => {
    const r = find("Error here\nno\nerror ERROR", "error", plain);
    expect(r.matches).toEqual([
      { line: 0, col: 0, len: 5 },
      { line: 2, col: 0, len: 5 },
      { line: 2, col: 6, len: 5 },
    ]);
    expect(r.lineCount).toBe(3);
    expect(r.error).toBeUndefined();
  });

  it("respects case sensitivity", () => {
    const r = find("Error error", "error", { caseSensitive: true, regex: false });
    expect(r.matches).toEqual([{ line: 0, col: 6, len: 5 }]);
  });

  it("treats regex metacharacters literally when regex is off", () => {
    const r = find("a.b axb (x)", "a.b", plain);
    expect(r.matches).toEqual([{ line: 0, col: 0, len: 3 }]);
    expect(find("f(x) [y]", "(x) [", plain).matches).toEqual([{ line: 0, col: 1, len: 5 }]);
  });

  it("matches regexes", () => {
    const r = find("id=12 id=345\nid=x", "id=\\d+", { caseSensitive: true, regex: true });
    expect(r.matches).toEqual([
      { line: 0, col: 0, len: 5 },
      { line: 0, col: 6, len: 6 },
    ]);
  });

  it("finds several non-overlapping hits per line", () => {
    expect(find("aaaa", "aa", plain).matches).toEqual([
      { line: 0, col: 0, len: 2 },
      { line: 0, col: 2, len: 2 },
    ]);
  });

  it("reports an invalid regex instead of throwing", () => {
    const r = find("abc", "a(", { caseSensitive: false, regex: true });
    expect(r).toEqual({ matches: [], lineCount: 1, error: "invalid-regex" });
  });

  it("skips zero-length regex hits without looping", () => {
    expect(find("abc", "x*", { caseSensitive: false, regex: true }).matches).toEqual([]);
    expect(find("ab", "^|b", { caseSensitive: false, regex: true }).matches).toEqual([
      { line: 0, col: 1, len: 1 },
    ]);
  });

  it("finds nothing for an empty query", () => {
    expect(find("abc\ndef", "", plain)).toEqual({ matches: [], lineCount: 2 });
  });

  it("counts trailing blank rows in the line count", () => {
    const r = find("x\n   \n\n", "x", plain);
    expect(r.lineCount).toBe(3);
    expect(r.matches).toEqual([{ line: 0, col: 0, len: 1 }]);
  });

  it("searches pre-split lines", () => {
    expect(findInLines(["foo", "bar foo"], "foo", plain).matches).toEqual([
      { line: 0, col: 0, len: 3 },
      { line: 1, col: 4, len: 3 },
    ]);
  });
});

describe("targetOffset", () => {
  // 100 lines of history on a 20-row screen: the live screen is lines 80..99.
  it("is 0 for a line on the live screen", () => {
    expect(targetOffset(99, 100, 20)).toBe(0);
    expect(targetOffset(80, 100, 20)).toBe(0);
  });

  it("centres a mid-history line", () => {
    const off = targetOffset(50, 100, 20);
    expect(off).toBe(40);
    expect(rowInViewport(50, 100, 20, off)).toBe(10);
  });

  it("centres a line just above the live screen", () => {
    const off = targetOffset(79, 100, 20);
    expect(off).toBe(11);
    expect(rowInViewport(79, 100, 20, off)).toBe(10);
  });

  it("clamps at the top of history", () => {
    expect(targetOffset(0, 100, 20)).toBe(80);
    expect(targetOffset(5, 100, 20)).toBe(80);
    expect(rowInViewport(5, 100, 20, 80)).toBe(5);
  });

  it("is 0 when history is shorter than the screen", () => {
    expect(targetOffset(0, 10, 20)).toBe(0);
    expect(targetOffset(0, 20, 20)).toBe(0);
  });

  it("handles an odd row count", () => {
    const off = targetOffset(30, 100, 21);
    expect(rowInViewport(30, 100, 21, off)).toBe(10);
  });
});

describe("rowInViewport", () => {
  it("maps lines on screen to rows", () => {
    expect(rowInViewport(80, 100, 20, 0)).toBe(0);
    expect(rowInViewport(99, 100, 20, 0)).toBe(19);
    expect(rowInViewport(60, 100, 20, 20)).toBe(0);
  });

  it("is null off screen", () => {
    expect(rowInViewport(79, 100, 20, 0)).toBeNull();
    expect(rowInViewport(80, 100, 20, 20)).toBeNull();
  });
});

describe("nearestMatch", () => {
  const hits = [
    { line: 2, col: 0, len: 1 },
    { line: 5, col: 9, len: 1 },
    { line: 5, col: 2, len: 1 },
    { line: 9, col: 0, len: 1 },
  ];

  it("picks the closest row, then the closest column", () => {
    expect(nearestMatch(hits, 6, 3)).toEqual({ line: 5, col: 2, len: 1 });
    expect(nearestMatch(hits, 5, 8)).toEqual({ line: 5, col: 9, len: 1 });
    expect(nearestMatch(hits, 0, 0)).toEqual({ line: 2, col: 0, len: 1 });
  });

  it("is null without hits", () => {
    expect(nearestMatch([], 3, 0)).toBeNull();
  });
});

describe("stepIndex", () => {
  it("wraps both ways", () => {
    expect(stepIndex(0, 3, -1)).toBe(2);
    expect(stepIndex(2, 3, 1)).toBe(0);
    expect(stepIndex(1, 3, -1)).toBe(0);
  });

  it("is 0 with no hits", () => {
    expect(stepIndex(4, 0, 1)).toBe(0);
  });
});

describe("reanchorIndex", () => {
  it("follows a hit as output appends below it", () => {
    const old = { line: 40, col: 3, len: 2 };
    const fresh = [
      { line: 10, col: 0, len: 2 },
      { line: 45, col: 3, len: 2 },
      { line: 47, col: 0, len: 2 },
    ];
    expect(reanchorIndex(old, 50, fresh, 55)).toBe(1);
  });

  it("follows a hit when a capped history drops lines from the top", () => {
    // Same line count, but 5 new lines pushed the window: the hit moved up by 5.
    const old = { line: 40, col: 1, len: 2 };
    const fresh = [
      { line: 35, col: 1, len: 2 },
      { line: 40, col: 1, len: 2 },
    ];
    expect(reanchorIndex(old, 100, fresh, 100)).toBe(1);
    expect(reanchorIndex(old, 100, [{ line: 35, col: 1, len: 2 }], 105)).toBe(0);
  });

  it("falls back to the nearest hit, then -1 when none", () => {
    const old = { line: 40, col: 0, len: 1 };
    expect(
      reanchorIndex(
        old,
        50,
        [
          { line: 10, col: 0, len: 1 },
          { line: 38, col: 0, len: 1 },
        ],
        50,
      ),
    ).toBe(1);
    expect(reanchorIndex(old, 50, [], 50)).toBe(-1);
  });
});
