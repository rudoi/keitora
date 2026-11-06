// The CLI-transfer phase machine: which worktrees are mid-transfer, what phase each is in,
// and what the app must do as the backend's signals arrive (restart the pane, inject the
// handoff summary, toast, tell the backend the transfer settled). Pure: the reducer returns
// effects instead of running them, and the controller owns the single backstop timer, so
// every decision is unit-testable without React or Tauri.

import type { CliKind, PtyResumeEvent, TransferOutcome, TransferProgress } from "./ipc";
import type { Status } from "./paneStatus";
import type { RestartSignal } from "./resumeNotice";
import type { SessionState } from "./sessions";

/** How long a transfer restart may take to report its spawn before the app gives up waiting. */
export const RESTARTING_BACKSTOP_MS = 120_000;

/** The transfer map key: worktree names repeat across projects, so both are part of it. */
export const transferKey = (project: string, worktree: string): string => `${project}/${worktree}`;

/** What the transfer produced; decides the toast shown once the spawn settles. */
export type TransferResult =
  | { kind: "imported" }
  /** `reasonText` null: the strict launch fell back to fresh after a good import. */
  | { kind: "fresh"; reasonText: string | null }
  /** Codex to Claude. */
  | { kind: "done" };

export interface TransferEntry {
  project: string;
  worktree: string;
  to: CliKind;
  phase: "importing" | "restarting";
  /** Epoch ms at `begin`; the overlay's elapsed clock. */
  startedAt: number;
  /** Latest progress event; advisory and only meaningful while importing. */
  progress: TransferProgress | null;
  /** The user clicked Cancel (or the backend reported it is cancelling). */
  cancelling: boolean;
  /** Null while importing. */
  result: TransferResult | null;
}

/** Keyed by {@link transferKey}. */
export type TransferMap = ReadonlyMap<string, TransferEntry>;

export type SpawnResult = { ok: true } | { ok: false; error: string };

export type TransferEvent =
  | { type: "begin"; project: string; worktree: string; to: CliKind; now: number }
  | { type: "progress"; key: string; progress: TransferProgress }
  | { type: "cancel_requested"; key: string }
  | { type: "outcome"; key: string; outcome: TransferOutcome }
  /** `transferWorktree` rejected: nothing changed. */
  | { type: "failed"; key: string; error: string }
  | { type: "spawn_settled"; key: string; result: SpawnResult }
  | { type: "resume_notice"; key: string; notice: PtyResumeEvent }
  | { type: "backstop"; key: string };

type Diagnostics = { project: string; worktree: string };

export type TransferEffect =
  /** Bump the worktree's restart signal with `transfer: true`. */
  | { kind: "restart"; project: string; worktree: string; fresh: boolean }
  | { kind: "inject_handoff"; project: string; worktree: string }
  | { kind: "toast"; message: string; diagnostics: Diagnostics | null }
  /** Tell the backend the transfer settled and drop the resume listener. */
  | { kind: "settle"; project: string; worktree: string };

export interface TransferStep {
  map: TransferMap;
  effects: TransferEffect[];
}

const noop = (map: TransferMap): TransferStep => ({ map, effects: [] });

function withEntry(map: TransferMap, key: string, entry: TransferEntry): TransferMap {
  const next = new Map(map);
  next.set(key, entry);
  return next;
}

function without(map: TransferMap, key: string): TransferMap {
  const next = new Map(map);
  next.delete(key);
  return next;
}

/** Diagnostics are offered only for Codex-bound transfers (the import is what leaves a trail). */
function codexDiagnostics(e: TransferEntry): Diagnostics | null {
  return e.to === "codex" ? { project: e.project, worktree: e.worktree } : null;
}

/** Remove the entry, toast `message`, and settle the transfer. */
function finish(
  map: TransferMap,
  key: string,
  e: TransferEntry,
  message: string,
  diagnostics: Diagnostics | null,
): TransferStep {
  return {
    map: without(map, key),
    effects: [
      { kind: "toast", message, diagnostics },
      { kind: "settle", project: e.project, worktree: e.worktree },
    ],
  };
}

/**
 * The next transfer map and the effects to run for `event`. Any event for an unknown key,
 * or for an entry in the wrong phase, is a no-op that returns the same map object.
 */
export function transferReducer(map: TransferMap, event: TransferEvent): TransferStep {
  if (event.type === "begin") {
    const key = transferKey(event.project, event.worktree);
    if (map.has(key)) return noop(map);
    return {
      map: withEntry(map, key, {
        project: event.project,
        worktree: event.worktree,
        to: event.to,
        phase: "importing",
        startedAt: event.now,
        progress: null,
        cancelling: false,
        result: null,
      }),
      effects: [],
    };
  }

  const key = event.key;
  const e = map.get(key);
  if (e === undefined) return noop(map);
  const target = { project: e.project, worktree: e.worktree };

  switch (event.type) {
    case "progress":
      if (e.phase !== "importing") return noop(map);
      return {
        map: withEntry(map, key, {
          ...e,
          progress: event.progress,
          cancelling: e.cancelling || event.progress.phase === "cancelling",
        }),
        effects: [],
      };

    case "cancel_requested":
      if (e.phase !== "importing" || e.to !== "codex" || e.cancelling) return noop(map);
      return { map: withEntry(map, key, { ...e, cancelling: true }), effects: [] };

    case "outcome": {
      if (e.phase !== "importing") return noop(map);
      const o = event.outcome;
      switch (o.kind) {
        case "imported":
          return {
            map: withEntry(map, key, { ...e, phase: "restarting", result: { kind: "imported" } }),
            effects: [{ kind: "restart", ...target, fresh: false }],
          };
        case "fresh":
          return {
            map: withEntry(map, key, {
              ...e,
              phase: "restarting",
              result: { kind: "fresh", reasonText: o.reasonText },
            }),
            effects: [{ kind: "restart", ...target, fresh: true }],
          };
        case "done":
          // The handoff is injected only once the restarted Claude reported its spawn
          // (see `spawn_settled`), so it never lands in the session being replaced.
          return {
            map: withEntry(map, key, { ...e, phase: "restarting", result: { kind: "done" } }),
            effects: [{ kind: "restart", ...target, fresh: false }],
          };
        case "cancelled":
          return finish(map, key, e, "Transfer cancelled.", null);
      }
      return noop(map);
    }

    case "failed":
      if (e.phase !== "importing") return noop(map);
      return finish(map, key, e, event.error, codexDiagnostics(e));

    case "spawn_settled":
      if (e.phase !== "restarting") return noop(map);
      if (event.result.ok) {
        const diagnostics =
          e.result?.kind === "fresh" || e.result?.kind === "imported" ? target : null;
        const step = finish(map, key, e, resultToast(e), diagnostics);
        if (e.result?.kind === "done") step.effects.push({ kind: "inject_handoff", ...target });
        return step;
      }
      return finish(map, key, e, event.result.error, codexDiagnostics(e));

    case "resume_notice":
      if (
        e.phase !== "restarting" ||
        e.to !== "codex" ||
        event.notice.fell_back !== true ||
        e.result?.kind !== "imported"
      ) {
        return noop(map);
      }
      return {
        map: withEntry(map, key, { ...e, result: { kind: "fresh", reasonText: null } }),
        effects: [],
      };

    case "backstop":
      if (e.phase !== "restarting") return noop(map);
      return finish(map, key, e, backstopMessage(e), codexDiagnostics(e));
  }
  return noop(map);
}

/** The CLI's display name. */
export function cliLabel(cli: CliKind): "Codex" | "Claude" {
  return cli === "codex" ? "Codex" : "Claude";
}

/** The toast for a transfer whose restart spawned successfully. */
export function resultToast(entry: TransferEntry): string {
  if (entry.result?.kind === "fresh") return freshToastMessage(entry.result.reasonText);
  return `Transferred ${entry.worktree} to ${cliLabel(entry.to)}.`;
}

/** The toast for a transfer that ended on a fresh Codex session. */
export function freshToastMessage(reasonText: string | null): string {
  if (reasonText === null) {
    return "Codex couldn't resume the imported conversation. Started a fresh Codex session.";
  }
  return `Couldn't carry the Claude history over (${reasonText}). Started a fresh Codex session.`;
}

/** The toast for a transfer restart that never reported its spawn. */
export function backstopMessage(entry: TransferEntry): string {
  const secs = Math.round(RESTARTING_BACKSTOP_MS / 1000);
  return `${cliLabel(entry.to)} did not report that it started within ${secs} seconds. Check the ${entry.worktree} pane.`;
}

/** The overlay's line for the latest progress event (`null`: nothing reported yet). */
export function progressLabel(p: TransferProgress | null): string {
  if (p === null) return "Preparing transfer…";
  switch (p.phase) {
    case "preflight":
      return "Checking the Claude session…";
    case "starting_codex":
      return "Starting Codex…";
    case "initializing":
      return "Initializing Codex…";
    case "importing":
      return "Importing the conversation…";
    case "awaiting_completion":
      return "Waiting for Codex to finish the import…";
    case "reading_ledger":
      return "Checking the imported session…";
    case "persisting":
      return "Saving the transfer…";
    case "cancelling":
      return "Cancelling…";
  }
  return "Preparing transfer…";
}

/** `ms` as `m:ss`; negative durations read as zero. */
export function formatElapsed(ms: number): string {
  const s = Math.floor(Math.max(ms, 0) / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/**
 * Whether the overlay's Cancel button is live: only a Codex-bound import that has not
 * started persisting and is not already cancelling can still be cancelled.
 */
export function canCancel(e: TransferEntry): boolean {
  return (
    e.phase === "importing" &&
    e.to === "codex" &&
    !e.cancelling &&
    e.progress?.phase !== "persisting" &&
    e.progress?.phase !== "cancelling"
  );
}

/** Whether terminal input is blocked: only while importing (typing would race the import). */
export function blocksInput(e: TransferEntry | null | undefined): boolean {
  return e?.phase === "importing";
}

/** Whether the agent is mid-turn (working, or blocked on a permission prompt). */
export function isMidTurn(state: SessionState | undefined): boolean {
  return state === "busy" || state === "waiting";
}

/** Appended to the Claude to Codex confirm body when Claude is mid-turn. */
export const MID_TURN_NOTE =
  "Claude is in the middle of a turn. That in-flight turn will not be carried over.";

/**
 * The spawn-failed screen's copy. The strict launch's own failure string already names
 * the CLI and the attempts, so it is shown as-is; any other error goes to the detail line.
 */
export function spawnFailedCopy(
  cli: CliKind,
  error: string | null,
): { message: string; detail: string | null } {
  const generic = `Failed to launch ${cli} — is it on PATH?`;
  if (error === null || error === "") return { message: generic, detail: null };
  if (error.startsWith(`${cli} did not start`)) return { message: error, detail: null };
  return { message: generic, detail: error };
}

/**
 * How the spawn-failed screen's buttons restart. After a failed transfer launch they repeat
 * the transfer flavour, so a worktree that came over fresh is never resumed with the
 * default `resume --last`; any other failure restarts as before.
 */
export function restartAfterFailure(
  failure: { transfer: boolean; fresh: boolean } | null,
  choice: "kill_restart" | "start_fresh",
): { fresh: boolean; transfer: boolean } {
  if (failure === null || !failure.transfer) {
    return { fresh: choice === "start_fresh", transfer: false };
  }
  return { fresh: choice === "start_fresh" ? true : failure.fresh, transfer: true };
}

/** Whether a pane in `status` asks before killing its session (a plain restart or a terminate). */
export function restartNeedsConfirm(status: Status): boolean {
  return (
    status !== "ended" &&
    status !== "ended-while-away" &&
    status !== "lost" &&
    status !== "spawn-failed" &&
    status !== "terminated"
  );
}

/**
 * What a pane does with a restart signal: a transfer restart was already confirmed in the
 * rail, so it never prompts; a plain restart prompts while the session is still live.
 */
export function restartPlan(
  signal: Pick<RestartSignal, "fresh" | "transfer">,
  status: Status,
): { confirm: boolean; fresh: boolean; transfer: boolean } {
  return {
    confirm: !signal.transfer && restartNeedsConfirm(status),
    fresh: signal.fresh,
    transfer: signal.transfer,
  };
}

/**
 * Whether a pane acts on a restart or terminate signal. While a transfer owns the pane
 * (an entry is present, importing or restarting) only the transfer's own restart is
 * honoured: a plain restart or terminate would kill the session the transfer is reading
 * or race the restart it drives.
 */
export function shouldHonourSignal(
  signal: { transfer: boolean },
  transfer: TransferEntry | null | undefined,
): boolean {
  return signal.transfer || transfer == null;
}

/** Whether a new transfer may start for `key` (none is already in flight for it). */
export function shouldStartTransfer(map: TransferMap, key: string): boolean {
  return !map.has(key);
}

export interface TransferController {
  getMap(): TransferMap;
  dispatch(event: TransferEvent): void;
  /** Clears timers only; the controller stays usable (a StrictMode remount keeps it). */
  dispose(): void;
}

/** Whether two entries render the same: phase, cancelling, and progress phase + message. */
function sameVisibleEntry(a: TransferEntry | undefined, b: TransferEntry | undefined): boolean {
  return (
    a !== undefined &&
    b !== undefined &&
    a.phase === b.phase &&
    a.cancelling === b.cancelling &&
    a.progress?.phase === b.progress?.phase &&
    a.progress?.message === b.progress?.message
  );
}

/**
 * Owns the transfer map and the backstop timer. Each dispatch reduces, stores the new map,
 * syncs timers (one per `restarting` entry; never for `importing`), notifies `onChange` if
 * the map changed, then runs the effects in order. A progress event that changes nothing
 * the overlay shows keeps the current map, so it does not re-render.
 */
export function createTransferController(opts: {
  run: (effect: TransferEffect) => void;
  onChange: (map: TransferMap) => void;
  backstopMs?: number;
}): TransferController {
  const backstopMs = opts.backstopMs ?? RESTARTING_BACKSTOP_MS;
  let map: TransferMap = new Map();
  const timers = new Map<string, ReturnType<typeof setTimeout>>();

  const syncTimers = () => {
    for (const [key, timer] of timers) {
      if (map.get(key)?.phase !== "restarting") {
        clearTimeout(timer);
        timers.delete(key);
      }
    }
    for (const [key, entry] of map) {
      if (entry.phase === "restarting" && !timers.has(key)) {
        timers.set(
          key,
          setTimeout(() => {
            timers.delete(key);
            dispatch({ type: "backstop", key });
          }, backstopMs),
        );
      }
    }
  };

  const dispatch = (event: TransferEvent) => {
    const step = transferReducer(map, event);
    const unseen =
      event.type === "progress" && sameVisibleEntry(map.get(event.key), step.map.get(event.key));
    const changed = step.map !== map && !unseen;
    if (changed) map = step.map;
    syncTimers();
    if (changed) opts.onChange(map);
    for (const effect of step.effects) opts.run(effect);
  };

  return {
    getMap: () => map,
    dispatch,
    dispose: () => {
      for (const timer of timers.values()) clearTimeout(timer);
      timers.clear();
    },
  };
}
