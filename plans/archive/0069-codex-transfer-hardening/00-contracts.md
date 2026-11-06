# 0069 / 00 — Shared contracts

Part of the 0069 "Harden Claude → Codex transfer" effort (proposal: `../0069-codex-transfer-hardening.md`).
Siblings: `01-import.md`, `02-launch-and-env.md`, `03-frontend.md`, `04-scoping-locking-diagnostics.md`.

This file pins the cross-document interfaces so the four implementation documents can proceed in parallel
without guessing. If a sibling needs to change anything here, change it here first. Paths are under
`src-tauri/src/` unless noted. Line numbers are as of `e9c5d27` (HEAD `4178d29` only added plan files, so
they still hold). Every rule below was checked against the repo's existing conventions; the "Changelog" at the
bottom lists what was corrected.

## Overall goal (restated)

"Transfer to Codex" must always end with a running Codex session in the pane: resuming the imported Claude
conversation if the import works, otherwise a fresh Codex conversation. It never leaves the app wedged,
never shows a misleading "Kill & Restart" screen, never drops the user into a bare shell claiming to be
Codex. When something goes wrong both the user and we (debugging a macOS machine remotely) can tell what.

## Repo conventions this file follows (verified)

- **Commands** are free `#[tauri::command]` functions listed in the single `tauri::generate_handler![...]`
  inside `host::run()` (`host.rs:2147`). The transfer commands live in `transfer.rs` and are listed as
  `crate::transfer::<name>` (`host.rs:2215-2217`). New commands go right after
  `crate::transfer::inject_handoff_summary`. Tauri converts Rust `snake_case` argument names to camelCase on
  the JS side (`target_cli` -> `targetCli`) and `Option<T>` args may be omitted.
- **Managed state** is registered with `.manage(...)`: global singletons on the builder chain next to
  `DispatchGate` (`host.rs:2083`), others in `.setup(|app| ...)`. Commands take `State<'_, T>`.
- **Events** are emitted from commands with `use tauri::Emitter as _; app.emit("<ns>://<name>", payload)`
  (`transfer.rs` does this for `worktrees://changed`), payload any `Serialize`. The webview side uses the
  `listen<T>(channel, cb)` helper at the top of `src/lib/ipc.ts` (`ipc.ts:15`) wrapped in a named exported
  function returning `Promise<UnlistenFn>` (e.g. `onWorktreesChanged`, `ipc.ts:929`).
- **IPC enums** serialize either `#[serde(tag = "type", rename_all = "lowercase")]` (`OutputMsg`,
  `host.rs:1431`) or `#[serde(tag = "kind", ...)]` (`DiffBase`, `config.rs:1068`); payload structs for the
  newer features use `#[serde(rename_all = "camelCase")]` (`diff/`). Older transfer types
  (`ConfirmPreview`) have no rename and so are snake_case on the wire (`first_user_line`); they stay as they
  are. The new types below use `kind` tagging + camelCase fields, matching `DiffBase`'s tag name and the
  `diff/` field convention.
- **Project identity** everywhere in IPC and in `SessionKey { project, worktree }` is the resolved
  `[project].name` (`ProjectConfig.project.name`), **not** the registry map key of `UserConfig.projects`.
  `launch_for` (`host.rs:1319`) matches `pc.project.name != key.project` while iterating
  `user.projects.values()`. "By name" in this effort means exactly that.

## Who owns what

| Contract | Defined here | Produced by | Consumed by |
|---|---|---|---|
| `TransferOutcome`, `TransferFailure`, `TransferPhase`, `SkipReason` | section 1 | `01-import.md` (import-side), `02-launch-and-env.md` (launch-side failures) | `03-frontend.md` |
| IPC command signatures | section 2 | `01-import.md` (`transfer_worktree`), `04-scoping-locking-diagnostics.md` (rest) | `03-frontend.md` |
| `transfer://progress` event | section 3 | `01-import.md` | `03-frontend.md` |
| `TransferRegistry` managed state | section 4 | `04-scoping-locking-diagnostics.md` | `01-import.md` (cancel flag), `02-launch-and-env.md` (trail), `03-frontend.md` (via IPC) |
| `RestartSignal.transfer`, `ptySpawn(..., transfer)`, `onSpawnSettled` | section 5 | `03-frontend.md` (UI side), `02-launch-and-env.md` (Rust side of `transfer`) | `02-launch-and-env.md` |
| Launch error / fallback event shape | section 6 | `02-launch-and-env.md` | `03-frontend.md` |
| Trail entry shape and helper | section 7 | `04-scoping-locking-diagnostics.md` | `01`, `02` (emitters) |

## 1. Outcome types

Serde convention: internally tagged with `kind`, variant tags `snake_case`, field names `camelCase` on the
wire. Exact attributes. `rename_all_fields` requires serde >= 1.0.185; the repo's `Cargo.lock` pins serde
1.0.228 (verified, `Cargo.lock:3475`), so it is available. (If the lock ever regressed below 1.0.185, the
fallback is a per-variant `#[serde(rename_all = "camelCase")]` on each struct-like variant.) All section-1 types
derive `Debug, Clone, PartialEq, Eq, Serialize`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum TransferOutcome {
    Imported  { thread_id: String },                          // Claude->Codex: resume the imported thread
    Fresh     { reason: TransferFailure, reason_text: String }, // Claude->Codex: import failed; worktree is Codex, start fresh
    Done,                                                      // Codex->Claude: summary handoff complete (unchanged semantics)
    Cancelled,                                                 // user cancelled during import; nothing changed, still Claude
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum TransferFailure {
    CodexNotFound      { message: String, stderr_tail: String },
    CodexTooOld        { version: Option<String>, message: String, stderr_tail: String },
    SessionNotDetected { message: String, stderr_tail: String },
    Skipped            { reason: SkipReason, message: String, stderr_tail: String }, // no-op, deferred, continued-in
    Timeout            { phase: TransferPhase, message: String, stderr_tail: String },
    ProtocolError      { message: String, stderr_tail: String },
    Other              { message: String, stderr_tail: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason { NoOp, Deferred, ContinuedIn }            // "no_op" | "deferred" | "continued_in"

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase { Initialize, Import, Completion }      // "initialize" | "import" | "completion"

impl TransferFailure {
    /// The single place plain-language copy lives; becomes `TransferOutcome::Fresh.reason_text`.
    pub fn user_copy(&self) -> String;
}
```

Home: `transfer.rs` (they are produced by the import and consumed by the commands there). `Serialize` only
(they never come back from the webview). `Option<String>` serializes to `null` (no `skip_serializing_if`).
The nested `reason` field inside `TransferOutcome::Fresh` and the `reason` field inside
`TransferFailure::Skipped` live at different levels and do not clash. Field names `phase`/`reason`/`version`
do not collide with the `kind` tag.

Wire examples (what the frontend sees):

```ts
type TransferOutcome =
  | { kind: "imported"; threadId: string }
  | { kind: "fresh"; reason: TransferFailure; reasonText: string }
  | { kind: "done" }
  | { kind: "cancelled" };

type TransferFailure =
  | { kind: "codex_not_found"; message: string; stderrTail: string }
  | { kind: "codex_too_old"; version: string | null; message: string; stderrTail: string }
  | { kind: "session_not_detected"; message: string; stderrTail: string }
  | { kind: "skipped"; reason: "no_op" | "deferred" | "continued_in"; message: string; stderrTail: string }
  | { kind: "timeout"; phase: "initialize" | "import" | "completion"; message: string; stderrTail: string }
  | { kind: "protocol_error"; message: string; stderrTail: string }
  | { kind: "other"; message: string; stderrTail: string };
```

Pinned wire JSON (the Rust test `transfer_outcome_wire_shape` in `transfer.rs`, owned by `01-import.md`,
asserts these byte-for-byte via `serde_json::to_value`; add them as `json!` literals):

- `Imported { thread_id: "t1" }` -> `{"kind":"imported","threadId":"t1"}`
- `Done` -> `{"kind":"done"}`; `Cancelled` -> `{"kind":"cancelled"}`
- `Fresh { reason: Skipped { reason: ContinuedIn, message: "m", stderr_tail: "" }, reason_text: "x" }` ->
  `{"kind":"fresh","reason":{"kind":"skipped","reason":"continued_in","message":"m","stderrTail":""},"reasonText":"x"}`
- `CodexTooOld { version: None, .. }` -> contains `"version":null`
- `Timeout { phase: Completion, .. }` -> contains `"phase":"completion"`

Rules:

- `reason_text` is the plain-language, one-sentence reason (for example "Codex did not recognise the Claude
  session"), **never ending with a period** (the frontend embeds it in a parenthesis and uses it verbatim;
  `01` pins this with a test over every variant), produced by a single `TransferFailure::user_copy()` function in Rust. The frontend never maps
  variants to copy itself; it renders `reasonText`. The raw `message` and `stderrTail` are for diagnostics,
  never for the toast body.
- `Fresh` is returned only after `persist_worktree_cli` succeeded with `cli=codex`, `codex_thread_id=Clear`.
  It is never returned for `CodexNotFound` (a fresh Codex could not start either; see `Err` below).
- `Imported` is returned only after persist succeeded with `cli=codex`, `codex_thread_id=Set(id)`.
- `Cancelled` is returned only before persistence; nothing was changed.
- `Err(String)` from `transfer_worktree` means "nothing changed": session terminated, worktree/project not
  resolvable, persisting failed, a transfer is already in progress for this worktree, the confirmed `jsonl`
  is not a transcript of this worktree, or (Claude -> Codex) Codex cannot be found — the import failed with
  `CodexNotFound` or the Codex environment could not be resolved. In the Codex-missing case the `Err` text is
  `TransferFailure::user_copy()` of `CodexNotFound` ("Codex could not be found"), nothing is persisted and the
  live Claude session keeps running. The frontend toasts it and clears the phase entry.
- `Done` is the Codex -> Claude result; its semantics are unchanged.

## 2. IPC commands

All take the explicit `project` (the resolved `[project].name`, see conventions) then `worktree`. None of
them calls `resolve_active()`. Rust shows the full real parameter list; parameters typed `State`/`AppHandle`
are injected by Tauri and are **not** IPC arguments.

```rust
// transfer.rs — existing commands, changed:
#[tauri::command]
pub async fn preview_claude_transfer(project: String, worktree: String)
    -> Result<ConfirmPreview, String>;                 // return type unchanged (ConfirmPreview, snake_case fields)

#[tauri::command]
pub async fn transfer_worktree(
    project: String, worktree: String, target_cli: CliKind,        // CliKind: "claude" | "codex"
    jsonl: Option<String>,   // Claude->Codex: the ConfirmPreview.path the user confirmed; omitted = newest pick
    gate: State<'_, DispatchGate>, client: State<'_, SharedBackend>,
    registry: State<'_, TransferRegistry>, app: tauri::AppHandle,
) -> Result<TransferOutcome, String>;                   // may take up to ~150 s (Claude->Codex)

#[tauri::command]
pub async fn inject_handoff_summary(project: String, worktree: String,
    client: State<'_, SharedBackend>) -> Result<bool, String>;     // return type unchanged: Ok(false) = nothing pending

// transfer.rs — new commands (synchronous, short):
#[tauri::command]
pub fn cancel_transfer(project: String, worktree: String, registry: State<'_, TransferRegistry>)
    -> Result<(), String>;      // sets the cancel flag; Ok(()) even if nothing in flight or already committed.
                                // Meaningful only for Claude->Codex imports; the UI offers Cancel only for to === "codex".
#[tauri::command]
pub fn transfer_settled(project: String, worktree: String, registry: State<'_, TransferRegistry>)
    -> Result<(), String>;      // clears a COMMITTED transferring marker; idempotent. A no-op while the
                                // entry is uncommitted: the import phase is owned by the backend guard.
#[tauri::command]
pub fn transfer_diagnostics(project: String, worktree: String, registry: State<'_, TransferRegistry>)
    -> Result<Option<String>, String>;   // scrubbed text of the last trail, None if there is none
```

Registration in `host.rs` `generate_handler![...]` (after `crate::transfer::inject_handoff_summary`):
`crate::transfer::cancel_transfer, crate::transfer::transfer_settled, crate::transfer::transfer_diagnostics`.
Managed state: `.manage(crate::transfer_registry::TransferRegistry::global())` on the builder chain right after
`.manage(crate::dispatch::DispatchGate::default())` (`host.rs:2083`); the module is declared
`mod transfer_registry;` in `lib.rs`.

TypeScript wrappers in `src/lib/ipc.ts` (currently `:1047-1068`, no project). `ConfirmPreview` is the
existing interface at `ipc.ts:1036` (not renamed); `CliKind` is `ipc.ts:23`:

```ts
previewClaudeTransfer(project: string, worktree: string): Promise<ConfirmPreview>
  // invoke("preview_claude_transfer", { project, worktree })
transferWorktree(project: string, worktree: string, targetCli: CliKind, jsonl?: string): Promise<TransferOutcome>
  // invoke("transfer_worktree", { project, worktree, targetCli, jsonl? })  — the `jsonl` key is omitted when undefined
injectHandoffSummary(project: string, worktree: string): Promise<boolean>
  // invoke("inject_handoff_summary", { project, worktree })
cancelTransfer(project: string, worktree: string): Promise<void>
  // invoke("cancel_transfer", { project, worktree })
transferSettled(project: string, worktree: string): Promise<void>
  // invoke("transfer_settled", { project, worktree })
transferDiagnostics(project: string, worktree: string): Promise<string | null>
  // invoke<string | null>("transfer_diagnostics", { project, worktree })
```

Behavioural contract:

- **Confirmed transcript.** When `jsonl` is given (Claude -> Codex), it must be an existing `.jsonl` regular
  file directly inside this worktree's Claude project dir (the dir `find_live_claude_jsonl` scans); anything
  else returns `Err` before anything runs. That exact file is imported. Absent, the newest pick is used.
  Ignored for Codex -> Claude.
- `transfer_worktree` calls `registry.begin(project, worktree)` first. If a transfer is already registered
  (and not expired) it returns `Err("a transfer is already in progress for this worktree")` without side
  effects.
- **Commit order (Claude -> Codex).** Under the dispatch gate the backend calls
  `registry.mark_committed(..)` **before** `persist_worktree_cli`; it returns `bool` and performs the cancel
  check atomically (`false` = a cancel was already requested, nothing is committed, the backend returns
  `Ok(Cancelled)`). After it returns `true`, `request_cancel` is a no-op.
- After a successful persist the marker stays set until `transfer_settled` (or the backend backstop).
  After `Cancelled` or any `Err`, the marker is released by the backend itself (guard drop), and a following
  `transfer_settled` is harmless. `transfer_settled` never clears an uncommitted entry, so a settle sent
  for a refused or failed `begin` cannot release another, still-importing transfer's marker. **If persisting fails after `mark_committed` returned `true`, the backend
  calls `registry.settle(..)` itself before returning `Err`**, so an `Err` never leaves a transferring marker
  (the guard drop alone would not release a committed marker).
- **Cancel scope.** `cancel_transfer` has an effect only for Claude -> Codex imports. The Codex -> Claude path
  does not observe the cancel flag (it calls `mark_committed` and ignores a `false`, whose only consequence is
  that the marker is released at guard drop instead of at `transfer_settled`).
- The backend emits `worktrees://changed` after a successful transfer exactly as today.
- `transfer_diagnostics` scrubs in the backend (`$HOME` -> `~`, prompt content dropped) so the frontend only
  copies the string to the clipboard (resolved ambiguity 2).
- The `dispatch_apply` command keeps its IPC signature; it gains an injected
  `registry: State<'_, TransferRegistry>` parameter (not an IPC argument) and refuses while the target
  worktree is transferring (see `04`).

## 3. `transfer://progress` event

Emitted by the import (`01-import.md`) with `app.emit("transfer://progress", &payload)` (`use tauri::Emitter
as _;`, from the `AppHandle` the command receives; clone it into the blocking task, it is `Send + Clone`). The
import runs on `spawn_blocking`, so the emitter closure it is given is `Fn(..) + Send + Sync` (an `AppHandle`
clone satisfies this).
Payload (camelCase on the wire):

```rust
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    pub project: String,
    pub worktree: String,
    pub phase: ProgressPhase,       // wire snake_case
    pub message: Option<String>,    // human-readable detail, e.g. Codex's `import/progress` text; null when none
    pub elapsed_ms: u64,            // since transfer_worktree's registry `begin` (guard.elapsed_ms())
    pub deadline_ms: u64,           // overall import deadline (const in transfer.rs, 150_000)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    Preflight, StartingCodex, Initializing, Importing, AwaitingCompletion,
    ReadingLedger, Persisting, Cancelling,
}
```

```ts
type TransferProgress = {
  project: string; worktree: string;
  phase: "preflight" | "starting_codex" | "initializing" | "importing"
       | "awaiting_completion" | "reading_ledger" | "persisting" | "cancelling";
  message: string | null; elapsedMs: number; deadlineMs: number;
};
// src/lib/ipc.ts — follows onWorktreesChanged (ipc.ts:929):
export function onTransferProgress(cb: (p: TransferProgress) => void): Promise<UnlistenFn> {
  return listen<TransferProgress>("transfer://progress", cb);
}
```

The frontend filters by `${project}/${worktree}`. The event is advisory; the overlay must work without it
(`importing` ends only when `transferWorktree` settles). The `restarting` phase is frontend-only and never
appears in this event. `guard.elapsed_ms()` is provided by the registry guard (section 4).

## 4. Managed state: `TransferRegistry`

Exact public API; implementation details, internal fields and tests are in `04-scoping-locking-diagnostics.md`
section F (authoritative for internals). Module `src-tauri/src/transfer_registry.rs`.

```rust
#[derive(Clone)]                       // cheap handle: Arc inside
pub struct TransferRegistry { /* Arc<Inner> */ }
impl TransferRegistry {
    pub fn new() -> Self;              // isolated instance (tests)
    pub fn global() -> Self;           // the process-wide instance (a clone of the same Arc each call)
    pub fn begin(&self, project: &str, worktree: &str) -> Result<TransferGuard<'_>, AlreadyTransferring>;
    pub fn request_cancel(&self, project: &str, worktree: &str) -> bool;
    pub fn mark_committed(&self, project: &str, worktree: &str) -> bool;   // false = cancelled, not committed
    pub fn is_transferring(&self, project: &str, worktree: &str) -> bool;
    pub fn settle(&self, project: &str, worktree: &str);
    pub fn push_trail(&self, project: &str, worktree: &str, stage: TrailStage, detail: serde_json::Value);
    pub fn last_trail(&self, project: &str, worktree: &str) -> Option<TrailSnapshot>;
}
#[derive(Debug, PartialEq, Eq)] pub struct AlreadyTransferring;
pub struct TransferGuard<'a> { /* &'a TransferRegistry, owned key strings, id, Arc<AtomicBool> cancel, started Instant */ }
// TransferGuard MUST be `Send` (it is held across `.await`s in the async `transfer_worktree`): it holds only a
// shared reference to the Arc-backed registry, owned strings and an `Arc<AtomicBool>`; no `MutexGuard`.
impl TransferGuard<'_> {
    pub fn cancel_flag(&self) -> Arc<AtomicBool>;  // polled by AppServer in ~250 ms slices
    pub fn elapsed_ms(&self) -> u64;               // since begin
}
impl Drop for TransferGuard<'_> { /* releases the marker unless mark_committed was called */ }
```

Semantics (all synchronous, short critical sections, never held across IO or `.await`):

- `begin`: refuses (`AlreadyTransferring`) when an entry for the key has `transferring` and has not expired;
  otherwise (re)creates the entry: fresh `cancel` flag (false), `committed=false`, `transferring=true`,
  `marker_expires_at = now + 300 s`, empty trail. The guard releases the marker on drop **unless**
  `mark_committed` was called for that transfer.
- `request_cancel`: sets the flag only if the entry is `transferring && !committed`; returns whether
  honoured (after a successful `mark_committed` it is always `false`). `cancel_transfer` ignores the bool and returns `Ok(())`.
- `mark_committed`: under the registry mutex, atomically with the cancel check. If the entry exists, is
  `transferring`, and its cancel flag is **not** set: `committed=true`, `marker_expires_at = now + 180 s`,
  return `true`. If the cancel flag is set (or there is no live entry): change nothing, return `false` (the
  Claude -> Codex caller then returns `Ok(Cancelled)` and the guard drop releases the marker). It is called
  **before** persist, under the dispatch gate. If persist then fails the caller calls `settle`.
- `is_transferring`: true while `transferring && now < marker_expires_at`. An expired marker is cleared on
  observation (and logged as `settle`/`backstop`).
- `settle`: `transferring=false`; idempotent; keeps the trail.
- Single source of truth: the cancel flag, the transferring marker and the trail live in this one map.
- Process-wide access: backend code with no `AppHandle` (the herdr spawn chain in `02`) uses the free
  helper `crate::transfer_registry::trail(...)` (section 7), which goes through `TransferRegistry::global()`.

## 5. Frontend-facing launch signals

Owned by `03-frontend.md`, semantics defined by `02-launch-and-env.md`:

```ts
// src/lib/resumeNotice.ts:16 — today { n: number; fresh: boolean }. `fresh` ALREADY exists
// (meaning: the user chose "fresh start"); only `transfer` is new.
interface RestartSignal { n: number; fresh: boolean; transfer: boolean }
// TerminalPane prop
onSpawnSettled?: (r: { ok: true } | { ok: false; error: string }) => void
```

- `transfer: true` means the pane must skip confirm-kill and treat the restart as part of a transfer.
- `fresh: true` means the pane must launch a fresh (non-resuming) agent. `restart` is
  `restart(fresh = false)` (`TerminalPane.tsx:748`): **`restart(true)` is fresh, `restart(false)` is resume**.
  Today the transfer branch hard-codes `restart(false)` (`TerminalPane.tsx:904`); it must become
  `restart(fresh)` using the signal's flag (the `restartFresh` prop that the non-transfer branch already
  passes at `:908`). Fresh is never inferred from a cleared thread id (that resolves to `Continue` =
  `resume --last`).
- On a transfer restart the backend spawn runs with `LaunchPolicy::TransferStrict` (`02-launch-and-env.md`).
  How the pane signals "transfer" to the backend: `pty_spawn` (`host.rs:1381`, params today
  `client, project, id, cwd, rows, cols, command: Option<String>, fresh: Option<bool>`) gains one more
  trailing parameter `transfer: Option<bool>` (absent = false); `spawn_pty` (`host.rs:1218`) gains
  `transfer: bool` after `fresh`; the TS wrapper appends it positionally after `fresh`:

  ```ts
  // src/lib/ipc.ts:582 — today (project, id, cwd, size, command?, fresh?)
  ptySpawn(project, id, cwd, size, command?, fresh?, transfer?: boolean): Promise<number>
  //   invoke("pty_spawn", { project, id, cwd, ...sizeArgs(size), command: command ?? null,
  //                         fresh: fresh ?? false, ...(transfer ? { transfer: true } : {}) })
  ```

  The `transfer` key is **omitted from the invoke payload unless it is `true`** (a missing key deserializes
  to `None`, i.e. false). This keeps the existing exact-payload `ipc.test.ts` cases green unchanged.
  `ptySpawn` stays positional (never an options object).

  The host maps `transfer=true` to `AgentLaunch.policy = TransferStrict` **for Codex worktrees only**:
  `transfer: true` for a Claude worktree (the Codex -> Claude restart) stays `Default`. When an explicit
  `command` is given `spawn_pty` ignores `transfer` (that path bypasses `AgentLaunch`). `session_launch` is
  unchanged. `02` owns the Rust side, `03` owns passing it.
- **Restarting after a failed transfer launch.** If a spawn fails after a transfer restart to Codex that had
  no thread id (outcome `Fresh`; or any spawn-failed after a transfer restart to Codex with no thread id), the
  pane's "Kill & Restart" restarts with `fresh = true` **and** `transfer = true` (strict, fresh chain), never
  the Default `resume --last`. With an `Imported` outcome the thread id is persisted, so the same button
  resumes it (strict). Owned by `03` (spawn-failed handler); `02` notes the consequence.

## 6. Launch failure and fallback shapes

- **Final spawn failure** (`TransferStrict` chain exhausted): `ptySpawn` rejects with a string. The pane
  shows it verbatim as the error text, with the CLI name taken from the worktree's current `cli` (never
  hard-coded). Format, produced by `02-launch-and-env.md`:
  `"<cli> did not start: <last herdr/Codex message> (tried: resume <thread-id> -> fresh -> fresh without optional flags)"`.
  The existing `spawn_err_kind` (`host.rs:1197`) passes unknown errors through as `other.to_string()` and
  only maps `Recovering`/`Terminated` to tokens; the final-failure string must therefore be returned as a
  `PtyBackendError::Other(String)` so it passes through unchanged. The pane keeps a separate
  `spawnError: string | null` next to the kind.
- **Strict-chain fallback while the spawn still succeeds** (resume failed, fresh worked): reported through
  the existing backend resume event on channel `pty://resume/<project>/<worktree>` (`session_event("resume",
  key)`, `backend/herdr/mod.rs:~925`), payload `{ "outcome": "resumed"|"continued"|"fresh", "fell_back":
  bool }` (snake_case; TS `ResumeNotice`, `resumeNotice.ts`). That event is emitted only when
  `fell_back || outcome != "fresh"`, so a strict-chain launch that ends on fresh after starting at Resume
  MUST have `fell_back=true` or the frontend never hears about it. Rule (pinned): the strict chain sets
  `fell_back=true` **whenever the launch decision became weaker than the first step** (Resume -> Fresh);
  dropping only the optional hook-trust flag on a Fresh -> Fresh step does **not** set it (the drop appears
  only in the trail). `03-frontend.md` turns a `fell_back` event for a worktree that is in the transfer
  `restarting` phase into the Fresh-style toast. The event is emitted inside the spawn, before `pty_spawn`
  resolves (verified: `spawn_agent_locked`'s emit at `backend/herdr/mod.rs:926-930` precedes its `Ok`
  return, and the strict copy keeps that order), which is what `03`'s ordering assumption relies on. No new
  event is introduced.
- `Default` policy behaviour of that event is unchanged.

## 7. Trail entry (diagnostics)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailStage { Env, Import, Classify, Persist, Launch, Verify, Settle }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TrailEntry {
    pub at_ms: u64,                // since the entry's `begin` (monotonic); computed by the registry, never by callers
    pub stage: TrailStage,
    pub detail: serde_json::Value, // structured; never contains prompt/transcript content
}
#[derive(Debug, Clone, PartialEq)]
pub struct TrailSnapshot { pub entries: Vec<TrailEntry>, pub dropped: u32, pub committed: bool, pub transferring: bool }
```

Emitters use one helper (defined in `transfer_registry.rs`, `04`):

```rust
pub fn trail(project: &str, worktree: &str, stage: TrailStage, detail: serde_json::Value);       // log::info!
pub fn trail_warn(project: &str, worktree: &str, stage: TrailStage, detail: serde_json::Value);  // log::warn!
```

Each call writes one `log::` line with target `transfer`, message `transfer: <project>/<worktree> [<stage>]
<json>` (so `<KEITORA_HOME>/logs/keitora.log` shows `[transfer] transfer: ...`) **and** pushes to the global
registry. Emitters do not call `log::` and `push_trail` separately, never supply a timestamp (`at_ms` is
derived by the registry from the entry's `begin`), and pass a `TrailStage` (never a string). Stage `Settle` is
emitted by the registry itself; siblings emit `Env`, `Import`, `Classify`, `Persist`, `Launch`, `Verify`.
`Verify` observations are pushed only when the observation changes. Redaction replaces sensitive values with
the marker `"[redacted]"` (keys are kept). Details in `04`.

## Resolved ambiguities in the proposal

1. **Serde tagging** was unspecified. Resolved as `kind`-tagged, snake_case variants, camelCase fields
   (section 1), verified against `DiffBase` (`kind` tag) and the `diff/` camelCase structs.
2. **Where the diagnostics scrubbing happens.** The proposal reads as frontend-side; resolved as
   backend-side inside `transfer_diagnostics` so the secret/`$HOME` scrub is testable in Rust and cannot be
   bypassed. The frontend only copies.
3. **Toast copy source.** Proposal says copy maps in one place; resolved as Rust `user_copy()` shipped as
   `reasonText` so there is exactly one place.
4. **Marker lifetime numbers.** Proposal says "backend backstop timeout" without a value; resolved as 300 s
   during import (above the ~150 s deadline) and 180 s after commit (above the frontend's 120 s
   `restarting` backstop, which is itself above the strict chain's ~100 s budget).
5. **How the pane asks for the strict policy.** Proposal does not say; resolved as a trailing optional
   `transfer` argument on `pty_spawn`/`ptySpawn` (section 5).
6. **Frontend "fresh" restart parameter.** Resolved and verified: `restart(fresh = false)`; fresh is
   `restart(true)`. `RestartSignal.fresh` already exists.
7. **Project identity.** Resolved as the resolved `[project].name`, not the registry key (conventions).

## Changelog

Every change made to this file. The first block is the Singletonize pass (verification against the repo); the
second block is the Reconcile pass (orchestrator decisions and sibling contract-change requests).

1. Added "Repo conventions this file follows (verified)" section (command registration, managed state,
   events, serde, project identity).
2. Section 1: attributes made exact and normative (`derive`s, `rename_all_fields`, serde 1.0.228 check);
   `SkipReason`/`TransferPhase` got their own `rename_all`; added `user_copy()` signature, home module,
   pinned wire JSON and the owning test name (`transfer_outcome_wire_shape`). Removed the "implementer must
   check the convention" hedge (done here).
3. Section 2: **fixed wrong types.** `preview_claude_transfer` returns the existing `ConfirmPreview` (the
   draft said a non-existent `ClaudeTransferPreview`); `inject_handoff_summary` returns `Result<bool,
   String>` / `Promise<boolean>` (draft said `()`/`void`); `transfer_worktree` is `async` with injected
   `State`/`AppHandle` params, `target_cli: CliKind` (draft showed a sync fn with `String`); TS wrapper
   `targetCli: CliKind`; added `invoke` arg shapes, `host.rs` registration lines, `.manage` location, and the
   `dispatch_apply` injected-state note. "By name/key" clarified as `[project].name` (draft cited a by-key
   lookup at `host.rs:1318-1324`; `launch_for` actually matches `pc.project.name`).
4. Section 3: added emit mechanism (`app.emit`, `Emitter`), the TS listener wrapper `onTransferProgress`,
   `Serialize`/`Clone` derives and `rename_all`, `guard.elapsed_ms()`.
5. Section 4: replaced the raw field list with the exact consumer-facing API (`new`, `global`, `Clone` handle,
   `TransferGuard<'_>`, `TrailSnapshot`); internals moved to `04`. `last_trail` now returns
   `Option<TrailSnapshot>` (was `Option<Vec<TrailEntry>>`) so the dropped-entry count and committed flag
   travel with it. `is_transferring` clears an expired marker on observation. Added `global()` because the
   herdr spawn chain has no `AppHandle`/`State`.
6. Section 5: **fixed**: `RestartSignal.fresh` already exists (draft said both fields are new); `restart`
   parameter confirmed (`restart(true)` = fresh); `pty_spawn`/`spawn_pty`/`ptySpawn` exact current and new
   signatures; `transfer` is positional (the draft's "optional argument" was unspecified; sibling `02`/`03`
   wrote an options-object form `{ transfer: true }`, which does not match the existing positional
   `ptySpawn` and is superseded by this section); `command` path ignores `transfer`.
7. Section 6: pinned the resume event channel and snake_case payload, noted it is only emitted when
   `fell_back || outcome != "fresh"` (so strict-chain-ends-on-fresh must set `fell_back`), and that the final
   failure string must pass through `spawn_err_kind` via `PtyBackendError::Other`.
8. Section 7: `stage` changed from `String` to the `TrailStage` enum (same wire/log strings); added
   `TrailSnapshot`, the `trail`/`trail_warn` helper signatures and log line format; `at_ms` is monotonic from
   `begin`.
9. Added resolved ambiguity 7 (project identity); updated 1, 5, 6.

### Reconcile pass

10. `TransferRegistry::mark_committed` now returns `bool` and is called **before** persist, atomically with the
    cancel check, under the dispatch gate (`false` = cancelled, nothing committed, `Ok(Cancelled)`). It sets the
    committed marker only on `true`. (Section 2 "Commit order", section 4.)
11. After a persist failure the backend calls `settle`, so an `Err` leaves no transferring marker (section 2).
12. `TransferGuard` is declared `Send` (owned strings, `Arc<AtomicBool>`, a shared registry reference; section 4).
13. Section 1 derives confirmed as `Debug, Clone, PartialEq, Eq, Serialize` with the serde >= 1.0.185 requirement
    for `rename_all_fields` stated and verified against `Cargo.lock` (1.0.228) plus the per-variant fallback.
    `TrailSnapshot` gained `Debug, Clone, PartialEq`; `TransferProgress` derives `Debug, Clone, Serialize`.
14. Preview type is `ConfirmPreview`, `target_cli` is `CliKind` (already correct here; `01`, `02`, `03` made to
    match).
15. Section 3: the progress emitter closure is `Fn + Send + Sync` (runs on `spawn_blocking`).
16. `reason_text` must not end with a period (section 1 rules); `03` uses it verbatim.
17. `cancel_transfer` applies only to Claude -> Codex imports; Cancel UI only for `to === "codex"`; the
    Codex -> Claude path ignores the flag (section 2).
18. Section 5: `ptySpawn`'s trailing positional `transfer` is **omitted from the invoke payload unless true**
    (was "always sent"); existing exact-payload tests stay green. The strict policy is Codex-only (`transfer:
    true` for a Claude worktree stays `Default`). Added the restart-after-failed-transfer rule (`fresh=true`
    and `transfer=true`, never `resume --last`).
19. Section 6: `fell_back` rule pinned (true whenever the decision got weaker, Resume -> Fresh; a hook-trust
    flag drop on Fresh -> Fresh does not set it); confirmed the resume event is emitted before `pty_spawn`
    resolves.
20. Section 7: `at_ms` is computed by the registry/trail helper (callers never pass it); emitters pass a
    `TrailStage`, push `Verify` entries only on a changed observation; redaction marker is `"[redacted]"`.
21. Marker TTLs: after-commit TTL raised from 120 s to **180 s**, because the frontend `restarting` backstop is
    now **120 s** (was ~60 s; the strict chain budget is ~100 s) and the marker must outlive it.
    (`COMMITTED_MARKER_TTL` in `04`.)
22. Codex -> Claude `DispatchGate` narrowing via `persist_under_gate` is approved and in scope (`04`).

### Cross-document scan (Reconcile pass)

23. `02`/`03` use the positional `ptySpawn(..., fresh?, transfer?)` (not an options object); `04`'s open question
    4 closed. `RestartSignal.fresh` already existed (confirmed in `02`, `03`, `04`); `TrailStage` enum,
    `last_trail -> Option<TrailSnapshot>` and `RestartSignal.fresh` stated identically everywhere.
24. `02`'s `LaunchTrailEntry.stage` is a `TrailStage` (was `&'static str`) and its hand-off calls
    `crate::transfer_registry::trail` (was `crate::transfer::trail`); `02`'s verify loop pushes a `Verify` entry
    only when the observation changes, and carries no timestamp field in `detail`.
25. `01`'s `ImportCtx.trail` closure takes a `TrailStage` (was `&str`) and `01` no longer calls `log::info!` plus
    `push_trail` itself (it calls the `trail` helper; `at_ms` is the registry's). `01`'s progress/trail
    closures are `Send + Sync`.
26. `CodexEnv` field types made identical between `01` and `02`: `path: Option<String>` (was `OsString` in `01`),
    plus `exe_source`, `codex_home_source`. `codex_home_from` in the `import` trail entry uses the
    `CodexHomeSource` names (`initialize | env_var | default`).
27. `02`: `CodexEnv.codex_home` is resolved before spawn (env var, else `~/.codex`); `with_initialize_home`
    overrides it for the ledger lookup only; `CODEX_HOME` goes on the child and pane only for `EnvVar`.
    `CodexTooOld` is immediate on `-32601` for the import method and never consults the ledger.
28. JSONL selection (`01` B9): the newest non-`agent-*` `.jsonl` wins; the persisted `claude_session_id` is only
    logged to the trail (it goes stale). `find_live_claude_jsonl`/`pick_jsonl` lost their `preferred`
    parameter. The proposal text that says "prefer the persisted id" is superseded.
29. AppServer teardown (`01` B3): graceful teardown runs on a detached thread after the outcome is returned;
    cancel and timeout abort and kill immediately.
30. Strict launch (`02` C3): "agent never appeared" consults `pane_process_info` before failing; a non-shell
    foreground process is accepted as present-unknown (trail-logged), otherwise the step fails and the chain
    advances. Kill & Restart after a transfer whose outcome was `Fresh` restarts with `fresh=true` and
    `transfer=true` (`03`: `restartAfterFailure`), never Default `resume --last`.
31. `persist_worktree_cli` signature unified: `02` (owner) takes the already-resolved `&ProjectConfig`; `04`
    previously said it takes `&str` and resolves inside, which is dropped. `02` references
    `resolve_project(&user, name)` (was `(state, name)`).
32. `04` gained `commit_then_persist` (+ `CommitCancel`, `Committed`) so the "mark committed before persist,
    settle on persist failure" rule is one tested helper used by both directions; `01` step 7 calls it.
    `04`'s internal Rust `TransferEntry` renamed `RegistryEntry` (it collided in name with `03`'s TS
    `TransferEntry`, which means something different).
33. `01`'s wire-shape test renamed to the contract's pinned name `transfer_outcome_wire_shape`.
34. `03`: `RESTARTING_BACKSTOP_MS` 60 s -> 120 s (constant, copy, tests 21/31/32); `freshToastMessage` uses
    `reasonText` verbatim (no period stripping); `ptySpawn` payload tests unchanged (key omitted unless true);
    new pure helper `restartAfterFailure` with test 29a.
