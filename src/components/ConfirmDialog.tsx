// Imperative, promise-based confirm dialog — reuses the app's existing Radix
// dialog primitive (`@radix-ui/react-dialog`, the same one QuitConfirmModal /
// TaskFormDialog / CreateWorktreeDialog already use) rather than adding a new
// dialog library. `useConfirmDialog()` mirrors `useToast()`'s context-hook shape;
// `confirmDialog(opts)` resolves `true` on confirm, `false` on cancel/dismiss —
// callers `await` it and bail out on `false` before any side effect
// (a transfer flow makes no backend call until the user confirms).
//
// `choiceDialog(opts)` adds a second action (`altLabel`) and resolves which of
// "confirm" / "alt" / "cancel" was picked; dismissing it (Esc, outside click,
// Cancel) is always "cancel", never one of the actions.
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

export interface ChoiceOptions extends ConfirmOptions {
  /** A second action beside confirm (rendered between Cancel and confirm). */
  altLabel: string;
}

export type Choice = "confirm" | "alt" | "cancel";

interface PendingConfirm extends ConfirmOptions {
  altLabel?: string;
  resolve: (choice: Choice) => void;
}

interface ConfirmCtx {
  confirmDialog: (opts: ConfirmOptions) => Promise<boolean>;
  choiceDialog: (opts: ChoiceOptions) => Promise<Choice>;
}

const Ctx = createContext<ConfirmCtx>({
  confirmDialog: () => Promise.resolve(false),
  choiceDialog: () => Promise.resolve("cancel"),
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

  const settle = useCallback((choice: Choice) => {
    // Side effects OUTSIDE the updater; `setCurrent(next)` is a pure value.
    currentRef.current?.resolve(choice);
    const next = queueRef.current.shift() ?? null;
    currentRef.current = next;
    setCurrent(next);
  }, []);

  const choiceDialog = useCallback((opts: ChoiceOptions | ConfirmOptions): Promise<Choice> => {
    return new Promise<Choice>((resolve) => {
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

  const confirmDialog = useCallback(
    (opts: ConfirmOptions): Promise<boolean> =>
      choiceDialog(opts).then((choice) => choice === "confirm"),
    [choiceDialog],
  );

  return (
    <Ctx.Provider value={{ confirmDialog, choiceDialog }}>
      {children}
      <Dialog.Root open={current !== null} onOpenChange={(open) => !open && settle("cancel")}>
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
                onClick={() => settle("cancel")}
              >
                {current?.cancelLabel ?? "Cancel"}
              </button>
              {current?.altLabel && (
                <button
                  className="rounded-chip border border-line bg-bg-3 px-3.5 py-1.5 text-[11.5px] font-semibold text-tx-1"
                  onClick={() => settle("alt")}
                >
                  {current.altLabel}
                </button>
              )}
              <button
                className={
                  current?.danger
                    ? "rounded-chip border border-urgent/60 bg-bg-3 px-3.5 py-1.5 text-[11.5px] font-semibold text-urgent"
                    : "rounded-chip border border-accent/60 bg-bg-3 px-3.5 py-1.5 text-[11.5px] font-semibold text-accent-text"
                }
                onClick={() => settle("confirm")}
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
