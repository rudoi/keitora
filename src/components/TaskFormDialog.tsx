// Reusable task form in a Radix dialog — used for "New task" (Board) and "Edit"
// (Inspector). Tags are editable only when creating: the store's TaskUpdate has
// no tags field, so edit covers title/priority/description.

import * as Dialog from "@radix-ui/react-dialog";
import { useEffect, useState } from "react";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";
import type { Priority } from "../lib/tasks";

const PRIORITIES: Priority[] = ["low", "normal", "high", "urgent"];

export interface TaskFormValues {
  title: string;
  priority: Priority;
  description: string;
  tags: string[];
}

export function TaskFormDialog({
  open,
  onOpenChange,
  mode,
  initial,
  onSubmit,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  mode: "new" | "edit";
  initial?: Partial<TaskFormValues>;
  onSubmit: (values: TaskFormValues) => Promise<void>;
}) {
  const [title, setTitle] = useState(initial?.title ?? "");
  const [priority, setPriority] = useState<Priority>(initial?.priority ?? "normal");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [tags, setTags] = useState((initial?.tags ?? []).join(", "));
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // Re-seed the form from `initial` each time the dialog opens. useState only
  // reads `initial` on first mount, but this dialog is kept mounted and reused
  // across tasks — without this, editing task B would show task A's fields.
  // biome-ignore lint/correctness/useExhaustiveDependencies: re-seed only on open/close transitions; initial object identity changes on every parent re-render intentionally excluded
  useEffect(() => {
    if (!open) return;
    setTitle(initial?.title ?? "");
    setPriority(initial?.priority ?? "normal");
    setDescription(initial?.description ?? "");
    setTags((initial?.tags ?? []).join(", "));
    setErr(null);
    // Re-seed only on open/close transitions, not on every parent re-render
    // (which recreates the inline `initial` object).
  }, [open]);

  const submit = async () => {
    if (!title.trim()) {
      setErr("Title is required.");
      return;
    }
    setBusy(true);
    setErr(null);
    try {
      await onSubmit({
        title: title.trim(),
        priority,
        description: description.trim(),
        tags: tags
          .split(",")
          .map((t) => t.trim())
          .filter(Boolean),
      });
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const field =
    "w-full bg-bg-3 border border-line rounded-chip px-3 py-2 text-tx-1 outline-none focus:border-accent/60 text-[13px]";
  const labelText = "text-[10.5px] uppercase tracking-wide text-tx-3";

  // Form body, rendered inside the dialog below.
  const fields = (
    <>
      <label className="flex flex-col gap-1.5">
        <span className={labelText}>Title</span>
        <input
          className={field}
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          autoFocus
          placeholder="What needs doing?"
        />
      </label>

      <label className="flex flex-col gap-1.5">
        <span className={labelText}>Priority</span>
        <select
          className={field}
          value={priority}
          onChange={(e) => setPriority(e.target.value as Priority)}
        >
          {PRIORITIES.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      </label>

      {mode === "new" && (
        <label className="flex flex-col gap-1.5">
          <span className={labelText}>
            Tags <span className="normal-case text-tx-3/70">(comma-separated)</span>
          </span>
          <input
            className={field}
            value={tags}
            onChange={(e) => setTags(e.target.value)}
            placeholder="ui, backend"
          />
        </label>
      )}

      <label className="flex flex-col gap-1.5">
        <span className={labelText}>Description</span>
        <textarea
          className={`${field} min-h-[88px] resize-y`}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          placeholder="Details, exit criteria…"
        />
      </label>

      {err && <p className="text-[11px] text-urgent">{err}</p>}
    </>
  );

  const title_ = mode === "new" ? "New task" : "Edit task";
  const desc_ =
    mode === "new"
      ? "Create a task in the shared board."
      : "Update this task. (Tags are set at creation.)";

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} w-[460px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
        >
          <Dialog.Title className="text-[15px] font-semibold text-tx-1">{title_}</Dialog.Title>
          <Dialog.Description className="mt-1 text-[11px] text-tx-3">{desc_}</Dialog.Description>

          <div className="mt-4 flex flex-col gap-3">{fields}</div>

          <div className="mt-5 flex justify-end gap-2">
            <Dialog.Close asChild>
              <button className="rounded-chip border border-line bg-bg-3 px-3.5 py-1.5 text-[11.5px] text-tx-1">
                Cancel
              </button>
            </Dialog.Close>
            <button
              className="rounded-chip border border-accent bg-accent px-3.5 py-1.5 text-[11.5px] font-semibold text-[var(--color-on-accent)] disabled:opacity-60"
              onClick={() => void submit()}
              disabled={busy}
            >
              {mode === "new" ? "Create" : "Save"}
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
