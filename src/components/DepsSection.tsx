// Settings "Dependencies" section: what Keitora found for herdr, git, claude and codex,
// with Install / Upgrade / Browse for herdr, Re-check, and the pinned herdr path with
// "Clear (use discovery)". State lives in `DepsProvider`; only the doctor rows are local.

import { Loader2 } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { btnGhost, btnPrimary } from "../lib/buttonStyles";
import { type SettingsRow, settingsModel } from "../lib/deps";
import {
  type DoctorRow,
  depsDoctor,
  depsRecheck,
  depsRestartToFinish,
  depsSetHerdrBin,
} from "../lib/ipc";
import { cliTitle, parseCliKind } from "../lib/providers";
import { useDaemonStatus } from "../lib/sessions";
import { errorText } from "../lib/updates";
import { useDeps, useDepsInstall } from "./DepsProvider";
import { browseForHerdr, copyDepsDetails, ManualCommands } from "./DepsStep";
import { RestartDaemonDialog } from "./RestartDaemonDialog";
import { useToast } from "./Toast";

const sectionTitle = "text-[12px] font-semibold uppercase tracking-wide text-tx-2";

const TONE: Record<SettingsRow["tone"], string> = {
  ok: "text-tx-3",
  warn: "text-busy",
  error: "text-urgent",
  muted: "text-tx-3",
};

/** A doctor row's display name: the provider title for an agent CLI, else the tool name. */
function toolTitle(tool: DoctorRow["tool"]): string {
  const cli = parseCliKind(tool);
  return cli ? cliTitle(cli) : tool;
}

export function DepsSection() {
  const { state, setUpgradeOpen } = useDeps();
  const install = useDepsInstall();
  const { showToast } = useToast();
  const daemon = useDaemonStatus();
  const [rows, setRows] = useState<DoctorRow[]>([]);
  const [checking, setChecking] = useState(false);
  const [confirmRestart, setConfirmRestart] = useState(false);

  const loadRows = useCallback(() => {
    void depsDoctor()
      .then(setRows)
      .catch(() => {});
  }, []);

  // The rows go stale when a run ends, the pin changes, or the floor moves.
  const snap = state.snapshot;
  const runType = snap?.run.type;
  const pinned = snap?.pinned_bin;
  const floorType = snap?.floor.type;
  // biome-ignore lint/correctness/useExhaustiveDependencies: reload on these changes only
  useEffect(loadRows, [runType, pinned, floorType]);

  const model = settingsModel(snap, rows, state.progress);
  const busy = model.running !== null;

  const recheck = () => {
    setChecking(true);
    void depsRecheck()
      .catch((err: unknown) => showToast(`Re-check failed: ${errorText(err)}`))
      .finally(() => {
        setChecking(false);
        loadRows();
      });
  };

  const clearPin = () => {
    void depsSetHerdrBin(null).catch((err: unknown) =>
      showToast(`Could not clear the herdr path: ${errorText(err)}`),
    );
  };

  // Restart onto the pinned herdr and refresh the agent integration, as an upgrade does.
  const restart = () => {
    setConfirmRestart(false);
    void depsRestartToFinish().catch((e: unknown) =>
      showToast(`Could not restart herdr: ${errorText(e)}`),
    );
  };

  const act = (kind: NonNullable<SettingsRow["action"]>["kind"]) => {
    switch (kind) {
      case "install":
        void install();
        break;
      case "upgrade":
        setUpgradeOpen(true);
        break;
      case "browse":
        void browseForHerdr(showToast);
        break;
      case "restart_to_finish":
        if (daemon.session_count > 0) setConfirmRestart(true);
        else restart();
        break;
    }
  };

  return (
    <section className="flex flex-col gap-3 border-t border-line pt-6">
      <div className={sectionTitle}>Dependencies</div>

      {model.rows.length === 0 && <p className="text-[11px] text-tx-3">Checking…</p>}
      {model.rows.map(({ action, ...row }) => (
        <div key={row.tool} className="flex flex-col gap-0.5">
          <div className="flex items-center justify-between gap-3">
            <span className="text-[12px] font-semibold text-tx-1">{toolTitle(row.tool)}</span>
            <span className="flex items-center gap-2">
              <span className={`text-[11px] ${TONE[row.tone]}`}>{row.detail}</span>
              {action && (
                <button
                  className={action.kind === "browse" ? btnGhost : btnPrimary}
                  disabled={busy}
                  onClick={() => act(action.kind)}
                >
                  {action.label}
                </button>
              )}
              {row.tool === "herdr" && action?.kind !== "browse" && (
                <button className={btnGhost} disabled={busy} onClick={() => act("browse")}>
                  Browse…
                </button>
              )}
            </span>
          </div>
          {row.path && (
            <span className="truncate font-mono text-[10.5px] text-tx-3" title={row.path}>
              {row.path}
            </span>
          )}
          {row.hint && row.detail === "not found" && (
            <span className="break-all font-mono text-[10.5px] text-tx-3 select-text">
              {row.hint}
            </span>
          )}
        </div>
      ))}

      {model.running && (
        <p className="flex items-center gap-1.5 text-[11px] text-tx-2">
          <Loader2 size={12} className="animate-spin" />
          {model.running}
        </p>
      )}

      {model.failure && (
        <div className="flex flex-col gap-1.5">
          <p className="text-[11px] text-urgent">{model.failure.text}</p>
          {model.failure.message && (
            <p className="break-words font-mono text-[10.5px] text-tx-3">{model.failure.message}</p>
          )}
          <div className="flex flex-wrap gap-2">
            {model.failure.tryDirect && (
              <button className={btnPrimary} onClick={() => void install("direct")}>
                Try direct download
              </button>
            )}
            <button className={btnGhost} onClick={() => copyDepsDetails(showToast)}>
              Copy details
            </button>
          </div>
        </div>
      )}

      {model.manual && <ManualCommands manual={model.manual} />}

      <div className="flex flex-col gap-1">
        <span className="text-[10.5px] uppercase tracking-wide text-tx-3">herdr path</span>
        <div className="flex items-center gap-2">
          <span
            className={`min-w-0 flex-1 truncate text-[11px] ${model.pinned ? "font-mono text-tx-2" : "text-tx-3"}`}
            title={model.pinned ?? undefined}
          >
            {model.pinned ?? "Found automatically"}
          </span>
          {model.pinned && (
            <button className={btnGhost} disabled={busy} onClick={clearPin}>
              Clear (use discovery)
            </button>
          )}
        </div>
      </div>

      <div>
        <button className={btnGhost} disabled={checking || busy} onClick={recheck}>
          {checking ? (
            <span className="flex items-center gap-1.5">
              <Loader2 size={12} className="animate-spin" />
              Checking…
            </span>
          ) : (
            "Re-check"
          )}
        </button>
      </div>

      <RestartDaemonDialog
        open={confirmRestart}
        onOpenChange={setConfirmRestart}
        sessionCount={daemon.session_count}
        title="Restart herdr to finish?"
        onConfirm={restart}
      />
    </section>
  );
}
