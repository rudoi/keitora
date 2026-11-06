# 0069 / 03 — Frontend: transfer phase machine, overlay, outcomes

Part of 0069 "Harden Claude → Codex transfer" (proposal: `../0069-codex-transfer-hardening.md`).
Shared interfaces: **`00-contracts.md`** (TypeScript shapes for `TransferOutcome`, `TransferProgress`, IPC
wrappers, `RestartSignal`, `onSpawnSettled`, launch failure string). Siblings: `01-import.md` (emits
outcome/progress, cancel), `02-launch-and-env.md` (strict launch, `fresh`, failure strings),
`04-scoping-locking-diagnostics.md` (project IPC args, `transfer_settled`, diagnostics command).

Line numbers below were re-verified against the working tree at `4178d29` (the plan-only commits after
`e9c5d27` do not touch source, so they equal `e9c5d27`). Where the earlier draft was off, the corrected
reference is in "Reference corrections".

Repo rules that apply to every file this document touches (`AGENTS.md`): no plan or task identifiers
(`#0069`, "Stage 3") in code or commit messages; fixtures use placeholder names only (`proj`, `other-proj`,
worktrees `alpha`/`bravo`/`wt`); new logic ships with vitest coverage in the same change; `yarn test` and
`yarn lint` (biome) stay green; `tsconfig.json` has `noUnusedLocals` and `noUnusedParameters`, so every
deleted code path must also delete its now-unused imports/params.

## Files

| File | Change |
|---|---|
| `src/lib/transferPhase.ts` (**new**) | Pure phase machine, controller with backstop timer, copy and helper functions. No React, no Tauri imports (type-only imports from `./ipc` and `./sessions`). |
| `src/lib/transferPhase.test.ts` (**new**) | All phase-machine, copy and controller tests. |
| `src/lib/ipc.ts` | New types, new/changed wrappers, `ptySpawn` gains `transfer`, new `onTransferProgress`. |
| `src/lib/ipc.test.ts` | Existing exact-payload `ptySpawn` tests stay unchanged (the `transfer` key is omitted unless true); add a `transfer` case and wrapper tests. |
| `src/lib/resumeNotice.ts` / `.test.ts` | `RestartSignal.transfer`, `nextRestartSignal(prev, fresh, transfer)`. |
| `src/lib/laneState.ts` / `.test.ts` | `transferDisabledReason`. |
| `src/lib/paneOutput.test.ts` | Stale-exit reproduction tests (written first). |
| `src/App.tsx` | Replace `transferring` state + 2 effects with the controller; rewrite `onTransfer`; new pane props. |
| `src/components/TerminalPane.tsx` | Remove `transferringTo`/`transferringRef`; new props; `spawn`/`restart` return a result; input gate; spawn-failed copy; render `TransferOverlay`. |
| `src/components/TransferOverlay.tsx` (**new**) | Small presentational overlay (importing and restarting). |
| `src/components/Rail.tsx` | Disable the Transfer menu item while a transfer is in flight. |
| `src/components/Toast.tsx` | `showToast(message, opts?)` gains an optional action button (needed for "Copy diagnostics"). |
| `src/App.css` | `.pane-msg-detail` (secondary line). |
| `.harness/app.tsx` | Mock the new transfer commands (inspect how the file mocks commands today and follow it). |

## Goal

"Transfer to Codex" must always end with a running Codex session in the pane (resuming the import if it
worked, else a fresh Codex). The UI must never wedge, never show a misleading "Kill & Restart" screen, and
must tell the user what happened. This document makes the frontend an explicit phase machine driven by real
signals instead of heuristics and timers.

## Root causes covered

### R5 — The transferring indicator is cleared by the wrong signals (high) → symptom: pane stuck, confirm-kill / "Kill & Restart" screen (3a)

Two effects clear `transferring`: `App.tsx:498-511` (after the session was seen offline then live again)
and a 20 s safety net at `:514-518`. The import can take up to 360 s today (≈150 s after `01-import.md`).
The timer fires mid-import. When the import finally succeeds, `bumpRestartNonce` reaches a pane whose
`transferringRef` is null and whose status is `running` (`TerminalPane.tsx:324-325, 895-910`), triggering
the confirm-kill. `cli=codex` is already persisted (`transfer.rs:257`), so cancelling leaves Claude running
while the rail says Codex.

### R4 (UI part) — Misleading spawn-failed screen

Any `ptySpawn` error other than `Recovering`/`Terminated` lands in `spawn-failed`
(`TerminalPane.tsx:593`). The error text is dropped (`msg` is only inspected, never stored) and the copy is
hard-coded to "Failed to launch claude — is it on PATH?" (`:1785`). Backend causes are in
`02-launch-and-env.md`.

### R6 (UI part) — No fallback

On error `onTransfer` only toasts (`App.tsx:585-588`). The transfer branch of the restart effect always
calls `restart(false)` (`TerminalPane.tsx:904`) and so can never request a fresh Codex.

### R1 (UI part) — Input during the import

The "Transferring…" overlay renders only when `status !== "running"` (`TerminalPane.tsx:1670`), and a running
pane stays `running` for the whole import, so the user keeps typing into Claude while Codex hashes the
JSONL; any write breaks the legacy ledger match (`01-import.md`). A second consequence found while
verifying: even in the `restarting` phase a *running* pane never leaves `running` (`restart()` does not
change status until the spawn succeeds, `:748-779`), so the status-gated overlay would not show during a
Claude → Codex restart either. The new overlay is therefore not status-gated.

### R8 (UI parts)

- **No re-entry guard and no cancel (medium)** in `onTransfer`.
- **The exit handler in the output gate checks no generation (low, unconfirmed)**
  (`TerminalPane.tsx:497-507`). The coalescer already cancels a deferred "ended" on restart, but only an
  `end` that started *before* the reset (see "Stale exit").
- **Project scoping (medium):** the frontend captures `activeProject` (`App.tsx:532`) but does not pass it:
  `transferWorktree`, `previewClaudeTransfer`, `injectHandoffSummary` (`src/lib/ipc.ts:1047-1070`) take no
  project. This document makes the frontend pass it; backend resolution is `04`.

## Scope and non-goals

In: everything in this document and its vitest tests. Out: token-size UI (#26), oversized-transfer routing
(#28), cost measurement (#27), redesign of the Codex → Claude direction (this document only adjusts it for
the project parameter, the outcome type, and the shared phase machine), default launch behaviour changes.

## Reference corrections (verified against the tree)

| Earlier reference | Reality |
|---|---|
| `App.tsx:499-511` / `:515-518` | Effects start at `:498` and `:514` (`useEffect(` lines). Bodies match. |
| "captures `activeProject` (`App.tsx:527`)" | `const activeProject = warmRef.current.active;` is `:532` (`:527` is inside `onTransfer`'s opening comment). |
| "`restart()` already awaits `spawn`, `:763`" | `await spawn(fresh)` is `TerminalPane.tsx:776`. |
| "overlay `TerminalPane.tsx:1669`" | `:1669` is the comment; the gate `{status !== "running" && (` is `:1670`. Kill & Restart copy: `:1785` (message), `:1801` (button). |
| "`host.rs:2257` `OutputMsg::Exit`" | The enum is `host.rs:1432-1437`; the wire-shape test asserting `{"type":"exit"}` is `host.rs:2255-2258`. |
| `previewClaudeTransfer` returns `ClaudeTransferPreview` (earlier contract draft) | The existing type is `ConfirmPreview` (`ipc.ts:1036`, snake_case fields `path`, `filename`, `mtime`, `first_user_line`). Keep it; `00-contracts.md` now says the same. |
| `injectHandoffSummary(...): Promise<void>` (earlier contract draft) | Existing return is `Promise<boolean>` (`ipc.ts:1068`, true = injected; Rust `Result<bool, String>`). Keep `boolean`; `00-contracts.md` now says the same; the caller ignores the value. |
| "`ptySpawn` ... the pane passes `true`" | `ptySpawn(project, id, cwd, size, command?, fresh?)` (`ipc.ts:582`) has 6 params; `transfer` becomes the 7th. |
| Rail transfer item | `WorktreeMenu` in `Rail.tsx:283-339`; the item disables only for `codexMissing` today (`:299, :302-307`). |
| Existing `ipc.test.ts` "defaults fresh to false" | Asserts the exact invoke payload (`ipc.test.ts:58-69`). Because `transfer` is omitted from the payload unless `true`, these tests stay green unchanged. |
| Existing `resumeNotice.test.ts` `nextRestartSignal` | Asserts exact `{n, fresh}` objects (`:60-67`); adding `transfer` changes it. |

Confirmed unchanged: `restartNonces` map `App.tsx:218` (`Map<string, RestartSignal>`, key
`${project}/${worktree}`), `bumpRestartNonce` `:229-242`, 20 s timer `:516`, `injectHandoffWhenIdle`
`:451-475`, pane props `App.tsx:1170-1188`, `RestartSignal` `resumeNotice.ts:16-19`, `nextRestartSignal`
`:78-80`, output-gate exit handler `TerminalPane.tsx:497-507`, `spawn` `:562-597`, restart-nonce effect
`:895-910`, `OutputMsg` type `ipc.ts:517`, resume event `PtyResumeEvent {outcome, fell_back}`
`ipc.ts:880-883` and `onPtyResume` `:907` (backend emit `backend/herdr/mod.rs:926-930`, before the spawn
returns), clipboard write pattern `TerminalPane.tsx:1094`.

## Dependencies on siblings (contracts)

- IPC wrappers and types exactly as in `00-contracts.md` sections 1-3 and 5-6 (which already use
  `ConfirmPreview` and `boolean`). Every `project` argument is the resolved `[project].name` (the string
  `ptySpawn` and `launch_for` use; in `App.tsx` that is `warm.active` / `wp.summary.name`). If the backend is not yet merged, develop against the
  contract: tests mock the transport (`vi.mock("./transport")` pattern in `ipc.test.ts`) and the UI mock
  harness `.harness/` (`mockIPC`) returns each outcome kind.
- `ptySpawn` gains a trailing positional `transfer?: boolean` (never an options object). The `transfer` key
  is **omitted from the invoke payload unless it is `true`** (`...(transfer ? { transfer: true } : {})`), so the
  Rust parameter is `Option<bool>` (absent = false, `02`) and the existing exact-payload tests stay green. The
  Codex -> Claude restart also sends `transfer: true` (it skips the confirm-kill); the backend treats it as the
  Default policy because the worktree is Claude (`02`: the strict policy is Codex-only).
- Final spawn failure string and `fell_back` event semantics: `00-contracts.md` section 6. `fell_back` is true
  whenever the strict chain's launch decision became weaker (Resume -> Fresh); a hook-trust-flag drop on a
  Fresh -> Fresh step does not set it. The backend emits the resume event inside the spawn, before `pty_spawn`
  resolves (verified, `00-contracts.md` section 6), so the event reaches the controller before
  `spawn_settled`.
- `transfer_settled` is called by the frontend when the phase entry is removed (any removal path).
- `cancel_transfer` only has an effect for Claude → Codex imports (`01`); the frontend only offers Cancel
  for `to === "codex"` (UI and the `cancel_requested` reducer rule both enforce it).

## Design

### 1. Types in `src/lib/ipc.ts`

Add exactly the shapes from `00-contracts.md` section 1 and 3 (export `TransferOutcome`, `TransferFailure`,
`TransferProgress`; the contract's `SkipReason`/`TransferPhase` literal unions stay inline). Wrappers (all
`invoke` through the existing local `invoke` helper; Tauri arg names camelCase):

```ts
export function previewClaudeTransfer(project: string, worktree: string): Promise<ConfirmPreview>
  // invoke("preview_claude_transfer", { project, worktree })
export function transferWorktree(project: string, worktree: string, targetCli: CliKind): Promise<TransferOutcome>
  // invoke("transfer_worktree", { project, worktree, targetCli })
export function injectHandoffSummary(project: string, worktree: string): Promise<boolean>
  // invoke("inject_handoff_summary", { project, worktree })
export function cancelTransfer(project: string, worktree: string): Promise<void>
  // invoke("cancel_transfer", { project, worktree })
export function transferSettled(project: string, worktree: string): Promise<void>
  // invoke("transfer_settled", { project, worktree })
export function transferDiagnostics(project: string, worktree: string): Promise<string | null>
  // invoke("transfer_diagnostics", { project, worktree })
export function onTransferProgress(cb: (p: TransferProgress) => void): Promise<UnlistenFn>
  // listen<TransferProgress>("transfer://progress", cb)   (module-local `listen`, ipc.ts:15)
export function ptySpawn(project, id, cwd, size, command?, fresh?, transfer?: boolean): Promise<number>
  // positional; the payload gains `transfer: true` ONLY when transfer is true (key omitted otherwise)
```

`onTransferProgress` uses the module-local `listen` (transport seam), like `onDaemonStatus` (`ipc.ts:99`).

### 2. `src/lib/resumeNotice.ts`

```ts
export interface RestartSignal { n: number; fresh: boolean; transfer: boolean }
export function nextRestartSignal(prev: RestartSignal | undefined, fresh: boolean, transfer = false): RestartSignal
  // => { n: (prev?.n ?? 0) + 1, fresh, transfer }
```

The terminate/launch nonce maps (`App.tsx:261-279`) also hold `RestartSignal`; they keep calling
`nextRestartSignal(prev, fresh)` and get `transfer: false`.

### 3. `src/lib/transferPhase.ts` — the pure module (exact API)

```ts
import type { CliKind, TransferOutcome, TransferProgress, PtyResumeEvent } from "./ipc";   // type-only
import type { SessionState } from "./sessions";                                             // type-only

export const RESTARTING_BACKSTOP_MS = 120_000;   // above the strict launch chain's ~100 s budget (02)
export const transferKey = (project: string, worktree: string): string => `${project}/${worktree}`;

/** What the transfer produced; decides the toast shown once the spawn settles. */
export type TransferResult =
  | { kind: "imported" }
  | { kind: "fresh"; reasonText: string | null }   // null = the strict chain fell back after a good import
  | { kind: "done" };                              // Codex -> Claude

export interface TransferEntry {
  project: string;
  worktree: string;
  to: CliKind;
  phase: "importing" | "restarting";
  startedAt: number;                  // epoch ms at `begin`; the overlay's elapsed clock
  progress: TransferProgress | null;  // latest event; only meaningful while importing; advisory
  cancelling: boolean;                // the user clicked Cancel (or progress said "cancelling")
  result: TransferResult | null;      // null while importing
}
export type TransferMap = ReadonlyMap<string, TransferEntry>;   // key = transferKey(project, worktree)

export type SpawnResult = { ok: true } | { ok: false; error: string };

export type TransferEvent =
  | { type: "begin"; project: string; worktree: string; to: CliKind; now: number }
  | { type: "progress"; key: string; progress: TransferProgress }
  | { type: "cancel_requested"; key: string }
  | { type: "outcome"; key: string; outcome: TransferOutcome }
  | { type: "failed"; key: string; error: string }                  // transferWorktree rejected
  | { type: "spawn_settled"; key: string; result: SpawnResult }
  | { type: "resume_notice"; key: string; notice: PtyResumeEvent }
  | { type: "backstop"; key: string };

export type TransferEffect =
  | { kind: "restart"; project: string; worktree: string; fresh: boolean }   // bump the restart signal with transfer=true
  | { kind: "inject_handoff"; project: string; worktree: string }
  | { kind: "toast"; message: string; diagnostics: { project: string; worktree: string } | null }
  | { kind: "settle"; project: string; worktree: string };                   // transferSettled + drop the resume listener

export interface TransferStep { map: TransferMap; effects: TransferEffect[] }

export function transferReducer(map: TransferMap, event: TransferEvent): TransferStep;
```

**Reducer rules (exhaustive; every other combination is a no-op returning the same `map` object and
`effects: []`).** "Unknown key" and "wrong phase" are always no-ops, which gives idempotence for free.

| Event | Precondition | New state | Effects (in order) |
|---|---|---|---|
| `begin` | key not in map | add entry `{phase:"importing", progress:null, cancelling:false, result:null, startedAt:now}` | none |
| `begin` | key in map | unchanged | none |
| `progress` | entry `importing` | entry.progress = event.progress; `cancelling = cancelling \|\| progress.phase === "cancelling"` | none |
| `cancel_requested` | entry `importing`, `to === "codex"`, `!cancelling` | `cancelling = true` | none (the caller invokes `cancelTransfer`) |
| `outcome` kind `imported` | entry `importing` | `phase:"restarting"`, `result:{kind:"imported"}` | `restart{fresh:false}` |
| `outcome` kind `fresh` | entry `importing` | `phase:"restarting"`, `result:{kind:"fresh", reasonText}` | `restart{fresh:true}` |
| `outcome` kind `done` | entry `importing` | `phase:"restarting"`, `result:{kind:"done"}` | `restart{fresh:false}`, `inject_handoff` |
| `outcome` kind `cancelled` | entry `importing` | remove entry | `toast("Transfer cancelled.", diagnostics:null)`, `settle` |
| `failed` | entry `importing` | remove entry | `toast(error verbatim, diagnostics: to==="codex" ? {…} : null)`, `settle` |
| `spawn_settled` ok | entry `restarting` | remove entry | `toast(resultToast(entry), diagnostics: result.kind==="fresh" ? {…} : null)`, `settle` |
| `spawn_settled` not ok | entry `restarting` | remove entry | `toast(error verbatim, diagnostics: to==="codex" ? {…} : null)`, `settle` |
| `resume_notice` | entry `restarting`, `to==="codex"`, `notice.fell_back === true`, `result.kind === "imported"` | `result = {kind:"fresh", reasonText:null}` | none (the toast is produced at `spawn_settled`) |
| `resume_notice` | anything else (already `fresh`; `fell_back` false; `to==="claude"`) | unchanged | none |
| `backstop` | entry `restarting` | remove entry | `toast(backstopMessage(entry), diagnostics: to==="codex" ? {…} : null)`, `settle` |

Why toasts wait for `spawn_settled`: a success toast before the spawn is known would be contradicted by a
spawn-failure toast a second later. The `fresh` toast is also produced at settle, so "exactly one Fresh
toast" holds structurally (one `result`, one settle). `inject_handoff` stays at outcome time because
`injectHandoffWhenIdle` already waits (800 ms initial delay, polls for `idle`, gives up after 15 s,
`App.tsx:451-475`).

**Pure helpers (all exported, all unit-tested):**

```ts
export function cliLabel(cli: CliKind): "Codex" | "Claude"
export function resultToast(entry: TransferEntry): string          // see "Toast copy"
export function freshToastMessage(reasonText: string | null): string
export function backstopMessage(entry: TransferEntry): string
export function progressLabel(p: TransferProgress | null): string  // see "Overlay"
export function formatElapsed(ms: number): string                  // "m:ss"; ms<0 treated as 0
export function canCancel(e: TransferEntry): boolean
    // e.phase==="importing" && e.to==="codex" && !e.cancelling
    //   && e.progress?.phase !== "persisting" && e.progress?.phase !== "cancelling"
export function blocksInput(e: TransferEntry | null | undefined): boolean   // e?.phase === "importing"
export function isMidTurn(state: SessionState | undefined): boolean         // state === "busy" || state === "waiting"
export const MID_TURN_NOTE: string                                           // see "Mid-turn warning"
export function spawnFailedCopy(cli: CliKind, error: string | null): { message: string; detail: string | null }
```

**Controller (owns the backstop timer so it is testable with fake timers):**

```ts
export interface TransferController {
  getMap(): TransferMap;
  dispatch(event: TransferEvent): void;
  dispose(): void;   // clears timers only; the controller stays usable (React StrictMode double-mount)
}
export function createTransferController(opts: {
  run: (effect: TransferEffect) => void;
  onChange: (map: TransferMap) => void;     // called only when the map object changed
  backstopMs?: number;                      // default RESTARTING_BACKSTOP_MS
}): TransferController;
```

`dispatch` order: reduce → store the new map → sync timers → `onChange(map)` (if changed) → `run` each
effect in order. Timer sync after every step: for each entry in `restarting` with no timer, arm
`setTimeout(() => dispatch({type:"backstop", key}), backstopMs)`; for every timer whose key is no longer in
the map or no longer `restarting`, `clearTimeout` it. Uses the global `setTimeout`/`clearTimeout` (so
`vi.useFakeTimers()` works). **No timer is ever armed for `importing`.**

### 4. `App.tsx` — state shape, deleted code, `onTransfer`

**Deleted** (all of it, nothing layered on top): `transferring` state `:483`; `transferSawOfflineRef`
`:486`; the offline→live effect `:498-511`; the 20 s timer effect `:514-518`; the `transferringTo` prop at
`:1183`; the `setTransferring` calls `:574, :586`; the `showToast("Transferring … to …")` `:566` (the
overlay is now the indicator); the explanatory comments above them. Remove imports that become unused
(`liveSessions` is still used elsewhere; verify with `tsc`).

**Added state:**

```ts
const [transfers, setTransfers] = useState<TransferMap>(() => new Map());   // render mirror only
const transferCtlRef = useRef<TransferController | null>(null);
if (transferCtlRef.current === null) {
  transferCtlRef.current = createTransferController({
    run: (e) => runTransferEffectRef.current(e),
    onChange: setTransfers,
  });
}
const transferCtl = transferCtlRef.current;
useEffect(() => () => transferCtl.dispose(), [transferCtl]);
const transferBusyKeys = useMemo(() => new Set(transfers.keys()), [transfers]);
```

The controller (not React state) is the source of truth (`transferCtl.getMap()`), so decisions made inside
async callbacks never read a stale closure. `transfers` exists only to re-render.

`bumpRestartNonce` gets a 4th positional parameter: `(worktree, project?, fresh = false, transfer = false)`
and calls `nextRestartSignal(next.get(key), fresh, transfer)`. Existing callers (`onRestartSession`
`:252`) are unchanged.

**Effect runner** (`runTransferEffectRef.current` is reassigned every render, like `warmRef`):

| Effect | Action |
|---|---|
| `restart` | `bumpRestartNonce(e.worktree, e.project, e.fresh, true)` |
| `inject_handoff` | `injectHandoffWhenIdle(e.worktree, e.project)` (its inner call becomes `injectHandoffSummary(project, worktree)`; `:460`) |
| `toast` | `showToast(e.message, e.diagnostics ? { action: { label: "Copy diagnostics", onClick: () => void copyTransferDiagnostics(e.diagnostics.project, e.diagnostics.worktree) } } : undefined)` |
| `settle` | `void transferSettled(e.project, e.worktree).catch(() => {})`; call and delete `resumeUnlistenRef.current.get(key)` |

`copyTransferDiagnostics(project, worktree)`: `const text = await transferDiagnostics(project, worktree)`;
`null` → `showToast("No diagnostics available.")`; else `await navigator.clipboard.writeText(text)` then
`showToast("Diagnostics copied.")`; any throw → `showToast("Could not copy diagnostics: " + String(e))`.

**Resume-event subscription** (for the `fell_back` signal): right after `begin`, call
`onPtyResume(project, worktree, (notice) => transferCtl.dispatch({type:"resume_notice", key, notice}))`;
on resolve, if the key is still in `transferCtl.getMap()` store the unlisten in `resumeUnlistenRef`
(`Map<string, UnlistenFn>`), else call it immediately. Subscribing at `begin` (not at `restarting`) removes
the race with the backend emitting during the spawn. Also unlisten all on unmount.

**Progress subscription:** one `useEffect` on mount: `onTransferProgress((p) =>
transferCtl.dispatch({type:"progress", key: transferKey(p.project, p.worktree), progress: p}))`; cleanup
calls the unlisten (handle the promise like `:430-442` does). Events for keys not in the map are no-ops.

**`onTransfer(worktree, targetCli)`** — `useCallback` deps `[showToast, confirmDialog, injectHandoffWhenIdle, transferCtl]`
(`bumpRestartNonce` is no longer used directly):

```
setSelected(worktree); setView("focus");
const project = warmRef.current.active;          // captured NOW: the resolved [project].name (keys nonces today and is what ptySpawn/launch_for use); string | null
if (project === null) return;
const key = transferKey(project, worktree);
if (transferCtl.getMap().has(key)) { showToast("A transfer is already in progress for this worktree."); return; }
void (async () => {
  // 1. confirm (no entry yet; any error here is a plain toast)
  try {
    if (targetCli === "codex") {
      const preview = await previewClaudeTransfer(project, worktree);
      const midTurn = isMidTurn(liveSessionsRef.current.get(worktree)?.state);
      const ok = await confirmDialog({ title, body: <existing body> + (midTurn ? "\n\n" + MID_TURN_NOTE : ""), confirmLabel: "Transfer to Codex", danger: true });
      if (!ok) return;
    } else { /* existing Codex -> Claude confirm, unchanged copy */ if (!ok) return; }
  } catch (e) { showToast(String(e)); return; }
  // 2. re-check: the confirm dialog is async
  if (transferCtl.getMap().has(key)) return;
  transferCtl.dispatch({ type: "begin", project, worktree, to: targetCli, now: Date.now() });
  subscribeResume(key, project, worktree);
  // 3. the import
  try {
    const outcome = await transferWorktree(project, worktree, targetCli);
    transferCtl.dispatch({ type: "outcome", key, outcome });
  } catch (e) {
    transferCtl.dispatch({ type: "failed", key, error: String(e) });
  }
})();
```

`liveSessionsRef` (`App.tsx:362`) is the active project's live map, and `onTransfer` only ever runs for the
active project (Rail actions on other projects switch first, `Rail.tsx:577-591` and `App.tsx:653-679`), so
it is the right source.

**Cancel handler** passed to panes: `onCancelTransfer(project, worktree)`: `dispatch({type:"cancel_requested", key})`
then `void cancelTransfer(project, worktree).catch(() => {})`. A failed cancel IPC leaves `cancelling` true
until the transfer settles on its own; acceptable (the import is bounded).

**Pane props** (replace `transferringTo` at `:1183`; inside the existing `.map((w) => ...)`):

```tsx
const tkey = transferKey(wp.summary.name, w.name);       // same project string as the nonce keys at :1181-1182
cli={w.cli}
transfer={transfers.get(tkey) ?? null}
restartTransfer={restartNonces.get(tkey)?.transfer ?? false}
onSpawnSettled={(r) => transferCtl.dispatch({ type: "spawn_settled", key: tkey, result: r })}
onCancelTransfer={() => onCancelTransfer(wp.summary.name, w.name)}
```

(`wp.summary.name` is what keys `restartNonces` lookups today and is what `bumpRestartNonce` writes via
`warmRef.current.active`; the transfer map therefore uses the same string. It is also the `project` the pane
passes to `ptySpawn`, i.e. the string `launch_for` expects.)

**Rail props:** pass `transferBusyKeys={transferBusyKeys}` to `<Rail>` (`App.tsx:1080-1096`).

### 5. `Rail.tsx` and `laneState.ts`

`laneState.ts` gains (pure, tested):

```ts
export function transferDisabledReason(o: { codexMissing: boolean; busy: boolean }): string | null
// busy       -> "A transfer is already in progress"      (busy wins)
// codexMissing -> "Codex isn't installed"                (the existing title text, Rail.tsx:307)
// neither    -> null
```

`Rail` gets prop `transferBusyKeys: ReadonlySet<string>`. In `Rail`, compute for the open menu:
`const menuBusy = menu !== null && transferBusyKeys.has(transferKey(groups.find((g) => g.key === menu.projectKey)?.resolvedName ?? "", menu.name))`
and pass `transferBusy={menuBusy}` to `WorktreeMenu` (`:708`). In `WorktreeMenu`, `off` becomes
`kind === "transfer" && reason !== null` where `reason = transferDisabledReason({codexMissing, busy: transferBusy})`,
and the item `title` is `reason ?? undefined`. `GroupModel.resolvedName` exists (`railGroups.ts:66`).
`onTransfer` also guards (section 4), so the menu state is cosmetic.

### 6. `Toast.tsx`

```ts
interface ToastCtx { showToast: (message: string, opts?: { action?: { label: string; onClick: () => void } }) => void }
```

State holds `action`; render `<RadixToast.Action asChild altText={action.label}><button className="pane-btn pane-btn-secondary" …>{label}</button></RadixToast.Action>`
before the close button; pass `duration={action ? 15000 : undefined}` to `RadixToast.Root`. The default
context value stays `{ showToast: () => {} }`. Every existing one-argument caller is unaffected.

### 7. `TerminalPane.tsx`

**Props: remove** `transferringTo`. **Add:**

```ts
cli: CliKind;                                   // worktree's resolved cli (WorktreeInfo.cli, ipc.ts:565)
transfer?: TransferEntry | null;                // this worktree's phase entry, if any
restartTransfer?: boolean;                      // latest restartNonce bump belongs to a transfer
onSpawnSettled?: (r: SpawnResult) => void;      // contract §5 shape
onCancelTransfer?: () => void;
```

**Refs replacing `transferringRef` (`:322-325`):**
`transferRef` (mirror of `transfer ?? null`), `cliRef` (mirror of `cli`), `onSpawnSettledRef` (mirror of
`onSpawnSettled`). All assigned during render like the existing mirrors.

**`spawn` returns a result** (`:562-597`): signature `spawn(fresh = false, via: "spawn" | "launch" = "spawn", transfer = false): Promise<SpawnResult>`.

- no `term` → `{ok:false, error:"The terminal is not ready."}`
- `via === "spawn" && terminatedRef.current` → `{ok:false, error:"The session is terminated."}`
- `ptySpawn(projectName, name, path, size, undefined, fresh, transfer)` (the launch variant `sessionLaunch`
  is unchanged and never `transfer`)
- success (after the existing post-spawn work) → `{ok:true}`
- `msg.includes("Recovering")` → `settleRecoveringRefusal()`; return `{ok:true}` (recovery brings the
  session back from the persisted codex state; not a launch failure; see open questions)
- `msg.includes("Terminated")` → existing handling; return `{ok:false, error:"The session is terminated."}`
- otherwise → `setSpawnFailure({ cli: transferRef.current?.to ?? cliRef.current, error: msg, transfer, fresh })`
  (the `transfer`/`fresh` arguments this `spawn` call ran with), `updateStatus("spawn-failed")`, return
  `{ok:false, error: msg}`.

`setSpawnFailure(null)` runs at the top of `spawn`. `spawnFailure` is
`useState<{cli: CliKind; error: string; transfer: boolean; fresh: boolean} | null>`.
It captures the CLI at failure time because `transfer` is removed by the time the failure screen shows
(settle removes the entry) and `w.cli` may still hold the pre-persist value; `transfer.to` is authoritative
during a transfer. It also captures how the failed spawn was launched, because the recovery buttons must
repeat that launch flavour (below).

**`restart` returns the result:** `restart(fresh = false, opts?: { transfer?: boolean }): Promise<SpawnResult>`;
body unchanged except `return await spawn(fresh, "spawn", opts?.transfer ?? false)`. Other callers
(`startFreshFromRecovery` `:881`, confirm button `:1835`, `handleKillRestart` `:1620`) ignore the value.
Note the existing meaning: `restart(false)` resumes, `restart(true)` is fresh (`:748-749, :1835`), which
is what `00-contracts.md` section 5 calls "the existing fresh variant".

**Restart-nonce effect (`:895-910`)** becomes:

```ts
useEffect(() => {
  if (restartNonce <= lastSeenRestartNonceRef.current) return;
  lastSeenRestartNonceRef.current = restartNonce;
  if (terminatedRef.current || statusRef.current === "terminated") {
    if (restartTransfer) onSpawnSettledRef.current?.({ ok: false, error: "The session is terminated." });
    return;
  }
  if (restartTransfer) {
    // already confirmed in the rail; never the Kill & Restart prompt
    void restart(restartFresh, { transfer: true })
      .then((r) => onSpawnSettledRef.current?.(r))
      .catch((e) => onSpawnSettledRef.current?.({ ok: false, error: String(e) }));
  } else if (needsConfirm(statusRef.current)) {
    setConfirmKill(restartFresh ? "fresh" : "resume");
  } else {
    void restart(restartFresh);
  }
}, [restartNonce, restartFresh, restartTransfer, restart]);
```

Update the `biome-ignore` comment (it mentions `transferringRef`). Plain restart signals
(`transfer=false`) behave exactly as before. `onSpawnSettled` fires **only** for transfer restarts.

**Input blocking (the actual gates).** All writes to the PTY go through exactly two closures inside the
mount effect: `term.onData` (`:1132-1139`, calls `ptyWrite`) and `sendPaste` (`:974-983`, calls `ptyPaste`;
it also serves Ctrl+V, native paste, and file drops via `sendPasteRef`, `:1543`). Both start with
`if (inputBlocked(statusRef.current)) return;`. Change both to
`if (inputBlocked(statusRef.current) || blocksInput(transferRef.current)) return;`. Scroll gates
(`:1223, :1259, :1341`) are left alone (scrolling does not touch the JSONL). Also blur the terminal when the
import begins:
`useEffect(() => { if (blocksInput(transfer)) termRef.current?.blur(); }, [transfer?.phase])` (biome
dependency comment as needed). Other writers (dispatch from the board) are refused by the backend marker
(`04`).

**Render.** Remove `transferringTo` from: the `onClick` condition `:1676`, the `transferringTo ? … :`
branch `:1681-1693` (delete the branch; keep its `<> … </>` else-content un-nested), and `confirmKill &&
!transferringTo` `:1824` (→ `confirmKill && !transfer`). Change the status gate to
`{status !== "running" && !transfer && (` so the recovery overlays never stack on the transfer overlay, and
add, directly after the `↓ live` pill block (`:1655-1667`) and before the status-gated overlay (`:1669`):

```tsx
{transfer && (
  <TransferOverlay entry={transfer} onCancel={onCancelTransfer ?? (() => {})} />
)}
```

The overlay is outside any status gate: it covers `importing` while Claude is `running`, and `restarting`
whatever the status. When the entry is removed (settle) the overlay disappears and, if the spawn failed,
the gated spawn-failed overlay shows (same render batch: `spawn` set the status before the result was
reported).

**Spawn-failed copy** (replaces `:1785`): inside the spawn-failed branch render
`const c = spawnFailedCopy(spawnFailure?.cli ?? cli, spawnFailure?.error ?? null)` then
`<div className="pane-msg">{c.message}</div>{c.detail && <div className="pane-msg pane-msg-detail">{c.detail}</div>}`.
`spawnFailedCopy(cli, error)`:

- `error` non-empty and `error.startsWith(`${cli} did not start`)` (the strict-chain string from
  `00-contracts.md` §6) → `{ message: error, detail: null }`
- `error` non-empty otherwise → `{ message: `Failed to launch ${cli} — is it on PATH?`, detail: error }`
- `error` null/empty → `{ message: `Failed to launch ${cli} — is it on PATH?`, detail: null }`

`cli` here is the lowercase CLI name (`"claude"` / `"codex"`). The buttons keep their labels, but after a
**failed transfer launch** they must not fall back to the Default `resume --last`. The spawn-failed handler
therefore derives the restart from the captured failure through a pure helper in `transferPhase.ts`:

```ts
export function restartAfterFailure(
  failure: { transfer: boolean; fresh: boolean } | null,
  choice: "kill_restart" | "start_fresh",
): { fresh: boolean; transfer: boolean }
// failure === null or !failure.transfer -> today's behaviour: kill_restart => { fresh: false, transfer: false },
//                                          start_fresh => { fresh: true, transfer: false }
// failure.transfer (a transfer restart to Codex failed):
//   kill_restart => { fresh: failure.fresh, transfer: true }   // outcome Fresh => fresh=true (never resume --last);
//                                                              // outcome Imported => resume the persisted thread, strict
//   start_fresh  => { fresh: true, transfer: true }
```

`handleKillRestart()` (`:1620`) and the "Start fresh conversation" button call
`restart(a.fresh, { transfer: a.transfer })` with `a = restartAfterFailure(spawnFailure, choice)`; these calls
do not report `onSpawnSettled` (the transfer entry is already settled). So a worktree that was transferred
with a `Fresh` outcome and then failed to start restarts with `fresh=true` and `transfer=true` (the strict
fresh chain), exactly as `00-contracts.md` section 5 and `02` require. Plain (non-transfer) failures behave as
before. (An earlier draft claimed Kill & Restart "retries as a fresh launch"; for non-transfer failures it does
not, and this document does not claim it.) `cli=codex` stays persisted either way.

**Where the CLI kind comes from:** `WorktreeInfo.cli` (`ipc.ts:565`, "resolved default CLI for this worktree"),
available on the `w` object App already maps over (`App.tsx:1166-1190`), passed as the new `cli` prop.
During a transfer the override is `transfer.to` (see above).

### 8. `TransferOverlay.tsx` (new, presentational)

Props: `{ entry: TransferEntry; onCancel: () => void }`. Root: `<div className="pane-overlay" role="status" aria-live="polite" tabIndex={-1}>`
(reuses `.pane-overlay`, `.pane-spinner`, `.pane-msg`, `.pane-btn`, `.pane-btn-row`). Content:

| Phase | Lines |
|---|---|
| `importing`, `to="codex"` | spinner; `Transferring to Codex…`; `progressLabel(entry.progress)`; `Elapsed {formatElapsed(now - entry.startedAt)}`; `Input is paused while the conversation is imported.`; button **Cancel** (disabled unless `canCancel(entry)`; label `Cancelling…` when `entry.cancelling`) |
| `importing`, `to="claude"` | same, without the Cancel button and with `Transferring to Claude…` |
| `restarting` | spinner; `Starting {cliLabel(to)}…`; no button |

The elapsed clock is local: `const [now, setNow] = useState(Date.now())` with a 1 s `setInterval` while
mounted (clear on unmount). It does not depend on `transfer://progress`. If `entry.progress?.message` is a
non-empty string, render it as a `.pane-msg-detail` line under the label. On mount the Cancel button (or,
when absent, the overlay root) takes focus (`useEffect` + ref), which together with the input gate and
`term.blur()` guarantees keystrokes cannot reach the terminal.

`progressLabel` (`null` → `Preparing transfer…`):

| `progress.phase` | label |
|---|---|
| `preflight` | `Checking the Claude session…` |
| `starting_codex` | `Starting Codex…` |
| `initializing` | `Initializing Codex…` |
| `importing` | `Importing the conversation…` |
| `awaiting_completion` | `Waiting for Codex to finish the import…` |
| `reading_ledger` | `Checking the imported session…` |
| `persisting` | `Saving the transfer…` |
| `cancelling` | `Cancelling…` |

`formatElapsed(ms)`: `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}` with `s = Math.floor(max(ms,0)/1000)`.

`.pane-msg-detail` (in `App.css` after `.pane-msg`, `:745`): `font-size: 12px; color: var(--tx-3); max-width: 420px; text-align: center; overflow-wrap: anywhere;`.

### 9. Mid-turn warning

Detectable: `WorktreeSession.state` (`sessions.ts:30-39`, `"idle" | "busy" | "waiting" | "offline"`) from
`liveSessionsRef.current.get(worktree)?.state`, the same live map `injectHandoffWhenIdle` polls. `isMidTurn`
is `busy` or `waiting` (a `waiting` session is blocked on a permission prompt, also mid-turn). Applies to
Claude → Codex only. `MID_TURN_NOTE = "Claude is in the middle of a turn. That in-flight turn will not be carried over."`,
appended after a blank line to the existing confirm body (`App.tsx:543-547`). It is advisory: it never
blocks the transfer, and a stale `state` only affects whether the sentence appears.

### 10. Toast copy (exact strings)

| When | Text | Copy diagnostics action |
|---|---|---|
| `imported` + spawn ok | `Transferred ${worktree} to Codex.` | yes (so a successful transfer can be smoke-verified too) |
| `done` + spawn ok | `Transferred ${worktree} to Claude.` | no |
| `fresh` + spawn ok | `Couldn't carry the Claude history over (${reasonText}). Started a fresh Codex session.` where `reasonText` is used verbatim (the contract guarantees it never ends with a period) | yes |
| `fresh` via `fell_back` (`reasonText === null`) | `Codex couldn't resume the imported conversation. Started a fresh Codex session.` | yes |
| `cancelled` | `Transfer cancelled.` | no |
| `transferWorktree` rejected | the error string verbatim | codex only |
| spawn failed (`ok:false`) | `error` verbatim (the `"<cli> did not start: …"` string from `02`) | codex only |
| 120 s backstop | `${Label} did not report that it started within 120 seconds. Check the ${worktree} pane.` | codex only |
| re-entry (menu bypassed) | `A transfer is already in progress for this worktree.` | no |
| diagnostics `null` | `No diagnostics available.` | n/a |
| diagnostics copied | `Diagnostics copied.` | n/a |
| diagnostics failed | `Could not copy diagnostics: ${error}` | n/a |

The frontend never maps `TransferFailure` variants to copy; it uses `reasonText` only (contract §1).

### 11. Stale exit — decision procedure (reproduction test first)

The pane's gate calls `coalescer.end(ended)` on an `exit` message (`TerminalPane.tsx:497-507`). `end`
captures the coalescer's `epoch` at the time of the exit, and `reset()` (called by `restart()` at `:764`)
bumps it, so an exit that **arrives before** the reset can never end the pane (`frameCoalescer.ts:183-195`).
The remaining question is an exit from the *old* session arriving **after** the reset, while the same
attach channel (a restart keeps the existing attach) carries the *new* session.

Steps, in order:

1. In `src/lib/paneOutput.test.ts` add a `describe("exit around a restart")` that wires
   `createOutputGate` + `createFrameCoalescer` exactly like `newOutputGate` does (an `ended` spy; a fake
   `FrameScheduler`; `visible: true`) and writes:
   - `exit before the restart's reset does not end the pane` — gate open; push a frame; deliver
     `{type:"exit"}`; call `coalescer.reset()` before the scheduled flush; run the scheduler; assert
     `ended` not called. (Passes today; pins the existing protection.)
   - `exit that arrives after the restart's reset ends the pane (known gap)` — gate open; `coalescer.reset()`;
     deliver `{type:"exit"}`; run the scheduler; assert `ended` **was** called. (Passes today and documents
     that the frontend cannot tell an old exit from a new one without a generation.)
2. Backend check (read-only, time-box 30 min): in `src-tauri/src/backend/herdr/bridge.rs` (`out.exit()` at
   `:329`, `:346`) and the kill path (`pty_kill` → `backend/herdr/mod.rs`), determine whether the exit of
   generation N can be delivered to the attach sink after `pty_kill` for N has returned and the spawn of
   N+1 has begun.
3. **Condition for adding `gen`:** only if step 2 shows that ordering is possible. Then: `OutputMsg::Exit`
   becomes `Exit { gen: u64 }` (`host.rs:1432-1437`, sender `:1448`, wire-shape test `:2255-2258` updated to
   `{"type":"exit","gen":<n>}`); `ipc.ts:517` becomes `{ type: "exit"; gen: number }`; `PaneOutputTarget.exit`
   (`paneOutput.ts:14`) becomes `exit(gen: number): void`; the pane ignores an exit whose `gen` is not
   `genRef.current` when `genRef.current !== null`; the "known gap" test flips to
   `exit from a superseded generation is ignored`. Add the matching `paneOutput.test.ts` case.
4. If step 2 shows it cannot happen, or cannot be determined in the time-box: add no field. Leave the
   "known gap" test with a comment saying so, and list it under open questions.

## Tests (vitest)

Repo test setup: `yarn test` = `vitest run` (`package.json`), no `vitest.config`/`test` block in
`vite.config.ts`, so the default **node** environment; only `*.test.ts` files exist (no `.test.tsx`), and
neither jsdom nor `@testing-library/*` is installed. React components (`App`, `TerminalPane`, `Rail`,
`TransferOverlay`, `Toast`) therefore **have no component tests in this repo and this document adds no
component-test infrastructure**; all logic lives in pure modules and is tested there. Component wiring is
verified by `tsc`, `biome`, code review against this document, and the manual smoke list under Risks. IPC
tests follow `ipc.test.ts` (`vi.hoisted` transport mock, `vi.mock("./transport")`).

Fixtures: project `proj`, other project `other-proj`, worktrees `alpha`/`bravo`.

**`src/lib/transferPhase.test.ts`**

Reducer:

1. `begin adds an importing entry with no effects` — one entry, `phase:"importing"`, `progress:null`, `cancelling:false`, `result:null`, `startedAt` = given `now`; `effects` empty.
2. `begin on an existing key is a no-op` — same map object returned, no effects.
3. `progress updates an importing entry` — `progress` replaced; later event replaces earlier.
4. `progress with phase cancelling sets cancelling` — entry `cancelling:true`.
5. `progress for a restarting entry or unknown key is ignored` — same map object.
6. `cancel_requested marks a codex import cancelling` / `is ignored for to=claude, when already cancelling, and when restarting`.
7. `outcome imported moves to restarting and asks for a non-fresh restart` — `phase:"restarting"`, `result.kind:"imported"`, effects exactly `[restart{fresh:false}]` (no toast yet).
8. `outcome fresh moves to restarting and asks for a fresh restart` — effects `[restart{fresh:true}]`; `result` carries `reasonText`.
9. `outcome done asks for a non-fresh restart and the handoff injection` — effects `[restart{fresh:false}, inject_handoff]`.
10. `outcome cancelled removes the entry, toasts "Transfer cancelled." and settles` — effects `[toast, settle]`; toast `diagnostics:null`.
11. `failed removes the entry, toasts the error verbatim and settles` — for `to:"codex"` the toast carries `diagnostics`; for `to:"claude"` it does not.
12. `spawn_settled ok after imported toasts "Transferred alpha to Codex." and settles` — entry removed.
13. `spawn_settled ok after done toasts "Transferred alpha to Claude."`.
14. `spawn_settled ok after fresh toasts the fresh copy with diagnostics` — message equals `Couldn't carry the Claude history over (<reason>). Started a fresh Codex session.`.
15. `spawn_settled not ok removes the entry and toasts the error verbatim with diagnostics for codex` — effects `[toast(error), settle]`.
16. `resume_notice fell_back during restarting turns an imported result into the fallback fresh toast` — then `spawn_settled ok` yields the `Codex couldn't resume…` message with diagnostics.
17. `resume_notice fell_back after a fresh outcome does not toast twice` — dispatch outcome fresh, resume_notice(fell_back), spawn_settled ok: exactly one `toast` effect in the whole sequence, with the original `reasonText`.
18. `resume_notice without fell_back, for a claude transfer, or for an importing entry is ignored`.
19. `a second outcome or a second spawn_settled is a no-op` — no effects, no second `settle`.
20. `spawn_settled while importing is ignored` and `outcome while restarting is ignored`.
21. `backstop on a restarting entry toasts the timeout copy and settles` — message `Codex did not report that it started within 120 seconds. Check the alpha pane.`; entry removed. `backstop on an importing entry is ignored`.
22. `same worktree name in two projects is isolated` — `begin` for `proj/alpha` and `other-proj/alpha`; `outcome` + `spawn_settled` for one leaves the other untouched.

Helpers:

23. `freshToastMessage uses reasonText verbatim and the null-reason fallback copy` — `("Codex did not start in time")` → `Couldn't carry the Claude history over (Codex did not start in time). Started a fresh Codex session.`; `null` → `Codex couldn't resume the imported conversation. Started a fresh Codex session.`
24. `progressLabel covers all eight phases and null` — table above.
25. `formatElapsed formats m:ss and clamps negatives` (`0`, `59_999`, `60_000`, `150_000`, `-5`).
26. `canCancel` table — importing/codex/idle → true; `cancelling` true → false; progress `persisting` → false; progress `cancelling` → false; `to:"claude"` → false; `restarting` → false.
27. `blocksInput is true only while importing` (including `null`/`undefined` → false).
28. `isMidTurn is true for busy and waiting only` (`idle`, `offline`, `undefined` → false).
29a. `restartAfterFailure repeats the transfer flavour` — `(null, "kill_restart")` → `{fresh:false, transfer:false}`; `(null, "start_fresh")` → `{fresh:true, transfer:false}`; `({transfer:false, fresh:true}, "kill_restart")` → `{fresh:false, transfer:false}`; `({transfer:true, fresh:true}, "kill_restart")` → `{fresh:true, transfer:true}` (never resume --last after a Fresh outcome); `({transfer:true, fresh:false}, "kill_restart")` → `{fresh:false, transfer:true}`; `({transfer:true, fresh:false}, "start_fresh")` → `{fresh:true, transfer:true}`.
29b. `restartPlan skips confirm-kill for a transfer restart and honours fresh` — new pure helper in `transferPhase.ts`: `restartPlan(signal: RestartSignal, status: PaneStatus): { confirm: boolean; fresh: boolean; transfer: boolean }`, called by TerminalPane's restart-nonce effect instead of inlining the decision. Cases: `({transfer:true, fresh:false}, "running")` → `{confirm:false, fresh:false, transfer:true}`; `({transfer:true, fresh:true}, "running")` → `{confirm:false, fresh:true, transfer:true}`; `({transfer:false, fresh:false}, "running")` → `{confirm:true, …}` (existing plain-restart behaviour); `({transfer:false, fresh:true}, "ended")` → `{confirm:false, fresh:true, transfer:false}`.
29c. `shouldStartTransfer rejects re-entry` — new pure helper `shouldStartTransfer(map, key): boolean` used as `onTransfer`'s first statement (early-return when false). No entry → true; an entry in any phase (`importing`, `restarting`) for the same key → false; an entry for another key (incl. same worktree name in another project) → true.
29. `spawnFailedCopy names the cli, passes the strict-chain string through, and moves other errors to detail` — `("codex","codex did not start: x (tried: …)")` → message is the string, `detail:null`; `("codex","boom")` → message `Failed to launch codex — is it on PATH?`, detail `boom`; `("claude", null)` → message names `claude`, `detail:null`; a Claude string passed with cli `codex` is not treated as strict-chain.

Controller (`vi.useFakeTimers()`, restored in `afterEach`):

30. `no timer is armed while importing` — `begin`; advance 10 minutes; map still `importing`, no effects run, `getMap().get(key).phase === "importing"`.
31. `backstop fires 120 s after entering restarting` — outcome imported; at 119_999 ms nothing; at 120_000 ms `run` received `toast` then `settle`, `onChange` saw the empty map. Also asserts `RESTARTING_BACKSTOP_MS === 120_000`.
32. `spawn_settled before the deadline cancels the backstop` — advance past 120 s afterwards: no extra effects.
33. `onChange runs before effects and only when the map changed` — record call order; a no-op event does not call `onChange`.
34. `dispose clears pending timers but the controller stays usable` — dispose then advance: no backstop; a later outcome re-arms a timer.
35. `a custom backstopMs is honoured`.

**`src/lib/resumeNotice.test.ts`** (update + add):

36. update `nextRestartSignal bumps the counter and carries the latest flavour` — expectations become `{n:1, fresh:true, transfer:false}` and `{n:2, fresh:false, transfer:false}`; fix the stale comment.
37. `nextRestartSignal carries transfer when asked` — `nextRestartSignal(undefined, false, true)` → `{n:1, fresh:false, transfer:true}`; a following plain bump resets `transfer` to `false`.

**`src/lib/laneState.test.ts`** (add):

38. `transferDisabledReason` — `{false,false}` → `null`; `{codexMissing:true,busy:false}` → `"Codex isn't installed"`; `{false,true}` → `"A transfer is already in progress"`; `{true,true}` → the busy text.

**`src/lib/ipc.test.ts`** (update + add, transport mock):

39. `ptySpawn omits the transfer key unless true` — the existing `defaults fresh to false` payload assertion stays unchanged (no `transfer` key); additionally an explicit `transfer` of `false` and of `undefined` produce a payload with no `transfer` key (`not.toHaveProperty("transfer")`).
40. `ptySpawn forwards transfer when true` — 7th positional arg `true` → payload `{ ..., fresh: false, transfer: true }`.
41. `transfer wrappers pass project then worktree` — each of `previewClaudeTransfer`, `transferWorktree` (also `targetCli`), `injectHandoffSummary`, `cancelTransfer`, `transferSettled`, `transferDiagnostics` calls the right command name with `{ project, worktree[, targetCli] }`; `transferWorktree` resolves the outcome object unchanged; `transferDiagnostics` resolves `null` unchanged.
42. `onTransferProgress listens on transfer://progress` — `transport.listen` called with that channel; the callback receives the payload.

**`src/lib/paneOutput.test.ts`** (write first): the two stale-exit tests from "Stale exit", plus the `gen` test only if the condition is met.

## Risks and mitigations

- **Blocking input during the import** could annoy a mid-thought user. Mitigation: Cancel is always
  available before persist and the import deadline is bounded (~150 s, `01-import.md`).
- **Backend/frontend contract drift.** Mitigation: all shapes are in `00-contracts.md`; wrapper tests assert
  the invoke payloads and the transfer tests use the contract's wire examples.
- **Overlay outside the running gate** touches a heavy component (`TerminalPane.tsx`). Mitigation: the
  overlay is a separate small component; every decision is in `transferPhase.ts`.
- **No component tests exist.** Mitigation: wiring is mechanical and reviewed against sections 4 and 7;
  manual smoke below.
- **Success toast ordering.** The backend emits the resume event inside the spawn, before `pty_spawn`
  resolves, but Tauri event vs invoke-result delivery order is still not strictly guaranteed; if the
  resume event ever lands after `spawn_settled`, the user gets the plain success toast instead of the
  fallback toast. Acceptable degradation (the pane still prints the dim "could not resume" line from
  `resumeNoticeLine`).
- **Manual smoke owed on macOS:** transfer idle; transfer mid-turn (warning shown); transfer from a
  non-active project; cancel mid-import; type during the import (nothing reaches Claude); force a failed
  import (fresh toast + Copy diagnostics); force a spawn failure (spawn-failed screen names `codex` and
  shows the backend text); menu item disabled during a transfer. Run **Copy diagnostics after every one of
  these transfers** (success included) and attach the trails; they are the evidence for section C's
  herdr-signal assumptions in `02-launch-and-env.md`.

## Implementation order within this doc

1. **Stale-exit repro tests** in `paneOutput.test.ts` (they pass against current code; do not change
   production code yet). Do the backend check (section 11 step 2) and record the decision.
2. `ipc.ts`: types, wrappers, `onTransferProgress`, `ptySpawn` `transfer`; add the `ipc.test.ts` cases
   (tests 39-42; the existing ptySpawn payload tests are untouched). Contract-dependent: if the backend is not merged, this still compiles and tests pass.
3. `resumeNotice.ts` (`transfer` field + `nextRestartSignal`) and its tests (36-37).
4. `laneState.ts` `transferDisabledReason` and test (38).
5. `transferPhase.ts` + `transferPhase.test.ts` (tests 1-35). Everything after this consumes it.
6. `Toast.tsx` action support.
7. `TransferOverlay.tsx` + `.pane-msg-detail` CSS.
8. `TerminalPane.tsx`: props/refs, `spawn`/`restart` results, restart-nonce effect, input gate, spawn-failed
   copy, overlay render, remove `transferringTo`. Run `tsc` (`yarn build` type-check step) to catch unused
   code.
9. `Rail.tsx`: `transferBusyKeys` prop and menu disable.
10. `App.tsx`: delete the old state and effects, add the controller, progress and resume subscriptions,
    rewrite `onTransfer`, `bumpRestartNonce` 4th param, `injectHandoffWhenIdle` project arg, pane and Rail
    props.
11. `.harness/app.tsx` mocks; run the harness for each outcome kind (no herdr contact).
12. `yarn test`, `yarn lint`, `yarn build`; grep the diff for plan identifiers
    (`git grep -nE '#[0-9]{3,4}' -- <changed files>`) and for real project names.

## Decisions this document depends on

- The frontend phase machine **replaces both existing clearing effects** (offline→live effect and the 20 s
  timer), with input blocked during the import and cancel allowed only before persist (and only for
  Claude -> Codex).
- The `restarting` backstop is 120 s (the strict launch chain's budget is ~100 s; the backend marker outlives
  it at 180 s).
- After a failed transfer launch the recovery buttons repeat the transfer flavour (`fresh=true`,
  `transfer=true` after a `Fresh` outcome); they never fall back to `resume --last`.
- The controller, not React state, is the source of truth for transfer entries; React state is a render
  mirror.
- Fresh travels as an explicit `fresh: true` on `RestartSignal`; never inferred from a cleared thread id.
- Transfer travels as an explicit `transfer: true` on `RestartSignal` and as `ptySpawn`'s trailing positional
  `transfer` argument (omitted from the payload unless true); the pane no longer reads an ambient ref.
- Toasts for imported/fresh/done are emitted at `spawn_settled`, not at outcome time.
- A failed import always switches to Codex; the Fresh fallback starts Codex blank; Claude stays live only
  until the outcome is known.
- An explicit `project` (the pane's `projectName` / `warmRef.current.active`) is threaded through the
  transfer IPC commands.
- The mid-turn warning is detected from `WorktreeSession.state` (`busy`/`waiting`) and is advisory.
- A stale-exit `gen` field is added only if the backend check (section 11) shows the ordering is possible.
- #29 is a likely symptom of R3; recount after landing before closing it.
- Launch changes are opt-in via `LaunchPolicy::TransferStrict` (`02`), so the pane passes `transfer: true`
  only on transfer restarts.

## Open questions

1. **Recovering refusal during a transfer restart** is reported as `{ok:true}` (recovery resumes the
   session from the persisted codex state under the default policy, not strict). Alternative: report
   `{ok:false}` with a "herdr is recovering" error. Chosen: `ok:true` to avoid a scary toast for a
   self-healing case; revisit if `02` wants recovery to use the strict policy for a just-transferred
   worktree.
2. **`fell_back` reason text.** The backend event has no reason, so the frontend uses fixed fallback copy.
   If `02` can supply a short reason on the event, add an optional field and use it.
3. **Stale-exit `gen`** depends on the backend ordering check (section 11).
4. **Other PTY writers** (Board dispatch, backend injections) are covered by the backend marker (`04`),
   not by this document; confirm `04` makes `dispatch_apply` and the injection path refuse during import.
