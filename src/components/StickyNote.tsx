// A sticky note floating over the terminal stage: drag it by its header, type into it,
// hide it with ×. Position and text are owned by the caller (useStickyNote), which
// persists them; this component only clamps the note inside the stage while it renders.

import { X } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { clampNotePosition, type StickyNote as Note } from "../lib/stickyNotes";

export function StickyNote({
  note,
  worktree,
  onChange,
  onHide,
}: {
  note: Note;
  worktree: string;
  /** Text and drag-end position edits; `immediate` saves without debouncing. */
  onChange: (patch: Partial<Note>, immediate?: boolean) => void;
  onHide: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  // Live position while dragging; null → follow the stored one.
  const [drag, setDrag] = useState<{ x: number; y: number } | null>(null);
  const [pos, setPos] = useState({ x: note.x, y: note.y });

  // Re-clamp against the current stage size: on mount, when the stored position changes,
  // and whenever the stage (or the note's own height) resizes.
  const reclamp = useCallback(() => {
    const el = ref.current;
    const stage = el?.parentElement;
    if (!el || !stage) return;
    const want = drag ?? { x: note.x, y: note.y };
    setPos(
      clampNotePosition(
        want,
        { width: el.offsetWidth, height: el.offsetHeight },
        { width: stage.clientWidth, height: stage.clientHeight },
      ),
    );
  }, [drag, note.x, note.y]);
  useLayoutEffect(reclamp, [reclamp]);
  useEffect(() => {
    const el = ref.current;
    const stage = el?.parentElement;
    if (!el || !stage) return;
    const ro = new ResizeObserver(reclamp);
    ro.observe(stage);
    ro.observe(el);
    return () => ro.disconnect();
  }, [reclamp]);

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
    e.preventDefault();
    const start = { px: e.clientX, py: e.clientY, x: pos.x, y: pos.y };
    const handle = e.currentTarget;
    handle.setPointerCapture(e.pointerId);
    let last = { x: pos.x, y: pos.y };
    const move = (ev: PointerEvent) => {
      last = { x: start.x + ev.clientX - start.px, y: start.y + ev.clientY - start.py };
      setDrag(last);
    };
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      setDrag(null);
      // Store the clamped spot so the note reopens where it was seen.
      const el = ref.current;
      const stage = el?.parentElement;
      const final =
        el && stage
          ? clampNotePosition(
              last,
              { width: el.offsetWidth, height: el.offsetHeight },
              { width: stage.clientWidth, height: stage.clientHeight },
            )
          : last;
      onChange(final, true);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  };

  return (
    <div
      ref={ref}
      className={`sticky-note${drag ? " dragging" : ""}`}
      style={{ left: pos.x, top: pos.y }}
      role="dialog"
      aria-label={`Sticky note for ${worktree}`}
    >
      <div className="sticky-note-head" onPointerDown={onPointerDown}>
        <span className="sticky-note-title">{worktree}</span>
        <button
          className="sticky-note-close"
          onClick={onHide}
          title="Hide note"
          aria-label="Hide note"
        >
          <X size={12} />
        </button>
      </div>
      <textarea
        className="sticky-note-text"
        value={note.text}
        placeholder="Notes for later…"
        spellCheck={false}
        onChange={(e) => onChange({ text: e.target.value })}
        onKeyDown={(e) => {
          if (e.key === "Escape") onHide();
        }}
      />
    </div>
  );
}
