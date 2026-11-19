// How the rail orders and filters a project's worktrees: custom order (what a drag sets),
// alphabetical, or grouped by emoji, with worktrees whose emoji is toggled off left out.
// Emoji are gated by the `[rail] emoji` setting: with it off the rail behaves as if no
// worktree had one. Pure, keyed by worktree name.

import { arrayMove } from "@dnd-kit/sortable";
import type { ProjectSummary, RailView, WorktreeSort } from "./ipc";
import { applyOrder, sameOrder } from "./railOrder";

/** What the rail shows before `get_rail_view` answers: the registry defaults. */
export const DEFAULT_RAIL_VIEW: RailView = {
  emoji: false,
  worktree_sort: "custom",
  hidden_emoji: [],
};

/** The emoji the picker offers, in the order "sort by emoji" groups them. */
export const EMOJI_PALETTE: readonly string[] = [
  "🔥",
  "⭐",
  "🚀",
  "🎯",
  "🐛",
  "🧪",
  "🛠️",
  "🎨",
  "📝",
  "📦",
  "🔒",
  "🧹",
  "💡",
  "⏳",
  "💤",
  "✅",
  "🟥",
  "🟧",
  "🟨",
  "🟩",
  "🟦",
  "🟪",
  "⬛",
  "⬜",
];

export const SORT_LABEL: Record<WorktreeSort, string> = {
  custom: "Custom order",
  alpha: "Alphabetical",
  emoji: "By emoji",
};

/** The view actually applied: with emoji off, no emoji sort and nothing hidden. */
export function effectiveView(view: RailView): RailView {
  if (view.emoji) return view;
  return {
    emoji: false,
    worktree_sort: view.worktree_sort === "emoji" ? "custom" : view.worktree_sort,
    hidden_emoji: [],
  };
}

/** Palette emoji first, in palette order; any other emoji after, by code point. */
function compareEmoji(a: string, b: string): number {
  const ia = EMOJI_PALETTE.indexOf(a);
  const ib = EMOJI_PALETTE.indexOf(b);
  if (ia !== -1 || ib !== -1) {
    if (ia === -1) return 1;
    if (ib === -1) return -1;
    return ia - ib;
  }
  return a < b ? -1 : a > b ? 1 : 0;
}

const byName = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

/**
 * `items` sorted for the rail. Custom: in `order`, with the rest after in their incoming
 * order. Alphabetical: by name, natural number order. Emoji: grouped by `emojiOf` (no
 * emoji last), custom order within a group.
 */
export function sortWorktrees<T extends { name: string }>(
  items: readonly T[],
  sort: WorktreeSort,
  order: readonly string[],
  emojiOf: (name: string) => string | null,
): T[] {
  if (sort === "alpha") return [...items].sort((a, b) => byName.compare(a.name, b.name));
  const custom = applyOrder(items, order, (it) => it.name);
  if (sort === "custom") return custom;
  return custom.sort((a, b) => {
    const ea = emojiOf(a.name);
    const eb = emojiOf(b.name);
    if (ea === null || eb === null) return ea === eb ? 0 : ea === null ? 1 : -1;
    return compareEmoji(ea, eb);
  });
}

/** Whether a worktree with `emoji` is filtered out by the toggled-off `hidden` emoji. */
export function isHidden(emoji: string | null, hidden: readonly string[]): boolean {
  return emoji !== null && hidden.includes(emoji);
}

/**
 * The emoji the filter lists: every one in use plus every hidden one (so a hidden emoji
 * can be turned back on after its last worktree lost it), in sort order.
 */
export function emojiChoices(inUse: Iterable<string>, hidden: readonly string[]): string[] {
  return [...new Set([...inUse, ...hidden])].sort(compareEmoji);
}

/**
 * The new custom order after dragging `active` onto `over`'s place, both names in
 * `names` (the full list as shown, hidden worktrees included); null when nothing moves.
 */
export function moveWorktree(
  names: readonly string[],
  active: string,
  over: string,
): string[] | null {
  const from = names.indexOf(active);
  const to = names.indexOf(over);
  if (from === -1 || to === -1 || from === to) return null;
  return arrayMove([...names], from, to);
}

/**
 * The dropped orders (by project key) still worth applying once the project list
 * reports `projects`: an entry goes once its project reports that order or is gone.
 * Hands back `overrides` itself when nothing settled.
 */
export function settleWorktreeOrders(
  overrides: Readonly<Record<string, readonly string[]>>,
  projects: readonly ProjectSummary[],
): Readonly<Record<string, readonly string[]>> {
  let out: Record<string, readonly string[]> | null = null;
  for (const [key, order] of Object.entries(overrides)) {
    const p = projects.find((x) => x.name === key);
    if (p === undefined || sameOrder(order, p.worktree_order ?? [])) {
      out ??= { ...overrides };
      delete out[key];
    }
  }
  return out ?? overrides;
}
