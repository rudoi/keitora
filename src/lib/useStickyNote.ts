// The selected worktree's sticky note: loads it when the project/worktree changes and
// saves edits back. Text and position edits are debounced; showing or hiding saves at
// once. A pending save is flushed (to the worktree it belongs to) before switching.

import { useCallback, useEffect, useRef, useState } from "react";
import { trailingDebounce } from "./debounce";
import { EMPTY_NOTE, getStickyNote, type StickyNote, saveStickyNote } from "./stickyNotes";

const SAVE_DEBOUNCE_MS = 400;

interface Pending {
  project: string;
  worktree: string;
  note: StickyNote;
}

export function useStickyNote(
  project: string | null,
  worktree: string | null,
  enabled: boolean,
  onError: (msg: string) => void,
): {
  /** The loaded note, or null while loading / when there is no worktree or the feature is off. */
  note: StickyNote | null;
  /** Merge `patch` into the note and save it (`immediate` skips the debounce). */
  update: (patch: Partial<StickyNote>, immediate?: boolean) => void;
} {
  const [note, setNote] = useState<StickyNote | null>(null);
  const noteRef = useRef<StickyNote | null>(null);
  const keyRef = useRef<{ project: string; worktree: string } | null>(null);
  const pending = useRef<Pending | null>(null);
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;

  const flush = useCallback(() => {
    const p = pending.current;
    pending.current = null;
    if (!p) return;
    void saveStickyNote(p.project, p.worktree, p.note).catch((err: unknown) =>
      onErrorRef.current(`Could not save sticky note: ${String(err)}`),
    );
  }, []);
  // `flush` only touches refs, so the first render's instance is safe to keep.
  const [debounced] = useState(() => trailingDebounce(flush, SAVE_DEBOUNCE_MS));

  useEffect(() => {
    noteRef.current = null;
    setNote(null);
    if (!enabled || !project || !worktree) {
      keyRef.current = null;
      return;
    }
    keyRef.current = { project, worktree };
    let cancelled = false;
    getStickyNote(project, worktree)
      .then((n) => {
        if (cancelled) return;
        noteRef.current = n ?? EMPTY_NOTE;
        setNote(noteRef.current);
      })
      .catch((err: unknown) => {
        if (!cancelled) onErrorRef.current(`Could not load sticky note: ${String(err)}`);
      });
    return () => {
      cancelled = true;
      // Save what was typed for this worktree before moving on.
      debounced.cancel();
      flush();
    };
  }, [project, worktree, enabled, debounced, flush]);

  const update = useCallback(
    (patch: Partial<StickyNote>, immediate = false) => {
      const key = keyRef.current;
      const current = noteRef.current;
      if (!key || !current) return;
      const next = { ...current, ...patch };
      noteRef.current = next;
      setNote(next);
      pending.current = { ...key, note: next };
      if (immediate) {
        debounced.cancel();
        flush();
      } else {
        debounced.call();
      }
    },
    [debounced, flush],
  );

  return { note, update };
}
