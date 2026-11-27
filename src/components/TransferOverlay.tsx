// Covers a pane while its worktree is mid CLI-transfer: the import (with elapsed time and,
// for Claude to Codex, a Cancel button) and then the restart into the target CLI. A switch
// has no import: it shows "Switching to …" until the backend answers, then the restart. It is
// rendered regardless of the pane's status, because a running pane stays `running` for the
// whole import and through the restart. Focus moves here on mount so keystrokes cannot
// reach the terminal underneath.

import { useEffect, useRef, useState } from "react";
import { cliTitle } from "../lib/providers";
import { canCancel, formatElapsed, progressLabel, type TransferEntry } from "../lib/transferPhase";

export function TransferOverlay({
  entry,
  onCancel,
}: {
  entry: TransferEntry;
  onCancel: () => void;
}) {
  const rootRef = useRef<HTMLDivElement | null>(null);
  const cancelRef = useRef<HTMLButtonElement | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const switching = entry.mode === "switch";
  const importing = !switching && entry.phase === "importing";

  // Only the importing view shows elapsed time, so only it ticks.
  useEffect(() => {
    if (!importing) return;
    setNow(Date.now());
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [importing]);

  const showCancel = importing && entry.mode === "transfer" && entry.to === "codex";

  // Take focus whenever the phase changes (mount included): the Cancel button when there
  // is one, else the overlay itself.
  // biome-ignore lint/correctness/useExhaustiveDependencies: refocus only when the phase or the button's presence changes
  useEffect(() => {
    (cancelRef.current ?? rootRef.current)?.focus();
  }, [entry.phase, showCancel]);

  const detail = entry.progress?.message;

  return (
    <div className="pane-overlay" role="status" aria-live="polite" tabIndex={-1} ref={rootRef}>
      <div className="pane-spinner" aria-label={switching ? "Switching" : "Transferring"} />
      {switching && entry.result === null ? (
        <div className="pane-msg">Switching to {cliTitle(entry.to)}…</div>
      ) : importing ? (
        <>
          <div className="pane-msg">Transferring to {cliTitle(entry.to)}…</div>
          <div className="pane-msg">{progressLabel(entry.progress)}</div>
          {typeof detail === "string" && detail !== "" && (
            <div className="pane-msg pane-msg-detail">{detail}</div>
          )}
          <div className="pane-msg pane-msg-detail">
            Elapsed {formatElapsed(now - entry.startedAt)}
          </div>
          <div className="pane-msg pane-msg-detail">
            Input is paused while the conversation is imported.
          </div>
          {showCancel && (
            <div className="pane-btn-row">
              <button
                type="button"
                ref={cancelRef}
                className="pane-btn pane-btn-secondary"
                disabled={!canCancel(entry)}
                onClick={onCancel}
              >
                {entry.cancelling ? "Cancelling…" : "Cancel"}
              </button>
            </div>
          )}
        </>
      ) : (
        <div className="pane-msg">Starting {cliTitle(entry.to)}…</div>
      )}
    </div>
  );
}
