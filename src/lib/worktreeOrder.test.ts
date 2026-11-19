import { describe, expect, it } from "vitest";
import type { ProjectSummary } from "./ipc";
import {
  DEFAULT_RAIL_VIEW,
  EMOJI_PALETTE,
  effectiveView,
  emojiChoices,
  isHidden,
  moveWorktree,
  settleWorktreeOrders,
  sortWorktrees,
} from "./worktreeOrder";

const items = (...names: string[]) => names.map((name) => ({ name }));
const names = (xs: { name: string }[]) => xs.map((x) => x.name);
const emojiOf = (m: Record<string, string>) => (n: string) => m[n] ?? null;
const none = emojiOf({});

describe("sortWorktrees", () => {
  it("custom: follows the order, the rest after in incoming order", () => {
    const out = sortWorktrees(
      items("main", "alpha", "bravo", "charlie"),
      "custom",
      ["bravo", "main"],
      none,
    );
    expect(names(out)).toEqual(["bravo", "main", "alpha", "charlie"]);
  });

  it("custom with no order keeps the incoming order", () => {
    expect(names(sortWorktrees(items("main", "bravo", "alpha"), "custom", [], none))).toEqual([
      "main",
      "bravo",
      "alpha",
    ]);
  });

  it("alpha: by name, case-insensitive, numbers in natural order", () => {
    const out = sortWorktrees(items("wt10", "Bravo", "wt2", "alpha"), "alpha", ["wt10"], none);
    expect(names(out)).toEqual(["alpha", "Bravo", "wt2", "wt10"]);
  });

  it("emoji: palette order, other emoji after, none last, custom order within a group", () => {
    const e = emojiOf({ alpha: "💤", bravo: "🔥", charlie: "🦄", main: "🔥" });
    const out = sortWorktrees(
      items("main", "alpha", "bravo", "charlie", "wt"),
      "emoji",
      ["wt", "bravo", "alpha", "charlie", "main"],
      e,
    );
    expect(names(out)).toEqual(["bravo", "main", "alpha", "charlie", "wt"]);
  });

  it("does not mutate its input", () => {
    const input = items("b", "a");
    sortWorktrees(input, "alpha", [], none);
    expect(names(input)).toEqual(["b", "a"]);
  });
});

describe("effectiveView", () => {
  it("with emoji off, drops the emoji sort and the filter", () => {
    expect(effectiveView({ emoji: false, worktree_sort: "emoji", hidden_emoji: ["💤"] })).toEqual(
      DEFAULT_RAIL_VIEW,
    );
    expect(
      effectiveView({ emoji: false, worktree_sort: "alpha", hidden_emoji: [] }).worktree_sort,
    ).toBe("alpha");
  });

  it("with emoji on, is the view itself", () => {
    const v = { emoji: true, worktree_sort: "emoji" as const, hidden_emoji: ["💤"] };
    expect(effectiveView(v)).toBe(v);
  });
});

describe("isHidden / emojiChoices", () => {
  it("hides only worktrees whose emoji is toggled off", () => {
    expect(isHidden("💤", ["💤"])).toBe(true);
    expect(isHidden("🔥", ["💤"])).toBe(false);
    expect(isHidden(null, ["💤"])).toBe(false);
  });

  it("lists used and hidden emoji once, in sort order", () => {
    expect(emojiChoices(["🦄", "💤", "🔥", "💤"], ["⭐"])).toEqual(["🔥", "⭐", "💤", "🦄"]);
  });

  it("palette has no repeats", () => {
    expect(new Set(EMOJI_PALETTE).size).toBe(EMOJI_PALETTE.length);
  });
});

describe("moveWorktree", () => {
  it("moves the dragged name to the target's place, either way", () => {
    expect(moveWorktree(["a", "b", "c", "d"], "a", "c")).toEqual(["b", "c", "a", "d"]);
    expect(moveWorktree(["a", "b", "c", "d"], "d", "b")).toEqual(["a", "d", "b", "c"]);
  });

  it("is null when nothing moves or a name is unknown", () => {
    expect(moveWorktree(["a", "b"], "a", "a")).toBeNull();
    expect(moveWorktree(["a", "b"], "x", "a")).toBeNull();
    expect(moveWorktree(["a", "b"], "a", "x")).toBeNull();
  });
});

describe("settleWorktreeOrders", () => {
  const proj = (name: string, order?: string[]): ProjectSummary => ({
    name,
    resolved_name: name,
    root: `/code/${name}`,
    active: false,
    db_path: `/code/${name}/tasks.db`,
    worktrees: [],
    backend: "sqlite",
    worktree_order: order,
  });

  it("keeps an override until its project reports that order", () => {
    const o = { proj: ["b", "a"] };
    expect(settleWorktreeOrders(o, [proj("proj", ["a", "b"])])).toBe(o);
    expect(settleWorktreeOrders(o, [proj("proj", ["b", "a"])])).toEqual({});
  });

  it("drops an override whose project is gone, keeping the others", () => {
    const o = { gone: ["a"], proj: ["b", "a"] };
    expect(settleWorktreeOrders(o, [proj("proj")])).toEqual({ proj: ["b", "a"] });
  });
});
