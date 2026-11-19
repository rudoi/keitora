// Strips under the title chrome announcing Claude / Codex CLI updates, one per CLI, plus
// the "restart sessions" row after an update ran. Keitora's own update banner goes
// first: while it shows, this renders nothing.

import { Copy } from "lucide-react";
import { bannerRows, cliTitle, runOutcomeCopy, updateDisabledReason } from "../lib/agentUpdates";
import type { RecoveryInfo } from "../lib/ipc";
import { bannerOf, errorText } from "../lib/updates";
import { useAgentUpdates } from "./AgentUpdatesProvider";
import { useToast } from "./Toast";
import { useUpdates } from "./UpdatesProvider";

export function AgentUpdateBanner({ recovery }: { recovery: RecoveryInfo }) {
  const { state: keitoraUpdate } = useUpdates();
  const {
    snapshot,
    later,
    runResult,
    outcomeDismissed,
    openDialog,
    putOffUntilLater,
    skip,
    restartSessions,
    dismissOutcome,
  } = useAgentUpdates();
  const { showToast } = useToast();
  if (bannerOf(keitoraUpdate) !== null) return null;

  const rows = bannerRows(snapshot, later);
  const outcome =
    runResult && !outcomeDismissed && runResult.outcome.type === "updated"
      ? { cli: runResult.cli, copy: runOutcomeCopy(runResult.cli, runResult.outcome) }
      : null;
  if (rows.length === 0 && !outcome) return null;

  const copy = (text: string) => {
    void navigator.clipboard
      .writeText(text)
      .then(() => showToast("Copied"))
      .catch((err: unknown) => showToast(`Could not copy: ${errorText(err)}`));
  };

  return (
    <>
      {outcome && (
        <div className="update-banner" data-tone="info" role="status" aria-live="polite">
          <span className="update-banner-text">{outcome.copy.text}</span>
          <button
            type="button"
            className="update-banner-btn update-banner-btn-primary"
            onClick={() => restartSessions(outcome.cli)}
          >
            restart {outcome.cli} sessions
          </button>
          <button type="button" className="update-banner-btn" onClick={dismissOutcome}>
            dismiss
          </button>
        </div>
      )}
      {rows.map((row) => {
        const disabled = updateDisabledReason(row, recovery.phase);
        return (
          <div
            key={row.cli}
            className="update-banner"
            data-tone="info"
            data-cli={row.cli}
            role="status"
            aria-live="polite"
          >
            <span className="update-banner-text">
              {row.text}
              {row.guidance && (
                <span className="agent-update-guidance">
                  {row.guidanceNote} <code className="agent-update-cmd">{row.guidance}</code>
                  <button
                    type="button"
                    className="agent-update-copy"
                    title="Copy the command"
                    aria-label="Copy the command"
                    onClick={() => copy(row.guidance ?? "")}
                  >
                    <Copy size={11} />
                  </button>
                </span>
              )}
            </span>
            <button
              type="button"
              className="update-banner-btn update-banner-btn-primary"
              disabled={disabled !== null}
              title={disabled ?? `Update ${cliTitle(row.cli)} in a terminal pane`}
              onClick={() => openDialog(row.cli)}
            >
              update
            </button>
            <button
              type="button"
              className="update-banner-btn"
              onClick={() => skip(row.cli, row.latest)}
            >
              skip this version
            </button>
            <button
              type="button"
              className="update-banner-btn"
              onClick={() => putOffUntilLater(row.cli)}
            >
              later
            </button>
          </div>
        );
      })}
    </>
  );
}
