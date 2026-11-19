// Pure logic behind the agent CLI update banner, dialog and pane hints. State comes from
// the backend's `agent_updates_status` snapshot plus the `agent-updates://…` events;
// components stay thin and render what these helpers return.

import type {
  AgentUpdateEntry,
  AgentUpdateState,
  AgentUpdatesSnapshot,
  CliKind,
  InstallKind,
  RecoveryInfo,
  RunOutcome,
  UpdateSession,
} from "./ipc";
import type { Status } from "./paneStatus";

/** Banner order: codex first, then claude. */
export const AGENT_CLIS: readonly CliKind[] = ["codex", "claude"];

/** "Codex" / "Claude", for sentence-initial copy. */
export function cliTitle(cli: CliKind): string {
  return cli === "codex" ? "Codex" : "Claude";
}

/** The key a pane is tracked by in the provider's sets. */
export function sessionKey(project: string, worktree: string): string {
  return `${project}/${worktree}`;
}

/** `keys` as session keys. */
export function sessionKeys(keys: readonly UpdateSession[]): string[] {
  return keys.map((k) => sessionKey(k.project, k.worktree));
}

const UPDATE_PROMPT_TOKEN = "AgentUpdatePrompt";

/** The CLI named by the backend's `AgentUpdatePrompt: <cli>` spawn error, or null for any
 *  other error. The token names the CLI the launch ran, which a transfer can make differ
 *  from the worktree's. */
export function updatePromptCliOf(error: string | null | undefined): CliKind | null {
  if (!error?.startsWith(UPDATE_PROMPT_TOKEN)) return null;
  const named = error.slice(UPDATE_PROMPT_TOKEN.length).replace(/^:\s*/, "").trim();
  return named === "claude" || named === "codex" ? named : null;
}

/** "Later" choices for this run, by CLI: the version the user put off. A newer release
 *  shows the banner again. */
export type LaterMap = Partial<Record<CliKind, string>>;

export interface BannerRow {
  cli: CliKind;
  latest: string;
  text: string;
  /** The update can run from Keitora (there is a command). */
  canRun: boolean;
  /** Copyable command for the user's own terminal when Keitora can't run one. */
  guidance: string | null;
  /** Shown with the guidance when Keitora can't run the update itself. */
  guidanceNote: string | null;
}

/** One row per CLI with an update available that the user neither skipped nor put off
 *  until later this run. Codex first. */
export function bannerRows(snapshot: AgentUpdatesSnapshot | null, later: LaterMap): BannerRow[] {
  if (!snapshot) return [];
  const rows: BannerRow[] = [];
  for (const cli of AGENT_CLIS) {
    const entry = snapshot[cli];
    const { state } = entry;
    if (state.type !== "available") continue;
    if (later[cli] === state.latest) continue;
    const canRun = state.command !== null;
    rows.push({
      cli,
      latest: state.latest,
      text: `${cliTitle(cli)} ${state.latest} is available (you have ${state.installed})`,
      canRun,
      guidance: canRun ? null : (entry.guidance ?? null),
      guidanceNote: canRun ? null : guidanceNote(cli, entry.install_kind),
    });
  }
  return rows;
}

function guidanceNote(cli: CliKind, kind: InstallKind | null): string {
  if (kind === "unknown" || kind === null) {
    return `Keitora couldn't tell how ${cli} was installed. Run this in your own terminal:`;
  }
  return "Run this in your own terminal:";
}

/** Why the banner's Update button is disabled, or null when it can run. */
export function updateDisabledReason(
  row: BannerRow,
  recovery: RecoveryInfo["phase"],
): string | null {
  if (!row.canRun) return `Keitora can't update this ${row.cli} install itself`;
  if (recovery !== "Healthy") return "Wait for session recovery to finish";
  return null;
}

export interface OutcomeCopy {
  text: string;
  tone: "info" | "error";
  /** Offer "restart {cli} sessions". */
  canRestart: boolean;
}

/** What an update run's outcome says, in the dialog and (for `updated`) the banner. */
export function runOutcomeCopy(cli: CliKind, outcome: RunOutcome): OutcomeCopy {
  const name = cliTitle(cli);
  switch (outcome.type) {
    case "updated":
      return {
        text: `${name} updated to ${outcome.to} — running sessions keep the old version until restarted`,
        tone: "info",
        canRestart: true,
      };
    case "unchanged":
      return {
        text: outcome.version
          ? `The update finished but ${cli} is still ${outcome.version}`
          : `The update finished but ${cli}'s version didn't change`,
        tone: "info",
        canRestart: false,
      };
    case "failed":
      return {
        text: outcome.hint
          ? `The update failed (exit ${outcome.code}): ${outcome.hint}`
          : `The update failed (exit ${outcome.code})`,
        tone: "error",
        canRestart: false,
      };
    case "timed_out":
      return {
        text: "The update never reported back — check the terminal",
        tone: "error",
        canRestart: false,
      };
  }
}

/**
 * Whether an update first asks to stop the `live` sessions. On Windows a running CLI holds
 * its executable open, so the update would fail while sessions run. Elsewhere, it runs.
 */
export function shouldAskToStop(windows: boolean, live: readonly UpdateSession[]): boolean {
  return windows && live.length > 0;
}

/**
 * Whether closing the update dialog would interrupt a run: the command is running in its
 * pane and no outcome has come back. Closing ends the pane, so every way out (the close
 * button, a click outside) asks first; Escape belongs to the terminal meanwhile.
 */
export function closeInterruptsRun(step: string, hasOutcome: boolean): boolean {
  return step === "running" && !hasOutcome;
}

/** The confirm shown before closing interrupts a run. */
export function interruptRunCopy(cli: CliKind): string {
  return `The ${cli} update is still running — closing will interrupt it.`;
}

/** "stop 1 session and update" / "stop N sessions and update". */
export function stopSessionsLabel(count: number): string {
  return count === 1 ? "stop 1 session and update" : `stop ${count} sessions and update`;
}

/** The pane-level hint while a launched pane sits on its CLI's update screen. */
export function updatePromptHint(cli: CliKind): string {
  return `${cli} is asking to update — answer in the terminal, or use keitora's updater`;
}

/** The ended overlay of a pane stopped so its CLI could be updated. */
export function stoppedForUpdateCopy(cli: CliKind): string {
  return `stopped for the ${cli} update`;
}

const STATUS_LINES: Record<Exclude<AgentUpdateState["type"], "available" | "skipped">, string> = {
  unchecked: "not checked yet",
  not_installed: "not installed",
  unknown: "couldn't check (offline?)",
  up_to_date: "up to date",
};

/** One line for the settings section, per CLI. */
export function entryStatusLine(entry: AgentUpdateEntry): string {
  const { state } = entry;
  if (state.type === "available" || state.type === "skipped") {
    return `${state.latest} ${state.type}`;
  }
  return STATUS_LINES[state.type];
}

const INSTALL_KIND_LABELS: Record<InstallKind, string> = {
  npm: "npm",
  brew: "homebrew",
  native: "native installer",
  unknown: "unknown",
};

/** The install-kind label for the settings section. */
export function installKindLabel(kind: InstallKind | null): string {
  return kind === null ? "—" : INSTALL_KIND_LABELS[kind];
}

/** Parse the interval input: a whole number of hours, 0..=168, or null when invalid. */
export function parseCheckHours(text: string): number | null {
  const t = text.trim();
  if (!/^\d+$/.test(t)) return null;
  const n = Number(t);
  return n <= 168 ? n : null;
}

/** What a pane does about a pending restart onto an updated CLI:
 *  - `restart`: kill and relaunch now (the user asked while this pane was mounted);
 *  - `notice`: offer the restart, never forcing it (the mark was set before this pane
 *    mounted, so the session it attached to may be busy with work the user returned to);
 *  - `drop`: forget the mark (a terminated session stays down);
 *  - `none`: nothing yet (no mark, a transfer owns the pane, or it hasn't settled). */
export type PendingRestartAction = "none" | "restart" | "notice" | "drop";

/** Settled statuses a restart asked for while the pane was mounted acts on. */
const RESTARTABLE_FOR_UPDATE: ReadonlySet<Status> = new Set<Status>([
  "running",
  "spawn-failed",
  "ended",
  "ended-while-away",
  "lost",
]);

/** The pane's [`PendingRestartAction`]. `markedBeforeMount`: the mark was already set
 *  when the pane mounted. Such a pane never restarts by itself: attached to a running
 *  session it shows a notice; with no session, its own next spawn runs the new version
 *  and settles the mark. */
export function pendingRestartAction({
  pending,
  markedBeforeMount,
  status,
  transferring,
}: {
  pending: boolean;
  markedBeforeMount: boolean;
  status: Status;
  transferring: boolean;
}): PendingRestartAction {
  if (!pending || transferring) return "none";
  if (status === "terminated") return "drop";
  if (markedBeforeMount) return status === "running" ? "notice" : "none";
  return RESTARTABLE_FOR_UPDATE.has(status) ? "restart" : "none";
}

/** The pending-restart notice: "restart to use Codex 0.163.0". */
export function restartNoticeCopy(cli: CliKind, version: string | null): string {
  return version
    ? `restart to use ${cliTitle(cli)} ${version}`
    : `restart to use the updated ${cliTitle(cli)}`;
}

/** `map` with each of `keys` set to `cli`. */
export function withCliKeys(
  map: ReadonlyMap<string, CliKind>,
  keys: readonly string[],
  cli: CliKind,
): ReadonlyMap<string, CliKind> {
  if (keys.every((k) => map.get(k) === cli)) return map;
  const next = new Map(map);
  for (const k of keys) next.set(k, cli);
  return next;
}

/** `map` without `key` (the same map when it wasn't there). */
export function withoutCliKey(
  map: ReadonlyMap<string, CliKind>,
  key: string,
): ReadonlyMap<string, CliKind> {
  if (!map.has(key)) return map;
  const next = new Map(map);
  next.delete(key);
  return next;
}
