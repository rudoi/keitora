import { describe, expect, it, vi } from "vitest";
import { isolateFromCard, menuShortcut } from "./DispatchMenu";

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

// With the menu open (`d` on a selected card), a second letter picks an item:
// `d, d` auto-dispatches and `d, b` moves to / reopens from the backlog.
describe("menuShortcut", () => {
  const key = (
    k: string,
    mods: Partial<Record<"ctrlKey" | "metaKey" | "altKey" | "shiftKey", boolean>> = {},
  ) => ({
    key: k,
    ctrlKey: false,
    metaKey: false,
    altKey: false,
    shiftKey: false,
    ...mods,
  });
  const all = { autoDispatch: true, backlog: true };

  it("maps d to the auto-pick Dispatch and b to the backlog item", () => {
    expect(menuShortcut(key("d"), all)).toBe("autoDispatch");
    expect(menuShortcut(key("b"), all)).toBe("backlog");
  });

  it("ignores keys whose item is not on offer", () => {
    expect(menuShortcut(key("d"), { autoDispatch: false, backlog: true })).toBeNull();
    expect(menuShortcut(key("b"), { autoDispatch: true, backlog: false })).toBeNull();
  });

  it("ignores other letters and modified keys", () => {
    expect(menuShortcut(key("x"), all)).toBeNull();
    expect(menuShortcut(key("D", { shiftKey: true }), all)).toBeNull();
    expect(menuShortcut(key("d", { ctrlKey: true }), all)).toBeNull();
    expect(menuShortcut(key("d", { metaKey: true }), all)).toBeNull();
    expect(menuShortcut(key("b", { altKey: true }), all)).toBeNull();
  });
});
