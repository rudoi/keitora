// Typed bridge to the settings backend commands (see src-tauri get_settings /
// save_project_settings / save_user_settings / remove_project). Response structs
// serialize snake_case (matching ProjectSummary); command args are passed
// camelCase and mapped to snake_case Rust params by Tauri v2.

import { invoke } from "@tauri-apps/api/core";
import type { CliKind, DiffBase } from "./ipc";

/** Editable + display fields of the active project's keitora.toml. */
export interface SettingsProject {
  name: string; // read-only (registry key)
  root: string; // read-only
  db_path: string; // read-only (resolved absolute path)
  worktrees_dir: string | null; // editable; null → ~/.keitora/<name> default
  archive_days: number; // editable (>= 1)
  log_retention_days: number; // editable (>= 1)
  claude_path: string | null; // editable project override; null → user default
  /** Project-level `[claude] skip_permissions`; true → forces the flag on. */
  skip_permissions: boolean;
  dispatch_steps: string[]; // editable ordered ritual; [] → built-in prompt
  /** Project-level `[codex].path` override. Mirrors `claude_path`. */
  codex_path: string | null;
  /** Project-level `[codex].autonomy`. Mirrors `skip_permissions`. */
  codex_autonomy: boolean;
  /** Project-level `[dispatch].codex_steps` override. Mirrors `dispatch_steps`. */
  codex_dispatch_steps: string[];
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
  claude_path: string | null; // editable user-level default
  active: string | null; // active project name
  projects: SettingsUserProject[];
  /**
   * Whether interactive Claude sessions launch with
   * `--dangerously-skip-permissions` (persisted in `[claude] skip_permissions`).
   */
  skip_permissions: boolean;
  /** User-level `[codex].path` default. Mirrors `claude_path`. */
  codex_path: string | null;
  /** User-level `[codex].autonomy` default. Mirrors `skip_permissions`. */
  codex_autonomy: boolean;
  /** User-level default CLI, falling back to `"claude"` when unset. */
  default_cli: CliKind;
  /** Whether a `codex` binary was found on this machine. Gates the
   *  Codex group's enabled state in the drawer. */
  codex_available: boolean;
  /** Whether the app checks for updates by itself (`[updates] auto_check`). */
  auto_check_updates: boolean;
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
 * Pass the full current form state — null worktrees_dir/claudePath/codexPath
 * clears that key; an empty dispatchSteps/codexDispatchSteps removes the
 * corresponding [dispatch] key. Emits `project://changed`.
 */
export function saveProjectSettings(input: {
  archiveDays: number;
  logRetentionDays: number;
  worktreesDir: string | null;
  claudePath: string | null;
  codexPath: string | null;
  dispatchSteps: string[];
  codexDispatchSteps: string[];
  /** Project default diff base; null → Auto. */
  diffBase: DiffBase | null;
}): Promise<void> {
  return invoke("save_project_settings", input);
}

/**
 * Write the user-level claude/codex paths and default CLI. Null/empty
 * paths clear that CLI's override. Emits `project://changed`.
 */
export function saveUserSettings(input: {
  claudePath: string | null;
  codexPath: string | null;
  defaultCli: CliKind;
}): Promise<void> {
  return invoke("save_user_settings", input);
}

/** Remove a project from the user registry (clears active if it matched). */
export function removeProject(name: string): Promise<void> {
  return invoke("remove_project", { name });
}

/**
 * Persist the user-level `[claude] autonomy` / `[codex] autonomy` flag for the
 * given CLI. Persist-only (no `project://changed` emit): the value is
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
 * Write the active project's current settings to a keitora.toml in the project
 * root, creating or overwriting it. Has no effect if the project has no root
 * path. Emits `project://changed` on success.
 */
export function commitSettingsToProject(): Promise<void> {
  return invoke("commit_settings_to_project");
}

// Re-export the active-project switch (lives in ipc.ts) for the user section.
export { setActiveProject } from "./ipc";
