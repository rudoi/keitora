// The update dialog: runs a CLI's update command in a visible terminal pane on the
// reserved session key, then reports how it went. On Windows a running CLI holds its
// executable open, so it first offers to stop that CLI's sessions. Closing ends the
// pane, so while the command runs every way out (close button, outside click) asks for
// confirmation first, and Escape stays with the terminal.

import * as Dialog from "@radix-ui/react-dialog";
import { Loader2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  cliTitle,
  closeInterruptsRun,
  interruptRunCopy,
  runOutcomeCopy,
  shouldAskToStop,
  stopSessionsLabel,
} from "../lib/agentUpdates";
import { btnGhost, btnPrimary } from "../lib/buttonStyles";
import {
  AGENT_UPDATE_PROJECT,
  AGENT_UPDATE_WORKTREE,
  agentUpdateClose,
  agentUpdateLiveSessions,
  agentUpdateRun,
  agentUpdateStop,
  type CliKind,
  type DaemonInfo,
  type RecoveryInfo,
  type RunPlan,
} from "../lib/ipc";
import { LAYER_MODAL, MODAL_OVERLAY_CLS } from "../lib/layers";
import { isWindows } from "../lib/platform";
import { errorText } from "../lib/updates";
import { useAgentUpdates } from "./AgentUpdatesProvider";
import { TerminalPane } from "./TerminalPane";

type Step =
  | { kind: "preparing" }
  | { kind: "ask"; count: number }
  | { kind: "stopping" }
  | { kind: "running"; plan: RunPlan }
  | { kind: "error"; message: string };

/** The dialog for `useAgentUpdates().dialogCli`, while one is set. */
export function AgentUpdateDialogHost({
  daemonInfo,
  recovery,
}: {
  daemonInfo: DaemonInfo;
  recovery: RecoveryInfo;
}) {
  const cli = useAgentUpdates().dialogCli;
  if (!cli) return null;
  return <AgentUpdateDialog key={cli} cli={cli} daemonInfo={daemonInfo} recovery={recovery} />;
}

/** Runs one update; remounted per open. */
export function AgentUpdateDialog({
  cli,
  daemonInfo,
  recovery,
}: {
  cli: CliKind;
  daemonInfo: DaemonInfo;
  recovery: RecoveryInfo;
}) {
  const { closeDialog, runResult, restartSessions } = useAgentUpdates();
  const [step, setStep] = useState<Step>({ kind: "preparing" });
  // The user tried to close mid-run and is being asked to confirm.
  const [confirmClose, setConfirmClose] = useState(false);
  // The run outcome seen when this run started; only a newer one belongs to it.
  const [baseSeq, setBaseSeq] = useState<number | null>(null);
  // The backend has an update pane to close.
  const startedRef = useRef(false);
  const runSeqRef = useRef(runResult?.seq ?? 0);
  runSeqRef.current = runResult?.seq ?? 0;

  const run = useCallback(async () => {
    setStep({ kind: "preparing" });
    try {
      startedRef.current = true;
      setBaseSeq(runSeqRef.current);
      const plan = await agentUpdateRun(cli);
      setStep({ kind: "running", plan });
    } catch (err) {
      setStep({ kind: "error", message: errorText(err) });
    }
  }, [cli]);

  // First step: on Windows, ask before updating under running sessions.
  // biome-ignore lint/correctness/useExhaustiveDependencies: runs once per dialog open
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      let live: Awaited<ReturnType<typeof agentUpdateLiveSessions>> = [];
      if (isWindows) {
        try {
          live = await agentUpdateLiveSessions(cli);
        } catch {
          /* can't tell: update and let the pane show any failure */
        }
      }
      if (cancelled) return;
      if (shouldAskToStop(isWindows, live)) setStep({ kind: "ask", count: live.length });
      else void run();
    })();
    return () => {
      cancelled = true;
    };
  }, [cli]);

  // Closing (or unmounting) ends the update pane.
  useEffect(
    () => () => {
      if (startedRef.current) void agentUpdateClose().catch(() => {});
    },
    [],
  );

  const stopThenRun = async () => {
    setStep({ kind: "stopping" });
    try {
      await agentUpdateStop(cli);
    } catch (err) {
      setStep({ kind: "error", message: `Could not stop sessions: ${errorText(err)}` });
      return;
    }
    await run();
  };

  const outcome =
    baseSeq !== null && runResult && runResult.cli === cli && runResult.seq > baseSeq
      ? runOutcomeCopy(cli, runResult.outcome)
      : null;
  const name = cliTitle(cli);
  const interrupts = closeInterruptsRun(step.kind, outcome !== null);
  const requestClose = () => {
    if (interrupts) setConfirmClose(true);
    else closeDialog();
  };

  return (
    <Dialog.Root open onOpenChange={(open) => !open && requestClose()}>
      <Dialog.Portal>
        <Dialog.Overlay className={MODAL_OVERLAY_CLS} />
        <Dialog.Content
          className={`fixed left-1/2 top-1/2 ${LAYER_MODAL} flex w-[760px] max-w-[94vw] -translate-x-1/2 -translate-y-1/2 flex-col gap-3 rounded-card border border-line bg-bg-1 p-5 shadow-[0_8px_30px_rgba(0,0,0,.4)]`}
          onEscapeKeyDown={(e) => {
            // Esc belongs to the terminal while the update runs.
            if (step.kind === "running") e.preventDefault();
          }}
        >
          <Dialog.Title className="text-[15px] font-semibold text-tx-1">Update {name}</Dialog.Title>

          {step.kind === "preparing" && (
            <Dialog.Description className="flex items-center gap-1.5 text-[12px] text-tx-3">
              <Loader2 size={12} className="animate-spin" />
              Starting the update…
            </Dialog.Description>
          )}

          {step.kind === "ask" && (
            <>
              <Dialog.Description className="text-[12px] text-tx-2">
                {step.count === 1 ? "1 session is" : `${step.count} sessions are`} running {cli}.
                Windows can't replace {cli} while it runs, so the update will likely fail unless
                they stop first. They can be restarted on the new version afterwards.
              </Dialog.Description>
              <div className="flex justify-end gap-2">
                <button className={btnGhost} onClick={closeDialog}>
                  cancel
                </button>
                <button className={btnGhost} onClick={() => void run()}>
                  update anyway
                </button>
                <button className={btnPrimary} onClick={() => void stopThenRun()}>
                  {stopSessionsLabel(step.count)}
                </button>
              </div>
            </>
          )}

          {step.kind === "stopping" && (
            <Dialog.Description className="flex items-center gap-1.5 text-[12px] text-tx-3">
              <Loader2 size={12} className="animate-spin" />
              Stopping {cli} sessions…
            </Dialog.Description>
          )}

          {step.kind === "error" && (
            <>
              <Dialog.Description className="text-[12px] text-urgent">
                {step.message}
              </Dialog.Description>
              <div className="flex justify-end">
                <button className={btnGhost} onClick={closeDialog}>
                  close
                </button>
              </div>
            </>
          )}

          {step.kind === "running" && (
            <>
              <Dialog.Description className="text-[12px] text-tx-3">
                Running <code className="agent-update-cmd">{step.plan.command}</code>
              </Dialog.Description>
              <div className="agent-update-term">
                <TerminalPane
                  name={AGENT_UPDATE_WORKTREE}
                  projectName={AGENT_UPDATE_PROJECT}
                  path={step.plan.cwd}
                  active
                  daemonInfo={daemonInfo}
                  recovery={recovery}
                  restartNonce={0}
                  cli={cli}
                  terminated={false}
                  terminateNonce={0}
                  launchNonce={0}
                  command={step.plan.command}
                />
              </div>
              <div className="flex items-center justify-end gap-2">
                <span
                  className={`mr-auto text-[12px] ${outcome?.tone === "error" ? "text-urgent" : "text-tx-2"}`}
                  role="status"
                  aria-live="polite"
                >
                  {outcome ? (
                    outcome.text
                  ) : (
                    <span className="flex items-center gap-1.5 text-tx-3">
                      <Loader2 size={12} className="animate-spin" />
                      Updating…
                    </span>
                  )}
                </span>
                {outcome?.canRestart && (
                  <button
                    className={btnPrimary}
                    onClick={() => {
                      restartSessions(cli);
                      closeDialog();
                    }}
                  >
                    restart {cli} sessions
                  </button>
                )}
                <button className={btnGhost} onClick={requestClose}>
                  close
                </button>
              </div>
              {confirmClose && interrupts && (
                <div
                  className="flex items-center justify-end gap-2"
                  role="alertdialog"
                  aria-label="Interrupt the update?"
                >
                  <span className="mr-auto text-[12px] text-urgent">{interruptRunCopy(cli)}</span>
                  <button className={btnGhost} onClick={() => setConfirmClose(false)}>
                    keep running
                  </button>
                  <button className={btnPrimary} onClick={closeDialog}>
                    close anyway
                  </button>
                </div>
              )}
            </>
          )}
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
