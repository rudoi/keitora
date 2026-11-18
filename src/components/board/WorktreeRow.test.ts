import { describe, expect, it } from "vitest";
import { selectsRow } from "./WorktreeRow";

// Stand-ins for the row and a click target: `inRow` says whether the row contains the
// target, `inCard` whether the target sits inside a card.
function click(inRow: boolean, inCard: boolean): boolean {
  const target = { closest: () => (inCard ? ({} as Element) : null) } as unknown as Element;
  const row = { contains: () => inRow } as unknown as Node;
  return selectsRow(row, target);
}

describe("selectsRow", () => {
  it("selects on a click in the row outside any card", () => {
    expect(click(true, false)).toBe(true);
  });

  it("leaves a click on a card to the card", () => {
    expect(click(true, true)).toBe(false);
  });

  it("ignores a click from outside the row's DOM, such as a card's portaled menu", () => {
    expect(click(false, false)).toBe(false);
  });
});
