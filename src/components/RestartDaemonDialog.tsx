// The "Restart daemon?" confirmation. Shared by every control that restarts the
// daemon on the user's click (Settings → Daemon, the rail's offline card), so
// they all ask the same question in the same words before interrupting sessions.

import * as Dialog from "@radix-ui/react-dialog";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";

const btnPrimary =
  "rounded-chip border border-accent bg-accent px-3.5 py-1.5 text-[11.5px] font-semibold text-[var(--color-on-accent)] disabled:opacity-60";
const btnGhost =
  "rounded-chip border border-line bg-bg-3 px-3 py-1.5 text-[11.5px] text-tx-1 disabled:opacity-50";

/** "1 running session" / "N running sessions". */
export function runningSessionsLabel(sessionCount: number): string {
  return sessionCount === 1 ? "1 running session" : `${sessionCount} running sessions`;
}

export function RestartDaemonDialog({
  open,
  onOpenChange,
  sessionCount,
  onConfirm,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Sessions the daemon reports as running (what the restart interrupts). */
  sessionCount: number;
  /** The user confirmed. The caller closes the dialog and starts the restart. */
  onConfirm: () => void;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} w-[420px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
        >
          <Dialog.Title className="text-[15px] font-semibold text-tx-1">
            Restart daemon?
          </Dialog.Title>
          <Dialog.Description className="mt-1 text-[12px] text-tx-3">
            This interrupts {runningSessionsLabel(sessionCount)} in this project mid-turn. Their
            agents resume afterwards with their conversation transcript kept.
          </Dialog.Description>
          <div className="mt-5 flex justify-end gap-2">
            <button className={btnGhost} onClick={() => onOpenChange(false)}>
              Cancel
            </button>
            <button className={btnPrimary} onClick={onConfirm}>
              Restart
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
