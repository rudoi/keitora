// Pure logic behind the update banner and the Updates settings section. State comes from the
// backend's `update_status` snapshot plus `updater://status` / `updater://progress` events;
// components stay thin and render what these helpers return.

import { formatBytes } from "./format";
import type {
  UpdatePhase,
  UpdateProgress,
  UpdateProgressEvent,
  UpdateSnapshot,
  UpdateStatus,
  UpdateStatusEvent,
} from "./ipc";

export interface UpdatesState {
  status: UpdateStatus;
  phase: UpdatePhase;
  progress: UpdateProgress | null;
  /** A user-requested check is in flight. */
  checking: boolean;
  /** An install (download then apply) is in flight from this window. */
  installing: boolean;
  /** Last install failure; cleared when the next install starts. */
  error: string | null;
  /** Version whose banner the user dismissed; a different version shows again. */
  dismissedVersion: string | null;
  currentVersion: string;
}

export const INITIAL_UPDATES: UpdatesState = {
  status: { type: "unchecked" },
  phase: "idle",
  progress: null,
  checking: false,
  installing: false,
  error: null,
  dismissedVersion: null,
  currentVersion: "",
};

export type UpdatesAction =
  | { type: "seed"; snapshot: UpdateSnapshot }
  | { type: "status"; event: UpdateStatusEvent }
  | { type: "progress"; event: UpdateProgressEvent }
  | { type: "check_start" }
  | { type: "check_end" }
  | { type: "install_start" }
  | { type: "install_end"; error: string | null }
  | { type: "dismiss" };

export function updatesReducer(state: UpdatesState, action: UpdatesAction): UpdatesState {
  switch (action.type) {
    case "seed": {
      const { snapshot } = action;
      // Events that already landed are fresher than the snapshot; status and phase are judged
      // separately, since progress events arrive often during a download and status rarely.
      const statusFresh = state.status.type === "unchecked";
      const phaseFresh = state.phase === "idle" && state.progress === null;
      return {
        ...state,
        currentVersion: snapshot.current_version,
        ...(statusFresh ? { status: snapshot.status } : {}),
        ...(phaseFresh ? { phase: snapshot.phase, progress: snapshot.progress } : {}),
      };
    }
    case "status": {
      const { manual, ...status } = action.event;
      return {
        ...state,
        status,
        // A user asking again wants to see the result, so a past dismissal no longer applies.
        dismissedVersion: manual && status.type === "available" ? null : state.dismissedVersion,
      };
    }
    case "progress": {
      const { phase, downloaded, total } = action.event;
      const same =
        phase === state.phase &&
        (phase === "idle"
          ? state.progress === null
          : state.progress?.downloaded === downloaded && state.progress?.total === total);
      if (same) return state;
      return {
        ...state,
        phase,
        progress: phase === "idle" ? null : { downloaded, total },
      };
    }
    case "check_start":
      return { ...state, checking: true };
    case "check_end":
      return { ...state, checking: false };
    case "install_start":
      return { ...state, installing: true, error: null };
    case "install_end":
      return { ...state, installing: false, error: action.error };
    case "dismiss":
      return state.status.type === "available"
        ? { ...state, dismissedVersion: state.status.version }
        : state;
  }
}

/** True while a download or install is running, here or reported by the backend. */
/** Whether an install is underway. A `ready` phase alone is not busy: it is also where the
 * backend parks a downloaded update after a failed apply, so it can be retried. */
export function isBusy(state: UpdatesState): boolean {
  return state.installing || state.phase === "downloading" || state.phase === "applying";
}

export interface BannerModel {
  version: string;
  text: string;
  tone: "info" | "error";
  buttonLabel: string;
  busy: boolean;
  canDismiss: boolean;
  releaseUrl: string;
}

/** The banner to show, or null when there is nothing to announce. */
export function bannerOf(state: UpdatesState): BannerModel | null {
  const { status } = state;
  if (status.type !== "available" || state.dismissedVersion === status.version) return null;
  const busy = isBusy(state);
  const failed = state.error !== null && !busy;
  return {
    version: status.version,
    text: failed
      ? `Update failed: ${state.error}`
      : `Keitora ${status.version} is available. Your sessions keep running.`,
    tone: failed ? "error" : "info",
    buttonLabel: installLabel(state),
    busy,
    canDismiss: !busy,
    releaseUrl: status.release_url,
  };
}

/** Label for the install button in both the banner and the settings section. */
export function installLabel(state: UpdatesState): string {
  if (state.phase === "applying" || (state.phase === "ready" && state.installing)) {
    return "Restarting…";
  }
  if (state.phase === "downloading" || state.installing) {
    return state.progress
      ? formatProgress(state.progress.downloaded, state.progress.total)
      : "Downloading…";
  }
  return state.error !== null ? "Retry" : "Install & restart";
}

/** The toast for a status event; only user-requested checks toast. */
export function toastFor(event: UpdateStatusEvent): string | null {
  if (!event.manual) return null;
  switch (event.type) {
    case "up_to_date":
      return "Keitora is up to date.";
    case "failed":
      return `Update check failed: ${event.message}`;
    case "unsupported":
      return "Updates aren't available on this platform.";
    default:
      return null;
  }
}

/** One line for the Updates settings section. */
export function statusLine(status: UpdateStatus): string {
  switch (status.type) {
    case "unchecked":
      return "Not checked yet";
    case "up_to_date":
      return "Up to date";
    case "available":
      return `Version ${status.version} is available`;
    case "failed":
      return `Last check failed: ${status.message}`;
    case "unsupported":
      return "Updates aren't available on this platform.";
  }
}

export function formatProgress(downloaded: number, total: number | null): string {
  if (total !== null && total > 0) {
    const pct = Math.min(100, Math.floor((downloaded / total) * 100));
    return `Downloading ${pct}%…`;
  }
  return `Downloading ${formatBytes(downloaded)}…`;
}

export interface InstallDeps {
  download: () => Promise<unknown>;
  apply: () => Promise<unknown>;
}

export type InstallResult = { ok: true } | { ok: false; error: string };

/** Download, then apply. A download failure skips the apply; any failure is returned, not thrown. */
export async function installUpdate(deps: InstallDeps): Promise<InstallResult> {
  try {
    await deps.download();
    await deps.apply();
    return { ok: true };
  } catch (err) {
    return { ok: false, error: errorText(err) };
  }
}

export function errorText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
