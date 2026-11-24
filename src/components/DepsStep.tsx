// First-run dependency step, shown before Add Project while herdr is missing or too old:
// install it (with live progress), recover from a failure (direct download, the manual
// commands, Copy details), point Keitora at an existing herdr, or skip.

import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useState } from "react";
import { stepModel } from "../lib/deps";
import { depsDetails, depsSetHerdrBin, type ManualRoute } from "../lib/ipc";
import { errorText } from "../lib/updates";
import { useDeps, useDepsInstall } from "./DepsProvider";
import { useToast } from "./Toast";

const ghostBtn = {
  background: "transparent",
  color: "var(--tx-2)",
  border: "1px solid var(--line)",
} as const;

/** Picks a herdr executable and pins it. Shared with the Settings section. */
export async function browseForHerdr(showToast: (msg: string) => void): Promise<void> {
  const path = await open({ directory: false, multiple: false, title: "Choose the herdr program" });
  if (path === null) return;
  try {
    await depsSetHerdrBin(path);
  } catch (err) {
    showToast(`Could not use that herdr: ${errorText(err)}`);
  }
}

/** Copies the "Copy details" report. */
export function copyDepsDetails(showToast: (msg: string) => void): void {
  void depsDetails()
    .then((text) => navigator.clipboard.writeText(text))
    .then(() => showToast("Details copied"))
    .catch((err: unknown) => showToast(`Could not copy details: ${errorText(err)}`));
}

/** The copyable install commands and the guide link. */
export function ManualCommands({ manual }: { manual: ManualRoute }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: "6px", width: "100%" }}>
      {manual.commands.map((c) => (
        <div key={c.command} style={{ textAlign: "left" }}>
          <div style={{ fontSize: "10.5px", color: "var(--tx-3)" }}>{c.shell}</div>
          <code
            className="mono"
            style={{
              display: "block",
              userSelect: "text",
              padding: "6px 8px",
              border: "1px solid var(--line)",
              borderRadius: "6px",
              background: "var(--bg-2)",
              color: "var(--tx-1)",
              wordBreak: "break-all",
            }}
          >
            {c.command}
          </code>
        </div>
      ))}
      <button
        type="button"
        onClick={() => void openUrl(manual.link).catch(() => {})}
        style={{
          alignSelf: "flex-start",
          background: "none",
          border: 0,
          padding: 0,
          color: "var(--accent)",
          fontSize: "11px",
          cursor: "pointer",
        }}
      >
        Install guide
      </button>
    </div>
  );
}

export function DepsStep({ onDone }: { onDone: () => void }) {
  const { state } = useDeps();
  const install = useDepsInstall();
  const { showToast } = useToast();
  const [showManual, setShowManual] = useState(false);
  const model = stepModel(state.snapshot, state.progress);

  if (model.kind === "hidden") {
    return null;
  }

  const browse = (
    <button className="pane-btn" style={ghostBtn} onClick={() => void browseForHerdr(showToast)}>
      I have herdr: browse…
    </button>
  );
  const skip = (
    <button className="pane-btn" style={ghostBtn} onClick={onDone}>
      Skip for now
    </button>
  );
  const column = {
    display: "flex",
    flexDirection: "column",
    gap: "8px",
    marginTop: "12px",
  } as const;

  if (model.kind === "running") {
    return (
      <div className="msg-card">
        <b>{model.title}</b>
        <p>{model.label}</p>
        {model.line && <p className="mono">{model.line}</p>}
      </div>
    );
  }

  if (model.kind === "done") {
    return (
      <div className="msg-card">
        <b>{model.title}</b>
        <p>{model.body}</p>
        <button className="pane-btn" onClick={onDone}>
          Continue
        </button>
      </div>
    );
  }

  if (model.kind === "failed") {
    return (
      <div className="msg-card">
        <b>{model.title}</b>
        <p>{model.body}</p>
        {model.message && <p className="mono">{model.message}</p>}
        {(!model.tryDirect || showManual) && <ManualCommands manual={model.manual} />}
        <div style={column}>
          {model.tryDirect ? (
            <button className="pane-btn" onClick={() => void install("direct")}>
              Try direct download
            </button>
          ) : (
            <button className="pane-btn" onClick={() => void install()}>
              Retry
            </button>
          )}
          {model.tryDirect && !showManual && (
            <button className="pane-btn" style={ghostBtn} onClick={() => setShowManual(true)}>
              Show install commands
            </button>
          )}
          {browse}
          <button className="pane-btn" style={ghostBtn} onClick={() => copyDepsDetails(showToast)}>
            Copy details
          </button>
          {skip}
        </div>
      </div>
    );
  }

  // offer
  return (
    <div className="msg-card">
      <b>{model.title}</b>
      <p>{model.body}</p>
      {!model.canInstall && <ManualCommands manual={model.manual} />}
      <div style={column}>
        {model.canInstall && (
          <button className="pane-btn" onClick={() => void install()}>
            {model.title}
          </button>
        )}
        {browse}
        {skip}
      </div>
    </div>
  );
}
