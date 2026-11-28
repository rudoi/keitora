// Settings "health" section: the app's self-diagnostics. Active findings grouped by
// project then worktree, the last day's cleared ones under a collapsed group, and a
// "copy as JSON" button that copies what `keitora status --json` prints.

import { useEffect, useRef, useState } from "react";
import { btnGhost } from "../lib/buttonStyles";
import {
  activeFindings,
  ageText,
  clockText,
  groupFindings,
  type HealthFinding,
  OPEN_HEALTH_EVENT,
  projectLabel,
  statusJson,
  takeHealthFocus,
  worktreeLabel,
} from "../lib/health";
import { sectionTitle } from "../lib/settingsStyles";
import { errorText } from "../lib/updates";
import { useHealthFindings } from "./HealthProvider";
import { useToast } from "./Toast";

const DOT: Record<HealthFinding["severity"], string> = {
  info: "bg-tx-3",
  warn: "bg-busy",
  error: "bg-urgent",
};

function FindingRow({ f, now }: { f: HealthFinding; now: number }) {
  return (
    <li className="flex gap-2" data-check={f.checkId}>
      <span
        className={`mt-[5px] h-1.5 w-1.5 flex-none rounded-full ${DOT[f.severity]}`}
        title={f.severity}
        aria-label={f.severity}
      />
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="font-mono text-[10.5px] text-tx-3">{f.checkId}</span>
        <span className="break-words text-[11px] text-tx-1 select-text">{f.message}</span>
        <span className="text-[10.5px] text-tx-3">
          first seen {clockText(f.firstSeen)} · last seen {ageText(now - f.lastSeen)} ago ·{" "}
          {f.occurrences}x{f.clearedAt !== null && <> · cleared {clockText(f.clearedAt)}</>}
        </span>
        {f.remedy && (
          <span className="text-[10.5px] text-tx-3">
            fixed by: {f.remedy}
            {f.remedyAt !== null && <> at {clockText(f.remedyAt)}</>}
          </span>
        )}
      </div>
    </li>
  );
}

function Groups({ findings, now }: { findings: HealthFinding[]; now: number }) {
  return (
    <>
      {groupFindings(findings).map((p) => (
        <div key={p.project} className="flex flex-col gap-1.5" data-project={p.project}>
          <span className="text-[12px] font-semibold text-tx-1">{projectLabel(p.project)}</span>
          {p.worktrees.map((w) => (
            <div key={w.worktree} className="flex flex-col gap-1 pl-2" data-worktree={w.worktree}>
              {p.project !== "" && (
                <span className="text-[10.5px] uppercase tracking-wide text-tx-3">
                  {worktreeLabel(w.worktree)}
                </span>
              )}
              <ul className="flex flex-col gap-1.5">
                {w.findings.map((f) => (
                  <FindingRow key={f.checkId} f={f} now={now} />
                ))}
              </ul>
            </div>
          ))}
        </div>
      ))}
    </>
  );
}

/** The section's content for `findings` at `now` (pure; the test renders it). */
export function HealthSectionView({
  findings,
  now,
  onCopy,
}: {
  findings: HealthFinding[];
  now: number;
  onCopy: () => void;
}) {
  const active = activeFindings(findings);
  const cleared = findings.filter((f) => f.clearedAt !== null);
  return (
    <>
      {active.length === 0 ? (
        <p className="text-[11px] text-tx-3">no issues</p>
      ) : (
        <Groups findings={active} now={now} />
      )}
      {cleared.length > 0 && (
        <details className="flex flex-col gap-1.5">
          <summary className="cursor-pointer text-[11px] text-tx-2">
            recently cleared ({cleared.length})
          </summary>
          <div className="flex flex-col gap-2 pt-1.5">
            <Groups findings={cleared} now={now} />
          </div>
        </details>
      )}
      <div>
        <button className={btnGhost} onClick={onCopy}>
          copy as JSON
        </button>
      </div>
    </>
  );
}

export function HealthSection() {
  const findings = useHealthFindings();
  const { showToast } = useToast();
  const ref = useRef<HTMLElement>(null);
  const [now, setNow] = useState(() => Date.now());

  // Ages move on their own: re-render every 30 s.
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(id);
  }, []);

  // A glyph click opens Settings here: scroll into view on mount, or when already open.
  useEffect(() => {
    const focus = () => {
      if (takeHealthFocus()) ref.current?.scrollIntoView({ block: "start", behavior: "smooth" });
    };
    focus();
    window.addEventListener(OPEN_HEALTH_EVENT, focus);
    return () => window.removeEventListener(OPEN_HEALTH_EVENT, focus);
  }, []);

  const copy = () => {
    void navigator.clipboard
      .writeText(statusJson(findings))
      .then(() => showToast("Copied health findings"))
      .catch((err: unknown) => showToast(`Could not copy: ${errorText(err)}`));
  };

  return (
    <section ref={ref} className="flex flex-col gap-3 border-t border-line pt-6">
      <div className={sectionTitle}>health</div>
      <HealthSectionView findings={findings} now={now} onCopy={copy} />
    </section>
  );
}
