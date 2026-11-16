import { describe, expect, it } from "vitest";
import { chromeStart, stripPromptChrome } from "./promptChrome";

const RULE = "─".repeat(40);
const join = (...lines: string[]) => lines.join("\n");
const strip = (text: string) => stripPromptChrome(text, "\n");

describe("stripPromptChrome", () => {
  it("cuts the prompt box and its footer off the end of a copy", () => {
    const text = join(
      "● Done. Tests pass.",
      "",
      RULE,
      "❯",
      RULE,
      "  ⏵⏵ auto mode on (shift+tab to cycle) · ← 1 agent",
    );
    expect(strip(text)).toBe("● Done. Tests pass.");
  });

  it("cuts a stale box out of the middle of history", () => {
    const text = join("first answer", RULE, "❯ fix the bug", RULE, "  ? for shortcuts", "second");
    expect(strip(text)).toBe(join("first answer", "second"));
  });

  it("takes a prompt typed over several lines", () => {
    const text = join("out", RULE, "❯ line one", "  line two", RULE, "after");
    expect(strip(text)).toBe(join("out", "after"));
  });

  it("keeps lines after the box that are not its footer", () => {
    const text = join("out", RULE, "❯", RULE, "  ⎿  Read 3 files");
    expect(strip(text)).toBe(join("out", "  ⎿  Read 3 files"));
  });

  it("leaves a rule that does not frame a prompt alone", () => {
    const text = join("Heading", RULE, "body text", RULE);
    expect(strip(text)).toBe(text);
  });

  it("leaves a selection of nothing but the box as it is", () => {
    const text = join(RULE, "❯ my prompt", RULE);
    expect(strip(text)).toBe(text);
  });

  it("works with CRLF line ends", () => {
    const text = ["out", RULE, "❯", RULE].join("\r\n");
    expect(stripPromptChrome(text, "\r\n")).toBe("out");
  });
});

describe("stripPromptChrome: jump-to-bottom hint", () => {
  it("drops a line that held only the hint", () => {
    const text = join("before", `${" ".repeat(60)}Jump to bottom (ctrl+End) ↓`, "after");
    expect(strip(text)).toBe(join("before", "after"));
  });

  it("cuts the hint out of a line it was drawn over", () => {
    const text = "the assets ma Jump to bottom (ctrl+End) ↓ ou know";
    expect(strip(text)).toBe("the assets ma ou know");
  });
});

describe("chromeStart", () => {
  it("finds the top rule of the prompt box", () => {
    const rows = ["● answer", "", RULE, "❯", RULE, "  ⏵⏵ auto mode on (shift+tab to cycle)"];
    expect(chromeStart(rows)).toBe(2);
  });

  it("takes the lowest box when history shows an older one too", () => {
    const rows = ["a", RULE, "❯ old", RULE, "b", RULE, "❯", RULE];
    expect(chromeStart(rows)).toBe(5);
  });

  it("is the screen's height when there is no box", () => {
    expect(chromeStart(["a", "b", RULE, "c"])).toBe(4);
  });
});

describe("Codex prompt box", () => {
  const box = [
    "› Ask Codex to do anything",
    "",
    "  model-x default · D:/repos/proj",
    "  enter/esc latest · ? shortcuts          ⚠ 2 warnings · f2 to view",
  ];

  it("cuts the box out wherever it appears", () => {
    const text = join("• first part", "", ...box, "", ...box, "  ## next part");
    expect(strip(text)).toBe(join("• first part", "", "", "  ## next part"));
  });

  it("keeps a past message and its reply", () => {
    const text = join("› what is a combiner?", "", "• A local reduce.", "", ...box);
    expect(strip(text)).toBe(join("› what is a combiner?", "", "• A local reduce."));
  });

  it("drops the back-to-bottom hint, on its own line or after text", () => {
    const text = join(
      "  it only works when the operation is associative,          ↓ Back to bottom · esc",
      `${" ".repeat(50)}↓ Back to bottom · esc`,
      "end",
    );
    expect(strip(text)).toBe(join("  it only works when the operation is associative,", "end"));
  });

  it("starts the fixed chrome at a hint just above the box", () => {
    const rows = ["• text", `${" ".repeat(40)}↓ Back to bottom · esc`, "", ...box];
    expect(chromeStart(rows)).toBe(1);
  });
});
