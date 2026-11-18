// Per-worktree sticky notes: a draggable note over the terminal for leaving yourself
// reminders about what is going on in a worktree. Stored per project beside the task DB
// (see src-tauri get_sticky_note / save_sticky_note); the whole feature is gated by the
// user-level `[sticky_notes] enabled` flag.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** One worktree's note. `x`/`y` are px from the stage's top-left corner. */
export interface StickyNote {
  text: string;
  visible: boolean;
  x: number;
  y: number;
}

/** Gap kept between the note and the stage edges. */
export const NOTE_MARGIN = 12;

/** A note never saved before: hidden, empty, parked top-right (the x clamps to the
 *  right edge, which stays out of the way of a prompt at the bottom-left). */
export const EMPTY_NOTE: StickyNote = { text: "", visible: false, x: 1e6, y: NOTE_MARGIN };

/** The stored note for `worktree` of `project` (registry key), or null when it has none. */
export function getStickyNote(project: string, worktree: string): Promise<StickyNote | null> {
  return invoke<StickyNote | null>("get_sticky_note", { project, worktree });
}

/** Store the note for `worktree` of `project`. A hidden, empty note is removed. */
export function saveStickyNote(project: string, worktree: string, note: StickyNote): Promise<void> {
  return invoke("save_sticky_note", { project, worktree, note });
}

/** Persist the user-level `[sticky_notes] enabled` flag. Emits `sticky-notes://enabled`. */
export function setStickyNotesEnabled(enabled: boolean): Promise<void> {
  return invoke("set_sticky_notes_enabled", { enabled });
}

/** Subscribe to changes of the `[sticky_notes] enabled` flag. */
export function onStickyNotesEnabled(cb: (enabled: boolean) => void): Promise<UnlistenFn> {
  return listen<boolean>("sticky-notes://enabled", (e) => cb(e.payload));
}

/**
 * Clamp a note's top-left corner so the whole note stays inside the stage, `NOTE_MARGIN`
 * from each edge. When the stage is smaller than the note, the top-left edge wins so the
 * drag handle stays reachable.
 */
export function clampNotePosition(
  pos: { x: number; y: number },
  note: { width: number; height: number },
  stage: { width: number; height: number },
): { x: number; y: number } {
  const clamp = (v: number, max: number) =>
    Math.max(NOTE_MARGIN, Math.min(Number.isFinite(v) ? v : NOTE_MARGIN, max));
  return {
    x: clamp(pos.x, stage.width - note.width - NOTE_MARGIN),
    y: clamp(pos.y, stage.height - note.height - NOTE_MARGIN),
  };
}
