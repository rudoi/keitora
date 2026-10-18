// On-demand "Add project" dialog: wraps AddProjectFlow in the same Radix Dialog
// primitive that SettingsDrawer uses. On completion the app reseeds automatically
// via the existing onProjectChanged → loadProject() listener in App.tsx (because
// onboard_project emits project://changed + siblings). Just close the dialog.

import * as Dialog from "@radix-ui/react-dialog";
import { X } from "lucide-react";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";
import { AddProjectFlow } from "./AddProjectFlow";

interface AddProjectDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

export function AddProjectDialog({ open, onOpenChange }: AddProjectDialogProps) {
  const close = () => onOpenChange(false);

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} w-[480px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
          aria-describedby={undefined}
        >
          <div className="mb-4 flex items-center justify-between">
            <Dialog.Title className="text-[15px] font-semibold text-tx-1">Add project</Dialog.Title>
            <Dialog.Close asChild>
              <button className="text-tx-3 hover:text-tx-1" aria-label="Close">
                <X size={16} />
              </button>
            </Dialog.Close>
          </div>

          {/* The flow handles its own steps; onComplete closes the dialog. The
              app reseeds via project://changed emitted by onboard_project. */}
          <AddProjectFlow onComplete={close} onCancel={close} />
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
