import { describe, expect, it, vi } from "vitest";
import { isolateFromCard } from "./DispatchMenu";

// The menu renders inside the draggable card; React bubbles its key and pointer
// events up the component tree (through the portal) to the card's drag listeners.
// Every handler must stop that, or Enter on a menu item starts a card drag.
describe("isolateFromCard", () => {
  it("stops key, pointer and click events from reaching the card", () => {
    for (const handler of [
      isolateFromCard.onKeyDown,
      isolateFromCard.onPointerDown,
      isolateFromCard.onClick,
    ]) {
      const stopPropagation = vi.fn();
      handler({ stopPropagation });
      expect(stopPropagation).toHaveBeenCalledTimes(1);
    }
  });
});
