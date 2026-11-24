// The herdr upgrade dialog. Opens on the backend's upgrade prompt (one per herdr version
// per session; a protocol mismatch is a hard prompt with no "Later") or from an Upgrade
// button elsewhere. Upgrading under live sessions asks first with the daemon restart
// dialog, since the upgrade restarts herdr. A herdr installed by a package manager is
// left alone: the dialog shows that manager's command and offers a Keitora-managed copy.

import * as Dialog from "@radix-ui/react-dialog";
import { Loader2 } from "lucide-react";
import { useEffect, useState } from "react";
import { btnGhost, btnPrimary } from "../lib/buttonStyles";
import { dialogModel, upgradeConfirmCopy } from "../lib/deps";
import { type DepsTier, depsRecheck, depsSnoozeUpgrade } from "../lib/ipc";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";
import { useDeps, useDepsInstall } from "./DepsProvider";
import { copyDepsDetails } from "./DepsStep";
import { RestartDaemonDialog } from "./RestartDaemonDialog";
import { useToast } from "./Toast";

/** Mounted once in the app shell. */
export function DepsUpgradeDialog() {
  const { state, upgradeOpen, setUpgradeOpen } = useDeps();
  const install = useDepsInstall();
  const { showToast } = useToast();
  // The install waiting on the live-session confirmation (`undefined` = none).
  const [pending, setPending] = useState<DepsTier | null | undefined>(undefined);
  // A prompt the user closed without snoozing (hard prompts, or mid-run).
  const [hidden, setHidden] = useState<string | null>(null);

  const model = dialogModel(state.snapshot, upgradeOpen, state.progress);
  const key = model ? `${model.title}|${model.body}` : null;
  const open = model !== null && hidden !== key;

  // An Upgrade button elsewhere reopens a prompt the user closed.
  useEffect(() => {
    if (upgradeOpen) setHidden(null);
  }, [upgradeOpen]);

  if (model === null) return null;
  const busy = model.running !== null;

  const close = () => {
    setUpgradeOpen(false);
    if (model.hard || busy) {
      setHidden(key);
    } else {
      void depsSnoozeUpgrade().catch(() => {});
    }
  };

  const start = (tier: DepsTier | null) => {
    if (model.liveSessions > 0) setPending(tier);
    else void install(tier);
  };

  const confirm = upgradeConfirmCopy(model.liveSessions);

  return (
    <>
      <Dialog.Root
        open={open}
        onOpenChange={(o) => {
          if (!o) close();
        }}
      >
        <Dialog.Portal>
          <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
          <Dialog.Content
            className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} w-[460px] max-w-[92vw] -translate-x-1/2 -translate-y-1/2 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
          >
            <Dialog.Title className="text-[15px] font-semibold text-tx-1">
              {model.title}
            </Dialog.Title>
            <Dialog.Description className="mt-1 text-[12px] text-tx-3">
              {model.body}
            </Dialog.Description>

            {model.external && (
              <div className="mt-3 flex flex-col gap-1.5">
                <span className="text-[11px] text-tx-3">
                  This herdr came from a package manager. Upgrade it there:
                </span>
                <code className="select-text whitespace-pre-wrap break-all rounded-chip border border-line bg-bg-2 px-3 py-2 font-mono text-[11px] text-tx-1">
                  {model.external}
                </code>
              </div>
            )}

            {model.liveSessions > 0 && !busy && (
              <p className="mt-3 text-[11px] text-tx-3">
                Upgrading restarts herdr; running sessions resume afterwards.
              </p>
            )}

            {busy && (
              <p className="mt-3 flex items-center gap-1.5 text-[12px] text-tx-2">
                <Loader2 size={12} className="animate-spin" />
                {model.running}
              </p>
            )}

            {model.failure && !busy && (
              <div className="mt-3 flex flex-col gap-1">
                <p className="text-[12px] text-urgent">{model.failure.text}</p>
                {model.failure.message && (
                  <p className="break-words font-mono text-[11px] text-tx-3">
                    {model.failure.message}
                  </p>
                )}
              </div>
            )}

            <div className="mt-5 flex flex-wrap justify-end gap-2">
              {model.failure && !busy && (
                <button className={btnGhost} onClick={() => copyDepsDetails(showToast)}>
                  Copy details
                </button>
              )}
              {model.external && (
                <button
                  className={btnGhost}
                  disabled={busy}
                  onClick={() => void depsRecheck().catch(() => {})}
                >
                  Re-check
                </button>
              )}
              {!model.hard && (
                <button className={btnGhost} onClick={close}>
                  {busy ? "Hide" : "Later"}
                </button>
              )}
              {model.failure?.tryDirect && !busy ? (
                <button className={btnPrimary} onClick={() => start("direct")}>
                  Try direct download
                </button>
              ) : (
                <button
                  className={btnPrimary}
                  disabled={busy}
                  onClick={() => start(model.primary.tier)}
                >
                  {model.failure && !busy && !model.external ? "Retry" : model.primary.label}
                </button>
              )}
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
      <RestartDaemonDialog
        open={pending !== undefined}
        onOpenChange={(o) => {
          if (!o) setPending(undefined);
        }}
        sessionCount={model.liveSessions}
        title={confirm.title}
        description={confirm.description}
        confirmLabel={confirm.confirmLabel}
        onConfirm={() => {
          const tier = pending ?? null;
          setPending(undefined);
          void install(tier);
        }}
      />
    </>
  );
}
