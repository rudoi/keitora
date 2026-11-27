// The settings drawer's per-provider form state and its pure seed/save mapping. The
// project and user sections seed separately, so refreshing one never discards the
// other's unsaved edits.

import type { CliKind } from "./ipc";
import { byCli, CLI_KINDS } from "./providers";
import type { SettingsSnapshot } from "./settings";

/** One provider's editable fields in the drawer. */
export interface ProviderForm {
  /** User-level path default ("" = none). */
  userPath: string;
  /** Project-level path override ("" = none). */
  projPath: string;
  /** Project-level dispatch steps, as edited (untrimmed). */
  steps: string[];
  /** User-level autonomy flag. */
  autonomy: boolean;
}

export type ProviderForms = Record<CliKind, ProviderForm>;

function mapForms(f: (cli: CliKind) => ProviderForm): ProviderForms {
  return byCli(f);
}

/** Every provider with empty fields. */
export function emptyForms(): ProviderForms {
  return mapForms(() => ({ userPath: "", projPath: "", steps: [], autonomy: false }));
}

/** `forms` with only the project fields (`projPath`, `steps`) seeded from `s`. */
export function seedProjectForms(forms: ProviderForms, s: SettingsSnapshot): ProviderForms {
  return mapForms((c) => ({
    ...forms[c],
    projPath: s.project.providers[c].path ?? "",
    steps: s.project.providers[c].dispatch_steps,
  }));
}

/** `forms` with only the user fields (`userPath`, `autonomy`) seeded from `s`. */
export function seedUserForms(forms: ProviderForms, s: SettingsSnapshot): ProviderForms {
  return mapForms((c) => ({
    ...forms[c],
    userPath: s.user.providers[c].path ?? "",
    autonomy: s.user.providers[c].autonomy,
  }));
}

/** Trimmed `v`, or null when it is blank. */
function nz(v: string): string | null {
  const t = v.trim();
  return t === "" ? null : t;
}

/** The per-provider payloads of `saveProjectSettings` and `saveUserSettings`: blank paths
 *  become null, steps are trimmed and blank ones dropped. */
export function buildProviderSaves(forms: ProviderForms): {
  project: Record<CliKind, { path: string | null; dispatchSteps: string[] }>;
  user: Record<CliKind, string | null>;
} {
  const project = byCli((c) => ({
    path: nz(forms[c].projPath),
    dispatchSteps: forms[c].steps.map((s) => s.trim()).filter((s) => s !== ""),
  }));
  const user = byCli((c) => nz(forms[c].userPath));
  return { project, user };
}

/** One default-agent button per provider: unavailable ones are shown but disabled, and
 *  the selected one stays pressed even when unavailable. */
export function defaultCliButtons(
  available: Record<CliKind, boolean>,
  selected: CliKind,
): { cli: CliKind; disabled: boolean; pressed: boolean }[] {
  return CLI_KINDS.map((cli) => ({
    cli,
    disabled: !available[cli],
    pressed: cli === selected,
  }));
}
