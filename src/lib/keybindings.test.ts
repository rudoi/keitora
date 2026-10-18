import { describe, expect, it, vi } from "vitest";
import {
  cycleProject,
  DEFAULT_BINDINGS,
  isGlobalAppChord,
  isScrollToLiveKey,
  type KeyCtx,
  matchesKey,
  paneScrollDirection,
} from "./keybindings";

// Build a KeyboardEvent-shaped object with sane defaults (no modifiers held).
// matchesKey only reads metaKey/ctrlKey/altKey/shiftKey/key, so a plain object is
// sufficient — no DOM required.
function ev(p: Partial<KeyboardEvent>): KeyboardEvent {
  return {
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    key: "",
    ...p,
  } as KeyboardEvent;
}

describe("matchesKey — Plus token", () => {
  it("matches Cmd+Plus typed as Cmd+Shift+= (key '+')", () => {
    expect(matchesKey(ev({ metaKey: true, shiftKey: true, key: "+" }), "Cmd+Plus")).toBe(true);
  });

  it("matches Cmd+Plus typed as bare Cmd+= (key '=')", () => {
    expect(matchesKey(ev({ metaKey: true, key: "=" }), "Cmd+Plus")).toBe(true);
  });

  it("matches the Windows/Linux Ctrl path", () => {
    expect(matchesKey(ev({ ctrlKey: true, shiftKey: true, key: "+" }), "Cmd+Plus")).toBe(true);
  });

  it("does not match a bare '+' without Cmd/Ctrl", () => {
    expect(matchesKey(ev({ shiftKey: true, key: "+" }), "Cmd+Plus")).toBe(false);
  });
});

describe("matchesKey — Shift token", () => {
  it("matches Cmd+Shift+D (meta path)", () => {
    expect(matchesKey(ev({ metaKey: true, shiftKey: true, key: "d" }), "Cmd+Shift+D")).toBe(true);
  });

  it("matches the Windows/Linux Ctrl+Shift+D path", () => {
    expect(matchesKey(ev({ ctrlKey: true, shiftKey: true, key: "D" }), "Cmd+Shift+D")).toBe(true);
  });

  it("does NOT match plain Ctrl+D (so terminal EOF is preserved)", () => {
    expect(matchesKey(ev({ ctrlKey: true, key: "d" }), "Cmd+Shift+D")).toBe(false);
  });

  it("requires Shift when the binding names it", () => {
    expect(matchesKey(ev({ metaKey: true, key: "d" }), "Cmd+Shift+D")).toBe(false);
  });
});

describe("matchesKey — Minus token", () => {
  it("matches Cmd+Minus (key '-')", () => {
    expect(matchesKey(ev({ metaKey: true, key: "-" }), "Cmd+Minus")).toBe(true);
  });

  it("does not match a bare '-' without Cmd/Ctrl", () => {
    expect(matchesKey(ev({ key: "-" }), "Cmd+Minus")).toBe(false);
  });

  it("does not match Cmd+Shift+- (key '_') — that is a distinct chord", () => {
    expect(matchesKey(ev({ metaKey: true, shiftKey: true, key: "_" }), "Cmd+Minus")).toBe(false);
  });
});

describe("matchesKey — Cmd+0 reset", () => {
  it("matches Cmd+0", () => {
    expect(matchesKey(ev({ metaKey: true, key: "0" }), "Cmd+0")).toBe(true);
  });

  it("does not match a bare '0' without Cmd/Ctrl", () => {
    expect(matchesKey(ev({ key: "0" }), "Cmd+0")).toBe(false);
  });
});

describe("matchesKey — regressions", () => {
  it("still matches existing Cmd+1", () => {
    expect(matchesKey(ev({ metaKey: true, key: "1" }), "Cmd+1")).toBe(true);
  });

  it("rejects when Alt is also held", () => {
    expect(matchesKey(ev({ metaKey: true, altKey: true, key: "+" }), "Cmd+Plus")).toBe(false);
  });

  it("rejects a stray Shift on an unshifted binding (Shift+Cmd+1)", () => {
    expect(matchesKey(ev({ metaKey: true, shiftKey: true, key: "1" }), "Cmd+1")).toBe(false);
  });
});

describe("isGlobalAppChord — font-size chords bypass the PTY", () => {
  it("treats Cmd+Plus / Cmd+Minus / Cmd+0 as global chords", () => {
    expect(isGlobalAppChord(ev({ metaKey: true, shiftKey: true, key: "+" }))).toBe(true);
    expect(isGlobalAppChord(ev({ metaKey: true, key: "-" }))).toBe(true);
    expect(isGlobalAppChord(ev({ metaKey: true, key: "0" }))).toBe(true);
  });

  it("does not treat a plain key (no Cmd) as a global chord", () => {
    expect(isGlobalAppChord(ev({ key: "+" }))).toBe(false);
  });
});

// ── cycleProject ─────────────────────────────────────────────────────────────

describe("cycleProject", () => {
  const names = ["alpha", "bravo", "charlie"];

  it("cycles forward wrapping from last to first", () => {
    expect(cycleProject(names, "charlie", 1)).toBe("alpha");
  });

  it("cycles backward wrapping from first to last", () => {
    expect(cycleProject(names, "alpha", -1)).toBe("charlie");
  });

  it("cycles forward from middle", () => {
    expect(cycleProject(names, "alpha", 1)).toBe("bravo");
  });

  it("cycles backward from middle", () => {
    expect(cycleProject(names, "charlie", -1)).toBe("bravo");
  });

  it("returns null for a single-project list", () => {
    expect(cycleProject(["only"], "only", 1)).toBeNull();
    expect(cycleProject(["only"], "only", -1)).toBeNull();
  });

  it("returns null for an empty list", () => {
    expect(cycleProject([], "", 1)).toBeNull();
  });

  it("forward with unknown active → first", () => {
    expect(cycleProject(names, "unknown", 1)).toBe("alpha");
  });

  it("backward with unknown active → last", () => {
    expect(cycleProject(names, "unknown", -1)).toBe("charlie");
  });
});

// ── Worktree number-jump bindings ─────────────────────────────────────────────

/** Build a minimal KeyCtx mock sufficient for worktree-select + project bindings. */
function makeCtx(overrides: Partial<KeyCtx> = {}): KeyCtx {
  return {
    view: "focus",
    selected: null,
    availableWorktrees: [],
    hasSelectedCard: false,
    toggleHelp: vi.fn(),
    togglePerf: vi.fn(),
    openDiff: vi.fn(),
    setView: vi.fn(),
    setSelected: vi.fn(),
    onSuggest: vi.fn(),
    openCardMenu: vi.fn(),
    adjustFontSize: vi.fn(),
    resetFontSize: vi.fn(),
    projectNames: [],
    activeProjectName: "",
    switchToProject: vi.fn(),
    ...overrides,
  };
}

describe("perf.toggle binding", () => {
  const binding = DEFAULT_BINDINGS.find((b) => b.id === "perf.toggle");

  it("is a global Cmd+Shift+P chord", () => {
    expect(binding).toBeDefined();
    expect(binding?.global).toBe(true);
    expect(matchesKey(ev({ ctrlKey: true, shiftKey: true, key: "P" }), binding?.keys ?? "")).toBe(
      true,
    );
  });

  it("run toggles the overlay", () => {
    const ctx = makeCtx();
    binding?.run(ctx);
    expect(ctx.togglePerf).toHaveBeenCalledTimes(1);
  });
});

describe("diff.open binding", () => {
  it("opens the diff for the selected worktree", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "diff.open");
    expect(binding).toBeDefined();
    const ctx = makeCtx({ view: "focus", selected: "bravo" });
    expect(binding?.when(ctx)).toBe(true);
    binding?.run(ctx);
    expect(ctx.openDiff).toHaveBeenCalledWith("bravo");
  });

  it("is disabled with no selection or outside focus/grid", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "diff.open");
    expect(binding?.when(makeCtx({ view: "focus", selected: null }))).toBe(false);
    expect(binding?.when(makeCtx({ view: "board", selected: "bravo" }))).toBe(false);
  });
});

describe("worktree number-jump bindings", () => {
  const worktrees = ["alpha", "bravo", "charlie"];

  it("Cmd+2 run sets selected to index 1 and view to focus", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "worktree.select.2");
    expect(binding).toBeDefined();

    const ctx = makeCtx({ availableWorktrees: worktrees });
    binding?.run(ctx);

    expect(ctx.setSelected).toHaveBeenCalledWith("bravo");
    expect(ctx.setView).toHaveBeenCalledWith("focus");
  });

  it("Cmd+1 run sets selected to index 0", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "worktree.select.1");
    expect(binding).toBeDefined();

    const ctx = makeCtx({ availableWorktrees: worktrees });
    binding?.run(ctx);

    expect(ctx.setSelected).toHaveBeenCalledWith("alpha");
    expect(ctx.setView).toHaveBeenCalledWith("focus");
  });

  it("Cmd+4 when is false when only 3 worktrees are available", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "worktree.select.4");
    expect(binding).toBeDefined();

    const ctx = makeCtx({ availableWorktrees: worktrees });
    expect(binding?.when(ctx)).toBe(false);
  });

  it("Cmd+3 when is true when 3 worktrees are available", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "worktree.select.3");
    expect(binding).toBeDefined();

    const ctx = makeCtx({ availableWorktrees: worktrees });
    expect(binding?.when(ctx)).toBe(true);
  });

  it("all 9 worktree-select bindings exist in DEFAULT_BINDINGS", () => {
    for (let n = 1; n <= 9; n++) {
      expect(DEFAULT_BINDINGS.find((b) => b.id === `worktree.select.${n}`)).toBeDefined();
    }
  });
});

// ── Dropped view bindings ─────────────────────────────────────────────────────

describe("view.* bindings removed", () => {
  it("view.board no longer exists in DEFAULT_BINDINGS", () => {
    expect(DEFAULT_BINDINGS.find((b) => b.id === "view.board")).toBeUndefined();
  });

  it("view.focus no longer exists in DEFAULT_BINDINGS", () => {
    expect(DEFAULT_BINDINGS.find((b) => b.id === "view.focus")).toBeUndefined();
  });

  it("view.grid no longer exists in DEFAULT_BINDINGS", () => {
    expect(DEFAULT_BINDINGS.find((b) => b.id === "view.grid")).toBeUndefined();
  });
});

describe("pane scroll keys", () => {
  it("Shift+PageUp / Shift+PageDown map to up / down", () => {
    expect(paneScrollDirection(ev({ shiftKey: true, key: "PageUp" }))).toBe("up");
    expect(paneScrollDirection(ev({ shiftKey: true, key: "PageDown" }))).toBe("down");
  });

  it("bare or otherwise-modified PageUp/PageDown are left for the app", () => {
    expect(paneScrollDirection(ev({ key: "PageUp" }))).toBeNull();
    expect(paneScrollDirection(ev({ key: "PageDown" }))).toBeNull();
    expect(paneScrollDirection(ev({ shiftKey: true, ctrlKey: true, key: "PageUp" }))).toBeNull();
    expect(paneScrollDirection(ev({ shiftKey: true, altKey: true, key: "PageDown" }))).toBeNull();
    expect(paneScrollDirection(ev({ shiftKey: true, key: "ArrowUp" }))).toBeNull();
  });

  it("are listed bindings the window keymap never claims (the pane consumes them)", () => {
    for (const id of ["terminal.scrollUp", "terminal.scrollDown"]) {
      const binding = DEFAULT_BINDINGS.find((b) => b.id === id);
      expect(binding).toBeDefined();
      expect(binding?.global).toBeFalsy();
      for (const view of ["focus", "grid", "board"] as const) {
        expect(binding?.when(makeCtx({ view }))).toBe(false);
      }
    }
    expect(isGlobalAppChord(ev({ shiftKey: true, key: "PageUp" }))).toBe(false);
  });
});

describe("isScrollToLiveKey", () => {
  it("matches bare End only", () => {
    expect(isScrollToLiveKey(ev({ key: "End" }))).toBe(true);
    expect(isScrollToLiveKey(ev({ shiftKey: true, key: "End" }))).toBe(false);
    expect(isScrollToLiveKey(ev({ ctrlKey: true, key: "End" }))).toBe(false);
    expect(isScrollToLiveKey(ev({ altKey: true, key: "End" }))).toBe(false);
    expect(isScrollToLiveKey(ev({ metaKey: true, key: "End" }))).toBe(false);
    expect(isScrollToLiveKey(ev({ key: "Home" }))).toBe(false);
  });

  it("is a listed binding the window keymap never claims", () => {
    const binding = DEFAULT_BINDINGS.find((b) => b.id === "terminal.scrollToLive");
    expect(binding?.keys).toBe("End");
    expect(binding?.global).toBeFalsy();
    expect(binding?.when(makeCtx({ view: "focus" }))).toBe(false);
    expect(isGlobalAppChord(ev({ key: "End" }))).toBe(false);
  });
});
