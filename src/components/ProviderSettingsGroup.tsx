// One provider's section of the settings drawer: path inputs, dispatch ritual,
// autonomy toggle and its warnings. Rendered once per CLI_KINDS member.

import { AlertTriangle } from "lucide-react";
import type { ReactNode } from "react";
import { btnGhost, btnPrimary } from "../lib/buttonStyles";
import type { CliKind } from "../lib/ipc";
import { cliTitle } from "../lib/providers";
import type { ProviderForm } from "../lib/settingsForm";

const field =
  "w-full bg-bg-3 border border-line rounded-chip px-3 py-2 text-tx-1 text-[13px] outline-none focus:border-accent/60";
const fieldRo = "w-full bg-bg-2 border border-line/60 rounded-chip px-3 py-2 text-tx-3 text-[13px]";
const label = "text-[10.5px] uppercase tracking-wide text-tx-3";
const sectionTitle = "text-[12px] font-semibold uppercase tracking-wide text-tx-2";

export type DispatchEditorRender = (
  steps: string[],
  onChange: (steps: string[]) => void,
) => ReactNode;

/** Per-provider strings; a `Record` so a missing provider is a type error. The optional
 *  fields override copy that is otherwise derived from the provider's name. */
export const PROVIDER_COPY: Record<
  CliKind,
  {
    autonomyLabel: string;
    flag: string;
    warning: string;
    projectForces?: string;
    notFound?: string;
  }
> = {
  claude: {
    autonomyLabel: "Skip permission prompts",
    flag: "--dangerously-skip-permissions",
    warning: "bypassing its per-action approval prompts. Only enable if you understand the risk.",
    projectForces: "enables skip-permissions",
  },
  codex: {
    autonomyLabel: "Bypass approvals & sandbox (danger)",
    flag: "--dangerously-bypass-approvals-and-sandbox",
    warning:
      "bypassing both its sandbox and its per-action approval prompts — broader than Claude's flag. Only enable if you understand the risk.",
  },
  antigravity: {
    autonomyLabel: "Auto-approve tool permissions (danger)",
    flag: "--dangerously-skip-permissions",
    warning:
      "auto-approving every tool permission request. Only enable if you understand the risk.",
    // `agy` is also found outside PATH, so the copy does not mention it.
    notFound: "Antigravity not found — install it, then reopen Settings.",
  },
};

/** The "not found" hint for `cli`. */
export function notFoundCopy(cli: CliKind): string {
  return (
    PROVIDER_COPY[cli].notFound ??
    `${cliTitle(cli)} not found on PATH — install it, then reopen Settings.`
  );
}

/** What a project-level autonomy override does, for `cli`. */
export function projectForcesCopy(cli: CliKind): string {
  return PROVIDER_COPY[cli].projectForces ?? `enables ${cliTitle(cli)} autonomy`;
}

/** What the group disables or shows. No path input is disabled by availability, so a
 *  user can type a path to fix "not found". */
export function providerGroupState(
  _cli: CliKind,
  available: boolean,
  hasCommittedFile: boolean,
): {
  autonomyDisabled: boolean;
  projectPathDisabled: boolean;
  showNotFound: boolean;
  showCommitNote: boolean;
  showEditor: boolean;
} {
  return {
    autonomyDisabled: !available,
    projectPathDisabled: !hasCommittedFile,
    showNotFound: !available,
    showCommitNote: !hasCommittedFile,
    showEditor: available,
  };
}

export function ProviderSettingsGroup({
  cli,
  available,
  hasCommittedFile,
  projectAutonomy,
  form,
  onChange,
  onToggleAutonomy,
  dispatchEditor,
}: {
  cli: CliKind;
  available: boolean;
  hasCommittedFile: boolean;
  projectAutonomy: boolean;
  form: ProviderForm;
  onChange: (patch: Partial<ProviderForm>) => void;
  onToggleAutonomy: () => void;
  dispatchEditor?: DispatchEditorRender;
}) {
  const title = cliTitle(cli);
  const copy = PROVIDER_COPY[cli];
  const st = providerGroupState(cli, available, hasCommittedFile);
  return (
    <section className="flex flex-col gap-3 border-t border-line pt-6">
      <div className={sectionTitle}>{title}</div>

      {st.showNotFound && <p className="text-[11px] text-tx-3">{notFoundCopy(cli)}</p>}

      <label className="flex flex-col gap-1.5">
        <span className={label}>{`${title} path (user default)`}</span>
        <input
          className={field}
          value={form.userPath}
          onChange={(e) => onChange({ userPath: e.target.value })}
          placeholder="(auto-discovered when empty)"
        />
      </label>

      <label className="flex flex-col gap-1.5">
        <span className={label}>{`${title} path (project override)`}</span>
        <input
          className={st.projectPathDisabled ? fieldRo : field}
          value={form.projPath}
          onChange={(e) => onChange({ projPath: e.target.value })}
          placeholder="(uses the user default when empty)"
          disabled={st.projectPathDisabled}
          readOnly={st.projectPathDisabled}
          tabIndex={st.projectPathDisabled ? -1 : undefined}
        />
        {st.showCommitNote && (
          <span className="text-[11px] text-tx-3">
            A project-level {title} override requires a committed <code>keitora.toml</code>. Commit
            a file below, or set the user-level default above.
          </span>
        )}
      </label>

      <div className="flex flex-col gap-1.5">
        <span className={label}>Dispatch ritual</span>
        {dispatchEditor && st.showEditor ? (
          dispatchEditor(form.steps, (steps) => onChange({ steps }))
        ) : (
          <p className="text-[11px] text-tx-3">
            {form.steps.length === 0
              ? "Using Keitora's built-in dispatch prompt."
              : `${form.steps.length} step(s) configured.`}
          </p>
        )}
      </div>

      <div className="flex items-center justify-between gap-3">
        <span className={label}>{copy.autonomyLabel}</span>
        <button
          className={form.autonomy ? btnPrimary : btnGhost}
          onClick={onToggleAutonomy}
          disabled={st.autonomyDisabled}
        >
          {form.autonomy ? "Enabled" : "Disabled"}
        </button>
      </div>
      <div className="flex items-start gap-1.5 text-[11px] text-tx-3">
        <AlertTriangle size={13} className="mt-0.5 flex-none" />
        <span>
          Launches {title} with <code>{copy.flag}</code>, {copy.warning}
        </span>
      </div>

      {projectAutonomy && (
        <div className="flex items-start gap-1.5 text-[11px] text-busy">
          <AlertTriangle size={13} className="mt-0.5 flex-none" />
          <span>
            This project's <code>keitora.toml</code> {projectForcesCopy(cli)}, so its sessions
            launch with the flag regardless of the toggle above.
          </span>
        </div>
      )}
    </section>
  );
}
