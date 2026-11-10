// Keybinding cheatsheet overlay. Toggled by `?` (via the central keymap) and
// closes on Esc or a second `?`. Sourced directly from DEFAULT_BINDINGS so it
// can never drift from the live binding table.

import { useEffect } from "react";
import { DEFAULT_BINDINGS, type Keybinding } from "../lib/keybindings";
import { modKeyLabel } from "../lib/platform";

/** Render a `keys` string ("Cmd+PageUp") as styled <kbd> chips. */
function KeyChips({ keys }: { keys: string }) {
  const SYMBOL: Record<string, string> = {
    Cmd: modKeyLabel,
    Up: "↑",
    Down: "↓",
    Left: "←",
    Right: "→",
    Plus: "+",
    Minus: "−",
  };
  const parts = keys.split("+").map((t) => t.trim());
  return (
    <span className="kbd-combo">
      {parts.map((p, i) => (
        // biome-ignore lint/suspicious/noArrayIndexKey: static keybinding parts array; never reordered
        <kbd key={i} className="kbd">
          {SYMBOL[p] ?? p}
        </kbd>
      ))}
    </span>
  );
}

const GROUPS: Keybinding["group"][] = ["Global", "Board"];

export function KeybindingHelp({ onClose }: { onClose: () => void }) {
  // Esc closes. (The second `?` is handled by the keymap's toggle.)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="kbd-backdrop" onClick={onClose}>
      <div className="kbd-card" onClick={(e) => e.stopPropagation()}>
        <div className="kbd-head">
          <span className="kbd-title">Keyboard shortcuts</span>
          <button className="kbd-close" onClick={onClose} aria-label="Close">
            ✕
          </button>
        </div>
        {GROUPS.map((group) => {
          // Collapse the nine worktree.select.N chords into a single 1–9 row so the
          // cheatsheet doesn't list nine near-identical entries (and doesn't imply
          // worktrees 4–9 always exist).
          const rows = DEFAULT_BINDINGS.filter(
            (b) => b.group === group && !/^worktree\.select\.[2-9]$/.test(b.id),
          );
          if (rows.length === 0) return null;
          return (
            <div key={group} className="kbd-group">
              <div className="kbd-group-label">{group}</div>
              <table className="kbd-table">
                <tbody>
                  {rows.map((b) => {
                    const isWtSelect = b.id === "worktree.select.1";
                    return (
                      <tr key={b.id}>
                        <td className="kbd-keys">
                          {isWtSelect ? (
                            <span className="kbd-combo">
                              <kbd className="kbd">{modKeyLabel}</kbd>
                              <kbd className="kbd">1</kbd>
                              <span>–</span>
                              <kbd className="kbd">9</kbd>
                            </span>
                          ) : (
                            <KeyChips keys={b.keys} />
                          )}
                        </td>
                        <td className="kbd-label">
                          {isWtSelect ? "Select worktree (1–9)" : b.label}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          );
        })}
        <div className="kbd-foot">
          <span className="kbd-hint">
            Press <kbd className="kbd">?</kbd> or <kbd className="kbd">Esc</kbd> to close
          </span>
        </div>
      </div>
    </div>
  );
}
