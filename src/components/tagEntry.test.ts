import { describe, expect, it } from "vitest";
import { applyTag, currentToken, tagSuggestions } from "./tagEntry";

const TAGS = ["backend", "Bug", "docs", "ui"];

describe("currentToken", () => {
  it("is the trimmed text after the last comma", () => {
    expect(currentToken("ui, ba")).toBe("ba");
    expect(currentToken("ui, ")).toBe("");
    expect(currentToken("do")).toBe("do");
    expect(currentToken("")).toBe("");
  });
});

describe("tagSuggestions", () => {
  it("lists every project tag for an empty field", () => {
    expect(tagSuggestions(TAGS, "")).toEqual(TAGS);
  });

  it("narrows by the tag being typed, case-insensitively", () => {
    expect(tagSuggestions(TAGS, "B")).toEqual(["backend", "Bug"]);
    expect(tagSuggestions(TAGS, "ui, do")).toEqual(["docs"]);
  });

  it("hides tags already entered, whatever their case", () => {
    expect(tagSuggestions(TAGS, "UI, bug, ")).toEqual(["backend", "docs"]);
  });

  it("still offers the tag being typed even when it matches exactly", () => {
    expect(tagSuggestions(TAGS, "docs")).toEqual(["docs"]);
  });

  it("is empty when nothing matches", () => {
    expect(tagSuggestions(TAGS, "zzz")).toEqual([]);
  });
});

describe("applyTag", () => {
  it("replaces the tag being typed and leaves room for the next", () => {
    expect(applyTag("ui, ba", "backend")).toBe("ui, backend, ");
    expect(applyTag("", "docs")).toBe("docs, ");
    expect(applyTag("ui,", "docs")).toBe("ui, docs, ");
  });

  it("normalizes earlier entries", () => {
    expect(applyTag(" ui ,, bug,d", "docs")).toBe("ui, bug, docs, ");
  });
});
