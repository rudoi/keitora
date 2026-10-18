// Imperative, promise-based confirm dialog — reuses the app's existing Radix
// dialog primitive (`@radix-ui/react-dialog`, the same one QuitConfirmModal /
// TaskFormDialog / CreateWorktreeDialog already use) rather than adding a new
// dialog library. `useConfirmDialog()` mirrors `useToast()`'s context-hook shape;
// `confirmDialog(opts)` resolves `true` on confirm, `false` on cancel/dismiss —
// callers `await` it and bail out on `false` before any side effect
// (a transfer flow makes no backend call until the user confirms).
//
// One dialog at a time: a second call while one is open queues behind it (FIFO)
// rather than clobbering the pending promise, since the transfer flow can't
// currently open two at once but a queued handler is a much smaller foot-gun than
// a resolved-then-abandoned Promise.

import * as Dialog from "@radix-ui/react-dialog";
import { createContext, useCallback, useContext, useRef, useState } from "react";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";

export interface ConfirmOptions {
  title: string;
  /** Plain text — newlines render as line breaks. */
  body: string;
  confirmLabel: string;
  cancelLabel?: string;
  /** Styles the confirm button as a destructive action (red) instead of the accent. */
  danger?: boolean;
}

interface PendingConfirm extends ConfirmOptions {
  resolve: (ok: boolean) => void;
}

interface ConfirmCtx {
  confirmDialog: (opts: ConfirmOptions) => Promise<boolean>;
}

const Ctx = createContext<ConfirmCtx>({
  confirmDialog: () => Promise.resolve(false),
});

export function useConfirmDialog(): ConfirmCtx {
  return useContext(Ctx);
}

export function ConfirmDialogProvider({ children }: { children: React.ReactNode }) {
  const [current, setCurrent] = useState<PendingConfirm | null>(null);
  // FIFO queue for a confirm requested while one is already open.
  const queueRef = useRef<PendingConfirm[]>([]);
  // Ref mirror of `current` so the stable (`[]`-dep) callbacks below can read
  // whether a dialog is already open and resolve its promise WITHOUT running those
  // side effects inside a `setCurrent` updater — React invokes updaters twice in
  // StrictMode (dev), which would double-shift the queue (dropping a queued
  // dialog's resolve → it hangs forever) or double-push a queued confirm.
  const currentRef = useRef<PendingConfirm | null>(null);
  currentRef.current = current;

  const settle = useCallback((ok: boolean) => {
    // Side effects OUTSIDE the updater; `setCurrent(next)` is a pure value.
    currentRef.current?.resolve(ok);
    const next = queueRef.current.shift() ?? null;
    currentRef.current = next;
    setCurrent(next);
  }, []);

  const confirmDialog = useCallback((opts: ConfirmOptions): Promise<boolean> => {
    return new Promise<boolean>((resolve) => {
      const pending: PendingConfirm = { ...opts, resolve };
      // Decide queue-vs-show OUTSIDE any updater. Eagerly sync `currentRef` so a
      // second call in the same tick (before the re-render) still sees a dialog
      // open and queues rather than clobbering the first.
      if (currentRef.current) {
        queueRef.current.push(pending);
      } else {
        currentRef.current = pending;
        setCurrent(pending);
      }
    });
  }, []);

  return (
    <Ctx.Provider value={{ confirmDialog }}>
      {children}
      <Dialog.Root open={current !== null} onOpenChange={(open) => !open && settle(false)}>
        <Dialog.Portal>
          <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
          <Dialog.Content
            className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} w-[460px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
          >
            <Dialog.Title className="text-[15px] font-semibold text-tx-1">
              {current?.title}
            </Dialog.Title>
            <Dialog.Description className="mt-2 whitespace-pre-line text-[12px] leading-relaxed text-tx-3">
              {current?.body}
            </Dialog.Description>
            <div className="mt-5 flex justify-end gap-2">
              <button
                className="rounded-chip border border-line bg-bg-3 px-3.5 py-1.5 text-[11.5px] text-tx-1"
                onClick={() => settle(false)}
              >
                {current?.cancelLabel ?? "Cancel"}
              </button>
              <button
                className={
                  current?.danger
                    ? "rounded-chip border border-urgent/60 bg-bg-3 px-3.5 py-1.5 text-[11.5px] font-semibold text-urgent"
                    : "rounded-chip border border-accent/60 bg-bg-3 px-3.5 py-1.5 text-[11.5px] font-semibold text-accent-text"
                }
                onClick={() => settle(true)}
              >
                {current?.confirmLabel}
              </button>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </Ctx.Provider>
  );
}
