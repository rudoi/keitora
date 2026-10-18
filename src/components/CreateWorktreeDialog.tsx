// Dialog for creating a new git worktree. Mirrors TaskFormDialog's structure:
// driven by open/onOpenChange, re-seeds on open, shows err inline, closes only
// on success.

import * as Dialog from "@radix-ui/react-dialog";
import { useEffect, useState } from "react";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";

export function CreateWorktreeDialog({
  open,
  onOpenChange,
  onSubmit,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSubmit: (name: string) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // Re-seed the field each time the dialog opens.
  useEffect(() => {
    if (!open) return;
    setName("");
    setErr(null);
  }, [open]);

  const submit = async () => {
    if (busy) return;
    const trimmed = name.trim();
    if (!trimmed) return;
    setBusy(true);
    setErr(null);
    try {
      await onSubmit(trimmed);
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const field =
    "w-full bg-bg-3 border border-line rounded-chip px-3 py-2 text-tx-1 text-[13px] outline-none focus:border-accent/60";

  return (
    // While a worktree is being created, ignore close requests so the dialog can't
    // be dismissed mid-flight (creation can take a while to stand up).
    <Dialog.Root
      open={open}
      onOpenChange={(o) => {
        if (!busy) onOpenChange(o);
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} w-[400px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
        >
          <Dialog.Title className="text-[15px] font-semibold text-tx-1">New worktree</Dialog.Title>
          <Dialog.Description className="mt-1 text-[11px] text-tx-3">
            Enter a name for the new git worktree.
          </Dialog.Description>

          <div className="mt-4 flex flex-col gap-3">
            <label className="flex flex-col gap-1.5">
              <span className="text-[10.5px] uppercase tracking-wide text-tx-3">Name</span>
              <input
                className={field}
                value={name}
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void submit();
                }}
                autoFocus
                placeholder="my-feature"
                disabled={busy}
              />
            </label>

            {busy && (
              <p className="flex items-center gap-2 text-[11px] text-tx-3">
                <span className="inline-block h-3 w-3 animate-spin rounded-full border-[1.5px] border-tx-3/40 border-t-tx-1" />
                Creating worktree… this can take a moment.
              </p>
            )}
            {err && <p className="text-[11px] text-urgent">{err}</p>}
          </div>

          <div className="mt-5 flex justify-end gap-2">
            <button
              className="rounded-chip border border-line bg-bg-3 px-3.5 py-1.5 text-[11.5px] text-tx-1 disabled:opacity-60"
              onClick={() => onOpenChange(false)}
              disabled={busy}
            >
              Cancel
            </button>
            <button
              className="flex items-center gap-2 rounded-chip border border-accent bg-accent px-3.5 py-1.5 text-[11.5px] font-semibold text-[var(--color-on-accent)] disabled:opacity-60"
              onClick={() => void submit()}
              disabled={busy || !name.trim()}
            >
              {busy && (
                <span className="inline-block h-3 w-3 animate-spin rounded-full border-[1.5px] border-current/40 border-t-current" />
              )}
              {busy ? "Creating…" : "Create"}
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
