// Pure helpers for the task form's comma-separated Tags field and its suggestion
// list. Kept free of React so the editing rules are unit-testable; TagInput.tsx
// is the thin component over them.

import { parseTags } from "../lib/tasks";
import { filterTags } from "./board/tagFilter";

/** The tag being typed: the text after the last comma, trimmed. */
export function currentToken(value: string): string {
  return value.slice(value.lastIndexOf(",") + 1).trim();
}

/**
 * The project tags to suggest for `value`: those not already entered (compared
 * case-insensitively; the tag being typed doesn't count as entered), narrowed by a
 * case-insensitive substring match on the tag being typed. Order of `tags` is kept.
 */
export function tagSuggestions(tags: string[], value: string): string[] {
  const entered = new Set(
    parseTags(value.slice(0, value.lastIndexOf(",") + 1)).map((t) => t.toLowerCase()),
  );
  return filterTags(tags, currentToken(value)).filter((t) => !entered.has(t.toLowerCase()));
}

/**
 * `value` with the tag being typed replaced by `tag`, followed by ", " so the next
 * tag can be typed straight away. Earlier entries are normalized to "a, b, ".
 */
export function applyTag(value: string, tag: string): string {
  const before = parseTags(value.slice(0, value.lastIndexOf(",") + 1));
  return `${[...before, tag].join(", ")}, `;
}
