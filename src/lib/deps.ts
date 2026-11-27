// Pure logic behind the dependency surfaces: the onboarding step, the rail card, the
// upgrade dialog and the Settings "Dependencies" section. State comes from the backend's
// `deps_status` snapshot plus `deps://status` / `deps://progress` events; components stay
// thin and render what these helpers return.

import type {
  DepsFailureKind,
  DepsProgressEvent,
  DepsRunState,
  DepsSnapshot,
  DepsStage,
  DepsTier,
  DoctorAction,
  DoctorRow,
  FloorState,
  ManualRoute,
} from "./ipc";
import { formatProgress } from "./updates";

/** The oldest herdr Keitora works with (matches the backend's floor). */
export const HERDR_FLOOR = "0.9.5";

export interface DepsState {
  /** `null` until the first snapshot arrives. */
  snapshot: DepsSnapshot | null;
  /** Latest progress for the running install, if any. */
  progress: DepsProgressEvent | null;
  /** The first `deps_status` answered (or failed), so callers can stop waiting. */
  loaded: boolean;
}

export const INITIAL_DEPS: DepsState = { snapshot: null, progress: null, loaded: false };

export type DepsAction =
  | { type: "status"; snapshot: DepsSnapshot }
  | { type: "progress"; event: DepsProgressEvent }
  | { type: "load_failed" };

/** Snapshots replace; progress folds into the running run (its stage, bytes, last line). */
export function depsReducer(state: DepsState, action: DepsAction): DepsState {
  switch (action.type) {
    case "status": {
      const { snapshot } = action;
      const run = snapshot.run;
      // Progress only outlives a status update while it belongs to the run still going.
      const keep =
        run.type === "running" && state.progress !== null && state.progress.run_id === run.run_id;
      return { snapshot, progress: keep ? state.progress : null, loaded: true };
    }
    case "progress": {
      const { event } = action;
      const snap = state.snapshot;
      if (snap === null) return { ...state, progress: event };
      const run = snap.run;
      if (run.type !== "running" || run.run_id !== event.run_id) {
        // A late event from a finished run; the terminal status already said how it ended.
        return state;
      }
      // Terminal stages wait for the status event that carries the outcome.
      const stage = event.stage === "done" || event.stage === "failed" ? run.stage : event.stage;
      return {
        ...state,
        snapshot: { ...snap, run: { ...run, stage } },
        progress: event,
      };
    }
    case "load_failed":
      return { ...state, loaded: true };
  }
}

/** One short label per stage. */
export function stageLabel(stage: DepsStage): string {
  switch (stage) {
    case "checking":
      return "Checking for a release…";
    case "downloading":
      return "Downloading…";
    case "installing":
      return "Installing…";
    case "verifying":
      return "Verifying…";
    case "restarting":
      return "Restarting herdr…";
    case "done":
      return "Done";
    case "failed":
      return "Failed";
  }
}

type RunningRun = Extract<DepsRunState, { type: "running" }>;

/** The running run's label, with a percentage or size while downloading. */
export function runLabel(run: RunningRun, p: DepsProgressEvent | null): string {
  if (run.stage === "downloading" && p && p.run_id === run.run_id && p.downloaded > 0) {
    return formatProgress(p.downloaded, p.total);
  }
  return stageLabel(run.stage);
}

/** herdr is installed but too old or speaks the wrong protocol: something must change. */
export function isBelowFloor(floor: FloorState): boolean {
  return floor.type === "too_old" || floor.type === "protocol_mismatch";
}

/** herdr speaks a newer protocol than Keitora: upgrading herdr can't help. */
export function isNewerProtocol(floor: FloorState): boolean {
  return floor.type === "protocol_mismatch" && floor.newer;
}

/** The fix for a herdr ahead of Keitora, in one sentence. */
function newerProtocolCopy(found: number | string, want: number): string {
  return `herdr speaks protocol ${found}, newer than this Keitora (${want}). Update Keitora, or install a Keitora-managed herdr that speaks protocol ${want}.`;
}

export interface RunFailure {
  text: string;
  message: string;
  tryDirect: boolean;
}

/** The running run's label and the failed run's copy, shared by every surface. */
export function runView(
  snapshot: DepsSnapshot | null,
  progress: DepsProgressEvent | null,
): { running: string | null; failure: RunFailure | null } {
  const run = snapshot?.run;
  return {
    running: run?.type === "running" ? runLabel(run, progress) : null,
    failure:
      run?.type === "failed"
        ? {
            text: failureCopy(run.kind),
            message: run.message,
            tryDirect: run.next === "try_direct",
          }
        : null,
  };
}

/** What went wrong, in one plain sentence per failure kind. */
export function failureCopy(kind: DepsFailureKind): string {
  switch (kind) {
    case "offline":
      return "You appear to be offline. Check your connection and try again.";
    case "network":
      return "The download failed. A proxy, firewall or TLS problem may be blocking it.";
    case "policy_blocked":
      return "A system policy blocked the install script.";
    case "checksum":
      return "The download didn't match its checksum, so nothing was installed.";
    case "blocked_by_av":
      return "Antivirus software blocked herdr. Allow it, then try again.";
    case "timeout":
      return "The install took too long and was stopped.";
    case "incomplete":
      return "The install didn't finish: herdr isn't where it should be.";
    case "too_old_after_install":
      return "The installed herdr is still older than Keitora needs.";
    case "no_compatible_release":
      return "No herdr release works with this Keitora on this machine yet.";
    case "other":
      return "The install failed.";
  }
}

/** What a run is doing, from its kind. */
function runVerb(upgrade: boolean): string {
  return upgrade ? "Upgrading herdr" : "Installing herdr";
}

// ── Rail card ─────────────────────────────────────────────────────────────────

type CardAction = "install" | "upgrade" | "restart_to_finish" | "try_direct";

export interface CardModel {
  text: string;
  tone: "info" | "warn" | "error";
  /** The button beside the text, if any. */
  action: { kind: CardAction; label: string } | null;
  /** An install is running: the button area shows progress instead. */
  busy: boolean;
}

/** The rail's dependency line, or `null` when herdr is fine and nothing is running. */
export function cardModel(
  snapshot: DepsSnapshot | null,
  progress: DepsProgressEvent | null = null,
): CardModel | null {
  if (snapshot === null) return null;
  const { run, floor } = snapshot;
  if (run.type === "running") {
    const label = runLabel(run, progress);
    return { text: `${runVerb(run.upgrade)}: ${label}`, tone: "info", action: null, busy: true };
  }
  const { failure } = runView(snapshot, progress);
  if (failure) {
    return {
      text: failure.text,
      tone: "error",
      action: failure.tryDirect
        ? { kind: "try_direct", label: "Try direct download" }
        : snapshot.herdr_installed
          ? null
          : { kind: "install", label: "Retry" },
      busy: false,
    };
  }
  if (run.type === "done" && run.restart_pending) {
    return {
      text: "herdr is installed and starts once recovery settles.",
      tone: "info",
      action: null,
      busy: false,
    };
  }
  if (!snapshot.herdr_installed) {
    return {
      text: "herdr is not installed.",
      tone: "error",
      action: snapshot.platform !== null ? { kind: "install", label: "Install herdr" } : null,
      busy: false,
    };
  }
  switch (floor.type) {
    case "too_old":
      return {
        text: `herdr ${floor.found} is older than Keitora needs (${HERDR_FLOOR}+).`,
        tone: "warn",
        action: { kind: "upgrade", label: "Upgrade" },
        busy: false,
      };
    case "protocol_mismatch":
      return floor.newer
        ? {
            text: `herdr speaks protocol ${floor.found}, newer than this Keitora (${floor.want}). Panes can't open.`,
            tone: "error",
            action: { kind: "upgrade", label: "Resolve" },
            busy: false,
          }
        : {
            text: `herdr speaks protocol ${floor.found}; Keitora needs ${floor.want}. Panes can't open.`,
            tone: "error",
            action: { kind: "upgrade", label: "Upgrade" },
            busy: false,
          };
    case "restart_to_finish":
      return {
        text: `herdr ${floor.binary} is installed; ${floor.server} is still running.`,
        tone: "info",
        action: { kind: "restart_to_finish", label: "Restart to finish" },
        busy: false,
      };
    default:
      return null;
  }
}

// ── Upgrade dialog ────────────────────────────────────────────────────────────

export interface DialogModel {
  title: string;
  body: string;
  /** No "Later" and no snooze: panes can't open until this is fixed. */
  hard: boolean;
  /** The package-manager instruction when herdr was installed outside Keitora. */
  external: string | null;
  /** The main button: Upgrade, or a managed copy for an External install. */
  primary: { label: string; tier: DepsTier | null };
  /** Running sessions an upgrade restarts (0 = no confirmation needed). */
  liveSessions: number;
  /** Progress while an install runs (buttons disabled). */
  running: string | null;
  failure: RunFailure | null;
}

/**
 * The upgrade dialog, or `null` when it should be closed. Driven by the backend's prompt;
 * `forced` (an Upgrade button elsewhere) opens it from the floor when the prompt was
 * snoozed. Never shown while the upgrade only needs a restart.
 */
export function dialogModel(
  snapshot: DepsSnapshot | null,
  forced = false,
  progress: DepsProgressEvent | null = null,
): DialogModel | null {
  if (snapshot === null) return null;
  const { floor } = snapshot;
  if (floor.type === "restart_to_finish") return null;
  let kind: "too_old" | "protocol_mismatch";
  let found: string;
  let hard: boolean;
  if (snapshot.prompt) {
    ({ kind, found, hard } = snapshot.prompt);
  } else if (forced && floor.type === "too_old") {
    kind = "too_old";
    found = floor.found;
    hard = false;
  } else if (forced && floor.type === "protocol_mismatch") {
    kind = "protocol_mismatch";
    found = String(floor.found);
    hard = true;
  } else {
    return null;
  }
  const want = floor.type === "protocol_mismatch" ? floor.want : null;
  if (kind === "protocol_mismatch" && isNewerProtocol(floor) && want !== null) {
    // Upgrading herdr can't fix this, and neither can the package manager's upgrade.
    return {
      title: "herdr is newer than Keitora",
      body: `${newerProtocolCopy(found, want)} Panes can't open until then.`,
      hard,
      external: null,
      primary: { label: "Install a compatible herdr", tier: "direct" },
      liveSessions: snapshot.live_sessions,
      ...runView(snapshot, progress),
    };
  }
  const body =
    kind === "too_old"
      ? `herdr ${found} is older than Keitora needs (${HERDR_FLOOR}+). Upgrade now?`
      : `herdr speaks protocol ${found}${want !== null ? `; Keitora needs ${want}` : ""}. Panes can't open until herdr is upgraded.`;
  const external = snapshot.route?.type === "external" ? snapshot.route.instruction : null;
  return {
    title: kind === "too_old" ? "Upgrade herdr?" : "herdr needs an upgrade",
    body,
    hard,
    external,
    primary: external
      ? { label: "Install a Keitora-managed copy instead", tier: "direct" }
      : { label: "Upgrade", tier: null },
    liveSessions: snapshot.live_sessions,
    ...runView(snapshot, progress),
  };
}

/** What the user picked in "Replace existing herdr?". */
export type OverwriteChoice = "replace" | "keep" | "cancel";

/**
 * The install a "Replace existing herdr?" answer starts, or `null` for none. Replace lets
 * the script overwrite it; Keep installs a separate Keitora-managed copy; dismissing the
 * dialog (Esc, outside click, Cancel) installs nothing.
 */
export function overwriteInstall(
  choice: OverwriteChoice,
  tier: DepsTier | null,
): { tier: DepsTier | null; confirmOverwrite: boolean } | null {
  switch (choice) {
    case "replace":
      return { tier, confirmOverwrite: true };
    case "keep":
      return { tier: "direct", confirmOverwrite: false };
    case "cancel":
      return null;
  }
}

/** The confirmation before an upgrade restarts live sessions. */
export function upgradeConfirmCopy(liveSessions: number): {
  title: string;
  description: string;
  confirmLabel: string;
} {
  const sessions = liveSessions === 1 ? "1 running session" : `${liveSessions} running sessions`;
  return {
    title: "Upgrade herdr?",
    description: `Upgrading restarts herdr and interrupts ${sessions} mid-turn. Their agents resume afterwards with their conversation transcript kept.`,
    confirmLabel: "Upgrade",
  };
}

// ── Onboarding step ───────────────────────────────────────────────────────────

export type StepModel =
  | { kind: "hidden" }
  | { kind: "offer"; title: string; body: string; canInstall: boolean; manual: ManualRoute }
  | { kind: "running"; title: string; label: string; line: string | null }
  | {
      kind: "failed";
      title: string;
      body: string;
      message: string;
      tryDirect: boolean;
      manual: ManualRoute;
    }
  | { kind: "done"; title: string; body: string };

/** Whether first-run onboarding should start on the dependency step. */
export function needsDepsStep(snapshot: DepsSnapshot | null): boolean {
  if (snapshot === null) return false;
  return (
    !snapshot.herdr_installed || isBelowFloor(snapshot.floor) || snapshot.run.type === "running"
  );
}

/**
 * Whether onboarding shows the dependency step: `null` while the first snapshot is
 * loading. Once any snapshot needed it (`seen`), it stays on so it doesn't vanish
 * mid-install or the moment herdr turns up; a snapshot that needs it later (the floor
 * is often `unknown` at first) turns it on too, until the user moved past it.
 */
export function depsStepVisible(opts: {
  loaded: boolean;
  needs: boolean;
  seen: boolean;
  dismissed: boolean;
}): boolean | null {
  if (!opts.loaded) return null;
  if (opts.dismissed) return false;
  return opts.seen || opts.needs;
}

/** The onboarding step's content. */
export function stepModel(
  snapshot: DepsSnapshot | null,
  progress: DepsProgressEvent | null = null,
): StepModel {
  if (snapshot === null) return { kind: "hidden" };
  const { run, floor } = snapshot;
  if (run.type === "running") {
    return {
      kind: "running",
      title: runVerb(run.upgrade),
      label: runLabel(run, progress),
      line: progress && progress.run_id === run.run_id ? progress.line : null,
    };
  }
  const { failure } = runView(snapshot, progress);
  if (failure) {
    return {
      kind: "failed",
      title: "herdr didn't install",
      body: failure.text,
      message: failure.message,
      tryDirect: failure.tryDirect,
      manual: snapshot.manual,
    };
  }
  if (snapshot.herdr_installed && !isBelowFloor(floor)) {
    return run.type === "done"
      ? { kind: "done", title: "herdr is ready", body: "Next, add a project." }
      : { kind: "hidden" };
  }
  const canInstall = snapshot.platform !== null;
  const newer = snapshot.herdr_installed && floor.type === "protocol_mismatch" && floor.newer;
  const body = !snapshot.herdr_installed
    ? "Keitora runs your agents inside herdr, a terminal multiplexer. It isn't installed yet."
    : floor.type === "too_old"
      ? `herdr ${floor.found} is older than Keitora needs (${HERDR_FLOOR}+).`
      : floor.type === "protocol_mismatch" && floor.newer
        ? newerProtocolCopy(floor.found, floor.want)
        : "This herdr speaks a protocol Keitora doesn't, so panes can't open.";
  return {
    kind: "offer",
    title: newer
      ? "Install a compatible herdr"
      : snapshot.herdr_installed
        ? "Upgrade herdr"
        : "Install herdr",
    body: canInstall ? body : `${body} Install it with one of the commands below.`,
    canInstall,
    manual: snapshot.manual,
  };
}

// ── Settings section ──────────────────────────────────────────────────────────

export interface SettingsRow {
  tool: DoctorRow["tool"];
  /** Version, or why there isn't one. */
  detail: string;
  tone: "ok" | "warn" | "error" | "muted";
  path: string | null;
  action: { kind: DoctorAction | "restart_to_finish"; label: string } | null;
  /** Install command for a missing agent CLI Keitora does not install itself. */
  hint: string | null;
}

export interface SettingsModel {
  rows: SettingsRow[];
  /** The pinned herdr, shown with "Clear (use discovery)". */
  pinned: string | null;
  running: string | null;
  failure: RunFailure | null;
  /** Manual commands when Keitora can't install on this machine. */
  manual: ManualRoute | null;
}

const ACTION_LABEL = { install: "Install", upgrade: "Upgrade", browse: "Browse…" } as const;

function rowDetail(row: DoctorRow): Pick<SettingsRow, "detail" | "tone"> {
  switch (row.status) {
    case "ok":
      return { detail: row.version ?? "found", tone: "ok" };
    case "missing":
      return { detail: "not found", tone: row.tool === "herdr" ? "error" : "muted" };
    case "too_old":
      return {
        detail: `${row.version ?? "?"}, needs ${row.floor ?? HERDR_FLOOR}+`,
        tone: "warn",
      };
    case "unknown":
      return { detail: "version unknown", tone: "muted" };
  }
}

/** Rows and status for the Settings section. */
export function settingsModel(
  snapshot: DepsSnapshot | null,
  rows: DoctorRow[],
  progress: DepsProgressEvent | null = null,
): SettingsModel {
  const restart = snapshot?.floor.type === "restart_to_finish" ? snapshot.floor : null;
  const out: SettingsRow[] = rows.map((row) => {
    const base = { tool: row.tool, path: row.path, hint: row.hint, ...rowDetail(row) };
    if (row.tool === "herdr" && restart) {
      return {
        ...base,
        detail: `${restart.binary} installed, ${restart.server} running`,
        tone: "warn",
        action: { kind: "restart_to_finish", label: "Restart to finish" },
      };
    }
    if (row.tool === "herdr" && snapshot?.floor.type === "protocol_mismatch") {
      const { found, want, newer } = snapshot.floor;
      return {
        ...base,
        detail: newer
          ? `protocol ${found}, newer than Keitora's ${want}`
          : `protocol ${found}, needs ${want}`,
        tone: "error",
        action: { kind: "upgrade", label: newer ? "Resolve" : ACTION_LABEL.upgrade },
      };
    }
    // Keitora can't install on an unsupported platform; offer Browse instead.
    const action = row.action === "install" && snapshot?.platform === null ? "browse" : row.action;
    return { ...base, action: action ? { kind: action, label: ACTION_LABEL[action] } : null };
  });
  return {
    rows: out,
    pinned: snapshot?.pinned_bin ?? null,
    ...runView(snapshot, progress),
    manual:
      snapshot && (snapshot.platform === null || snapshot.manual.reason !== null)
        ? snapshot.manual
        : null,
  };
}
