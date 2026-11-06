# 0069 — Harden Claude → Codex transfer

Task: #69 (absorbs the product question in #29). Branch: `llm/andrew/keitora/0069-codex-transfer-hardening`.

## Goal

"Transfer to Codex" must **always end with a running Codex session in the pane**. If the import works,
that session resumes the imported Claude conversation; otherwise it is a fresh Codex conversation. The
transfer must also meet three conditions:

- It never leaves the app wedged.
- It never shows a misleading "Kill & Restart" screen.
- It never drops the user into a bare shell that claims to be Codex.

When something goes wrong, the user must be able to tell *what*. So must we, working from a macOS
machine whose logs we can't see.

## Symptoms reported (macOS, Codex CLI 0.166.0, no `CODEX_HOME`/`CLAUDE_CONFIG_DIR` override)

1. `Codex reported that the Claude import completed, but did not record an imported thread.`
2. A "successful" transfer that lands in a fresh terminal with no `codex` process.
3. The pane gets stuck and shows the confirm-kill / "Kill & Restart" screen.

## Root causes (research fanout + automated verification)

Paths are under `src-tauri/src/` unless noted. Line numbers are as of `e9c5d27`.

### R1 — Ledger hash race (high) → symptom 1

`run_claude_import` (`transfer.rs:533`) handles the import like this:

- It discards the `externalAgentConfig/import` response (`:545`).
- It waits for `.../import/completed` and discards its params (`:549`).
- `match_ledger_thread` (`:661`) then re-hashes the **live** JSONL. `ledger_match` (`:681`) requires an
  exact `source_path` + `content_sha256` + thread-id match in
  `$CODEX_HOME/external_agent_session_imports.json`.

Two things make that hash go stale:

- Claude is not stopped (`transfer.rs:186`, "does not touch the daemon client").
- The "Transferring…" overlay renders only when `status !== "running"` (`TerminalPane.tsx:1670`), so
  the user can keep typing.

Any write to the JSONL during the import breaks the match.

Upstream Codex (≥ 0.149) returns the answer directly:

- The import response carries `{importId}`.
- The matching completion notification carries `itemTypeResults[].successes[].target`, which is the
  imported **thread id**.
- It also carries `failures[]` with `failureStage`, `errorType`, `subErrorType` and `message`.

These are upstream source claims, not verifiable in this repo. The fake-server tests pin the shape.

Other "completed without a new record" cases, all currently flattened into R1's message:

- A no-op re-import of an unchanged source.
- A deferred import: the thread is open, or diverged, or the hash changed but the transcript didn't.
- A transcript with no `cwd`, messages or timestamps.
- A `continued-in` record (≥ 0.164, which applies to the reporter's 0.166).
- A path outside `$HOME/.claude/projects`. Codex ignores `CLAUDE_CONFIG_DIR`, but Keitora honours it
  (`claude_projects_dir_from`, `transfer.rs:727-740`).

### R2 — Codex's real error is always lost (high)

The stderr reader uses `read_to_string` (`transfer.rs:1212`), which returns only at EOF. At the error
site (`:558`) Codex is alive, so `stderr_tail()` is always empty. `run_claude_import` logs nothing.

### R3 — A bare shell is reported as a running agent (medium-high) → symptom 2

The launch path works like this:

1. `spawn_agent_locked` (`backend/herdr/mod.rs:816`) creates a shell pane via `workspace_create`.
2. It sleeps `SHELL_READY_DELAY_MS` = 300 ms (`mod.rs:111`).
3. It calls `herdr agent start --kind codex … -- resume <thread> …`.

`agent_start_result_from` (`backend/herdr/cli.rs:217-228`) treats `agent_not_ready` and "blocked during
startup" as success. If `codex` then exits, the pane falls back to its shell prompt. Codex can exit
for several reasons:

- an unknown thread;
- a rejected flag;
- no `node` for an npm shim;
- `agent.start` racing a slow zsh rc file, where 300 ms is unverified on macOS.

`observe()` records `agent_present = pane.agent.is_some()` (`resume.rs:191`, stored at `:666-679`). The
UI only sees a live pane.

### R4 — Launch failure → misleading screen (high) → symptom 3b

Any `ptySpawn` error other than `Recovering`/`Terminated` lands in `spawn-failed`
(`TerminalPane.tsx:593`). The error text is dropped, and the copy is hard-coded to "Failed to launch
claude — is it on PATH?" (`:1785`). Several things lead there:

- Only `agent_start_failed`/`agent_name_not_found` advance the fallback chain (`agent.rs:91-96`,
  `is_early_exit`). Every other error, including the 10 s `AGENT_START_TIMEOUT_MS` (`mod.rs:104`),
  closes the pane and returns `Err` (`mod.rs:~885`).
- Every variant, including the fresh ones (`agent.rs:239`), carries `--dangerously-bypass-hook-trust`
  (`agent.rs:73`).
- When no path is configured, the launch falls back to the bare string `"codex"` (`host.rs:1291-1294`).
  It never calls `discover_cli`.

### R5 — The transferring indicator is cleared by the wrong signals (high) → symptom 3a

Two effects clear `transferring`:

- `App.tsx:499-511`, after the session was seen offline and then live again;
- a 20 s safety net at `:515-518`.

The import can take up to 360 s: `initialize`, the request and the completion are each bounded at
120 s (`transfer.rs:61-65`). The timer fires mid-import. When the import finally succeeds,
`bumpRestartNonce` reaches a pane whose `transferringRef` is null and whose status is `running`
(`TerminalPane.tsx:324-325, 895-910`). That triggers the confirm-kill. `cli=codex` is already persisted
(`transfer.rs:257`), so cancelling leaves Claude running while the rail says Codex.

### R6 — No fallback (certain)

On error, `onTransfer` only toasts (`App.tsx:587`). `persist_worktree_cli` keeps the old thread id when
passed `None` (`transfer.rs:1042-1048`).

Clearing the id still would not give a fresh session. With no thread id and no stored row,
`ResumeStore::resolve` returns `Continue` (`resume.rs:293`), which for Codex is `resume --last`
(`agent.rs:55-58`) and can attach an *unrelated* thread in the same cwd.

The transfer branch of the restart effect also always calls `restart(false)`
(`TerminalPane.tsx:904`).

### R7 — Environment on macOS GUI launches (medium)

The import child (`AppServer::spawn`, `transfer.rs:1170`, through `codex_command` `:1411-1440`)
inherits Keitora's own environment. A Finder- or Dock-launched app has launchd's minimal PATH, so:

- a bare `"codex"` (`transfer.rs:236`) may not resolve;
- an npm `#!/usr/bin/env node` shim can fail even by absolute path.

This is the likelier macOS environment cause. Launch panes are better off:
`env::pane_env` (`backend/herdr/env.rs:750`) builds on the login-shell PATH (`unix_spawn_path`,
`crates/keitora-tasks/src/config.rs:1940`) plus the exe dir, applied at `workspace_create`.

`CODEX_HOME` is read from Keitora's own env (`transfer.rs:643-653`). The reporter has no override, so
drift is unlikely, but nothing guarantees the import and the pane agree.

### R8 — Smaller correctness issues

- **Active-project resolution (medium).** `resolve_active()` is used by `preview_claude_transfer`
  (`:156`), `transfer_worktree` (`:173`), both directions (`:228`, `:280`), `inject_handoff_summary`
  (`:469`) and `persist_worktree_cli` (`:1036`). The frontend captures `activeProject` (`App.tsx:527`)
  but does **not** pass it: `transferWorktree`, `previewClaudeTransfer` and `injectHandoffSummary`
  (`src/lib/ipc.ts:1047-1068`) take no project. The respawn resolves by key (`launch_for`,
  `host.rs:1318-1324`), so a transfer from a non-active project persists to the wrong project, or
  fails.
- **Global `DispatchGate` held across the whole import (medium)** (`transfer.rs:244`). It blocks all
  dispatch for minutes.
- **No re-entry guard and no cancel (medium)** in `onTransfer`.
- **Import teardown force-kills 200 ms after closing stdin (low-medium)** (`Drop for AppServer`,
  `:1389`).
- **`classify` treats string-id server requests as notifications (low)** and silently ignores them
  (`:1117-1144`).
- **`find_live_claude_jsonl` picks the newest `.jsonl` (low)** (`:792`). It ignores the persisted
  `claude_session_id` and doesn't skip `agent-*.jsonl`.
- **The resume store outranks the worktree registry (low)** (`resume.rs:276`). A stale stored Codex
  row can beat the freshly imported thread.
- **The exit handler in the output gate checks no generation (low, unconfirmed)**
  (`TerminalPane.tsx:497-507`). The coalescer already cancels a deferred "ended" on restart.

**History note.** The repo was re-rooted at open-sourcing, and the first commit already uses herdr, so
pre- and post-herdr behaviour can't be diffed. Under herdr's `agent.start`, fallbacks became a Rust
loop (`agent.rs:28-32`, `next_fallback` `:80`). That loop covers early exits only. It does not cover
"started then vanished", timeouts or flag rejection. That gap (R3/R4) is the herdr-shaped part of this
bug.

## Scope

**In:**
- Protocol-driven import with classified failures.
- A guaranteed outcome: resume the import, else fresh Codex.
- Transfer-only launch verification and fallback.
- A frontend transfer phase machine, with input blocked during the import, a re-entry guard and cancel.
- Shared Codex environment resolution for the import child and the pane.
- Diagnostics.
- Correct project scoping.
- Narrower locking.
- The R8 fixes.
- Tests for all of the above.

**Out:**
- Token-size UI (#26).
- Oversized-transfer routing (#28).
- Cost measurement (#27).
- Changing default (non-transfer) launch, boot-resume or recovery behaviour.
- Upstream herdr patches.
- A redesign of the Codex → Claude direction. It changes only where shared code moves: the project
  parameter, the outcome type, and gate placement.

## Architecture

### A. Transfer outcome and cancel

`transfer_worktree(project, worktree, target_cli)` returns `TransferOutcome`:

```
Imported { thread_id }            // Claude→Codex: resume the imported thread
Fresh    { reason: TransferFailure }  // Claude→Codex: import failed; worktree is Codex, start fresh
Done                              // Codex→Claude: summary handoff complete (unchanged semantics)
Cancelled                         // user cancelled during import; nothing changed, still Claude
```

`Err` is reserved for "nothing changed": the session is terminated, the worktree or project can't be
resolved, or persisting failed.

`TransferFailure` is a classified enum. Its variants are `CodexNotFound`, `CodexTooOld`,
`SessionNotDetected`, `Skipped{reason}` (no-op, deferred or `continued-in`), `Timeout{phase}`,
`ProtocolError` and `Other`. Each variant carries Codex's `message` and the stderr tail, and maps to
plain-language toast copy in one place.

Cancel works through a per-`(project, worktree)` `Arc<AtomicBool>` registered in managed state for the
duration of the import:

- `AppServer`'s `request` and `wait_notification` slice their `recv_timeout` into ~250 ms steps
  (`transfer.rs:1245, 1285`) and check the flag at each step.
- When the flag is set, the import tears down the child, including the Windows `taskkill /T`, and
  returns `Cancelled`.
- A `cancel_transfer(project, worktree)` command sets the flag.
- Cancel is only honoured **before** persistence. Once `persist_worktree_cli` runs, the transfer is
  committed and cancel is a no-op.

### B. Import: trust the protocol, not the hash

- **Spawn.** `AppServer::spawn` takes a `CodexEnv` (section D) and sets `PATH` and `CODEX_HOME`
  explicitly on the child. This works through `cmd /C` on Windows, using the existing `short_path`
  quoting.
- **Handshake.** Keep the `initialize` result (`handshake`, `:1232`) instead of discarding it.
  - Record the Codex version and `codexHome` from it, when present.
  - Use `codexHome` for the ledger fallback; recompute the home only if it is absent.
- **Completion.**
  - Capture `importId` from the import response, which `request` already returns.
  - Wait for the `completed` notification whose `importId` matches. This needs a predicate-based
    `wait_notification` that also scans `notif_buf` (`:1175`), because the completion can arrive
    before the response.
  - From the SESSIONS item:
    - a success uses `successes[].target`;
    - a failure classifies `failures[]` into `TransferFailure`;
    - empty successes with no failure (a no-op or deferred import) falls back to the ledger.
- **Ledger fallback.** It is used for empty successes, or when the response has no `importId`
  (Codex < 0.149).
  - Match by `source_path` only, taking the newest record that has a thread id. There is no sha
    check.
  - Re-read up to ~3 s for a late flush.
  - Accept either the canonicalized path or the raw path we sent.
- **Deadline and progress.** One overall deadline of ~150 s replaces the three independent 120 s
  bounds. Emit `.../import/progress` and the phase transitions as a `transfer://progress` event.
- **Capture and protocol.**
  - Read stderr line by line into a bounded ring buffer so the tail exists while Codex is alive.
  - `classify` treats any request with an `id` (integer *or* string) plus a `method` as a server
    request, preserving the `-32601` reply.
- **Teardown.** Close stdin, wait up to ~3 s for exit, then kill the process tree (Windows
  `taskkill /T`, unix SIGKILL).
- **Preflight.** If the JSONL is not under `$HOME/.claude/projects` (because `CLAUDE_CONFIG_DIR` points
  elsewhere), skip the import. Return `Fresh{SessionNotDetected}` with a clear reason.
- **JSONL selection.** `find_live_claude_jsonl` prefers the persisted `claude_session_id`'s file and
  skips `agent-*.jsonl`.

### C. Guaranteed Codex in the pane, without touching default launches

**Persistence.**

- `persist_worktree_cli` takes `project` and explicit field intents (`Set(v)`, `Clear` and `Keep`).
  - `Imported` persists `cli=codex`, `codex_thread_id=Set(id)` and `claude_session_id=Keep|Set`.
  - `Fresh` persists `cli=codex`, `codex_thread_id=Clear` and `claude_session_id=Keep|Set`, so the
    round trip back to Claude still works.
- The resume-store row for the key is updated in **both** cases, because the store outranks the
  registry (`resume.rs:276`):
  - `Imported` overwrites it with `cli=codex` + the new id;
  - `Fresh` deletes it.

**Fresh is an explicit launch, not an inference.** The restart signal carries `fresh: true`, and
`TerminalPane`'s transfer branch honours it instead of hard-coding `restart(false)`. A cleared thread
id alone would resolve to `Continue` (`resume --last`), so we never rely on it.

**A transfer-strict launch policy.** `spawn_agent_locked` is shared with boot-resume and recovery
(`resume_locked`, `mod.rs:996`; `BackendResumeRunner`, `mod.rs:1444-1510`). Those callers depend on
timeouts surfacing as `Err`/`Failed` for the memory-gated backoff (`is_load_related_error`,
`fell_back || timed_out`, `mod.rs:1424-1488`).

So the new behaviour is opt-in through a field on `AgentLaunch`, `policy: LaunchPolicy { Default,
TransferStrict }`:

- **`Default`** (all existing callers, about 6 construction sites) behaves exactly as today, pinned by
  regression tests.
- **`TransferStrict`** is passed by a spawn that follows a transfer:
  - **Chain:** `resume <thread>` → fresh → fresh without optional flags (drops
    `--dangerously-bypass-hook-trust`). **Never `Continue`.**
  - **Verify:** after `agent.start` returns OK, poll `pane_get` for up to ~10 s.
    - For `Resume`, success is the pane's `agent_session` reporting kind `Id` with
      `value == thread_id`.
    - For a fresh launch, success is `agent == "codex"`.
    - `agent` going absent after having been present counts as "vanished" and advances the chain.
  - **Timeout is not failure.** An `agent_start_timeout` or a slow verify keeps polling, up to an
    overall ~45 s cap, before advancing. This stops a large imported thread from being abandoned
    to fresh just because it loads slowly, which would orphan it.
  - **Early `agent.start` errors** (shell not ready) re-issue `agent.start` up to 2 times with a short
    backoff, and log the error code. There is no prompt-ready check, because herdr exposes no such
    signal.
  - **Final failure** returns a structured error that carries the herdr/Codex message and the chain
    steps tried. The pane shows spawn-failed with that text and the correct CLI name (section E).
    `cli=codex` stays persisted, and "Kill & Restart" retries as a fresh Codex launch.

Herdr's `agent`/`agent_session` latency has never been observed in this repo. The verification is
designed defensively, logging every observation, and remains **unproven until a user smoke test** on
the reporting machine.

### D. Codex environment, shared

`CodexEnv { exe, codex_home, path, version }` is resolved once per transfer. It is built from
existing helpers, not a new resolver:

- **`exe`**: try each source in turn.
  1. The configured path (`resolve_provider_path`, `config.rs:1770`).
  2. `discover_cli(Codex)` (`config.rs:2253` unix / `:2276` Windows). Today neither the transfer nor
     `agent_launch_with` calls it.
  3. `/opt/homebrew/bin/codex`, which `discover_cli` gains as a unix candidate.
  4. A bare `"codex"`, as the last resort.
- **`path`**: on unix, the login-shell PATH (`unix_spawn_path`/`cached_login_path`, `config.rs:1940`)
  plus `exe`'s dir. On Windows, the process PATH plus `exe`'s dir.
- **`codex_home`**: the `initialize` result if known, else the env var, else `~/.codex`.
- **`version`**: `codex --version`, probed once and cached per exe path.

Uses:

- The import child gets `path` and `codex_home` (section B).
- The pane gets `CODEX_HOME` through `env::pane_env` (`env.rs:750`), which already handles PATH.
  `herdr agent start` takes no env (`real.rs:351-379`). `host.rs` launch resolution uses the same
  `exe` resolution as the import.

### E. Frontend: a phase machine, not heuristics

**Per-worktree transfer state.** `transferring` becomes a per-worktree map (key
`${project}/${worktree}`, matching the nonce maps at `App.tsx:218`). Each entry holds `{phase:
importing | restarting, to}`. It **replaces** both the offline→live clearing effect
(`App.tsx:499-511`) and the 20 s timer (`:515-518`); nothing is layered on top of them.

- `importing` lasts until `transferWorktree` settles, with no timer.
- `restarting` lasts until the pane reports the spawn settled, through a new `onSpawnSettled(ok, err)`
  prop from `TerminalPane` (`restart()` already awaits `spawn`, `:763`).
- `restarting` has a generous backstop of ~60 s that surfaces an error rather than silently clearing.

**The restart signal carries the transfer.** `RestartSignal` gains `transfer: boolean`
(`src/lib/resumeNotice.ts:16`). The pane skips confirm-kill when `transfer` is set, and honours
`fresh`. The ambient `transferringRef` is removed.

**Overlay during the import.** A new overlay sits outside the `status !== "running"` gate
(`TerminalPane.tsx:1669`). It shows during `importing` while Claude is still live, displays progress,
**blocks keyboard input** so the JSONL stops changing, and offers Cancel.

The confirm dialog warns when Claude is mid-turn (pane `agent_status` working): the in-flight turn
won't be carried over.

**Re-entry guard.** The Transfer menu item is disabled while the worktree has a `transferring` entry,
and `onTransfer` early-returns in that case.

**Outcome toasts.**

- `Imported` shows "Transferred to Codex."
- `Fresh` shows "Couldn't carry the Claude history over (*reason*). Started a fresh Codex session." It
  includes a "Copy diagnostics" action.
- A strict-chain fallback reported by the backend resume event (`outcome`, `fell_back`, `mod.rs:~925`)
  produces the same Fresh-style toast.

**Spawn-failed copy.** It uses the actual CLI name and shows the backend error text. The error is
passed through, not collapsed by `spawn_err_kind`.

**Stale exit.** Add a reproduction test first. Only add a `gen` field to `OutputMsg::Exit` (`ipc.ts:517`,
`host.rs:2257`, which needs its wire-shape test updated) if the race reproduces. The coalescer may
already cover it.

### F. Project scoping and locking

**Project scoping.** `transferWorktree`, `previewClaudeTransfer`, `injectHandoffSummary` and
`cancelTransfer` take an explicit `project` (IPC change in `src/lib/ipc.ts` and the command
signatures). The frontend passes the captured `activeProject`.

All transfer code resolves the project config by name, not `resolve_active()`. That covers the preview,
both directions, persist and inject. It uses the same by-key lookup as `launch_for`
(`host.rs:1318-1324`).

**Locking.** `DispatchGate` keeps `dispatch_apply` atomic per task (`dispatch.rs:34,244`). So it stays
global, but it is **held only for the short persist step**, mirroring `transfer_codex_to_claude`. The
long import runs lock-free, under a per-worktree "transferring" marker in managed state, the same map
that holds the cancel flag:

- `dispatch_apply` refuses to inject into a worktree with the marker set.
- The marker is held until the frontend's restart completes, closing today's window where `cli=codex`
  is persisted but the live session is still Claude. The marker is cleared by an explicit
  `transfer_settled(project, worktree)` call from the frontend, with a backend backstop timeout.

### G. Diagnostics

Each transfer writes one structured trail through `log::` to `<KEITORA_HOME>/logs/keitora.log`
(`logging.rs`). The trail records:

- the `CodexEnv` (exe, version, whether `codex_home` came from env or default);
- the `importId`;
- the classified result and the raw `failures[]`;
- the stderr tail;
- each launch-chain step;
- each verification observation (`agent`, `agent_session`, timings).

The last trail per worktree is also kept in memory. "Copy diagnostics" fetches it through a command,
replaces `$HOME` with `~` and drops prompt content, then copies it with
`navigator.clipboard.writeText`, the existing pattern at `TerminalPane.tsx:1094`.

## Testing

- **Import (Rust).** Refactor `AppServer` to be generic over its reader and writer, with child spawn as
  one constructor. A scripted in-process fake then exercises `run_claude_import`'s protocol logic on
  both platforms. Scenarios:
  - success with `target`;
  - completion before the response;
  - `failures[]` with `session_not_detected`;
  - empty successes, deferred, falling back to the ledger by path;
  - no `importId`, the legacy ledger path;
  - a late ledger flush;
  - a deadline timeout;
  - stderr while the child is alive;
  - a string-id server request;
  - a JSONL appended mid-import, which must still succeed;
  - cancel mid-wait.

  One real-spawn smoke test uses a script fake (`.sh`/`.cmd`), platform-gated.
- **Unit (Rust).**
  - `persist_worktree_cli` intents: `Set`/`Clear`/`Keep`, resolution by project name, and the
    non-active project.
  - The resume-row overwrite and delete.
  - `TransferFailure` classification and copy.
  - Ledger path-only matching.
  - `CodexEnv` resolution order, unix including `/opt/homebrew/bin`, plus Windows.
  - Version-gated flags.
  - JSONL selection skipping `agent-*` and preferring the session id.
  - The `dispatch_apply` refusal while transferring.
- **herdr spawn (Rust, mocked through `backend/herdr/fake.rs`; never the real herdr).**
  - `TransferStrict`:
    - the agent vanishes after start → fresh;
    - a timeout keeps polling, then succeeds;
    - `agent_session` mismatch → fallback;
    - a flag rejection → fresh without flags;
    - every step fails → a structured error;
    - the chain never uses `Continue`.
  - **`Default` regressions:**
    - a timeout still surfaces as `Err`/load-related for recovery;
    - no silent fresh downgrade;
    - the existing chain is unchanged.
- **Frontend (vitest).**
  - Phase-machine transitions.
  - No confirm-kill when `RestartSignal.transfer` is set.
  - `fresh` honoured on a transfer restart.
  - The re-entry guard.
  - Outcome toasts.
  - Spawn-failed copy names the right CLI and shows the error.
  - The stale-exit repro.
- **Manual smoke (owed by the user on macOS; this is what proves section C).**
  - Transfer an idle session.
  - Transfer mid-turn.
  - Transfer with `codex` absent from the GUI PATH.
  - Transfer an already-imported session.
  - Transfer from a non-active project.
  - Cancel mid-import.
  - Copy diagnostics after each.

Tests must never touch the user's running herdr. Gated herdr tests stay gated.

## Dependencies

- Codex CLI ≥ 0.149 for the `importId` path. Older versions use the ledger fallback.
- No herdr changes.
- Touches:
  - `src-tauri/src/`: `transfer.rs`, `backend/herdr/{mod,agent,cli,resume,env}.rs`, `host.rs`,
    `dispatch.rs`, `logging.rs` (read-only use);
  - `crates/keitora-tasks/src/config.rs` (`discover_cli` candidates);
  - `src/`: `App.tsx`, `components/TerminalPane.tsx`, `components/Rail.tsx` (menu disable),
    `lib/ipc.ts`, `lib/resumeNotice.ts`.

## Risks

- **herdr signal reliability.** `agent`/`agent_session` latency and clearing behaviour are unobserved.
  Mitigation:
  - treat "agent never appeared but pane is alive" after the cap as present-unknown, logged as such,
    not as failure;
  - log every observation;
  - the macOS smoke test is the proof.
- **Regressing boot-resume and recovery.** Mitigation: the new behaviour is opt-in through
  `LaunchPolicy`, `Default` is pinned by regression tests, and no shared-chain edits are made.
- **Orphaning a valid imported thread.** Mitigation: a timeout is not failure under the strict policy,
  and the overall cap is generous.
- **Codex protocol churn.** Mitigation:
  - everything is parsed as `Value` with optional fields;
  - capabilities are detected by behaviour (a `-32601` reply or a missing `importId`);
  - the fake-server tests pin the expected shapes.
- **Windows.** `cmd /C` + `short_path` for the env-carrying import child, `taskkill /T` on cancel and
  teardown, and the `.cmd` fake for the smoke test.
- **Blocking input during the import** could annoy a user who is mid-thought. Mitigation: Cancel is
  always available, and the import deadline is bounded.

## Decisions

### Refine round 1 (user)

- **Complexity is large.** All stages run, and Fracture splits along import / launch+env / frontend /
  diagnostics.
- **The Fresh fallback starts Codex blank.**
- **A failed import always switches to Codex.** Claude stays live only until the outcome is known.
- **The reporter runs Codex 0.166.0 with no env overrides.** The `importId` path is available, and the
  `continued-in` skip must classify as `Skipped`. `CODEX_HOME` drift is not the likely cause. PATH for
  the import child is (R7).
- **#29 is treated as a likely symptom of R3.** Recount after landing before closing it.

### Refine round 2 (automated verification + feasibility review)

- **Launch changes are opt-in** through `LaunchPolicy::TransferStrict`, so boot-resume and recovery are
  untouched.
- **Fresh travels as an explicit `fresh: true`.** It is never inferred from a cleared thread id, which
  would mean `resume --last`.
- **A timeout is not failure under the strict policy.** No prompt-ready check, because herdr has no
  such signal; early errors re-issue `agent.start` instead.
- **The Imported path also overwrites the resume-store row.**
- **`DispatchGate` stays global, held only for persist.** The import runs under a per-worktree marker
  that `dispatch_apply` respects.
- **An explicit `project` is threaded through the transfer IPC commands.**
- **The frontend phase machine replaces both existing clearing effects**, with input blocked during
  the import and cancel allowed only before persist.
- **Environment resolution reuses `discover_cli`/`unix_spawn_path`/`pane_env`**, and the import child
  gets an explicit env.
- **The fake app-server is in-process**, through a reader/writer-generic `AppServer`.
- **A stale-exit `gen` field is added only if the race reproduces.**
