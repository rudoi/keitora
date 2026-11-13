import { describe, expect, it } from "vitest";
import { type DepNode, depChanges, directDeps, parseTags, parseTaskIds, sameTags } from "./tasks";

describe("parseTags", () => {
  it("trims, drops empties and repeats, keeps first-seen order", () => {
    expect(parseTags(" ui, backend,,ui , docs")).toEqual(["ui", "backend", "docs"]);
  });

  it("yields no tags for a blank field", () => {
    expect(parseTags("")).toEqual([]);
    expect(parseTags(" , ")).toEqual([]);
  });
});

describe("sameTags", () => {
  it("ignores order", () => {
    expect(sameTags(["a", "b"], ["b", "a"])).toBe(true);
  });

  it("notices an added, removed or swapped tag", () => {
    expect(sameTags(["a"], ["a", "b"])).toBe(false);
    expect(sameTags(["a", "b"], ["a"])).toBe(false);
    expect(sameTags(["a", "b"], ["a", "c"])).toBe(false);
  });
});

describe("parseTaskIds", () => {
  it("accepts ids with or without #, split by commas or spaces, de-duplicated", () => {
    expect(parseTaskIds("#3, 5 #7,3")).toEqual({ ids: [3, 5, 7], invalid: [] });
  });

  it("reports tokens that are not task ids", () => {
    expect(parseTaskIds("3, abc, #0, 4x")).toEqual({ ids: [3], invalid: ["abc", "#0", "4x"] });
  });

  it("treats a blank field as no dependencies", () => {
    expect(parseTaskIds("  ")).toEqual({ ids: [], invalid: [] });
  });
});

describe("directDeps", () => {
  const node = (id: number, depth: number, status: DepNode["status"] = "open"): DepNode => ({
    id,
    depth,
    title: `task ${id}`,
    status,
  });

  it("keeps only depth-1 nodes, done ones included, once each", () => {
    const tree = [node(2, 1, "done"), node(3, 1), node(4, 2), node(3, 1)];
    expect(directDeps(tree)).toEqual([2, 3]);
  });
});

describe("depChanges", () => {
  it("splits an edit into edges to add and edges to remove", () => {
    expect(depChanges([1, 2, 3], [2, 4])).toEqual({ add: [4], remove: [1, 3] });
  });

  it("is a no-op when nothing changed", () => {
    expect(depChanges([1, 2], [2, 1])).toEqual({ add: [], remove: [] });
  });
});
