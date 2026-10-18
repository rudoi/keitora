// Quit-confirm modal. When the window close is intercepted because the
// daemon holds live sessions (`app://close-requested`), this lets the user choose
// to keep the sessions running (just quit the GUI) or stop everything. "Cancel"
// just dismisses locally — the window close was already prevented in Rust.

import * as Dialog from "@radix-ui/react-dialog";
import { useEffect, useState } from "react";

import { appQuitKeep, appQuitStop, onAppCloseRequested } from "../lib/ipc";
import { LAYER_QUIT, QUIT_OVERLAY_CLS } from "../lib/layers";

export function QuitConfirmModal() {
  const [open, setOpen] = useState(false);
  const [count, setCount] = useState(0);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    // `mounted` guards against setState calls that arrive after unmount: the
    // listen() promise may resolve after cleanup, and an event can fire in the
    // window between listen() resolving and the cleanup running.
    let mounted = true;
    onAppCloseRequested((sessionCount) => {
      if (!mounted) return;
      setCount(sessionCount);
      setBusy(false);
      setOpen(true);
    }).then((fn) => {
      if (mounted) unlisten = fn;
      else fn(); // already unmounted while listen() was resolving — cancel immediately
    });
    return () => {
      mounted = false;
      unlisten?.();
    };
  }, []);

  const keep = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await appQuitKeep();
    } catch {
      // exit(0) tears the webview down, so the promise may never resolve; ignore.
      setBusy(false);
    }
  };

  const stop = async () => {
    if (busy) return;
    setBusy(true);
    try {
      await appQuitStop();
    } catch {
      setBusy(false);
    }
  };

  const sessionLabel = count === 1 ? "1 live session" : `${count} live sessions`;

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(o) => {
        if (!busy) setOpen(o);
      }}
    >
      <Dialog.Portal>
        <Dialog.Overlay className={QUIT_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-1/2 top-1/2 ${LAYER_QUIT} w-[420px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
        >
          <Dialog.Title className="text-[15px] font-semibold text-tx-1">Quit Keitora?</Dialog.Title>
          <Dialog.Description className="mt-1 text-[12px] text-tx-3">
            The daemon is holding {sessionLabel}. Choose what happens when you quit.
          </Dialog.Description>

          <div className="mt-5 flex flex-col gap-2">
            <button
              className="rounded-chip border border-line bg-bg-3 px-3.5 py-2 text-left text-[12px] text-tx-1 disabled:opacity-60"
              onClick={() => void keep()}
              disabled={busy}
            >
              <span className="font-semibold">Quit anyway</span>
              <span className="ml-1 text-tx-3">(sessions keep running)</span>
            </button>
            <button
              className="rounded-chip border border-urgent/60 bg-bg-3 px-3.5 py-2 text-left text-[12px] text-urgent disabled:opacity-60"
              onClick={() => void stop()}
              disabled={busy}
            >
              <span className="font-semibold">Stop all &amp; quit</span>
            </button>
          </div>

          <div className="mt-5 flex justify-end">
            <button
              className="rounded-chip border border-line bg-bg-3 px-3.5 py-1.5 text-[11.5px] text-tx-1 disabled:opacity-60"
              onClick={() => setOpen(false)}
              disabled={busy}
            >
              Cancel
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
