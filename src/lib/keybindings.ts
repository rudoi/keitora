// Central keymap: a single declarative table of keybindings, a pure key-combo
// matcher, the `KeyCtx` shape assembled at the call site, and the
// `useKeybindings` hook that installs one window keydown listener with a shared
// INPUT/TEXTAREA/contentEditable guard. Replaces the scattered per-component
// keydown handlers that used to live in App.tsx (Cmd+K) and Board.tsx (d/D).

import { useEffect, useRef } from "react";
import type { View } from "../components/TopBar";
import type { ScrollDirection } from "./ipc";

// ── Context ──────────────────────────────────────────────────────────────────

/** Live state + stable callbacks the bindings act on. Assembled in App.tsx. */
export interface KeyCtx {
  view: View;
  /** Focused worktree name (not a slug), or null. */
  selected: string | null;
  /** Ordered list of available worktree names — worktree.prev/next cycle this. */
  availableWorktrees: string[];
  /** True when a board card is selected (App's `selectedTaskId` is not null). */
  hasSelectedCard: boolean;
  /** Toggle showing the KeybindingHelp cheatsheet overlay. */
  toggleHelp: () => void;
  /** Toggle the performance overlay. */
  togglePerf: () => void;
  /** Open the full-window diff view for a worktree (by name). */
  openDiff: (name: string) => void;
  setView: (v: View) => void;
  setSelected: (name: string) => void;
  /** Toggle the per-card DispatchMenu for the selected card (Board → App wired). */
  openCardMenu: () => void;
  /** Step the terminal font size by `delta` px (clamped), and flash the size HUD. */
  adjustFontSize: (delta: number) => void;
  /** Reset the terminal font size to the default, and flash the size HUD. */
  resetFontSize: () => void;
  /** Ordered project names from the registry (same order as listProjects()). */
  projectNames: string[];
  /** The active project's registry key (same namespace as `projectNames`) — used by
   *  cycleProject in run. */
  activeProjectName: string;
  /** Switch to a project by registry key via the same handleSwitchProject path in App.tsx. */
  switchToProject: (name: string) => void;
}

// ── Binding record ───────────────────────────────────────────────────────────

export interface Keybinding {
  id: string;
  /** Human-readable combo per the convention below, e.g. "Cmd+F". */
  keys: string;
  /** Shown in the KeybindingHelp cheatsheet. */
  label: string;
  /** Grouping for the cheatsheet. */
  group: "Global" | "Board";
  /**
   * When true, this chord fires even while a terminal/input is focused: it
   * bypasses the input guard, and TerminalPane lets it bubble to the window
   * keymap instead of feeding it to the PTY. Use for app-navigation chords
   * (view switch, worktree cycle) that should work everywhere. Plain-letter
   * bindings (d, ?) leave this false so they can still be typed into a terminal.
   */
  global?: boolean;
  when: (ctx: KeyCtx) => boolean;
  run: (ctx: KeyCtx) => void;
}

// ── Key matching ─────────────────────────────────────────────────────────────

/**
 * Match a KeyboardEvent against a human-readable `keys` string.
 *
 * Tokens (split on `+`):
 *   Cmd            → e.metaKey || e.ctrlKey  (Mac Cmd / non-Mac Ctrl)
 *   Up    / Down   → e.key === "ArrowUp" / "ArrowDown"
 *   Left  / Right  → e.key === "ArrowLeft" / "ArrowRight"
 *   PageUp / PageDown → e.key === "PageUp" / "PageDown"
 *   End            → e.key === "End"
 *   ?              → e.key === "?"
 *   Plus           → e.key === "+" || "="  (US layout; "+" is Shift+"=")
 *   Minus          → e.key === "-"  (unshifted; "_" / Cmd+Shift+- deliberately excluded)
 *   single letter  → e.key.toLowerCase() === letter.toLowerCase()
 *   digit          → e.key === digit
 *
 * Match is exact: every token must hold, and no modifier beyond those named in
 * `keys` may be set (so Cmd+K does not fire while Alt is also held, and the `d`
 * binding does not fire on Shift+D).
 */
// Tokens that are themselves produced by holding Shift (so an unnamed Shift is
// expected, not a stray modifier). "Plus" is here because on a US layout the only
// way to type "+" is Shift+"=" — without this, Cmd+Plus would be rejected by the
// shift-guard below and never fire from a real keypress. "Minus" is deliberately
// NOT here: "-" is unshifted, so Cmd+Minus matches without any exemption, and
// adding it would let Cmd+Shift+- (which produces "_") spuriously fire decrease.
const SHIFT_PRODUCED = new Set(["?", "Plus"]);

export function matchesKey(e: KeyboardEvent, keys: string): boolean {
  const tokens = keys.split("+").map((t) => t.trim());
  const wantCmd = tokens.includes("Cmd");

  // Modifier discipline: Cmd token ⇒ require meta/ctrl; absence ⇒ forbid both.
  if (wantCmd) {
    if (!(e.metaKey || e.ctrlKey)) return false;
  } else if (e.metaKey || e.ctrlKey) {
    return false;
  }
  // Alt is never part of our combos.
  if (e.altKey) return false;
  // Shift is only allowed when explicitly named ("...+Shift+...") or when the bound key is
  // itself a shifted glyph (e.g. "?"), so Shift+D / Shift+Cmd+1 don't spuriously trigger the
  // "d" / "Cmd+1" bindings.
  if (e.shiftKey && !tokens.includes("Shift") && !tokens.some((t) => SHIFT_PRODUCED.has(t))) {
    return false;
  }

  for (const tok of tokens) {
    if (tok === "Cmd") continue;
    if (tok === "Shift") {
      if (!e.shiftKey) return false;
    } else if (tok === "Up") {
      if (e.key !== "ArrowUp") return false;
    } else if (tok === "Down") {
      if (e.key !== "ArrowDown") return false;
    } else if (tok === "Left") {
      if (e.key !== "ArrowLeft") return false;
    } else if (tok === "Right") {
      if (e.key !== "ArrowRight") return false;
    } else if (tok === "PageUp" || tok === "PageDown" || tok === "End") {
      if (e.key !== tok) return false;
    } else if (tok === "?") {
      if (e.key !== "?") return false;
    } else if (tok === "Plus") {
      // The +/= key, with or without Shift (US layout): "+" is Shift+"=", and we
      // also accept bare "=" so Cmd+= (browser zoom-in muscle memory) works too.
      if (e.key !== "+" && e.key !== "=") return false;
    } else if (tok === "Minus") {
      // The "-" key (unshifted on US layout). We do NOT accept "_" (Shift+"-"):
      // Cmd+Shift+- is a distinct chord that should not trigger decrease.
      if (e.key !== "-") return false;
    } else if (tok.length === 1) {
      if (e.key.toLowerCase() !== tok.toLowerCase()) return false;
    } else {
      return false; // unknown token
    }
  }
  return true;
}

// ── Default bindings ─────────────────────────────────────────────────────────

/** Cycle a value forward/backward within a list, wrapping. */
function cycle(list: string[], current: string | null, dir: 1 | -1): string | null {
  if (list.length === 0) return null;
  const i = current ? list.indexOf(current) : -1;
  // From an unknown/null position, forward → first, backward → last.
  const start = i < 0 ? (dir === 1 ? 0 : list.length - 1) : (i + dir + list.length) % list.length;
  return list[start];
}

/**
 * Cycle to the next/previous project name, wrapping at both ends.
 * Returns null if there are fewer than 2 names (no-op).
 * If active is not found, forward (+1) → first, backward (-1) → last.
 */
export function cycleProject(names: string[], active: string, dir: 1 | -1): string | null {
  if (names.length < 2) return null;
  const i = names.indexOf(active);
  // From an unknown position, forward → first, backward → last.
  const start =
    i < 0 ? (dir === 1 ? 0 : names.length - 1) : (i + dir + names.length) % names.length;
  return names[start];
}

export const DEFAULT_BINDINGS: Keybinding[] = [
  {
    id: "worktree.prev",
    keys: "Cmd+Up",
    label: "Previous worktree",
    group: "Global",
    global: true,
    when: (ctx) => ctx.availableWorktrees.length > 1,
    run: (ctx) => {
      const next = cycle(ctx.availableWorktrees, ctx.selected, -1);
      if (next) ctx.setSelected(next);
    },
  },
  {
    id: "worktree.next",
    keys: "Cmd+Down",
    label: "Next worktree",
    group: "Global",
    global: true,
    when: (ctx) => ctx.availableWorktrees.length > 1,
    run: (ctx) => {
      const next = cycle(ctx.availableWorktrees, ctx.selected, 1);
      if (next) ctx.setSelected(next);
    },
  },
  {
    id: "diff.open",
    // Cmd+Shift+D, not Cmd+D: on Windows/Linux Cmd maps to Ctrl, and this is a global chord
    // that bypasses the terminal — a plain Ctrl+D would be swallowed and break shell/REPL EOF.
    // Ctrl+Shift+D is not a terminal control code, so bypassing it is safe.
    keys: "Cmd+Shift+D",
    label: "Open diff view",
    group: "Global",
    global: true,
    when: (ctx) => (ctx.view === "focus" || ctx.view === "grid") && ctx.selected !== null,
    run: (ctx) => {
      if (ctx.selected) ctx.openDiff(ctx.selected);
    },
  },
  {
    id: "search.open",
    keys: "Cmd+F",
    label: "Terminal search",
    group: "Global",
    // Listed for discoverability. The focused TerminalPane opens its search box from
    // its own key handler; this window-level binding never fires while the terminal is
    // focused (its input is a <textarea>), and `run` is a no-op. useKeybindings still
    // preventDefaults every Cmd+F so the webview's native find bar never opens.
    when: (ctx) => ctx.view === "focus" || ctx.view === "grid",
    run: () => {
      /* handled by the focused TerminalPane's own key handler */
    },
  },
  {
    id: "terminal.scrollUp",
    keys: "Shift+PageUp",
    label: "Scroll terminal history up a page",
    group: "Global",
    // Listed for the help sheet only: the focused TerminalPane consumes the key (see
    // paneScrollDirection) so it scrolls the pane instead of reaching the app. The
    // window keymap never claims it, so Shift+PageUp keeps its default elsewhere.
    when: () => false,
    run: () => {
      /* handled by the focused TerminalPane's own key handler */
    },
  },
  {
    id: "terminal.scrollDown",
    keys: "Shift+PageDown",
    label: "Scroll terminal history down a page",
    group: "Global",
    // Help sheet only, like terminal.scrollUp.
    when: () => false,
    run: () => {
      /* handled by the focused TerminalPane's own key handler */
    },
  },
  {
    id: "terminal.scrollToLive",
    keys: "End",
    label: "Back to the live output (while scrolled into history)",
    group: "Global",
    // Listed for the help sheet only: the focused TerminalPane consumes End while its
    // "↓ live" pill is up (see isScrollToLiveKey); otherwise End belongs to whatever
    // has focus, so the window keymap never claims it.
    when: () => false,
    run: () => {
      /* handled by the focused TerminalPane's own key handler */
    },
  },
  {
    id: "font.increase",
    keys: "Cmd+Plus",
    label: "Increase font size",
    group: "Global",
    // Global so it adjusts the terminal even while a pane is focused; the size is a
    // single shared preference, so every terminal resizes together.
    global: true,
    when: () => true,
    run: (ctx) => ctx.adjustFontSize(1),
  },
  {
    id: "font.decrease",
    keys: "Cmd+Minus",
    label: "Decrease font size",
    group: "Global",
    global: true,
    when: () => true,
    run: (ctx) => ctx.adjustFontSize(-1),
  },
  {
    id: "font.reset",
    keys: "Cmd+0",
    label: "Reset font size",
    group: "Global",
    global: true,
    when: () => true,
    run: (ctx) => ctx.resetFontSize(),
  },
  {
    // Cmd+Shift+P (Ctrl+Shift+P on Windows/Linux) is not a terminal control code, so it is
    // safe to bypass the terminal with a global chord.
    id: "perf.toggle",
    keys: "Cmd+Shift+P",
    label: "Performance overlay",
    group: "Global",
    global: true,
    when: () => true,
    run: (ctx) => ctx.togglePerf(),
  },
  {
    id: "help.toggle",
    keys: "?",
    label: "Show keybinding help",
    group: "Global",
    when: () => true,
    run: (ctx) => ctx.toggleHelp(),
  },
  {
    id: "board.dispatch",
    keys: "d",
    label: "Dispatch selected card",
    group: "Board",
    when: (ctx) => ctx.view === "board" && ctx.hasSelectedCard,
    run: (ctx) => ctx.openCardMenu(),
  },
  // Worktree number jumps: Cmd+1..9 selects the Nth available worktree → Focus view.
  ...[1, 2, 3, 4, 5, 6, 7, 8, 9].map((N) => ({
    id: `worktree.select.${N}`,
    keys: `Cmd+${N}`,
    label: `Select worktree ${N}`,
    group: "Global" as const,
    global: true,
    when: (ctx: KeyCtx) => ctx.availableWorktrees.length >= N,
    run: (ctx: KeyCtx) => {
      const name = ctx.availableWorktrees[N - 1];
      if (!name) return;
      ctx.setSelected(name);
      ctx.setView("focus");
    },
  })),
  // Project cycling: Cmd+Left/Right moves between projects (wrapping).
  {
    id: "project.prev",
    keys: "Cmd+Left",
    label: "Previous project",
    group: "Global",
    global: true,
    when: (ctx: KeyCtx) => ctx.projectNames.length > 1,
    run: (ctx: KeyCtx) => {
      const name = cycleProject(ctx.projectNames, ctx.activeProjectName, -1);
      if (name) ctx.switchToProject(name);
    },
  },
  {
    id: "project.next",
    keys: "Cmd+Right",
    label: "Next project",
    group: "Global",
    global: true,
    when: (ctx: KeyCtx) => ctx.projectNames.length > 1,
    run: (ctx: KeyCtx) => {
      const name = cycleProject(ctx.projectNames, ctx.activeProjectName, 1);
      if (name) ctx.switchToProject(name);
    },
  },
];

/**
 * True if the event matches a `global` chord — one that must win over a focused
 * terminal. TerminalPane calls this from its key handler and returns `false`
 * (don't consume) for matches, so the chord bubbles to the window-level keymap
 * instead of being fed to the PTY.
 *
 * Cross-module contract: this is the single bridge between the keymap and the
 * terminal. Any binding marked `global: true` automatically stops being fed to
 * the PTY when a pane is focused — and any `global` binding that a terminal
 * should still receive must NOT set the flag. Keep that in mind when adding
 * bindings.
 */
export function isGlobalAppChord(e: KeyboardEvent): boolean {
  return DEFAULT_BINDINGS.some((b) => b.global && matchesKey(e, b.keys));
}

/** The keys of the binding `id`, resolved once at module load. */
function bindingKeys(id: string): string {
  const binding = DEFAULT_BINDINGS.find((b) => b.id === id);
  if (!binding) throw new Error(`keybindings: no binding ${id}`);
  return binding.keys;
}

const PANE_SCROLL_KEYS: ReadonlyArray<readonly [string, ScrollDirection]> = [
  [bindingKeys("terminal.scrollUp"), "up"],
  [bindingKeys("terminal.scrollDown"), "down"],
];

const SCROLL_TO_LIVE_KEYS = bindingKeys("terminal.scrollToLive");

const FIND_KEYS = bindingKeys("search.open");

/**
 * Whether the event is the find chord (plain Cmd/Ctrl+F, no Shift or Alt). The window
 * keymap preventDefaults it everywhere, since the webview's native find bar cannot see
 * terminal text; a focused TerminalPane opens its own search box on it.
 */
export function isFindChord(e: KeyboardEvent): boolean {
  return matchesKey(e, FIND_KEYS);
}

/**
 * The scroll direction for a pane history key (Shift+PageUp / Shift+PageDown), or null.
 * TerminalPane calls this from its key handler and consumes matches, so the key scrolls
 * the pane and is never sent to the app.
 */
export function paneScrollDirection(e: KeyboardEvent): ScrollDirection | null {
  if (e.key !== "PageUp" && e.key !== "PageDown") return null;
  for (const [keys, direction] of PANE_SCROLL_KEYS) {
    if (matchesKey(e, keys)) return direction;
  }
  return null;
}

/**
 * Whether the event is the back-to-live key (bare End). TerminalPane consumes it only
 * while its "↓ live" pill is showing; otherwise End goes to the app as usual.
 */
export function isScrollToLiveKey(e: KeyboardEvent): boolean {
  return matchesKey(e, SCROLL_TO_LIVE_KEYS);
}

// ── Hook ─────────────────────────────────────────────────────────────────────

/**
 * Install a single window keydown listener driven by DEFAULT_BINDINGS. The
 * shared input guard runs first; then the first binding whose `keys` match and
 * whose `when(ctx)` is true wins (preventDefault + run, stop).
 *
 * `ctx` is read through a ref so the listener is attached exactly once for the
 * component's lifetime: `ctx` changes on every session tick, and re-subscribing on each change would needlessly churn the
 * window listener.
 */
export function useKeybindings(ctx: KeyCtx): void {
  const ctxRef = useRef(ctx);
  ctxRef.current = ctx;

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const cur = ctxRef.current;
      const target = e.target as HTMLElement | null;
      const tag = target?.tagName;
      const inEditable = tag === "INPUT" || tag === "TEXTAREA" || !!target?.isContentEditable;
      // Never let the webview open its native find bar, whatever has focus. Propagation
      // is left alone: a focused pane's own handler opens the terminal search.
      if (isFindChord(e)) e.preventDefault();

      for (const binding of DEFAULT_BINDINGS) {
        if (!matchesKey(e, binding.keys)) continue;
        // Non-global bindings yield to a focused field or terminal so the user
        // can type `d`, `?`, search text, etc. Global chords (view switch,
        // worktree cycle) fire everywhere — including over a focused terminal
        // (xterm's input is a <textarea>, which would otherwise trip the guard).
        if (inEditable && !binding.global) continue;
        if (!binding.when(cur)) continue;
        e.preventDefault();
        binding.run(cur);
        break;
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);
}
