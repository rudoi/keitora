// TagInput — the task form's comma-separated Tags field with a suggestion list of
// the project's tags. The list opens when the field is focused or clicked and
// narrows to the tag being typed (the text after the last comma); picking one
// replaces that text. Editing rules live in tagEntry.ts.
//
// Escape closes the list rather than the dialog: while the list is open the input
// carries `data-suggest-open`, which TaskFormDialog's onEscapeKeyDown checks
// (Radix handles Escape on the document before this input's own handler runs).

import { useId, useState } from "react";
import { applyTag, currentToken, tagSuggestions } from "./tagEntry";

export function TagInput({
  id,
  value,
  onChange,
  tags,
  className,
  placeholder,
}: {
  id?: string;
  value: string;
  onChange: (value: string) => void;
  /** The project's tags, in display order. */
  tags: string[];
  className?: string;
  placeholder?: string;
}) {
  const listId = useId();
  const [open, setOpen] = useState(false);
  // Index into `suggestions` of the keyboard-highlighted option; -1 for none.
  const [active, setActive] = useState(-1);

  const suggestions = tagSuggestions(tags, value);
  const shown = open && suggestions.length > 0;
  const highlighted = active < suggestions.length ? active : -1;

  const edit = (next: string) => {
    onChange(next);
    setOpen(true);
    // Typing a partial tag highlights the first match, so Enter completes it.
    setActive(currentToken(next) ? 0 : -1);
  };
  const pick = (tag: string) => edit(applyTag(value, tag));

  return (
    <div className="relative">
      <input
        id={id}
        className={className}
        value={value}
        placeholder={placeholder}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={shown}
        aria-controls={listId}
        aria-activedescendant={shown && highlighted >= 0 ? `${listId}-${highlighted}` : undefined}
        data-suggest-open={shown ? "true" : undefined}
        onChange={(e) => edit(e.target.value)}
        onFocus={() => setOpen(true)}
        onClick={() => setOpen(true)}
        onBlur={() => {
          setOpen(false);
          setActive(-1);
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            if (suggestions.length === 0) return;
            e.preventDefault();
            setOpen(true);
            const step = e.key === "ArrowDown" ? 1 : -1;
            setActive((i) => {
              const from = i < suggestions.length ? i : -1;
              if (from < 0) return step === 1 ? 0 : suggestions.length - 1;
              return (from + step + suggestions.length) % suggestions.length;
            });
          } else if (e.key === "Enter" && shown && highlighted >= 0) {
            e.preventDefault();
            pick(suggestions[highlighted]);
          } else if (e.key === "Escape" && shown) {
            setOpen(false);
            setActive(-1);
          }
        }}
      />
      {shown && (
        <div
          id={listId}
          role="listbox"
          aria-label="Project tags"
          className="absolute left-0 right-0 top-full z-10 mt-1 max-h-[180px] overflow-y-auto rounded-card border border-line bg-bg-2 p-1 shadow-[0_8px_24px_rgba(0,0,0,.45)]"
        >
          {suggestions.map((tag, i) => (
            <div
              key={tag}
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === highlighted}
              // Highlighted via aria-activedescendant; focus stays in the input.
              tabIndex={-1}
              // Keep focus in the input so the list stays open for the next tag.
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => pick(tag)}
              onMouseEnter={() => setActive(i)}
              className={`cursor-pointer truncate rounded-chip px-2 py-1.5 text-[11.5px] ${
                i === highlighted ? "bg-bg-3 text-tx-1" : "text-tx-2"
              }`}
            >
              {tag}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
