import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CliKind, TransferOutcome, TransferProgress } from "./ipc";
import {
  blocksInput,
  canCancel,
  createTransferController,
  formatElapsed,
  freshToastMessage,
  isMidTurn,
  midTurnNote,
  progressLabel,
  RESTARTING_BACKSTOP_MS,
  restartAfterFailure,
  restartPlan,
  shouldHonourSignal,
  shouldStartTransfer,
  spawnFailedCopy,
  type TransferEffect,
  type TransferEntry,
  type TransferEvent,
  type TransferMap,
  transferKey,
  transferReducer,
} from "./transferPhase";

const KEY = transferKey("proj", "alpha");
const DIAG = { project: "proj", worktree: "alpha" };

const begin = (to: "codex" | "claude" = "codex", project = "proj", worktree = "alpha") =>
  ({ type: "begin", project, worktree, to, now: 1000 }) as const;

function progress(phase: TransferProgress["phase"], over: Partial<TransferProgress> = {}) {
  return {
    project: "proj",
    worktree: "alpha",
    phase,
    message: null,
    elapsedMs: 0,
    deadlineMs: 150_000,
    ...over,
  } satisfies TransferProgress;
}

const imported: TransferOutcome = { kind: "imported", threadId: "thread-a" };
const fresh: TransferOutcome = {
  kind: "fresh",
  reason: { kind: "other", message: "m", stderrTail: "" },
  reasonText: "Codex did not start in time",
};
const done: TransferOutcome = { kind: "done" };

/** The outcome report `finish` emits after the toast and settle. */
const report = (ok: boolean, message: string) =>
  ({ kind: "report", ...DIAG, ok, message }) as const;

/** Fold `events` from an empty map, collecting every effect. */
function run(events: TransferEvent[], start: TransferMap = new Map()) {
  let map = start;
  const effects: TransferEffect[] = [];
  for (const ev of events) {
    const step = transferReducer(map, ev);
    map = step.map;
    effects.push(...step.effects);
  }
  return { map, effects };
}

function entry(over: Partial<TransferEntry> = {}): TransferEntry {
  return {
    project: "proj",
    worktree: "alpha",
    to: "codex",
    mode: "transfer",
    phase: "importing",
    startedAt: 0,
    progress: null,
    cancelling: false,
    result: null,
    ...over,
  };
}

describe("transferReducer", () => {
  it("begin adds an importing entry with no effects", () => {
    const { map, effects } = run([begin()]);
    expect(map.size).toBe(1);
    expect(map.get(KEY)).toEqual({
      project: "proj",
      worktree: "alpha",
      to: "codex",
      mode: "transfer",
      phase: "importing",
      startedAt: 1000,
      progress: null,
      cancelling: false,
      result: null,
    });
    expect(effects).toEqual([]);
  });

  it("begin on an existing key is a no-op", () => {
    const { map } = run([begin()]);
    const step = transferReducer(map, begin("claude"));
    expect(step.map).toBe(map);
    expect(step.effects).toEqual([]);
  });

  it("progress updates an importing entry", () => {
    const { map } = run([
      begin(),
      { type: "progress", key: KEY, progress: progress("preflight") },
      { type: "progress", key: KEY, progress: progress("importing", { message: "50%" }) },
    ]);
    expect(map.get(KEY)?.progress).toEqual(progress("importing", { message: "50%" }));
    expect(map.get(KEY)?.cancelling).toBe(false);
  });

  it("progress with phase cancelling sets cancelling", () => {
    const { map } = run([
      begin(),
      { type: "progress", key: KEY, progress: progress("cancelling") },
    ]);
    expect(map.get(KEY)?.cancelling).toBe(true);
  });

  it("progress for a restarting entry or unknown key is ignored", () => {
    const { map } = run([begin(), { type: "outcome", key: KEY, outcome: imported }]);
    const a = transferReducer(map, { type: "progress", key: KEY, progress: progress("importing") });
    expect(a.map).toBe(map);
    const other = transferKey("proj", "bravo");
    const b = transferReducer(map, {
      type: "progress",
      key: other,
      progress: progress("importing"),
    });
    expect(b.map).toBe(map);
  });

  it("cancel_requested marks a codex import cancelling", () => {
    const { map, effects } = run([begin(), { type: "cancel_requested", key: KEY }]);
    expect(map.get(KEY)?.cancelling).toBe(true);
    expect(effects).toEqual([]);
  });

  it("cancel_requested is ignored for to=claude, when already cancelling, and when restarting", () => {
    const claude = run([begin("claude")]).map;
    expect(transferReducer(claude, { type: "cancel_requested", key: KEY }).map).toBe(claude);
    const cancelling = run([begin(), { type: "cancel_requested", key: KEY }]).map;
    expect(transferReducer(cancelling, { type: "cancel_requested", key: KEY }).map).toBe(
      cancelling,
    );
    const restarting = run([begin(), { type: "outcome", key: KEY, outcome: imported }]).map;
    expect(transferReducer(restarting, { type: "cancel_requested", key: KEY }).map).toBe(
      restarting,
    );
  });

  it("outcome imported moves to restarting and asks for a non-fresh restart", () => {
    const { map, effects } = run([begin(), { type: "outcome", key: KEY, outcome: imported }]);
    expect(map.get(KEY)?.phase).toBe("restarting");
    expect(map.get(KEY)?.result).toEqual({ kind: "imported" });
    expect(effects).toEqual([{ kind: "restart", ...DIAG, fresh: false }]);
  });

  it("outcome fresh moves to restarting and asks for a fresh restart", () => {
    const { map, effects } = run([begin(), { type: "outcome", key: KEY, outcome: fresh }]);
    expect(map.get(KEY)?.phase).toBe("restarting");
    expect(map.get(KEY)?.result).toEqual({
      kind: "fresh",
      reasonText: "Codex did not start in time",
    });
    expect(effects).toEqual([{ kind: "restart", ...DIAG, fresh: true }]);
  });

  it("outcome done asks for a non-fresh restart only (the handoff waits for the spawn)", () => {
    const { map, effects } = run([begin("claude"), { type: "outcome", key: KEY, outcome: done }]);
    expect(map.get(KEY)?.result).toEqual({ kind: "done" });
    expect(effects).toEqual([{ kind: "restart", ...DIAG, fresh: false }]);
  });

  it('outcome cancelled removes the entry, toasts "Transfer cancelled." and settles', () => {
    const { map, effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: { kind: "cancelled" } },
    ]);
    expect(map.has(KEY)).toBe(false);
    expect(effects).toEqual([
      { kind: "toast", message: "Transfer cancelled.", diagnostics: null },
      { kind: "settle", ...DIAG },
      report(false, "Transfer cancelled."),
    ]);
  });

  it("failed removes the entry, toasts the error verbatim and settles", () => {
    const codex = run([begin(), { type: "failed", key: KEY, error: "boom" }]);
    expect(codex.map.has(KEY)).toBe(false);
    expect(codex.effects).toEqual([
      { kind: "toast", message: "boom", diagnostics: DIAG },
      { kind: "settle", ...DIAG },
      report(false, "boom"),
    ]);
    const claude = run([begin("claude"), { type: "failed", key: KEY, error: "boom" }]);
    expect(claude.effects).toEqual([
      { kind: "toast", message: "boom", diagnostics: null },
      { kind: "settle", ...DIAG },
      report(false, "boom"),
    ]);
  });

  it('spawn_settled ok after imported toasts "Transferred alpha to Codex." and settles', () => {
    const { map, effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: imported },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(map.has(KEY)).toBe(false);
    expect(effects.slice(1)).toEqual([
      { kind: "toast", message: "Transferred alpha to Codex.", diagnostics: DIAG },
      { kind: "settle", ...DIAG },
      report(true, "Transferred alpha to Codex."),
    ]);
  });

  it('spawn_settled ok after done toasts "Transferred alpha to Claude." and injects the handoff', () => {
    const { map, effects } = run([
      begin("claude"),
      { type: "outcome", key: KEY, outcome: done },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(map.has(KEY)).toBe(false);
    expect(effects.slice(1)).toEqual([
      { kind: "toast", message: "Transferred alpha to Claude.", diagnostics: null },
      { kind: "settle", ...DIAG },
      report(true, "Transferred alpha to Claude."),
      { kind: "inject_handoff", ...DIAG },
    ]);
  });

  it("spawn_settled not ok after done never injects the handoff", () => {
    const { effects } = run([
      begin("claude"),
      { type: "outcome", key: KEY, outcome: done },
      { type: "spawn_settled", key: KEY, result: { ok: false, error: "no claude" } },
    ]);
    expect(effects.some((e) => e.kind === "inject_handoff")).toBe(false);
    // Nor does a backstop.
    const late = run([
      begin("claude"),
      { type: "outcome", key: KEY, outcome: done },
      { type: "backstop", key: KEY },
    ]);
    expect(late.effects.some((e) => e.kind === "inject_handoff")).toBe(false);
  });

  it("spawn_settled ok after fresh toasts the fresh copy with diagnostics", () => {
    const { effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: fresh },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(effects.slice(1)).toEqual([
      {
        kind: "toast",
        message:
          "Couldn't carry the Claude history over (Codex did not start in time). Started a fresh Codex session.",
        diagnostics: DIAG,
      },
      { kind: "settle", ...DIAG },
      report(
        true,
        "Couldn't carry the Claude history over (Codex did not start in time). Started a fresh Codex session.",
      ),
    ]);
  });

  it("spawn_settled not ok removes the entry and toasts the error verbatim with diagnostics for codex", () => {
    const err = "codex did not start: x (tried: fresh)";
    const { map, effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: imported },
      { type: "spawn_settled", key: KEY, result: { ok: false, error: err } },
    ]);
    expect(map.has(KEY)).toBe(false);
    expect(effects.slice(1)).toEqual([
      { kind: "toast", message: err, diagnostics: DIAG },
      { kind: "settle", ...DIAG },
      report(false, err),
    ]);
  });

  it("resume_notice fell_back during restarting turns an imported result into the fallback fresh toast", () => {
    const { effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: imported },
      { type: "resume_notice", key: KEY, notice: { outcome: "fresh", fell_back: true } },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(effects.slice(1)).toEqual([
      {
        kind: "toast",
        message: "Codex couldn't resume the imported conversation. Started a fresh Codex session.",
        diagnostics: DIAG,
      },
      { kind: "settle", ...DIAG },
      report(
        true,
        "Codex couldn't resume the imported conversation. Started a fresh Codex session.",
      ),
    ]);
  });

  it("resume_notice fell_back after a fresh outcome does not toast twice", () => {
    const { effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: fresh },
      { type: "resume_notice", key: KEY, notice: { outcome: "fresh", fell_back: true } },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    const toasts = effects.filter((e) => e.kind === "toast");
    expect(toasts).toHaveLength(1);
    expect(toasts[0]).toMatchObject({ message: freshToastMessage("Codex did not start in time") });
  });

  it("resume_notice without fell_back, for a claude transfer, or for an importing entry is ignored", () => {
    const notFellBack = run([begin(), { type: "outcome", key: KEY, outcome: imported }]).map;
    expect(
      transferReducer(notFellBack, {
        type: "resume_notice",
        key: KEY,
        notice: { outcome: "resumed", fell_back: false },
      }).map,
    ).toBe(notFellBack);
    const claude = run([begin("claude"), { type: "outcome", key: KEY, outcome: done }]).map;
    expect(
      transferReducer(claude, {
        type: "resume_notice",
        key: KEY,
        notice: { outcome: "fresh", fell_back: true },
      }).map,
    ).toBe(claude);
    const importing = run([begin()]).map;
    expect(
      transferReducer(importing, {
        type: "resume_notice",
        key: KEY,
        notice: { outcome: "fresh", fell_back: true },
      }).map,
    ).toBe(importing);
  });

  it("a second outcome or a second spawn_settled is a no-op", () => {
    const restarting = run([begin(), { type: "outcome", key: KEY, outcome: imported }]).map;
    const again = transferReducer(restarting, { type: "outcome", key: KEY, outcome: fresh });
    expect(again.map).toBe(restarting);
    expect(again.effects).toEqual([]);
    const settled = run([{ type: "spawn_settled", key: KEY, result: { ok: true } }], restarting);
    const twice = transferReducer(settled.map, {
      type: "spawn_settled",
      key: KEY,
      result: { ok: true },
    });
    expect(twice.map).toBe(settled.map);
    expect(twice.effects).toEqual([]);
  });

  it("spawn_settled while importing is ignored", () => {
    const importing = run([begin()]).map;
    const step = transferReducer(importing, {
      type: "spawn_settled",
      key: KEY,
      result: { ok: true },
    });
    expect(step.map).toBe(importing);
    expect(step.effects).toEqual([]);
  });

  it("outcome while restarting is ignored", () => {
    const restarting = run([begin(), { type: "outcome", key: KEY, outcome: fresh }]).map;
    const step = transferReducer(restarting, { type: "outcome", key: KEY, outcome: imported });
    expect(step.map).toBe(restarting);
    expect(step.effects).toEqual([]);
  });

  it("backstop on a restarting entry toasts the timeout copy and settles", () => {
    const { map, effects } = run([
      begin(),
      { type: "outcome", key: KEY, outcome: imported },
      { type: "backstop", key: KEY },
    ]);
    expect(map.has(KEY)).toBe(false);
    expect(effects.slice(1)).toEqual([
      {
        kind: "toast",
        message: "Codex did not report that it started within 120 seconds. Check the alpha pane.",
        diagnostics: DIAG,
      },
      { kind: "settle", ...DIAG },
      report(
        false,
        "Codex did not report that it started within 120 seconds. Check the alpha pane.",
      ),
    ]);
  });

  it("report is ok only for a restart that spawned, and every terminal path reports once", () => {
    const reports = (events: TransferEvent[]) =>
      run(events).effects.filter((e) => e.kind === "report");
    const ok = reports([
      begin(),
      { type: "outcome", key: KEY, outcome: imported },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(ok).toEqual([report(true, "Transferred alpha to Codex.")]);
    const failures: TransferEvent[][] = [
      [begin(), { type: "failed", key: KEY, error: "boom" }],
      [begin(), { type: "outcome", key: KEY, outcome: { kind: "cancelled" } }],
      [
        begin(),
        { type: "outcome", key: KEY, outcome: imported },
        { type: "spawn_settled", key: KEY, result: { ok: false, error: "no codex" } },
      ],
      [begin(), { type: "outcome", key: KEY, outcome: imported }, { type: "backstop", key: KEY }],
    ];
    for (const events of failures) {
      const r = reports(events);
      expect(r).toHaveLength(1);
      expect(r[0]).toMatchObject({ ok: false });
    }
    // A step that does not end the transfer reports nothing.
    expect(reports([begin(), { type: "outcome", key: KEY, outcome: imported }])).toEqual([]);
  });

  it("backstop on an importing entry is ignored", () => {
    const importing = run([begin()]).map;
    const step = transferReducer(importing, { type: "backstop", key: KEY });
    expect(step.map).toBe(importing);
    expect(step.effects).toEqual([]);
  });

  it("same worktree name in two projects is isolated", () => {
    const otherKey = transferKey("other-proj", "alpha");
    const { map, effects } = run([
      begin("codex", "proj", "alpha"),
      begin("codex", "other-proj", "alpha"),
      { type: "outcome", key: KEY, outcome: imported },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(map.has(KEY)).toBe(false);
    expect(map.get(otherKey)?.phase).toBe("importing");
    expect(effects.every((e) => !("project" in e) || e.project === "proj")).toBe(true);
  });
});

describe("transferReducer: switch", () => {
  const beginSwitch = (to: CliKind = "antigravity") =>
    ({ type: "begin", project: "proj", worktree: "alpha", to, now: 1000, mode: "switch" }) as const;
  const switched: TransferOutcome = { kind: "switched", from: "claude", to: "antigravity" };

  it("begin switch enters restarting with no result", () => {
    const { map, effects } = run([beginSwitch()]);
    expect(map.get(KEY)).toEqual({
      project: "proj",
      worktree: "alpha",
      to: "antigravity",
      mode: "switch",
      phase: "restarting",
      startedAt: 1000,
      progress: null,
      cancelling: false,
      result: null,
    });
    expect(effects).toEqual([]);
  });

  it("switched outcome requests a fresh restart", () => {
    const { map, effects } = run([beginSwitch(), { type: "outcome", key: KEY, outcome: switched }]);
    expect(effects).toEqual([{ kind: "restart", project: "proj", worktree: "alpha", fresh: true }]);
    expect(map.get(KEY)?.result).toEqual({ kind: "switched" });
    expect(map.get(KEY)?.phase).toBe("restarting");
  });

  it("switched outcome is ignored for a transfer entry", () => {
    const start = run([begin()]).map;
    const step = transferReducer(start, { type: "outcome", key: KEY, outcome: switched });
    expect(step.map).toBe(start);
    expect(step.effects).toEqual([]);
  });

  it("a second outcome is ignored", () => {
    const start = run([beginSwitch(), { type: "outcome", key: KEY, outcome: switched }]).map;
    const step = transferReducer(start, { type: "outcome", key: KEY, outcome: switched });
    expect(step.map).toBe(start);
    expect(step.effects).toEqual([]);
  });

  it("switch failure finishes with the error and no diagnostics", () => {
    const { map, effects } = run([
      beginSwitch(),
      { type: "failed", key: KEY, error: "Antigravity isn't installed" },
    ]);
    expect(map.size).toBe(0);
    expect(effects).toEqual([
      { kind: "toast", message: "Antigravity isn't installed", diagnostics: null },
      { kind: "settle", project: "proj", worktree: "alpha" },
      report(false, "Antigravity isn't installed"),
    ]);
  });

  it("a switch to Codex gets no Codex diagnostics on failure", () => {
    const start: TransferMap = new Map([
      [
        KEY,
        entry({ mode: "switch", to: "codex", phase: "restarting", result: { kind: "switched" } }),
      ],
    ]);
    const { effects } = run(
      [{ type: "spawn_settled", key: KEY, result: { ok: false, error: "boom" } }],
      start,
    );
    expect(effects[0]).toEqual({ kind: "toast", message: "boom", diagnostics: null });
  });

  it('switch spawn_settled ok toasts "Switched alpha to Antigravity." and settles', () => {
    const { map, effects } = run([
      beginSwitch(),
      { type: "outcome", key: KEY, outcome: switched },
      { type: "spawn_settled", key: KEY, result: { ok: true } },
    ]);
    expect(map.size).toBe(0);
    expect(effects.slice(1)).toEqual([
      { kind: "toast", message: "Switched alpha to Antigravity.", diagnostics: null },
      { kind: "settle", project: "proj", worktree: "alpha" },
      report(true, "Switched alpha to Antigravity."),
    ]);
  });

  it("spawn_settled before the outcome is ignored", () => {
    const start = run([beginSwitch()]).map;
    const step = transferReducer(start, {
      type: "spawn_settled",
      key: KEY,
      result: { ok: true },
    });
    expect(step.map).toBe(start);
    expect(step.effects).toEqual([]);
  });

  it("cancel_requested is ignored for a switch", () => {
    const start = run([beginSwitch("codex")]).map;
    const step = transferReducer(start, { type: "cancel_requested", key: KEY });
    expect(step.map).toBe(start);
  });

  it("switch backstop names the target", () => {
    const { effects } = run([beginSwitch(), { type: "backstop", key: KEY }]);
    expect(effects).toEqual([
      {
        kind: "toast",
        message:
          "Antigravity did not report that it started within 120 seconds. Check the alpha pane.",
        diagnostics: null,
      },
      { kind: "settle", project: "proj", worktree: "alpha" },
      report(
        false,
        "Antigravity did not report that it started within 120 seconds. Check the alpha pane.",
      ),
    ]);
  });
});

describe("transfer helpers", () => {
  it("freshToastMessage uses reasonText verbatim and the null-reason fallback copy", () => {
    expect(freshToastMessage("Codex did not start in time")).toBe(
      "Couldn't carry the Claude history over (Codex did not start in time). Started a fresh Codex session.",
    );
    expect(freshToastMessage(null)).toBe(
      "Codex couldn't resume the imported conversation. Started a fresh Codex session.",
    );
  });

  it("progressLabel covers all eight phases and null", () => {
    expect(progressLabel(null)).toBe("Preparing transfer…");
    const table: [TransferProgress["phase"], string][] = [
      ["preflight", "Checking the Claude session…"],
      ["starting_codex", "Starting Codex…"],
      ["initializing", "Initializing Codex…"],
      ["importing", "Importing the conversation…"],
      ["awaiting_completion", "Waiting for Codex to finish the import…"],
      ["reading_ledger", "Checking the imported session…"],
      ["persisting", "Saving the transfer…"],
      ["cancelling", "Cancelling…"],
    ];
    for (const [phase, label] of table) expect(progressLabel(progress(phase))).toBe(label);
  });

  it("formatElapsed formats m:ss and clamps negatives", () => {
    expect(formatElapsed(0)).toBe("0:00");
    expect(formatElapsed(59_999)).toBe("0:59");
    expect(formatElapsed(60_000)).toBe("1:00");
    expect(formatElapsed(150_000)).toBe("2:30");
    expect(formatElapsed(-5)).toBe("0:00");
  });

  it("canCancel table", () => {
    expect(canCancel(entry())).toBe(true);
    expect(canCancel(entry({ cancelling: true }))).toBe(false);
    expect(canCancel(entry({ progress: progress("persisting") }))).toBe(false);
    expect(canCancel(entry({ progress: progress("cancelling") }))).toBe(false);
    expect(canCancel(entry({ to: "claude" }))).toBe(false);
    expect(canCancel(entry({ phase: "restarting" }))).toBe(false);
  });

  it("blocksInput is true only while importing", () => {
    expect(blocksInput(entry())).toBe(true);
    expect(blocksInput(entry({ phase: "restarting" }))).toBe(false);
    expect(blocksInput(null)).toBe(false);
    expect(blocksInput(undefined)).toBe(false);
  });

  it("blocksInput is true while a switch awaits its outcome and false after", () => {
    const awaiting = entry({ mode: "switch", to: "antigravity", phase: "restarting" });
    expect(blocksInput(awaiting)).toBe(true);
    expect(blocksInput({ ...awaiting, result: { kind: "switched" } })).toBe(false);
  });

  it("isMidTurn is true for busy and waiting only", () => {
    expect(isMidTurn("busy")).toBe(true);
    expect(isMidTurn("waiting")).toBe(true);
    expect(isMidTurn("idle")).toBe(false);
    expect(isMidTurn("offline")).toBe(false);
    expect(isMidTurn(undefined)).toBe(false);
  });

  it("restartAfterFailure repeats the transfer flavour", () => {
    expect(restartAfterFailure(null, "kill_restart")).toEqual({ fresh: false, transfer: false });
    expect(restartAfterFailure(null, "start_fresh")).toEqual({ fresh: true, transfer: false });
    expect(restartAfterFailure({ transfer: false, fresh: true }, "kill_restart")).toEqual({
      fresh: false,
      transfer: false,
    });
    // Never resume --last after a Fresh outcome.
    expect(restartAfterFailure({ transfer: true, fresh: true }, "kill_restart")).toEqual({
      fresh: true,
      transfer: true,
    });
    expect(restartAfterFailure({ transfer: true, fresh: false }, "kill_restart")).toEqual({
      fresh: false,
      transfer: true,
    });
    expect(restartAfterFailure({ transfer: true, fresh: false }, "start_fresh")).toEqual({
      fresh: true,
      transfer: true,
    });
  });

  it("restartPlan skips confirm-kill for a transfer restart and honours fresh", () => {
    expect(restartPlan({ transfer: true, fresh: false }, "running")).toEqual({
      confirm: false,
      fresh: false,
      transfer: true,
      drive: false,
    });
    expect(restartPlan({ transfer: true, fresh: true }, "running")).toEqual({
      confirm: false,
      fresh: true,
      transfer: true,
      drive: false,
    });
    expect(restartPlan({ transfer: false, fresh: false }, "running")).toEqual({
      confirm: true,
      fresh: false,
      transfer: false,
      drive: false,
    });
    expect(restartPlan({ transfer: false, fresh: true }, "ended")).toEqual({
      confirm: false,
      fresh: true,
      transfer: false,
      drive: false,
    });
  });

  it("restartPlan's drive flavour never confirms and honours fresh", () => {
    expect(restartPlan({ transfer: false, fresh: true, drive: true }, "running")).toEqual({
      confirm: false,
      fresh: true,
      transfer: false,
      drive: true,
    });
    expect(restartPlan({ transfer: false, fresh: false, drive: true }, "running")).toEqual({
      confirm: false,
      fresh: false,
      transfer: false,
      drive: true,
    });
    // Without the flag a live session still asks first.
    const plain = restartPlan({ transfer: false, fresh: true, drive: false }, "running");
    expect(plain.confirm).toBe(true);
  });

  it("shouldStartTransfer rejects re-entry", () => {
    expect(shouldStartTransfer(new Map(), KEY)).toBe(true);
    const importing = run([begin()]).map;
    expect(shouldStartTransfer(importing, KEY)).toBe(false);
    const restarting = run([begin(), { type: "outcome", key: KEY, outcome: imported }]).map;
    expect(shouldStartTransfer(restarting, KEY)).toBe(false);
    expect(shouldStartTransfer(importing, transferKey("proj", "bravo"))).toBe(true);
    expect(shouldStartTransfer(importing, transferKey("other-proj", "alpha"))).toBe(true);
  });

  it("spawnFailedCopy names the cli, passes the strict-chain string through, and moves other errors to detail", () => {
    const strict = "codex did not start: x (tried: resume thread-a -> fresh)";
    expect(spawnFailedCopy("codex", strict)).toEqual({ message: strict, detail: null });
    expect(spawnFailedCopy("codex", "boom")).toEqual({
      message: "Failed to launch codex — is it on PATH?",
      detail: "boom",
    });
    expect(spawnFailedCopy("claude", null)).toEqual({
      message: "Failed to launch claude — is it on PATH?",
      detail: null,
    });
    expect(spawnFailedCopy("codex", "")).toEqual({
      message: "Failed to launch codex — is it on PATH?",
      detail: null,
    });
    // A Claude strict-chain string under cli codex is just another error.
    const claudeStrict = "claude did not start: y";
    expect(spawnFailedCopy("codex", claudeStrict)).toEqual({
      message: "Failed to launch codex — is it on PATH?",
      detail: claudeStrict,
    });
  });

  it("spawnFailedCopy names the provider's binary", () => {
    expect(spawnFailedCopy("antigravity", null).message).toBe(
      "Failed to launch agy — is it on PATH?",
    );
    const strict = "agy did not start: x";
    expect(spawnFailedCopy("antigravity", strict)).toEqual({ message: strict, detail: null });
  });

  it("midTurnNote names the agent that is mid-turn", () => {
    expect(midTurnNote("codex").startsWith("Codex is in the middle of a turn")).toBe(true);
    expect(midTurnNote("claude")).toBe(
      "Claude is in the middle of a turn. That in-flight turn will not be carried over.",
    );
  });

  it("spawnFailedCopy names an update prompt instead of blaming PATH", () => {
    expect(spawnFailedCopy("codex", "AgentUpdatePrompt: codex")).toEqual({
      message: "Codex wants to update before it starts",
      detail: null,
    });
    expect(spawnFailedCopy("claude", "AgentUpdatePrompt: claude")).toEqual({
      message: "Claude wants to update before it starts",
      detail: null,
    });
    // The token names the CLI that launched, whatever the worktree's default.
    expect(spawnFailedCopy("claude", "AgentUpdatePrompt: codex").message).toBe(
      "Codex wants to update before it starts",
    );
    // Echoed pane text is lower-cased by the backend, so it can't pose as the token.
    expect(spawnFailedCopy("codex", "agentupdateprompt: x").message).toBe(
      "Failed to launch codex — is it on PATH?",
    );
  });
});

describe("createTransferController", () => {
  let run: ReturnType<typeof vi.fn<(e: TransferEffect) => void>>;
  let onChange: ReturnType<typeof vi.fn<(m: TransferMap) => void>>;

  beforeEach(() => {
    vi.useFakeTimers();
    run = vi.fn<(e: TransferEffect) => void>();
    onChange = vi.fn<(m: TransferMap) => void>();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("no timer is armed while importing", () => {
    const ctl = createTransferController({ run, onChange });
    ctl.dispatch(begin());
    expect(vi.getTimerCount()).toBe(0);
    vi.advanceTimersByTime(10 * 60_000);
    expect(run).not.toHaveBeenCalled();
    expect(ctl.getMap().get(KEY)?.phase).toBe("importing");
  });

  it("backstop fires 120 s after entering restarting", () => {
    expect(RESTARTING_BACKSTOP_MS).toBe(120_000);
    const ctl = createTransferController({ run, onChange });
    ctl.dispatch(begin());
    ctl.dispatch({ type: "outcome", key: KEY, outcome: imported });
    run.mockClear();
    onChange.mockClear();
    vi.advanceTimersByTime(119_999);
    expect(run).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(run.mock.calls.map(([e]) => e.kind)).toEqual(["toast", "settle", "report"]);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange.mock.calls[0][0].size).toBe(0);
    expect(ctl.getMap().size).toBe(0);
  });

  it("spawn_settled before the deadline cancels the backstop", () => {
    const ctl = createTransferController({ run, onChange });
    ctl.dispatch(begin());
    ctl.dispatch({ type: "outcome", key: KEY, outcome: imported });
    vi.advanceTimersByTime(60_000);
    ctl.dispatch({ type: "spawn_settled", key: KEY, result: { ok: true } });
    expect(vi.getTimerCount()).toBe(0);
    run.mockClear();
    vi.advanceTimersByTime(200_000);
    expect(run).not.toHaveBeenCalled();
  });

  it("onChange runs before effects and only when the map changed", () => {
    const order: string[] = [];
    const ctl = createTransferController({
      run: (e) => order.push(`run:${e.kind}`),
      onChange: () => order.push("change"),
    });
    ctl.dispatch(begin());
    ctl.dispatch({ type: "outcome", key: KEY, outcome: done });
    expect(order).toEqual(["change", "change", "run:restart"]);
    order.length = 0;
    // No-op: a second begin and an event for an unknown key.
    ctl.dispatch(begin());
    ctl.dispatch({ type: "backstop", key: transferKey("proj", "bravo") });
    expect(order).toEqual([]);
    // The handoff is injected after the restarted session reported its spawn.
    ctl.dispatch({ type: "spawn_settled", key: KEY, result: { ok: true } });
    expect(order).toEqual([
      "change",
      "run:toast",
      "run:settle",
      "run:report",
      "run:inject_handoff",
    ]);
  });

  it("a progress event that changes nothing visible does not notify", () => {
    const ctl = createTransferController({ run, onChange });
    ctl.dispatch(begin());
    ctl.dispatch({
      type: "progress",
      key: KEY,
      progress: progress("importing", { message: "50%" }),
    });
    expect(onChange).toHaveBeenCalledTimes(2);
    const before = ctl.getMap();
    // Same phase and message, only the elapsed time moved on.
    ctl.dispatch({
      type: "progress",
      key: KEY,
      progress: progress("importing", { message: "50%", elapsedMs: 5_000 }),
    });
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(ctl.getMap()).toBe(before);
    // A new message does notify.
    ctl.dispatch({
      type: "progress",
      key: KEY,
      progress: progress("importing", { message: "75%" }),
    });
    expect(onChange).toHaveBeenCalledTimes(3);
    expect(ctl.getMap().get(KEY)?.progress?.message).toBe("75%");
  });

  it("dispose clears pending timers but the controller stays usable", () => {
    const ctl = createTransferController({ run, onChange });
    ctl.dispatch(begin());
    ctl.dispatch({ type: "outcome", key: KEY, outcome: imported });
    ctl.dispose();
    expect(vi.getTimerCount()).toBe(0);
    run.mockClear();
    vi.advanceTimersByTime(200_000);
    expect(run).not.toHaveBeenCalled();
    const otherKey = transferKey("proj", "bravo");
    ctl.dispatch(begin("codex", "proj", "bravo"));
    ctl.dispatch({ type: "outcome", key: otherKey, outcome: imported });
    expect(vi.getTimerCount()).toBeGreaterThan(0);
    vi.advanceTimersByTime(RESTARTING_BACKSTOP_MS);
    expect(ctl.getMap().has(otherKey)).toBe(false);
  });

  it("switch backstop fires after RESTARTING_BACKSTOP_MS, covering the backend call", () => {
    const ctl = createTransferController({ run, onChange });
    ctl.dispatch({
      type: "begin",
      project: "proj",
      worktree: "alpha",
      to: "antigravity",
      now: 0,
      mode: "switch",
    });
    expect(vi.getTimerCount()).toBe(1);
    vi.advanceTimersByTime(RESTARTING_BACKSTOP_MS - 1);
    expect(run).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(run.mock.calls.map(([e]) => e.kind)).toEqual(["toast", "settle", "report"]);
    const toast = run.mock.calls[0][0];
    expect(toast.kind === "toast" && toast.message.startsWith("Antigravity did not report")).toBe(
      true,
    );
    expect(ctl.getMap().size).toBe(0);
  });

  it("a custom backstopMs is honoured", () => {
    const ctl = createTransferController({ run, onChange, backstopMs: 5_000 });
    ctl.dispatch(begin());
    ctl.dispatch({ type: "outcome", key: KEY, outcome: fresh });
    run.mockClear();
    vi.advanceTimersByTime(4_999);
    expect(run).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(run.mock.calls.map(([e]) => e.kind)).toEqual(["toast", "settle", "report"]);
  });
});

describe("shouldHonourSignal", () => {
  const entry = (phase: TransferEntry["phase"]): TransferEntry => ({
    project: "proj",
    worktree: "alpha",
    to: "codex",
    mode: "transfer",
    phase,
    startedAt: 0,
    progress: null,
    cancelling: false,
    result: null,
  });

  it("honours every signal when no transfer owns the pane", () => {
    expect(shouldHonourSignal({ transfer: false }, null)).toBe(true);
    expect(shouldHonourSignal({ transfer: false }, undefined)).toBe(true);
    expect(shouldHonourSignal({ transfer: true }, null)).toBe(true);
  });

  it("ignores plain restarts and terminates during a transfer, but not the transfer's restart", () => {
    for (const phase of ["importing", "restarting"] as const) {
      expect(shouldHonourSignal({ transfer: false }, entry(phase))).toBe(false);
      expect(shouldHonourSignal({ transfer: true }, entry(phase))).toBe(true);
    }
  });
});
