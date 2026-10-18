import { describe, expect, it } from "vitest";
import { type ClipboardKeyAction, clipboardActionForKey } from "./terminalClipboard";

// KeyboardEvent-shaped object with no modifiers held. clipboardActionForKey only
// reads ctrlKey/shiftKey/altKey/metaKey/key, so a plain object suffices (no DOM) —
// same approach as keybindings.test.ts.
function ev(p: Partial<KeyboardEvent>): KeyboardEvent {
  return {
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    key: "",
    ...p,
  } as KeyboardEvent;
}

const act = (p: Partial<KeyboardEvent>, isWindows = true): ClipboardKeyAction =>
  clipboardActionForKey(ev(p), isWindows);

describe("clipboardActionForKey — paste", () => {
  it("Ctrl+V → paste", () => {
    expect(act({ ctrlKey: true, key: "v" })).toBe("paste");
  });

  it("Ctrl+Shift+V → paste", () => {
    expect(act({ ctrlKey: true, shiftKey: true, key: "v" })).toBe("paste");
  });

  it("uppercase key 'V' → paste", () => {
    expect(act({ ctrlKey: true, key: "V" })).toBe("paste");
  });
});

describe("clipboardActionForKey — copy", () => {
  it("Ctrl+C → copy", () => {
    expect(act({ ctrlKey: true, key: "c" })).toBe("copy");
  });

  it("uppercase key 'C' → copy", () => {
    expect(act({ ctrlKey: true, key: "C" })).toBe("copy");
  });

  it("Ctrl+Shift+C → null (copy is Ctrl+C only)", () => {
    expect(act({ ctrlKey: true, shiftKey: true, key: "c" })).toBeNull();
  });
});

describe("clipboardActionForKey — platform gating", () => {
  it("non-Windows: Ctrl+V → null", () => {
    expect(act({ ctrlKey: true, key: "v" }, false)).toBeNull();
  });

  it("non-Windows: Ctrl+C → null", () => {
    expect(act({ ctrlKey: true, key: "c" }, false)).toBeNull();
  });
});

describe("clipboardActionForKey — modifier discipline", () => {
  it("Ctrl+Alt+V (AltGr) → null", () => {
    expect(act({ ctrlKey: true, altKey: true, key: "v" })).toBeNull();
  });

  it("Meta+V → null", () => {
    expect(act({ metaKey: true, key: "v" })).toBeNull();
  });

  it("bare 'v' (no Ctrl) → null", () => {
    expect(act({ key: "v" })).toBeNull();
  });

  it("bare 'c' (no Ctrl) → null", () => {
    expect(act({ key: "c" })).toBeNull();
  });

  it("unrelated chord Ctrl+A → null", () => {
    expect(act({ ctrlKey: true, key: "a" })).toBeNull();
  });
});
