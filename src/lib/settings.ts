// Typed bridge to the settings backend commands (see src-tauri get_settings /
// save_project_settings / save_user_settings / remove_project). Response structs
// serialize snake_case (matching ProjectSummary); command args are passed
// camelCase and mapped to snake_case Rust params by Tauri v2.

import { invoke } from "@tauri-apps/api/core";
import type { CliKind, DiffBase } from "./ipc";

/** One provider's project-level settings. */
export interface SettingsProjectProvider {
  /** Project-level `[<provider>].path` override; null → user default. */
  path: string | null;
  /** Project-level `[<provider>].autonomy`; true → forces the flag on. */
  autonomy: boolean;
  /** Project-level dispatch ritual; [] → built-in prompt. */
  dispatch_steps: string[];
}

/** One provider's user-level settings. */
export interface SettingsUserProvider {
  /** User-level `[<provider>].path` default. */
  path: string | null;
  /** User-level `[<provider>].autonomy` default. */
  autonomy: boolean;
}

/** Editable + display fields of the active project's keitora.toml. */
export interface SettingsProject {
  name: string; // read-only (registry key)
  root: string; // read-only
  db_path: string; // read-only (resolved absolute path)
  worktrees_dir: string | null; // editable; null → ~/.keitora/<name> default
  archive_days: number; // editable (>= 1)
  log_retention_days: number; // editable (>= 1)
  /** Project-level settings for every provider. */
  providers: Record<CliKind, SettingsProjectProvider>;
  /** Project default diff-view base (`[project].diff_base`); null → Auto. Per-worktree
   *  overrides (set from the diff header) win over this. */
  diff_base: DiffBase | null;
  worktrees: string[]; // read-only (managed from the rail)
  /** True when a keitora.toml exists in the project root (committed file). False
   *  when settings live only in the user registry (~/.keitora/keitora.toml). */
  has_committed_file: boolean;
}

/** One registered project in the user registry. */
export interface SettingsUserProject {
  name: string;
  root: string;
  active: boolean;
}

/** Editable fields of the user keitora.toml plus the project registry. */
export interface SettingsUser {
  active: string | null; // active project name
  projects: SettingsUserProject[];
  /** User-level settings for every provider. */
  providers: Record<CliKind, SettingsUserProvider>;
  /** User-level default CLI, falling back to `"claude"` when unset. */
  default_cli: CliKind;
  /** Per provider: a configured path resolves, or its executable was found on this
   *  machine. Gates each provider group's autonomy and dispatch controls. */
  available: Record<CliKind, boolean>;
  /** Whether the app checks for updates by itself (`[updates] auto_check`). */
  auto_check_updates: boolean;
  /** Hours between agent CLI update checks (`[updates] agent_check_hours`); 0 = app start only. */
  agent_update_check_hours: number;
}

/** Everything the settings drawer needs in one round-trip. */
export interface SettingsSnapshot {
  project: SettingsProject;
  user: SettingsUser;
}

/** Load a full snapshot of the active project + user config. */
export function getSettings(): Promise<SettingsSnapshot> {
  return invoke<SettingsSnapshot>("get_settings");
}

/**
 * Write the editable project fields to the active project's keitora.toml.
 * Pass the full current form state, every provider included — a null
 * worktreesDir or provider path clears that key; an empty provider
 * dispatchSteps clears that provider's dispatch ritual. Emits `project://changed`.
 */
export function saveProjectSettings(input: {
  archiveDays: number;
  logRetentionDays: number;
  worktreesDir: string | null;
  providers: Record<CliKind, { path: string | null; dispatchSteps: string[] }>;
  /** Project default diff base; null → Auto. */
  diffBase: DiffBase | null;
}): Promise<void> {
  return invoke("save_project_settings", input);
}

/**
 * Write every provider's user-level path and the default CLI. Null/empty
 * paths clear that provider's override. Emits `project://changed`.
 */
export function saveUserSettings(input: {
  providers: Record<CliKind, string | null>;
  defaultCli: CliKind;
}): Promise<void> {
  return invoke("save_user_settings", input);
}

/** Remove a project from the user registry (clears active if it matched). */
export function removeProject(name: string): Promise<void> {
  return invoke("remove_project", { name });
}

/**
 * Persist the user-level `[<provider>] autonomy` flag for the given CLI. Persist-only (no `project://changed` emit): the value is
 * read when the next session is spawned, so the drawer prompts the user to
 * restart the daemon for running sessions to pick it up.
 */
export function setSkipPermissions(cli: CliKind, enabled: boolean): Promise<void> {
  return invoke("set_skip_permissions", { cli, enabled });
}

/**
 * Persist the user-level `[updates] auto_check` flag. Persist-only (no
 * `project://changed` emit).
 */
export function setUpdateAutoCheck(enabled: boolean): Promise<void> {
  return invoke("set_update_auto_check", { enabled });
}

/**
 * Persist the user-level `[updates] agent_check_hours` (0 = app start only; the backend
 * clamps other values to 1..=168). Persist-only; the scheduler reads it at its next tick.
 */
export function setAgentUpdateCheckHours(hours: number): Promise<void> {
  return invoke("set_agent_update_check_hours", { hours });
}

/**
 * Write the active project's current settings to a keitora.toml in the project
 * root, creating or overwriting it. Has no effect if the project has no root
 * path. Emits `project://changed` on success.
 */
export function commitSettingsToProject(): Promise<void> {
  return invoke("commit_settings_to_project");
}

// Re-export the active-project switch (lives in ipc.ts) for the user section.
export { setActiveProject } from "./ipc";
