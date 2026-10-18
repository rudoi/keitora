// Maps a Windows terminal keydown to a clipboard action, or null. Pure + DOM-free
// so it can be unit-tested like keybindings.matchesKey. The caller (TerminalPane)
// decides whether a "copy" actually copies (only when a selection exists) — this
// just classifies the chord.

export type ClipboardKeyAction = "paste" | "copy" | null;

/**
 * Classify a keydown as a Windows terminal clipboard action.
 *
 *  - paste: Ctrl+V or Ctrl+Shift+V   (Windows-app + terminal paste muscle memory)
 *  - copy:  Ctrl+C                    (caller copies only when a selection exists;
 *                                      with no selection it falls through to SIGINT)
 *
 * Returns null off Windows — Ctrl+V is "verbatim insert" (lnext) on Unix and macOS
 * pastes via the native Cmd+V path, so this remapping must never engage there. Also
 * null when Alt (AltGr on international layouts reports Ctrl+Alt) or Meta is held.
 */
export function clipboardActionForKey(
  e: Pick<KeyboardEvent, "ctrlKey" | "shiftKey" | "altKey" | "metaKey" | "key">,
  isWindows: boolean,
): ClipboardKeyAction {
  if (!isWindows) return null;
  if (!e.ctrlKey || e.altKey || e.metaKey) return null;

  const k = e.key.toLowerCase();
  if (k === "v") return "paste"; // Ctrl+V and Ctrl+Shift+V
  if (k === "c" && !e.shiftKey) return "copy"; // Ctrl+C only
  return null;
}
