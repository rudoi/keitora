import { describe, expect, it } from "vitest";
import { keepStable } from "./projectsOverview";

describe("keepStable", () => {
  it("keeps the previous value when the next one serializes the same", () => {
    const first = keepStable(null, [{ a: 1 }]);
    const again = keepStable(first, [{ a: 1 }]);
    expect(again).toBe(first);
    expect(again.value).toBe(first.value);
  });

  it("takes the next value when it differs", () => {
    const first = keepStable(null, [{ a: 1 }]);
    const next = [{ a: 2 }];
    expect(keepStable(first, next).value).toBe(next);
  });
});
