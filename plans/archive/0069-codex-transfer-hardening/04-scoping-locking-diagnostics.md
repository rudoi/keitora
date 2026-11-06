# 0069 / 04 — Project scoping, locking, diagnostics

Part of 0069 "Harden Claude → Codex transfer" (proposal: `../0069-codex-transfer-hardening.md`).
Shared interfaces: **`00-contracts.md`** (IPC signatures, `TransferRegistry` public API, trail entry, project
identity). Siblings: `01-import.md` (uses cancel flag, emits trail/progress), `02-launch-and-env.md` (persist,
launch steps emit trail), `03-frontend.md` (calls the commands, "Copy diagnostics").

Paths are under `src-tauri/src/` unless noted. Line numbers are as of `e9c5d27` (HEAD `4178d29` only added plan files, so they still hold).
A less capable agent can implement this document top to bottom; where a name or number is given, use it.

## Goal

"Transfer to Codex" must always end with a running Codex session in the pane, never wedge the app, never
mislead. This document covers the plumbing that makes that safe and debuggable: transfers act on the
**right project**, the **global dispatch lock is not held for minutes**, dispatch cannot inject into a
worktree mid-transfer, and when something goes wrong the user and we (debugging a macOS machine whose logs
we can't see) get a structured trail.

## Root causes covered

### R8 (project / gate parts)

- **Active-project resolution (medium).** `keitora_tasks::resolve_active()` is called at `transfer.rs:156`
  (`preview_claude_transfer`), `:173` (`transfer_worktree`), `:228` (`transfer_claude_to_codex`), `:280`
  (`transfer_codex_to_claude`), `:469` (`inject_handoff_summary`) and `:1036` (`persist_worktree_cli`) — six
  sites, verified. The frontend captures `activeProject` (`App.tsx:527`) but does **not** pass it:
  `transferWorktree`, `previewClaudeTransfer` and `injectHandoffSummary` (`src/lib/ipc.ts:1047-1068`) take no
  project. The respawn resolves by project **name** (`launch_for`, `host.rs:1319`, compares
  `pc.project.name != key.project` while iterating `user.projects.values()`), so a transfer from a
  non-active project persists to the wrong project, or fails.
- **Global `DispatchGate` held across the whole import (medium)** (`transfer.rs:244`,
  `let _gate = gate.0.lock().await;`). It blocks all dispatch for minutes. **Correction to the proposal:**
  `transfer_codex_to_claude` does *not* hold the gate narrowly either: it takes it at `:314`, before the
  kill/summarize step, and holds it through `persist_worktree_cli` (`:347`). Both directions are narrowed
  below.
- **No re-entry guard (medium)**; the backend half is the registry's `begin` refusal (UI half is `03`).
- Window today: `cli=codex` is persisted (`transfer.rs:257`) while the live session is still Claude, and
  `dispatch_apply` can inject into it.

### R2 (diagnostics side) — Codex's real error is always lost (high)

`run_claude_import` logs nothing (`transfer.rs:558` error site), and stderr is read only at EOF
(`:1212`, fixed in `01-import.md`). Nothing the macOS reporter can send us says what happened.

## Scope and non-goals

In: sections F and G, the `TransferRegistry` (public API in `00-contracts.md` section 4, implementation here),
the by-name project lookup, and their tests. Out: protocol/import logic (`01`), persistence semantics and
launch chain (`02`), UI (`03`). Out of the whole effort: default launch/boot-resume/recovery changes,
upstream herdr patches, a redesign of Codex → Claude (it changes only where shared code moves: the project
parameter, the outcome type, and gate placement).

## Dependencies on siblings

- `01-import.md` calls `TransferRegistry::begin`, `guard.cancel_flag()`, `guard.elapsed_ms()`,
  `mark_committed` (**before** persist, inside the gate, via `commit_then_persist` below; it returns `bool`)
  and `trail(...)` for `Env`, `Import`, `Classify`; it calls `persist_under_gate` (below) around the commit and
  persist.
- `02-launch-and-env.md` calls `trail`/`trail_warn` with `Launch`/`Verify`/`Persist` entries (a `TrailStage`,
  never a string; no timestamp argument). `persist_worktree_cli` (owned by `02`) takes the already-resolved
  `&ProjectConfig`; its callers obtain it with `resolve_project` (below).
- `03-frontend.md` calls `cancel_transfer`, `transfer_settled`, `transfer_diagnostics` and passes `project`.
- Ordering of the shared edits to `transfer.rs` is in "Implementation order within this doc".

## Design

### F. Project scoping and locking

#### F1. By-name project lookup (new function, with tests)

There is no existing by-name resolver (`resolve_active`, `effective_config(&ProjectRef)`, and the inline loops
in `launch_for` `host.rs:1319`, `worktree_exists` `host.rs:1345` and `terminal_links.rs:25` each do their own
thing). Add one minimal function and reuse it in the transfer module.

**`crates/keitora-tasks/src/config.rs`**, inside `impl UserConfig` (near `project_ref_by_root`):

```rust
/// The effective config of the registered project whose resolved `[project].name` is `name`.
/// Registry map keys are not consulted: session keys and IPC carry the resolved name.
/// Entries whose config fails to resolve are skipped (as `launch_for` does). If two registry
/// entries resolve to the same name, the first in `BTreeMap` (registry-key) order wins.
pub fn project_config_by_name(&self, name: &str) -> Option<ProjectConfig> {
    self.projects
        .values()
        .filter_map(|pref| effective_config(pref).ok())
        .find(|pc| pc.project.name == name)
}
```

No re-export needed (it is a method on the already exported `UserConfig`).

**`transfer.rs`** (free function, pure over an already-loaded registry so it is unit-testable without
touching the user's config file):

```rust
/// Resolve a project by its resolved name. Never falls back to the active project.
fn resolve_project(user: &keitora_tasks::UserConfig, name: &str) -> Result<keitora_tasks::ProjectConfig, String> {
    user.project_config_by_name(name)
        .ok_or_else(|| format!("project `{name}` not found"))
}
```

Callers load the registry themselves with `keitora_tasks::UserConfig::load().map_err(|e| e.to_string())?`
(as `transfer_claude_to_codex` already does at `:229`; share that one `user` value instead of loading twice).

**Replace each of the six `resolve_active()` sites:**

| Site | Replacement |
|---|---|
| `:156` `preview_claude_transfer` | add `project: String` param; `let user = UserConfig::load()...?; let config = resolve_project(&user, &project)?;` |
| `:173` `transfer_worktree` | `project` is now a param; no resolution needed for the `SessionKey` (it is the name). Resolve inside the two direction functions. |
| `:228` `transfer_claude_to_codex` | add `project: &str` param (or `project: String`); reuse the `user` already loaded at `:229`: `let config = resolve_project(&user, &project)?;` (reorder so `user` loads first) |
| `:280` `transfer_codex_to_claude` | same as `:228` |
| `:469` `inject_handoff_summary` | add `project: String` param; **no config resolution at all** — it only used `config.project.name`, which is now the param. `handoff_summary_path(&project, &worktree)` already keys on it (`:1005`), and `transfer_codex_to_claude` writes with `config.project.name`, which equals the by-name project, so the paths match. |
| `:1036` `persist_worktree_cli` | signature owned by `02`: it takes the **already-resolved** `project: &ProjectConfig` as its first parameter (and the field intents); it does not resolve anything itself. Its callers (`transfer_claude_to_codex`, `transfer_codex_to_claude`) obtain the config with `resolve_project(&user, &project)?`. The "not registered" error must name the project: `format!("project {} is not registered; cannot persist transfer state", project.project.name)` (`02` section C1). |

After the change `resolve_active` must not appear anywhere in the non-test part of `transfer.rs` (not even
in a comment); a test enforces it (`transfer_code_never_resolves_the_active_project`). `resolve_active()` in
other modules is untouched.

#### F2. `TransferRegistry` (implementation)

New file `src-tauri/src/transfer_registry.rs`; add `mod transfer_registry;` to `src-tauri/src/lib.rs`
(alphabetical, after `mod transfer;`). Register as managed state and commands per `00-contracts.md` section
2. No new Cargo dependencies (`std` + `serde_json` + `log` only; `tokio` is not needed).

Locking primitive: one `std::sync::Mutex` over a `HashMap`. Every method locks, mutates, unlocks and
returns; **no lock is held across IO, an `.await`, or while taking `DispatchGate`**. Always lock with
`.lock().unwrap_or_else(|e| e.into_inner())` (a poisoned lock must never wedge dispatch; see the activity
boot panic). Do not store the std `MutexGuard` in any future.

Backstop mechanism: **checked lazily on access, no timer task.** Expiry is evaluated against
`Instant::now()` in `is_transferring`, `begin` and `request_cancel`; nothing runs in the background. Never
compute an expiry with `Instant - Duration` (it panics on a short-uptime machine); only `Instant + Duration`.

```rust
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const IMPORT_MARKER_TTL: Duration = Duration::from_secs(300);    // from begin
pub const COMMITTED_MARKER_TTL: Duration = Duration::from_secs(180); // from mark_committed (> frontend 120 s backstop)
pub const MAX_TRAIL_ENTRIES: usize = 300;       // oldest dropped first; count kept in `dropped`
pub const MAX_TRAIL_STRING_CHARS: usize = 2000; // per string value inside an entry's detail
const MAX_DETAIL_DEPTH: usize = 8;

type Key = (String, String); // (project, worktree)

struct RegistryEntry {
    id: u64,                         // generation; a guard only releases its own transfer
    cancel: Arc<AtomicBool>,
    started: Instant,                // trail `at_ms` origin, `elapsed_ms` origin
    committed: bool,                 // persist ran; cancel is a no-op afterwards
    transferring: bool,              // the marker dispatch_apply refuses on
    marker_expires_at: Instant,      // lazy backstop
    trail: VecDeque<TrailEntry>,     // <= MAX_TRAIL_ENTRIES
    dropped: u32,                    // entries evicted from the front of `trail`
}

struct Inner { map: Mutex<HashMap<Key, RegistryEntry>>, next_id: AtomicU64 }

#[derive(Clone)]
pub struct TransferRegistry(Arc<Inner>);
```

`TransferRegistry::new()` builds an empty `Inner`. `TransferRegistry::global()` clones one process-wide
instance held in `static GLOBAL: OnceLock<TransferRegistry>` (`GLOBAL.get_or_init(Self::new).clone()`).
Implement `Default` as `new()` (an isolated instance); `host.rs` calls `global()` explicitly.

Every public method is a thin wrapper over a `pub(crate)` `*_at(.., now: Instant)` twin so tests inject the
clock by passing `base + Duration::from_secs(n)` (never subtract). The wrappers pass `Instant::now()`.

Method semantics (exact):

- `begin_at(project, worktree, now) -> Result<TransferGuard, AlreadyTransferring>`: lock; if an entry exists
  with `transferring && now < marker_expires_at` -> `Err(AlreadyTransferring)` (no mutation). If an entry
  exists with `transferring && now >= marker_expires_at`, `log::warn!("transfer: {p}/{w} stale marker
  replaced")`. Otherwise insert a **new** `RegistryEntry`: `id = next_id.fetch_add(1)`, `cancel = Arc::new
  (AtomicBool::new(false))`, `started = now`, `committed=false`, `transferring=true`,
  `marker_expires_at = now + IMPORT_MARKER_TTL`, empty trail, `dropped=0` — replacing any previous entry for
  the key (this is what "begin replaces the previous trail" means). Return `TransferGuard { registry: self,
  key, id, started: now, cancel }` (`cancel` = the `Arc<AtomicBool>` just created). The guard holds only a
  shared reference to the Arc-backed registry, owned strings, an `id`, an `Instant` and the `Arc<AtomicBool>`:
  it is `Send` (it lives across `.await`s in the async command); add a compile-time assertion
  `fn assert_send<T: Send>() {} assert_send::<TransferGuard<'static>>();` in the tests.
- `TransferGuard::cancel_flag()` returns `Arc::clone` of the entry's flag **captured at begin** (store it in
  the guard as a field so it works without the lock). `elapsed_ms()` = `started.elapsed().as_millis() as u64`.
- `request_cancel(project, worktree) -> bool`: lock; entry exists && `transferring && !committed` &&
  `now < marker_expires_at` -> `cancel.store(true, SeqCst)`, push trail `Import {"cancel_requested": true}`
  (internal helper, no log duplication needed beyond one `log::info!`), return `true`; else `false`.
- `mark_committed_at(project, worktree, now) -> bool`: lock; if the entry exists, is `transferring` (and not
  expired) and its cancel flag is **not** set: `committed=true`, `marker_expires_at = now +
  COMMITTED_MARKER_TTL`, return `true`. If the cancel flag is set, or there is no live entry: mutate nothing and
  return `false` (the cancel check and the commit are one critical section, so a cancel can never slip in
  between). Called **before** persist (see F3).
- `is_transferring_at(project, worktree, now) -> bool`: lock; no entry -> false; `transferring` false -> false;
  `now >= marker_expires_at` -> set `transferring=false`, push trail `Settle {"reason":"backstop"}`, emit
  `log::warn!("transfer: {p}/{w} [settle] backstop")`, return false; else true.
- `settle_at(project, worktree, now)`: lock; if entry exists and `transferring`: `transferring=false`, push
  trail `Settle {"reason":"settled","committed": <bool>}`. Otherwise do nothing (idempotent, no duplicate
  trail line).
- Guard `Drop`: lock; if entry exists with `entry.id == self.id && !entry.committed && entry.transferring`:
  `transferring=false`, push trail `Settle {"reason":"guard_drop","committed":false}`. A committed transfer's
  guard drop does nothing (the marker stays until `settle` or the backstop). A stale guard (different `id`)
  does nothing.
- `push_trail_at(project, worktree, stage, detail, now)`: `detail = sanitize_detail(detail)`; lock; **no entry
  -> drop silently** (never create an entry; the `log::` half still happened in the helper); otherwise
  `at_ms = now.duration_since(entry.started).as_millis() as u64` (saturating: use `checked_duration_since`,
  default 0), push to `trail`; if `trail.len() > MAX_TRAIL_ENTRIES` -> `pop_front` and `dropped += 1`. Late
  entries after `settle` are still accepted (the entry persists until the next `begin`).
- `last_trail(project, worktree) -> Option<TrailSnapshot>`: `None` if no entry or empty trail; else a clone
  of the trail plus `dropped`, `committed`, `transferring` (the raw flag; no expiry evaluation).
- Entries are never removed from the map (bounded by the number of worktrees; each holds <= 300 small
  entries). No eviction logic.

`AlreadyTransferring` is a unit struct; `transfer_worktree` maps it to
`Err("a transfer is already in progress for this worktree".to_string())`.

#### F3. Locking: where the gate is taken

`DispatchGate` (`dispatch.rs:34`, `pub struct DispatchGate(pub(crate) tokio::sync::Mutex<()>)`) stays a global
`tokio` mutex and keeps `dispatch_apply` atomic per task. It is now held **only for the persist step**.

Add this helper to `transfer.rs` (both directions and the tests use it, so the placement is testable):

```rust
/// Run the short persist step with the dispatch gate held. The long work (import, summarize) runs
/// outside it, under the registry marker.
pub(crate) async fn persist_under_gate<R>(gate: &DispatchGate, persist: impl FnOnce() -> R) -> R {
    let _gate = gate.0.lock().await;
    persist()
}

pub(crate) enum CommitCancel { Honour, Ignore }
pub(crate) enum Committed<T> { Cancelled, Done(Result<T, String>) }

/// The commit step, run inside `persist_under_gate`: mark the transfer committed (atomically with the
/// cancel check) BEFORE persisting; settle the marker if persisting then fails.
pub(crate) fn commit_then_persist<T>(
    registry: &TransferRegistry, project: &str, worktree: &str, cancel: CommitCancel,
    persist: impl FnOnce() -> Result<T, String>,
) -> Committed<T> {
    let committed = registry.mark_committed(project, worktree);
    if !committed && matches!(cancel, CommitCancel::Honour) {
        return Committed::Cancelled;
    }
    let r = persist();
    if r.is_err() {
        registry.settle(project, worktree);
    }
    Committed::Done(r)
}
```

- `transfer_claude_to_codex` (`:244`): delete `let _gate = gate.0.lock().await;`. After the import returns
  success/failure and the outcome is decided (`01`), wrap only the commit + persist:
  `persist_under_gate(&gate, || commit_then_persist(&registry, &project, &worktree, CommitCancel::Honour,
  || persist_worktree_cli(...))).await`. `commit_then_persist` calls `mark_committed` **first**; `false` →
  `Committed::Cancelled` (nothing persisted; the caller returns `Ok(Cancelled)` and the guard drop releases
  the marker); `true` → run the persist closure, and if it returns `Err`, call `registry.settle(..)` so the
  `Err` leaves no marker.
- `transfer_codex_to_claude` (`:314`): move the lock so it covers only
  `write_handoff_summary(...)` + `persist_worktree_cli(&project_config, &worktree, CliKind::Claude, ...)`
  (`:343-347`), via `persist_under_gate(&gate, || commit_then_persist(&registry, &project, &worktree,
  CommitCancel::Ignore, || { write_handoff_summary(..)?; persist_worktree_cli(..) })).await`. `Ignore` means
  the cancel flag is never honoured in this direction (cancel applies only to Claude -> Codex imports): the
  bool from `mark_committed` is discarded and persist always runs; a persist `Err` still settles. The session
  release (`release_codex_session`) and the summarize run now execute without the gate; they are protected
  from dispatch by the registry marker. (This is the "gate placement" change for Codex → Claude, **approved
  and in scope**; no other Codex → Claude behaviour changes.)
- The registry mutex and the gate are never held together: `persist_under_gate` is entered with no registry
  lock held, and the registry methods called inside the closure (`push_trail`) take the std mutex only
  briefly and never wait on the gate, so lock order is always gate -> (short) registry mutex, never the
  reverse.

#### F4. Marker lifetime and `dispatch_apply` refusal

The marker is the registry entry's `transferring` flag: set by `begin` at the start of `transfer_worktree`
(either direction), held until the frontend's restart completes (the frontend calls `transfer_settled`),
closing today's window where `cli=codex` is persisted but the live session is still Claude.

- Backstop: 300 s from `begin` while importing (above the ~150 s import deadline); 180 s from
  `mark_committed` (above the frontend's 120 s `restarting` backstop, which is above the strict chain's ~100 s
  budget). Lazy, per above. Expiry is recorded
  as trail `Settle {"reason":"backstop"}` and a `log::warn!`.
- `begin` on a live (unexpired) marker returns `AlreadyTransferring` (backend re-entry guard).
- Guard drop on `Cancelled`/`Err` releases the marker; a committed transfer keeps it until settled.

**`dispatch_apply` (`dispatch.rs:234`).** Add a trailing injected parameter
`registry: State<'_, crate::transfer_registry::TransferRegistry>` (not an IPC argument; the JS call is
unchanged) and add `use crate::transfer_registry::TransferRegistry;`. The command today takes only
`worktree` (no project): the project is the active project resolved by `crate::tasks::open_backend()`
(`dispatch.rs:246`, returns `(backend, config)`; `config.project.name`). The check goes **immediately after
`open_backend()?` and before `backend.dispatch_reserve(...)`** (`:255`), so a refusal never reserves a task
(preserving the "never Err after reserve" invariant stated in that function):

```rust
let (backend, config) = crate::tasks::open_backend()?;
if let Some(reason) = transfer_refusal(&registry, &config.project.name, &worktree) {
    return Err(reason);
}
```

with a pure, unit-testable helper in `dispatch.rs`:

```rust
/// `Some(error)` when `worktree` of `project` is mid-transfer; dispatch must not inject into it.
fn transfer_refusal(registry: &TransferRegistry, project: &str, worktree: &str) -> Option<String> {
    registry.is_transferring(project, worktree).then(|| {
        format!("a transfer is in progress for worktree `{worktree}`; try again when it finishes")
    })
}
```

Exact error string: ``a transfer is in progress for worktree `<worktree>`; try again when it finishes``.
The check sits inside the gate (the gate is taken at `:244`, before `open_backend`); that is fine because the
gate is now held only briefly by transfers, and `is_transferring` reads only a copied bool under the registry
mutex. Other worktrees are unaffected. No other injection path is changed (`inject_handoff_summary` is part
of the transfer itself).

#### F5. `transfer_worktree` skeleton (for orientation; `01` owns the body)

1. `let guard = registry.begin(&project, &worktree).map_err(|_| "a transfer is already in progress for this worktree".to_string())?;`
2. `refuse_terminated`, recovery lock, direction functions (project resolved by name inside them, F1).
3. Gate only inside `persist_under_gate` (F3); `registry.mark_committed` runs **before** persist, inside the
   gate (`commit_then_persist`); a persist failure settles the marker.
4. On `Ok(Cancelled)` or any `Err`, dropping `guard` releases the marker.
5. Keep emitting `worktrees://changed` after success (existing behaviour at `:200-205`).

`guard` must be declared before `with_recovery_lock(...)` so it outlives it; `TransferGuard<'a>` borrows the
`State<TransferRegistry>` deref and is `Send` (the registry is `Mutex`-based), so holding it across `.await`
in the async command is fine.

### G. Diagnostics

Each transfer writes one structured trail through `log::` to `<KEITORA_HOME>/logs/keitora.log`
(`logging.rs`: `log_dir()` = `paths::keitora_home().join("logs")`, file `keitora.log`; read-only use here,
no changes to the logging setup). `logging::format_line` renders `{ts} {LEVEL} [{target}] {message}` with
newlines escaped, so a record is one line. Use target `transfer`; message prefix `transfer:` plus
`project/worktree` and the stage, so a user can `grep "transfer:"`. The trail records:

- the `CodexEnv` (exe, version, whether `codex_home` came from env or default) — stage `Env`;
- the `importId` — stage `Import`;
- the classified result and the raw `failures[]` — stage `Classify`;
- the stderr tail — stage `Import`/`Classify`;
- each launch-chain step — stage `Launch`;
- each verification observation (`agent`, `agent_session`, timings) — stage `Verify` (emitters should log
  an observation only when it changes, not on every poll, so one transfer stays well under
  `MAX_TRAIL_ENTRIES`);
- persistence intents and the outcome — stage `Persist`;
- settle/backstop/guard drop — stage `Settle` (emitted by the registry itself).

#### G1. Helper (the only way emitters write the trail)

In `transfer_registry.rs`:

```rust
pub fn trail(project: &str, worktree: &str, stage: TrailStage, detail: serde_json::Value) {
    TransferRegistry::global().trail_on(log::Level::Info, project, worktree, stage, detail)
}
pub fn trail_warn(/* same */) { /* Level::Warn */ }

impl TransferRegistry {
    /// Sanitize `detail` once, write the log line, push to this registry. Public so tests can use an
    /// isolated registry; production code calls the free functions above.
    pub fn trail_on(&self, level: log::Level, project: &str, worktree: &str, stage: TrailStage, detail: serde_json::Value) {
        let detail = sanitize_detail(detail);
        log::log!(target: "transfer", level, "{}", format_trail_line(project, worktree, stage, &detail));
        self.push_trail_sanitized(project, worktree, stage, detail);
    }
}

/// `transfer: <project>/<worktree> [<stage>] <compact json>` — the message part of the log line.
pub(crate) fn format_trail_line(project: &str, worktree: &str, stage: TrailStage, detail: &serde_json::Value) -> String {
    format!("transfer: {project}/{worktree} [{}] {detail}", stage.as_str())
}
```

`TrailStage::as_str()` returns `"env" | "import" | "classify" | "persist" | "launch" | "verify" | "settle"`
(same strings as the serde snake_case form). `push_trail` (public, contract) = `sanitize_detail` then the
internal push, for callers that only want the in-memory half; `trail_on` must not sanitize twice (it calls
the `push_trail_sanitized` internal). The `Settle` entries the registry emits itself go straight to the
internal push plus their own `log::` line.

**`sanitize_detail(Value) -> Value`** (applied at write time, so the log file and the trail share it):

1. Recursion depth > `MAX_DETAIL_DEPTH` (8): replace the subtree with the string `"[depth limit]"`.
2. For every object key, normalize it (ASCII-lowercase, remove `_` and `-`). If the normalized key is one of
   `prompt, content, text, summary, transcript, firstuserline, token, accesstoken, apikey, secret, password,
   authorization`, replace the **value** with the string `"[redacted]"` (the key stays so a reader sees the
   field existed). `message`, `stderrtail`, `exe`, `version`, `path`, `codexhome` and all others are kept.
3. Every string value longer than `MAX_TRAIL_STRING_CHARS` (2000 chars) is cut to its first 2000 chars plus
   `…[truncated N chars]` (N = chars removed). Count by `chars()`, never by byte index.
4. Numbers, bools and nulls pass through. Arrays are walked element-wise (no element cap; the string cap and
   entry cap bound the size).

Entries are still built to avoid prompt/transcript content in the first place: only paths, ids, versions,
exit codes, durations, protocol error codes and messages, Codex `message` strings and stderr lines (the
diagnostic value, length-capped above).

#### G2. `transfer_diagnostics(project, worktree) -> Result<Option<String>, String>`

```rust
#[tauri::command]
pub fn transfer_diagnostics(project: String, worktree: String, registry: State<'_, TransferRegistry>)
    -> Result<Option<String>, String> {
    Ok(registry.last_trail(&project, &worktree).map(|snap| {
        render_diagnostics(&project, &worktree, &snap, dirs::home_dir().as_deref().and_then(|p| p.to_str()))
    }))
}
```

`dirs` is already a dependency of `src-tauri` (`dirs = "6"`). `render_diagnostics(project, worktree,
&TrailSnapshot, home: Option<&str>) -> String` is a pure function in `transfer_registry.rs`. Output, exactly:

```
keitora transfer diagnostics
app: keitora <CARGO_PKG_VERSION> (<std::env::consts::OS>/<std::env::consts::ARCH>)
transfer: <project>/<worktree>
state: transferring=<true|false> committed=<true|false>
entries: <N>[ (<dropped> earlier entries dropped)]
+<at_ms>ms [<stage>] <compact json of scrubbed detail>
...one line per entry, oldest first...
```

(The `(... dropped)` suffix appears only when `dropped > 0`.) The JSON is `serde_json::to_string` (compact;
object keys sorted because the repo does not enable `preserve_order`). The whole text ends with `\n`.
The header includes the project and worktree names (the user triggers the copy knowingly).

**Home scrub (exact rules).** Applied to every string **value inside the `detail` `Value` tree before
serializing** (so JSON backslash-escaping cannot hide a Windows path) and also to the header lines:

- Source of home: the `home` argument (`dirs::home_dir()` in production; passed explicitly in tests). If
  `None`, empty, or (after trimming trailing `/` and `\`) shorter than 2 chars, no scrub is applied.
- Candidates, from the trimmed home `H`: `H`; `H` with all `\` -> `/`; `H` with all `/` -> `\`; plus the
  Windows verbatim forms `\\?\` + (backslash form) and `//?/` + (forward-slash form). Deduplicate. Replace
  longest candidate first.
- Matching is ASCII case-insensitive (lowercase both copies with `to_ascii_lowercase`, which preserves byte
  offsets) so `C:\Users\Me` also matches `c:\users\me`.
- A match is replaced by `~` only if the next char is end of string, `/`, `\`, or a char that is not
  alphanumeric and not one of `_ - .` — so home `/Users/x` does not eat `/Users/xavier`.
- Nothing else is scrubbed (no username-only replacement, no hostname handling).
- The prompt/secret field redaction already happened at write time (G1); `render_diagnostics` does not
  re-redact. (Defense in depth is cheap: it also runs the key redaction once over the `Value` before
  rendering, via the same `redact_keys` function, so an entry pushed through some future path that skipped
  `sanitize_detail` is still safe.)

The frontend ("Copy diagnostics" toast action, `03-frontend.md`) copies the returned string with
`navigator.clipboard.writeText(text)` — the existing pattern at `TerminalPane.tsx:1094-1095` (there is no
shared clipboard helper in `src/`; wrap in `.catch(() => {})`/a toast on failure as the pane does). The scrub
is in the backend so it is Rust-testable (`00-contracts.md`, resolved ambiguity 2).

## Tests (Rust unless noted)

All use isolated instances (`TransferRegistry::new()`), temp dirs, or pure functions. None touches the
user's running herdr, real config or the real log. Time-based tests use a fixed `let base = Instant::now();`
and pass `base + Duration::from_secs(n)` to the `*_at` twins (never subtract from an `Instant`). Test modules
are the existing `#[cfg(test)] mod tests` in each file; the registry tests live in
`transfer_registry.rs`.

**`crates/keitora-tasks/src/config.rs` (tests module):**

- `project_config_by_name_matches_resolved_name_not_registry_key` — temp dirs `proj-a`, `proj-b` (no
  `keitora.toml`, so the resolved name is the dir basename); registry keys `key-one`, `key-two`. Assert
  `by_name("proj-b")` is `Some` with `.root` of `proj-b`; `by_name("key-two")` is `None`.
- `project_config_by_name_unknown_is_none` — `by_name("nope")` is `None` on a non-empty registry and on an
  empty one.
- `project_config_by_name_skips_unresolvable_entries` — one entry whose `keitora.toml` is invalid TOML plus a
  good one; the good one is found, no panic.

**`transfer_registry.rs`:**

- `begin_twice_is_refused` — second `begin` for the same key while the first guard lives -> `Err(AlreadyTransferring)`; first entry untouched.
- `begin_after_guard_drop_succeeds` — drop the guard (uncommitted), `begin` again -> `Ok`.
- `begin_after_settle_succeeds` — committed transfer, `settle`, then `begin` -> `Ok`.
- `cancel_only_before_commit` — `request_cancel` returns `true` and `cancel_flag().load()` is `true`; after
  `mark_committed`, `request_cancel` returns `false` and a fresh flag from a new transfer is untouched.
- `cancel_without_transfer_is_false` — no entry -> `false`, no panic.
- `guard_drop_releases_marker_unless_committed` — uncommitted: `is_transferring` false after drop; committed:
  still `true` after drop.
- `settle_is_idempotent` — two `settle` calls leave exactly one `Settle` trail entry and `is_transferring`
  false.
- `marker_expires_after_import_ttl` — `is_transferring_at(base + 299s)` true, `(base + 301s)` false and the
  trail ends with a `Settle {"reason":"backstop"}` entry; a subsequent `begin_at` succeeds.
- `marker_expires_after_commit_ttl` — `mark_committed_at(base + 100s)` returns `true`;
  `is_transferring_at(base + 279s)` true, `(base + 281s)` false (180 s TTL).
- `mark_committed_returns_false_after_cancel_and_does_not_commit` — `request_cancel` first; `mark_committed`
  returns `false`; the entry is not `committed`; dropping the guard then releases the marker.
- `mark_committed_true_then_cancel_is_a_noop` — `mark_committed` returns `true`; `request_cancel` returns
  `false` and the flag stays unset.
- `mark_committed_without_an_entry_is_false`.
- `transfer_guard_is_send` — compile-time `assert_send::<TransferGuard<'static>>()`.
- `begin_replaces_trail_and_settle_keeps_it` — push entries, `settle`: `last_trail` still has them; `begin`
  again: `last_trail` is `None` (empty).
- `keys_are_independent` — same worktree name in projects `proj-a` and `proj-b`: marker, cancel and trail do
  not cross.
- `stale_guard_does_not_release_a_newer_transfer` — begin A, let A's marker expire (`begin_at(base+301s)`
  creates B), drop A's guard -> B still `is_transferring`.
- `poisoned_mutex_does_not_wedge` — panic inside a thread while holding the lock (via a test-only closure
  that locks `inner.map`), then `is_transferring` and `begin` still work.
- `trail_is_capped_and_counts_drops` — push `MAX_TRAIL_ENTRIES + 5`; `entries.len() == 300`,
  `dropped == 5`, oldest entry is the 6th pushed.
- `push_trail_without_entry_is_dropped` — no `begin`; `push_trail` then `last_trail` is `None` and no entry
  was created.
- `trail_at_ms_is_monotonic_from_begin` — pushes at `base+10ms`, `base+25ms` give `at_ms` 10 and 25.
- `sanitize_detail_redacts_prompt_and_secret_keys` — nested object with `prompt`, `Content`, `first_user_line`,
  `apiKey`, `message`: the first four values become `"[redacted]"`, `message` kept; keys preserved.
- `sanitize_detail_caps_long_strings_by_chars` — a 5000-char string of multi-byte chars becomes 2000 chars +
  `…[truncated 3000 chars]` and is valid UTF-8; a 2000-char string is untouched.
- `sanitize_detail_depth_limit` — 12-level nested object: deeper subtree replaced by `"[depth limit]"`.
- `trail_on_pushes_sanitized_entry_and_line_format` — the process-wide logger cannot be captured per test, so
  assert the two halves separately: after `registry.trail_on(Level::Info, "proj-a", "alpha", Env,
  json!({"exe":"codex","prompt":"x"}))`, `last_trail` holds one entry with stage `Env` and `prompt` redacted;
  and `format_trail_line("proj-a","alpha",Env,&json!({"exe":"codex"}))` equals
  `transfer: proj-a/alpha [env] {"exe":"codex"}`.
- `render_diagnostics_header_and_order` — two entries: output starts with the 4 header lines + `entries: 2`,
  then `+0ms [env] ...` before `+120ms [classify] ...`; ends with `\n`; dropped suffix absent; with `dropped:
  3` the `entries:` line ends with `(3 earlier entries dropped)`.
- `scrub_home_unix_path` — home `/Users/me`; `"/Users/me/.claude/projects/x.jsonl"` -> `"~/.claude/projects/x.jsonl"`;
  `"/Users/me"` -> `"~"`; `"/Users/medium/x"` untouched.
- `scrub_home_windows_variants` — home `C:\Users\me`; all of `C:\Users\me\.codex`, `C:/Users/me/.codex`,
  `\\?\C:\Users\me\.codex`, `c:\users\ME\.codex` become `~\.codex` / `~/.codex` (the remainder is kept
  verbatim); also scrubbed when the home appears inside a longer string such as a PATH list.
- `scrub_home_noop_cases` — `None`, `""`, `"/"` home: strings unchanged.
- `render_diagnostics_scrubs_values_before_json_escaping` — detail `{"exe":"C:\\Users\\me\\bin\\codex.cmd"}`
  (a single-backslash Windows path in the Rust string) with home `C:\Users\me` renders `~\\bin\\codex.cmd`
  in JSON form and contains no `me\\`.
- `render_diagnostics_redacts_late_unsanitized_entries` — an entry whose detail still has `"prompt":"x"`
  (pushed through the raw internal path) renders `"[redacted]"`.
- `transfer_diagnostics_none_without_trail` (on the pure seam `registry.last_trail(..).map(render..)`) — `None`.

**`transfer.rs`:**

- `resolve_project_finds_non_active_project` — a `UserConfig` with `proj-a` (set as `active`) and `proj-b`:
  `resolve_project(&user, "proj-b")` returns proj-b's config.
- `resolve_project_unknown_errors_without_fallback` — `resolve_project(&user, "nope")` is
  ``Err("project `nope` not found")`` even though an active project exists.
- `transfer_code_never_resolves_the_active_project` — `include_str!("transfer.rs")`, take the text before the
  first `#[cfg(test)]`, assert it does not contain `resolve_active`.
- `persist_under_gate_holds_gate_only_during_persist` — `block_on`: before the call `gate.0.try_lock()`
  succeeds; inside the closure `gate.0.try_lock().is_err()`; after, `try_lock()` succeeds again.
- `gate_is_free_during_the_long_phase` — model the import as an ordinary blocking step *outside*
  `persist_under_gate`: while a spawned thread sleeps 200 ms standing in for the import, the main thread
  asserts `gate.0.try_lock().is_ok()` repeatedly (a concurrent dispatch would not wait).
- `commit_then_persist_cancelled_does_not_persist` — cancel requested, `CommitCancel::Honour`: the persist
  closure is never called, result `Committed::Cancelled`; guard drop releases the marker.
- `commit_then_persist_failure_settles_the_marker` — persist returns `Err`: `is_transferring` is `false`
  afterwards (and a later `begin` succeeds).
- `commit_then_persist_success_keeps_the_marker_until_settle` — persist `Ok`: `is_transferring` still `true`
  after the guard drops; `settle` clears it.
- `commit_then_persist_ignore_runs_persist_even_if_cancelled` — `CommitCancel::Ignore` with the flag set:
  persist runs (Codex -> Claude never honours cancel).
- `handoff_summary_path_is_per_project` — same worktree, projects `proj-a`/`proj-b` give different paths;
  the same pair gives the same path (guards the `inject_handoff_summary` change that keys on the `project`
  param).
- The persist-on-non-active-project, `preview_claude_transfer` and both-direction behaviour tests are
  `02`/`01`'s (they extend the persist fixtures); they must call `resolve_project` with a non-active name.
  This document's coverage of those call sites is the grep-style test above plus the `resolve_project` tests.

**`dispatch.rs`:**

- `transfer_refusal_blocks_only_the_transferring_worktree` — `registry.begin("proj-a","alpha")` held:
  `transfer_refusal(&r,"proj-a","alpha")` is `Some(..)` containing "`alpha`" and "in progress"; for
  `("proj-a","bravo")` and `("proj-b","alpha")` it is `None`.
- `transfer_refusal_clears_after_settle` — after `settle("proj-a","alpha")` returns `None`; after
  `begin_at` + `mark_committed_at` + 181 s it is `None` (backstop).
- `transfer_refusal_message_is_exact` — equals ``a transfer is in progress for worktree `alpha`; try again when it finishes``.

**Frontend (vitest):** none in this document (the wrappers are type-checked; `03` tests behaviour). The
`ipc.ts` wrappers listed in `00-contracts.md` section 2 are covered by `tsc`/`yarn test` compiling.

## Implementation order within this doc

Each step compiles and tests green on its own; run `cargo test -p keitora-tasks` and
`cargo test -p keitora` (the `src-tauri` package) at the end of steps 1, 4 and 7.

1. `crates/keitora-tasks/src/config.rs`: `UserConfig::project_config_by_name` + its three tests.
2. `src-tauri/src/transfer_registry.rs` + `mod transfer_registry;` in `lib.rs`: types, `TransferRegistry`
   (`new`/`global`, all methods with `*_at` twins, guard), `TrailStage`/`TrailEntry`/`TrailSnapshot`,
   `sanitize_detail`, `trail`/`trail_warn`/`trail_on`, `render_diagnostics`, scrub helpers, and all
   registry/diagnostics tests. No other file depends on it yet.
3. `host.rs`: `.manage(crate::transfer_registry::TransferRegistry::global())` after `DispatchGate` (`:2083`);
   add the three new commands to `generate_handler!` after `crate::transfer::inject_handoff_summary`
   (`:2217`). Add the three command stubs to `transfer.rs` (`cancel_transfer`, `transfer_settled`,
   `transfer_diagnostics` per `00-contracts.md`); they only touch the registry so they are complete now.
4. `transfer.rs`: `resolve_project` + tests; thread `project` through `preview_claude_transfer`,
   `inject_handoff_summary`, `transfer_worktree` (add the `project` param, `registry` state, `begin`/guard,
   `mark_committed`), `transfer_claude_to_codex`, `transfer_codex_to_claude`; replace all six
   `resolve_active()`; add `persist_under_gate` and narrow the gate in both directions; add the grep-style and
   gate tests. If `01`/`02` have not landed, `persist_worktree_cli` gets only the new leading `project`
   parameter here and `transfer_worktree` keeps returning `()` until `01` changes the return type — in that
   case merge only the signature items that exist (the contract fixes the final shapes).
5. `dispatch.rs`: `registry` State param, `transfer_refusal`, the check after `open_backend()?`, and tests.
6. `src/lib/ipc.ts`: update `previewClaudeTransfer`, `transferWorktree`, `injectHandoffSummary` to take
   `project` first (and `transferWorktree` to return `TransferOutcome` once `01` lands); add
   `cancelTransfer`, `transferSettled`, `transferDiagnostics`; update every caller in `App.tsx`/components to
   pass the captured `activeProject` (the call-site behavioural change belongs to `03`, but the build must
   stay green in this step, so pass `activeProject` mechanically).
7. Grep gates before commit: `git grep -n "resolve_active" -- src-tauri/src/transfer.rs` is empty;
   `git grep -nE '#[0-9]{3,4}' -- <changed files>` is empty (AGENTS.md); test fixtures use placeholder
   names only (`proj-a`, `proj-b`, `alpha`, `/Users/me`).

## Risks and mitigations

- **Marker never cleared** (frontend crash / lost call). Mitigation: lazy backstop expiry (300 s / 180 s)
  plus idempotent `transfer_settled`. The backstop is evaluated on access, so a never-polled marker is
  harmless (nothing reads it) and an expired one is cleared by the next `begin`/`is_transferring`.
- **Gate narrowing exposes interleaving.** Mitigation: the marker refuses dispatch to the transferring
  worktree for the whole window; other worktrees' dispatches are unaffected by design.
- **Diagnostics leaking sensitive data.** Mitigation: no prompt/transcript content captured at the source,
  key redaction at write time and again at render, `$HOME` replacement, length caps; the user explicitly
  triggers the copy.
- **Signature churn across the IPC boundary** (frontend and backend merge order). Mitigation: signatures
  are fixed in `00-contracts.md`; step 6 keeps the build green mechanically.
- **Poisoned lock.** Mitigation: `unwrap_or_else(|e| e.into_inner())` everywhere; tested.
- **Wrong project resolved when two registry entries share a resolved name.** Mitigation: documented
  first-in-registry-key-order rule; same exposure as `launch_for` today.

## Decisions this document depends on

- `DispatchGate` stays global, held only for persist (both directions). The import/summarize runs under the
  per-worktree marker that `dispatch_apply` respects; the marker clears via `transfer_settled` with a lazy
  backend backstop (values resolved in `00-contracts.md`: 300 s / 180 s).
- An explicit `project` (the resolved `[project].name`) is threaded through the transfer IPC commands; all
  transfer code resolves by name via `UserConfig::project_config_by_name`.
- Cancel allowed only before persist (`mark_committed(..) -> bool`, called before persist and atomic with the
  cancel check, makes it a no-op afterwards), and only for Claude -> Codex imports.
- Redaction uses the `"[redacted]"` marker (approved); the Codex -> Claude gate narrowing through
  `persist_under_gate` is approved and in scope.
- Diagnostics: structured `log::` trail (target `transfer`) to `<KEITORA_HOME>/logs/keitora.log`, last trail
  kept in memory per worktree (<= 300 entries, 2000 chars per string), "Copy diagnostics" via a command with
  `$HOME` scrubbing and prompt content redacted.
- The reporter runs Codex 0.166.0 with no env overrides; the trail records whether `codex_home` came from
  env or default so drift can be ruled in or out remotely.
- #29 is a likely symptom of R3; recount after landing before closing it.
- Touches: `transfer.rs`, new `transfer_registry.rs`, `lib.rs`, `dispatch.rs`, `host.rs` (state + handler
  registration only), `crates/keitora-tasks/src/config.rs` (one method), `logging.rs` (read-only use),
  `src/lib/ipc.ts` (+ mechanical caller updates).

## Open questions (for the orchestrator)

1. `01` lists `begin` as step 1 and project resolution as step 2; this document keeps that order (begin
   first; a resolution failure then drops the guard and returns `Err`). Equivalent outcome, noted only so
   `01` does not reorder.
2. (Resolved: approved.) Narrowing the gate in Codex → Claude is a behaviour change to a direction the
   proposal otherwise leaves alone (that path used to hold the gate across the summarize step); the marker
   covers the safety.
3. (Resolved.) `02`'s verification loop now pushes a `Verify` entry only on a changed observation.
4. (Resolved.) `02`/`03` now use the positional trailing `ptySpawn(..., fresh?, transfer?)`; the `transfer`
   key is omitted from the payload unless true.
5. (Resolved.) `last_trail` returns `Option<TrailSnapshot>`; `00`, `01`, `02`, `04` agree, and `01`/`02` tests
   that inspect the trail use it.
