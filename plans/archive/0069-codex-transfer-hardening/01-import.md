# 0069 / 01 — Import: outcome, cancel, protocol-driven import

Part of 0069 "Harden Claude → Codex transfer" (proposal: `../0069-codex-transfer-hardening.md`).
Shared interfaces: **`00-contracts.md`** (read first: `TransferOutcome`, `TransferFailure`, IPC signatures,
`transfer://progress`, `TransferRegistry`). Siblings: `02-launch-and-env.md`, `03-frontend.md`,
`04-scoping-locking-diagnostics.md`.

Paths are under `src-tauri/src/` unless noted. Line numbers are as of `4178d29`; `transfer.rs`, `host.rs`
and `src/` are unchanged since `e9c5d27` (only plan documents were committed since), and every reference
below was re-verified against that code.

Conventions for the implementer (from `AGENTS.md`): no plan or task identifiers (`R1`, `Stage`, `#0069`)
in code comments or commit messages; fixtures use placeholder names only (`/home/u/...`, `proj`,
`thread-a`); new logic ships with `#[cfg(test)]` coverage in the same change.

## Goal

"Transfer to Codex" must always end with a running Codex session in the pane: resuming the imported Claude
conversation when the import works, otherwise a fresh Codex conversation. This document covers the
import half: deciding, reliably and with classified failures, whether a Claude session was imported into
Codex and under which thread id, and producing the `TransferOutcome`. The launch that follows is
`02-launch-and-env.md`; the UI is `03-frontend.md`.

## Root causes covered

### R1 — Ledger hash race (high) → symptom "Codex reported that the Claude import completed, but did not record an imported thread."

`run_claude_import` (`transfer.rs:533`):

- discards the `externalAgentConfig/import` response (`:545`);
- waits for `.../import/completed` and discards its params (`:551`);
- `match_ledger_thread` (`:661`, called at `:555`) then re-hashes the **live** JSONL. `ledger_match`
  (`:681`) requires an exact `source_path` + `content_sha256` + thread-id match in
  `$CODEX_HOME/external_agent_session_imports.json`.

Two things make that hash go stale: Claude is not stopped (`transfer.rs:186`, "does not touch the daemon
client"), and the "Transferring…" overlay renders only when `status !== "running"` (`TerminalPane.tsx:1670`)
so the user can keep typing (the overlay fix is in `03-frontend.md`). Any write to the JSONL during the
import breaks the match.

Upstream Codex (≥ 0.149) returns the answer directly. **These field names come from upstream source and
cannot be verified in this repo** (see "Assumptions about upstream Codex" below for how tests pin them):

- the import response carries `{importId}`;
- the matching completion notification carries `itemTypeResults[].successes[].target`, which is the
  imported **thread id**;
- it also carries `failures[]` with `failureStage`, `errorType`, `subErrorType` and `message`.

Other "completed without a new record" cases that today are all flattened into R1's message:

- a no-op re-import of an unchanged source;
- a deferred import: the thread is open, or diverged, or the hash changed but the transcript didn't;
- a transcript with no `cwd`, messages or timestamps;
- a `continued-in` record (≥ 0.164, which applies to the reporter's 0.166);
- a path outside `$HOME/.claude/projects`. Codex ignores `CLAUDE_CONFIG_DIR`, but Keitora honours it
  (`claude_projects_dir_from`, `transfer.rs:735-744`; `claude_projects_dir`, `:730`).

### R2 — Codex's real error is always lost (high)

The stderr reader uses `read_to_string` (`transfer.rs:1212`), which returns only at EOF. At the error site
(`:555-560`, `stderr_tail()` at `:558`) Codex is alive, so the tail is always empty. `run_claude_import`
logs nothing. (The log trail itself is `04-scoping-locking-diagnostics.md`; this document supplies the
stderr ring and the classified failures that feed it.)

### R8 (parts) — smaller correctness issues owned here

- **No cancel (medium)** in the import path.
- **Import teardown force-kills 200 ms after closing stdin (low-medium)** (`Drop for AppServer`, `:1384`;
  the 200 ms deadline is `:1389`).
- **`classify` treats string-id server requests as notifications (low)** and silently ignores them
  (`classify`, `:1117-1146`; `Frame::ServerRequest.id` is `i64`, `:1087`; `Value::as_i64` at `:1119` returns
  `None` for a string id, so the frame falls to the `(None, Some(m))` notification arm).
- **`find_live_claude_jsonl` picks the newest `.jsonl` (low)** (`:792`). It doesn't skip `agent-*.jsonl`
  (sub-agent transcripts). The persisted `claude_session_id` is deliberately **not** preferred: it is only
  written by a transfer, so it goes stale (see B9).

(Project scoping and the global `DispatchGate` are in `04-scoping-locking-diagnostics.md`; persistence and
the resume-store outranking the registry are in `02-launch-and-env.md`; re-entry guard is `03`/`04`.)

## Scope and non-goals

In: everything in sections A and B below, and their tests. Out of this document: persistence/launch
(`02`), env resolution internals (`02` provides `CodexEnv`; this document consumes it), frontend (`03`),
registry/gate/diagnostics (`04`). Out of the whole effort: token-size UI (#26), oversized-transfer routing
(#28), cost measurement (#27), default launch/boot-resume/recovery changes, upstream herdr patches, a
redesign of Codex → Claude (it changes only where shared code moves: project parameter, outcome type, gate
placement). The Codex → Claude path (`transfer_codex_to_claude`, `run_codex_handoff_summary`, `run_bounded`,
`SUMMARY_TIMEOUT`) is **not** modified except for the signature plumbing in section A.

## Dependencies on siblings

- `CodexEnv` from `02-launch-and-env.md` (section D there; `src-tauri/src/codex_env.rs`). Fields this document
  relies on: `exe: String`, `exe_source: ExeSource`, `path: Option<String>` (value for the child's `PATH`;
  `None` = leave inherited), `codex_home: PathBuf`, `codex_home_source: CodexHomeSource`
  (`Initialize | EnvVar | Default`), `version: Option<String>`. **`codex_home` is resolved before the child
  exists** (the `CODEX_HOME` env var if set and non-empty, else `~/.codex`); the `initialize` result's
  `codexHome` overrides it **for the ledger lookup only** (`CodexEnv::with_initialize_home`, which also flips
  the source to `Initialize`; in `import_over` this is the `ledger_home` computed in B7). `CODEX_HOME` is set
  on the import child (and on the pane, `02`) **only when `codex_home_source == EnvVar`**. Until `02` lands,
  add a stub in `transfer.rs` (`CodexEnv::legacy(codex: String)` = `exe: codex`, `exe_source: BareName`,
  `path: None`, `codex_home: codex_home()?` (`:644`), `codex_home_source: EnvVar` when the env var is set
  else `Default`, `version: None`) so this document is buildable in parallel; the `AppServer::spawn`
  signature below is the contract.
- `FieldIntent<T> { Set(T), Clear, Keep }` and the project-parameterised `persist_worktree_cli` from `02`
  (`02` section C). Until it lands, add the same enum locally with the same name.
- `TransferRegistry` (`00-contracts.md` section 4, implemented in `04`): `begin` guard (which is `Send`) and
  `cancel_flag()`, `mark_committed(..) -> bool` (called before persist, atomically with the cancel check),
  `settle`, and the free `crate::transfer_registry::trail(project, worktree, TrailStage, detail)` helper
  (which writes the log line and pushes to the registry; `at_ms` is computed there). This document calls
  them; if `04` has not landed, use a minimal local shim with the same signatures.
- `transfer://progress` payload and `TransferOutcome`/`TransferFailure` types: defined in
  `00-contracts.md` sections 1 and 3. **This document creates the Rust definitions** in `transfer.rs`
  exactly as written there (the section-1 types derive `Debug, Clone, PartialEq, Eq, Serialize`, with
  `rename_all_fields = "camelCase"` which needs serde >= 1.0.185 (the repo's `Cargo.lock` has 1.0.228, so
  it is available); `TransferProgress` derives `Debug, Clone, Serialize`, `ProgressPhase` additionally
  `Copy, PartialEq, Eq`).
- Project lookup by name (`04`). Until it lands: `keitora_tasks::resolve_active()` and `Err` if its
  `project.name != project`.

## Assumptions about upstream Codex (explicit; unverifiable locally)

Everything below is parsed as `serde_json::Value` with every field optional. A wrong assumption must degrade
to the ledger fallback or to `Other`/`ProtocolError`, never to a panic and never to a wrong thread id.

| # | Assumption | Used for | If wrong |
|---|---|---|---|
| A1 | `externalAgentConfig/import` response `result.importId` is a string | match the completion | `import_id=None` → legacy path: first `completed` wins, then ledger fallback |
| A2 | completion `params.importId` is a string; absent is treated as a match | match | a completion for another import with an id is ignored; absent id is accepted |
| A3 | completion `params.itemTypeResults` is an array of `{ itemType, successes[], failures[] }`; the entry we use has `itemType == "SESSIONS"` (else the only entry, else none) | thread id / failures | no item → `LedgerFallback{noop:false}` |
| A4 | `successes[].target` is the imported thread id (string) | thread id | no string target → treated as no success |
| A5 | `failures[]` entries have string `failureStage`, `errorType`, `subErrorType`, `message` | classification | unknown → `Other` with whatever `message` exists |
| A6 | `initialize` result has `codexHome` (absolute path string) and `userAgent` (string) | ledger home, trail | absent → `CodexEnv.codex_home`; relative/non-existent → ignored |
| A7 | progress notifications use method `externalAgentConfig/import/progress`, with optional string `params.message` | progress detail | no detail; phases still emitted |
| A8 | an unknown method returns JSON-RPC error `-32601` | `CodexTooOld` | other code → `ProtocolError` |
| A9 | `codex app-server` exits when its stdin reaches EOF | graceful teardown | the 3 s grace expires and the tree is killed (costs 3 s, never wedges) |

**How tests pin them.**

1. All shapes live in named fixtures (`const COMPLETION_SUCCESS: &str`, `COMPLETION_SESSION_NOT_DETECTED`,
   `COMPLETION_CONTINUED_IN`, `COMPLETION_DEFERRED`, `COMPLETION_NOOP`, `COMPLETION_EMPTY`,
   `IMPORT_RESPONSE`, `INITIALIZE_RESULT`) in `transfer.rs`'s `mod tests`. Changing an upstream field name
   means editing one fixture, which makes the drift explicit in review.
2. `upstream_shape_fixtures_pin_field_names` parses every fixture and asserts each field path in A1-A7
   exists in the fixture that is supposed to carry it (so a fixture cannot silently lose a field).
3. Behavioural detection for A1/A8 is tested directly (`import_over_legacy_response_without_import_id_uses_ledger`,
   `import_over_import_method_not_found_is_codex_too_old`).
4. One-time manual check at the review gate (not CI): run one real transfer against Codex ≥ 0.149 and compare
   the trail's `completion_keys` / `response_keys` entries (below) with the fixtures. If the installed Codex
   offers `codex app-server generate-json-schema`, comparing against its output is a second option; whether
   that subcommand exists is itself unverified, so the plan does not depend on it.
5. Runtime visibility: the `import` trail entries record only key names and counts (never values that could
   contain transcript text), so a remote debugging session on another machine can see shape drift.

## Design

### A. Transfer outcome and cancel

`transfer_worktree(project, worktree, target_cli)` returns `Result<TransferOutcome, String>` (exact shape in
`00-contracts.md` section 1). `target_cli` stays `CliKind` (`transfer.rs:168`; it deserialises from
`"claude"`/`"codex"`), not `String`.

```
Imported { thread_id }                    // Claude→Codex: resume the imported thread
Fresh    { reason: TransferFailure, .. }  // Claude→Codex: import failed; worktree is Codex, start fresh
Done                                      // Codex→Claude: summary handoff complete (unchanged semantics)
Cancelled                                 // user cancelled during import; nothing changed, still Claude
```

`Err` is reserved for "nothing changed": the session is terminated (`refuse_terminated`, `:203`), the
worktree or project can't be resolved, a transfer is already in flight, or persisting failed.

**Internal import result** (private to `transfer.rs`; the only thing the blocking code returns):

```rust
enum ImportResult {
    Imported { thread_id: String },
    Failed(TransferFailure),
    Cancelled,
}
```

**`TransferFailure`** is the contract enum (`CodexNotFound`, `CodexTooOld`, `SessionNotDetected`,
`Skipped{reason}`, `Timeout{phase}`, `ProtocolError`, `Other`). Each carries Codex's `message` and the
stderr tail. `impl TransferFailure { fn user_copy(&self) -> String }` is the single place that maps a
variant to `reason_text` (no trailing period; exact strings, pinned by a test):

| Variant | `user_copy()` |
|---|---|
| `CodexNotFound` | `Codex could not be found` |
| `CodexTooOld` | `This version of Codex cannot import Claude sessions` |
| `SessionNotDetected` | `Codex did not recognise the Claude session` |
| `Skipped{NoOp}` | `Codex found nothing new to import` |
| `Skipped{Deferred}` | `Codex postponed the import because the session is still in use` |
| `Skipped{ContinuedIn}` | `This Claude conversation was continued in another session` |
| `Timeout{Initialize}` | `Codex did not start in time` |
| `Timeout{Import}` | `Codex did not accept the import in time` |
| `Timeout{Completion}` | `Codex did not finish the import in time` |
| `ProtocolError` | `Codex returned an unexpected response` |
| `Other` | `The import did not complete` |

**Outcome → persistence.** A pure function decides the intents; `02`'s `persist_worktree_cli` executes them:

```rust
struct PersistPlan {
    cli: CliKind,                          // always CliKind::Codex
    codex_thread_id: FieldIntent<String>,
    claude_session_id: FieldIntent<String>,
}
fn persist_plan(result: &ImportResult, claude_session_id: Option<String>) -> Option<PersistPlan>;
```

| `ImportResult` | `persist_plan` | resume-store row (done by `02`) | Outcome |
|---|---|---|---|
| `Imported{id}` | `Some{ codex, Set(id), claude_session_id.map_or(Keep, Set) }` | replace with codex + id (`ResumeStore::replace`) | `Imported` |
| `Failed(_)` | `Some{ codex, Clear, claude_session_id.map_or(Keep, Set) }` | clear (`ResumeStore::clear`) | `Fresh` |
| `Cancelled` | `None` (no persist call at all) | unchanged | `Cancelled` |

`claude_session_id` is `session_id_from_jsonl(&jsonl)` (`:838`), or `None` when no JSONL was selected (then
`Keep`). A failed import **always** switches to Codex; Claude stays live only until the outcome is known.
The Fresh fallback starts Codex blank.

**Cancel mechanism (how the flag reaches blocking code).**

- `transfer_worktree` calls `registry.begin(&project, &worktree)` first; the returned `TransferGuard` is
  `Send` and is held across the `await`s. `guard.cancel_flag()` returns the `Arc<AtomicBool>`.
- The `Arc` is moved into the `spawn_blocking` closure and passed down: `run_claude_import(.., cancel, ..)`
  → `AppServer::spawn(.., cancel, ..)` (stored in the `AppServer.cancel` field) and into `ledger_lookup`.
  Nothing else observes it; there is no global.
- `AppServer::next_frame` (below) checks `cancel.load(Ordering::Acquire)` at the top of **every** loop
  iteration and slices `recv_timeout` to `POLL_SLICE = 250 ms`, so cancel latency is at most 250 ms.
  `ledger_lookup` checks it on every poll (also 250 ms).
- On observing the flag the blocking code: emits `Progress{Cancelling}`, calls `server.abort()` (selects the
  immediate teardown, below), drops the server (tree kill included), and returns `ImportResult::Cancelled`.
- `cancel_transfer(project, worktree)` calls `registry.request_cancel` (04) and returns `Ok(())`
  regardless of the bool.
- Cancel is only honoured **before** persistence. The commit point is `registry.mark_committed(..) -> bool`,
  called **before** `persist_worktree_cli`, under the dispatch gate (inside `persist_under_gate`, `04`), and
  atomically with the cancel check. It returns `true` when the transfer is now committed, `false` when cancel
  had already been requested (then the result is `Cancelled` and nothing is persisted). After it returns
  `true`, `request_cancel` is a no-op. A cancel flag seen after the import returned but before
  `mark_committed` also yields `Cancelled` (nothing was changed yet). If persisting fails after a `true`, the
  backend calls `registry.settle(..)` so the `Err` leaves no transferring marker.
- Cancel applies to this (Claude → Codex) path only; the Codex → Claude path never reads the flag.

### B. Import: trust the protocol, not the hash

#### B1. Constants (replace `IMPORT_COMPLETED_TIMEOUT` `:61` and `REQUEST_TIMEOUT` `:65`; delete both)

```rust
const IMPORT_DEADLINE: Duration = Duration::from_secs(150);   // one budget: spawn + initialize + import + completion
const POLL_SLICE: Duration = Duration::from_millis(250);      // recv_timeout slice; cancel latency bound
const TEARDOWN_GRACE: Duration = Duration::from_secs(3);      // stdin-EOF wait before the kill
const LEDGER_WINDOW: Duration = Duration::from_secs(3);       // late-flush window; OUTSIDE the deadline
const LEDGER_POLL: Duration = Duration::from_millis(250);
const STDERR_RING_MAX_LINES: usize = 200;
const STDERR_RING_MAX_BYTES: usize = 16 * 1024;
const STDERR_LINE_MAX_BYTES: usize = 2 * 1024;
```

`SUMMARY_TIMEOUT` (`:69`) and `EXTERNAL_AGENT_IMPORT_COMPLETED` (`:58`) are unchanged. Add
`const EXTERNAL_AGENT_IMPORT_PROGRESS: &str = "externalAgentConfig/import/progress";`.

**How the deadline is threaded.** `run_claude_import` computes `let deadline = Instant::now() + IMPORT_DEADLINE;`
as its first statement (before preflight and spawn) and passes it to `AppServer::spawn`/`from_streams`,
which store it in `AppServer.deadline`. `request`, `wait_notification` and `handshake` lose their `timeout`
parameters and all use `self.deadline`. Which phase timed out is known by the call site (see B5), not by
the AppServer. Worst case wall time until the outcome is returned: 150 s deadline + 3 s ledger window (the ledger window
is the only thing past the deadline). A graceful teardown (up to 3 s) runs on a detached thread **after** the
outcome is returned (B3), so it does not add to the wall time; a cancel or timeout kills immediately. The
registry marker's 300 s initial expiry (`00-contracts.md` section 4) covers all of it. The progress event's `deadline_ms` is
`IMPORT_DEADLINE.as_millis() as u64`.

#### B2. `Frame` and `classify` (change `:1077-1146`)

```rust
enum Frame {
    Response { id: i64, result: Option<Value>, error: Option<RpcErrData> },
    Notification { method: String, params: Value },
    ServerRequest { id: Value, method: String },   // id: Value (was i64): integer OR string
    Junk,
}
```

`classify` rules (replace the `match`):

1. `method = v["method"].as_str()`; `id = v.get("id").filter(|i| !i.is_null())`.
2. `id` present and `method` present → `ServerRequest { id: id.clone(), method }` (integer or string id).
3. `id` present, no `method`: if `id.as_i64()` is `Some` → `Response` (as today); a **string** id on a
   response is `Junk` (we only ever send integer ids).
4. no `id`, `method` present → `Notification` (params default `Value::Null`).
5. otherwise `Junk`.

`reject_server_request(&mut self, id: &Value)` replies `{"id": <same value>, "error": {"code": -32601,
"message": "Unsupported server request"}}` (the id is echoed verbatim, string stays a string).

#### B3. Generic `AppServer` (reader/writer-generic, child spawn is one constructor)

Decision: **generic over the writer only**, `AppServer<W: Write>`. The reader and stderr streams are consumed
by their pump threads at construction, so they are generic *constructor* parameters
(`R: Read + Send + 'static`), not struct parameters. (The earlier "R, W" sketch is dropped: a struct
parameter for a stream the struct never stores buys nothing.) Teardown is a boxed closure so the fake has no
child.

```rust
struct AppServer<W: Write> {
    stdin: Option<W>,                       // None after close
    rx: Receiver<Frame>,                    // fed by the stdout pump thread
    stderr: StderrRing,                     // clone-able handle, see B4
    next_id: i64,
    notif_buf: VecDeque<(String, Value)>,   // unchanged (was :1158)
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    aborted: bool,                          // set by abort(): teardown uses grace 0
    teardown: Option<Box<dyn FnOnce(Duration) + Send>>,  // called once from Drop with the grace period
}

impl<W: Write> AppServer<W> {
    fn from_streams<R, E>(reader: R, stderr: E, writer: W, cancel: Arc<AtomicBool>, deadline: Instant,
                          teardown: Option<Box<dyn FnOnce(Duration) + Send>>) -> Self
    where R: Read + Send + 'static, E: Read + Send + 'static;
    fn abort(&mut self);                    // aborted = true
    fn handshake(&mut self) -> Result<HandshakeInfo, ReqErr>;
    fn request(&mut self, method: &str, params: Value) -> Result<Value, ReqErr>;
    fn wait_notification(&mut self, method: &str, matches: impl FnMut(&Value) -> bool,
                         on_other: impl FnMut(&str, &Value)) -> Result<Value, ReqErr>;
    fn stderr_tail(&self) -> String;        // no leading newline; "" when empty
    fn stderr_tail_settled(&self) -> String; // waits up to 200 ms for stderr EOF, then stderr_tail()
}

impl AppServer<ChildStdin> {
    /// The child-spawn constructor. Builds the Command, takes the three pipes, wraps the Child in the
    /// teardown closure, and calls `from_streams`.
    fn spawn(env: &CodexEnv, cwd: &str, cancel: Arc<AtomicBool>, deadline: Instant)
        -> Result<Self, SpawnError>;
}

enum SpawnError { NotFound(String), Other(String) }   // NotFound = io::ErrorKind::NotFound

struct HandshakeInfo { codex_home: Option<PathBuf>, user_agent: Option<String> }  // A6
```

`impl<W: Write> Drop for AppServer<W>` replaces `:1384-1411`:

1. `drop(self.stdin.take())` (EOF).
2. `if let Some(t) = self.teardown.take() { t(if self.aborted { Duration::ZERO } else { TEARDOWN_GRACE }) }`.

**Who drops it, and where.** `AppServer<ChildStdin>` is `Send` (a `Receiver`, a `ChildStdin`, `Arc`s, a
`Box<dyn FnOnce + Send>`), so `run_claude_import` decides after `import_over` has produced the result:

- the server was **aborted** (cancel, deadline/timeout; `import_over` calls `abort()` on every such path, B8):
  drop it inline, so the kill is immediate and synchronous;
- otherwise (the import finished or failed normally): move it to a detached thread,
  `std::thread::Builder::new().name("codex-teardown".into()).spawn(move || drop(server))`, and return the
  result at once. The thread closes stdin, waits up to `TEARDOWN_GRACE` for the child to exit, then kills the
  tree. If the thread cannot be spawned the server is dropped inline (correct, just slower).

The result (including `stderr_tail`) is fully computed before the hand-off, so nothing reads the server
afterwards. The detached thread is not joined; if the app quits first the closed pipes end the child
(A9) or the OS reaps it with the app.

`ReqErr` (`:1100`) gains variants; existing ones and their `Display` text are unchanged:

```rust
enum ReqErr { Rpc { code: i64, message: String }, Transport(String), Closed, TimedOut, Cancelled }
```
`Display`: `Closed` → `codex app-server closed unexpectedly`, `TimedOut` → `timed out waiting for codex app-server`,
`Cancelled` → `cancelled`.

**The one receive loop** (used by `request` and `wait_notification`; this is where cancel and the deadline
are enforced):

```rust
fn next_frame(&mut self) -> Result<Frame, ReqErr> {
    loop {
        if self.cancel.load(Ordering::Acquire) { return Err(ReqErr::Cancelled); }
        let now = Instant::now();
        if now >= self.deadline { return Err(ReqErr::TimedOut); }
        let slice = (self.deadline - now).min(POLL_SLICE);
        match self.rx.recv_timeout(slice) {
            Ok(f) => return Ok(f),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err(ReqErr::Closed),
        }
    }
}
```
(The two old inline loops at `:1278-1315` and `:1330-1354` are deleted.)

- `request`: send, then loop on `next_frame`: matching-id `Response` returns (error → `ReqErr::Rpc`);
  `Notification` → push to `notif_buf`; `ServerRequest{id,..}` → `reject_server_request(&id)`; others ignored.
- `wait_notification(method, matches, on_other)`: first scan `notif_buf` for the first entry with
  `m == method && matches(&params)`; remove and return it (this is what handles a completion that arrived
  before the response, `:1322` today). Otherwise loop on `next_frame`: a `Notification` with the right method
  and `matches(params)` returns; any other `Notification` is passed to `on_other(method, params)` (used for
  progress) and dropped; `ServerRequest` → reject; `Response`/`Junk` ignored.
- `handshake`: the `initialize` request body is unchanged (`:1233-1245`); the result is parsed into
  `HandshakeInfo` (`codexHome` kept only if it is a non-empty absolute path; `userAgent` if a string), then
  the `initialized` notification is sent.
- `send` (`:1252`) is unchanged except it uses `W: Write`.

**`AppServer::spawn` details (replaces `:1165-1227`).**

- Command: `codex_command(&env.exe, &["app-server"])` (`:1427`/`:1435`, unchanged: Windows goes through
  `cmd /C` with `keitora_tasks::short_path`, `config.rs:1405`), `.current_dir(cwd)`, piped stdin/stdout/stderr,
  `apply_no_window` (`:1444`).
- Env: `.env("PATH", p)` when `env.path` is `Some(p)`; `.env("CODEX_HOME", &env.codex_home)` **only when
  `env.codex_home_source == CodexHomeSource::EnvVar`** (the user set it explicitly; otherwise Codex applies its
  own default and the pane agrees). Set explicitly on the `Command` (on Windows the `cmd.exe` process inherits
  them and the short-path program resolves under them).
- Unix only: `cmd.process_group(0)` (`std::os::unix::process::CommandExt`, stable since Rust 1.64) so the
  child leads its own process group and teardown can kill codex and any wrapper-spawned grandchild.
- Failure: `io::ErrorKind::NotFound` → `SpawnError::NotFound(msg)`, anything else → `SpawnError::Other(msg)`.
  (On Windows `cmd.exe` always spawns; a missing `codex` shows up as stdout EOF plus stderr text, handled in
  B5.)
- Pumps: the stdout pump is the existing reader loop (`:1188-1204`) with `reader` generic. The stderr pump is
  B4. Neither is joined.
- Teardown closure (child captured by move), called with `grace`:
  1. poll `child.try_wait()` every 20 ms until it exits or `grace` elapses (`grace == 0` → no polling);
  2. **Windows:** if still running, `kill_process_tree_windows(child.id())` (`:1456`, `taskkill /PID <pid> /T /F`);
  3. **Unix:** `nix::sys::signal::killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL)` (ignore `ESRCH`),
     done whether or not the direct child had already exited, so a lingering grandchild in the group dies;
     then `child.kill()` as a backstop (ignore errors). `nix` with the `signal` feature is already a unix
     dependency (`src-tauri/Cargo.toml`, `[target.'cfg(unix)'.dependencies]`);
  4. `let _ = child.wait();`.

**Teardown sequence summary.**

| Situation | Windows | Unix |
|---|---|---|
| Normal (import finished or failed), on a **detached thread after the outcome is returned** | close stdin, wait ≤ 3 s for `cmd.exe` exit, else `taskkill /T /F` | close stdin, wait ≤ 3 s, then `killpg(SIGKILL)` + `kill` |
| Cancel, deadline/timeout, protocol wedge (`abort()`), inline **before** returning | close stdin, immediate `taskkill /T /F` | close stdin, immediate `killpg(SIGKILL)` |
| Fake (tests) | closure records the `grace` value it was called with and flips an `AtomicBool` | same |

#### B4. Stderr ring (fixes R2)

```rust
#[derive(Clone)]
struct StderrRing { inner: Arc<Mutex<RingInner>>, done: Arc<AtomicBool> }
struct RingInner { lines: VecDeque<String>, bytes: usize }
```

- The pump reads with `BufRead::read_until(b'\n', ..)` (bytes, so invalid UTF-8 cannot end the loop), converts
  with `String::from_utf8_lossy`, trims the trailing `\r\n`, skips empty lines, and truncates a line to
  `STDERR_LINE_MAX_BYTES` (on a char boundary).
- `push(line)`: append; then while `lines.len() > STDERR_RING_MAX_LINES` or `bytes > STDERR_RING_MAX_BYTES`
  pop from the **front**. The newest lines always survive. On EOF or read error the pump sets `done`.
- `stderr_tail()` = lines joined by `\n` (the ring never holds more than 16 KiB). It never waits.
- `stderr_tail_settled()` = poll `done` every 10 ms for up to 200 ms, then `stderr_tail()`. It is used only
  where the process is known or suspected dead (`Closed`, spawn problems), so the last words of a crashing
  Codex are not lost to a race.
- A poisoned mutex is recovered with `into_inner` (stderr text must never panic the import).

#### B5. Failure mapping from transport errors

`fn result_from_req_err(err: ReqErr, phase: TransferPhase, version: Option<&str>, stderr_tail: String) -> ImportResult`.
Replaces `map_import_error` (`:566`), which is deleted (its two tests are ported, see "Existing tests").
`UPGRADE_MSG` (`:73`) stays and becomes the `message` of `CodexTooOld`.

| `ReqErr` | `phase` | Result |
|---|---|---|
| `Cancelled` | any | `ImportResult::Cancelled` |
| `TimedOut` | `p` | `Failed(Timeout{ phase: p, message: "timed out in <p> after <IMPORT_DEADLINE secs>s" })`; `import_over` also calls `server.abort()` (immediate kill, no 3 s grace) |
| `Rpc{ code: -32601, .. }` | `Import` | `Failed(CodexTooOld{ version, message: UPGRADE_MSG })` — **immediate** (A8); the ledger is never consulted (no import happened, so a ledger record could only be a stale earlier snapshot) |
| `Rpc{ code: -32601, .. }` | `Initialize` | `Failed(ProtocolError{ message: err.to_string() })` |
| `Rpc{ other, message }` | any | `Failed(ProtocolError{ message: "codex app-server error <code>: <message>" })` |
| `Transport(m)` | any | `Failed(ProtocolError{ message: m })` |
| `Closed` | any | if the lowercased tail contains any of `is not recognized as an internal or external command`, `command not found`, `no such file or directory`, `cannot find the file` → `Failed(CodexNotFound{ message: "codex could not be started" })`; else `Failed(ProtocolError{ message: "codex app-server exited during <p>" })` |

Every `Failed` carries `stderr_tail` (from `stderr_tail_settled()` for `Closed`, else `stderr_tail()`).
`SpawnError::NotFound(m)` → `Failed(CodexNotFound{ message: m })`; `SpawnError::Other(m)` →
`Failed(ProtocolError{ message: m })`; both with empty `stderr_tail`.

#### B6. Completion judgement (pure, table-driven)

```rust
enum CompletionVerdict {
    Thread(String),
    Failed { kind: FailureKind, message: String },   // never consults the ledger
    LedgerFallback { noop: bool },                    // look up by path, see B7
}
enum FailureKind { SessionNotDetected, ContinuedIn, Deferred, Other }

fn judge_completion(params: &Value) -> CompletionVerdict;
```

`normalize(s)`: insert `_` between a lowercase letter and a following uppercase letter (camelCase → snake),
lowercase, then replace every non-alphanumeric char with `_`. So `continued-in`, `continuedIn` and
`CONTINUED_IN` all become `continued_in`.

Marker lists (constants; each is matched as a **substring** of a normalized string):

- `SESSION_NOT_DETECTED = ["session_not_detected"]`
- `CONTINUED_IN = ["continued_in"]`
- `DEFERRED = ["deferred", "diverged", "thread_open", "thread_is_open"]`
- `NOOP = ["no_op", "noop", "unchanged", "already_imported"]`

The deferred and no-op word lists are educated guesses about upstream wording (A5); the `Other` default and
the ledger fallback make a miss safe, and the fixtures (`COMPLETION_DEFERRED`, `COMPLETION_NOOP`) document
what we assumed.

`judge_completion` steps, in order:

1. **Select the item** per A3: `params.itemTypeResults[]` entry with `itemType == "SESSIONS"`; else, if the
   array has exactly one entry, that entry; else `None`.
2. **Thread**: if the item has `successes[]`, take the **last** entry whose `target` is a non-empty string →
   `Thread(target)`.
3. **Failures**: if `failures[]` is non-empty, for each failure compute `structured =
   normalize(failureStage + " " + errorType + " " + subErrorType)` (missing → empty). Kind = first match in
   this priority over all failures: `SESSION_NOT_DETECTED` → `SessionNotDetected`; `CONTINUED_IN` →
   `ContinuedIn`; `DEFERRED` → `Deferred`. If no failure matches on structured fields, repeat the same
   priority on `normalize(message)`. Still nothing → `Other`. The verdict is `Failed { kind, message }` where
   `message` is the `message` string of the first failure that matched (or the first failure when `Other`),
   falling back to `"<failureStage>/<errorType>"`, falling back to `"import failed"`.
4. **No successes, no failures** (or no item): let `blob = normalize(serde_json::to_string(item_or_params))`.
   If it contains a `CONTINUED_IN` marker → `Failed{ContinuedIn}`; else a `DEFERRED` marker →
   `Failed{Deferred}`; else `LedgerFallback { noop: blob contains a NOOP marker }`. (No item at all →
   `LedgerFallback { noop: false }`.)

Mapping a verdict to a `TransferFailure`: `SessionNotDetected` → `SessionNotDetected`; `ContinuedIn` →
`Skipped{ContinuedIn}`; `Deferred` → `Skipped{Deferred}`; `Other` → `Other`; messages are the verdict's
`message`.

#### B7. Ledger fallback (path-only)

Used **only** for `CompletionVerdict::LedgerFallback`, or when the import response had no `importId` (A1,
Codex < 0.149), in which case the first `completed` notification is judged the same way and a
`LedgerFallback` verdict (or any non-`Thread` verdict whose kind is not a hard failure) leads here too. The
hash is gone: `sha256_hex` (`:716`), `LedgerRecord.content_sha256` and `ledger_match`'s sha argument are
removed.

```rust
fn ledger_source_paths(jsonl: &Path) -> Vec<String>;
// [raw jsonl.to_string_lossy() (the exact string we sent), fs::canonicalize() form with any \\?\ retained,
//  that form with the prefix stripped via keitora_tasks::strip_extended_prefix_str]; deduped, in that order.
//  A canonicalize error just omits the last two.

fn ledger_thread_for_paths(text: &str, source_paths: &[String]) -> Option<String>;
// Pure. Parses `Ledger` non-strictly (unchanged structs, minus content_sha256). Returns the
// `imported_thread_id` of the LAST record in `records` whose `source_path` equals ANY of source_paths and
// whose `imported_thread_id` is Some. (Last in array order, as `ledger_match` does today with `.rev()`;
// `imported_at` is not consulted.)

enum LedgerOutcome { Found(String), NotFound, Cancelled }
fn ledger_lookup(codex_home: &Path, source_paths: &[String], window: Duration, poll: Duration,
                 cancel: &AtomicBool) -> LedgerOutcome;
// Reads `<codex_home>/external_agent_session_imports.json` (a missing file is just "no records yet") at least
// once, then every `poll` until `window` has elapsed. Checks `cancel` before every read.
```

`match_ledger_thread` (`:661`) and `ledger_match` (`:681`) are deleted. `codex_home()` (`:644`) stays
(it is what `02` uses to fill `CodexEnv.codex_home`). The ledger home used here
(`ledger_home`) is `handshake.codex_home` (A6) when present, otherwise `ctx.codex_home` (= `CodexEnv.codex_home`,
resolved before spawn); this is exactly `CodexEnv::with_initialize_home(handshake.codex_home)` of `02` section D.
The `initialize` home never changes what was set on the child.

After `NotFound`: `LedgerFallback{noop: true}` → `Failed(Skipped{NoOp, message: "Codex found nothing new to import and recorded no thread"})`;
otherwise `Failed(Other{ message: "Codex completed the import without recording a thread" })` (replaces
today's misleading message at `:557`). Production calls pass `LEDGER_WINDOW`/`LEDGER_POLL`; tests pass small
values.

#### B8. `import_over` and `run_claude_import`

```rust
struct ImportCtx<'a> {
    cwd: &'a str,
    jsonl: &'a Path,
    codex_home: &'a Path,                    // CodexEnv.codex_home (resolved before spawn)
    codex_home_source: CodexHomeSource,      // for the `import` trail entry (A6 / `codex_home_from`)
    version: Option<&'a str>,                // CodexEnv.version, for CodexTooOld
    ledger_window: Duration,
    ledger_poll: Duration,
    // Both closures run on the `spawn_blocking` thread, so they are `Send + Sync`.
    progress: &'a (dyn Fn(ProgressPhase, Option<String>) + Send + Sync),
    trail: &'a (dyn Fn(TrailStage, Value) + Send + Sync), // stages `Import` and `Classify` only
}

/// Handshake, one import request, completion wait, judgement, ledger fallback. Pure protocol logic over any
/// AppServer, so the in-process fake drives it. Does not spawn and does not preflight.
fn import_over<W: Write>(server: &mut AppServer<W>, ctx: &ImportCtx) -> ImportResult;

/// Preflight, spawn, then `import_over`; tears the server down (inline when aborted, otherwise on a detached
/// thread after the result is ready, see B3) and returns the result.
fn run_claude_import(env: &CodexEnv, cwd: &str, jsonl: &Path, cancel: Arc<AtomicBool>,
                     progress: &(dyn Fn(ProgressPhase, Option<String>) + Send + Sync),
                     trail: &(dyn Fn(TrailStage, Value) + Send + Sync)) -> ImportResult;
```

`run_claude_import` (replaces `:533-562`):

1. `let deadline = Instant::now() + IMPORT_DEADLINE;`
2. `progress(Preflight, None)`; `jsonl_under_home_projects(jsonl, dirs::home_dir().as_deref())` (below); false
   → `Failed(SessionNotDetected{ message: "the Claude transcript is outside ~/.claude/projects (CLAUDE_CONFIG_DIR is set?)" })`.
3. `progress(StartingCodex, None)`; `AppServer::spawn(env, cwd, cancel, deadline)`; map `SpawnError` per B5.
4. `import_over(&mut server, &ctx)`; then, per B3, `drop(server)` inline if it was aborted, else hand it to a
   detached teardown thread; return the result.

`fn jsonl_under_home_projects(jsonl: &Path, home: Option<&Path>) -> bool`: canonicalize both
`jsonl` and `<home>/.claude/projects`; true iff the first `starts_with` the second; any canonicalize failure
or `home == None` → false. (Codex only recognises transcripts under `$HOME/.claude/projects`; the live
lookup itself keeps honouring `CLAUDE_CONFIG_DIR`.)

`import_over` steps (phases are the `TransferPhase` used in B5):

1. `progress(Initializing, None)`; `server.handshake()`; error → `result_from_req_err(e, Initialize, ..)`.
   Keep `HandshakeInfo`; `trail(TrailStage::Import, {"user_agent_present": bool, "codex_home_from":
   "initialize"|"env_var"|"default"})` (the `CodexHomeSource` of the ledger home actually used).
2. Build the payload with the existing `migration_payload(&source_path, ctx.cwd)` (`:578`, unchanged, with
   `source_path = ctx.jsonl.to_string_lossy()`).
3. `progress(Importing, None)`; `server.request("externalAgentConfig/import", payload)`; error →
   `result_from_req_err(e, Import, ..)`. `import_id = result["importId"].as_str()` (A1).
   `trail(TrailStage::Import, {"import_id_present": bool, "response_keys": [..sorted keys..]})`.
4. `progress(AwaitingCompletion, None)`; `server.wait_notification(EXTERNAL_AGENT_IMPORT_COMPLETED,
   |p| match import_id { None => true, Some(id) => p["importId"].as_str().map_or(true, |x| x == id) },
   |m, p| if m == EXTERNAL_AGENT_IMPORT_PROGRESS { progress(AwaitingCompletion, p["message"].as_str().map(String::from)) })`.
   Error → `result_from_req_err(e, Completion, ..)`.
   `trail(TrailStage::Import, {"completion_keys": [..], "successes": n, "failures": n})`.
5. `judge_completion(&params)` → `trail(TrailStage::Classify, {"verdict": "thread"|"failed:<kind>"|"ledger_fallback", "noop": bool})`:
   - `Thread(id)` → `Imported{id}`;
   - `Failed{..}` → `Failed(<mapped, with server.stderr_tail()>)`;
   - `LedgerFallback{noop}` → `progress(ReadingLedger, None)`; `ledger_lookup(home, &ledger_source_paths(ctx.jsonl),
     ctx.ledger_window, ctx.ledger_poll, &server.cancel)`; `Found(id)` → `Imported{id}`; `Cancelled` →
     `ImportResult::Cancelled` (after `progress(Cancelling, None)` and `abort()`); `NotFound` → see B7.
6. Every cancellation path (`ReqErr::Cancelled` or ledger `Cancelled`) does `progress(Cancelling, None)` and
   `server.abort()` before returning, and a `ReqErr::TimedOut` (any phase) does `server.abort()` too, so the
   inline drop that follows kills immediately. Only a normal finish or failure leaves the server un-aborted
   (graceful, detached teardown).

`import_over` never reads the live JSONL's content: JSONL growth during the import cannot affect the result.

#### B9. JSONL selection and the JSONL-less path

```rust
fn find_live_claude_jsonl(worktree_cwd: &Path) -> Result<PathBuf, String>;
fn pick_jsonl(dir: &Path) -> Option<PathBuf>;                       // pure over the filesystem
fn selection_trail(picked: &Path, persisted: Option<&str>) -> Value; // pure; for the `import` trail entry
```

**Decision: the newest non-`agent-*` `.jsonl` in the project directory wins.** `pick_jsonl` returns the newest by
mtime among `*.jsonl` regular files whose file name does **not** start with `agent-`, else `None`. The
persisted `claude_session_id` is **not** preferred (it is only written by a transfer, `host.rs:1296` only reads
it, so a later `claude --resume`, `/clear` or a new session makes it stale and preferring it could import an
older conversation than the one on screen). It is still read from `config.worktree_entries()` (`:293-297` shows
the pattern) by the Claude→Codex entry flow (`:234`) and **only logged**: the entry flow emits
`trail(TrailStage::Import, selection_trail(&jsonl, persisted_id))` =
`{"selection":"newest","persisted_session_id_present":bool,"persisted_differs":bool}` (`persisted_differs` is
true when the persisted id is present and is not the picked file's stem), so a wrong pick is diagnosable
without changing behaviour. After a successful transfer the persisted id is overwritten with the picked file's
id (`persist_plan`), which keeps it fresh for the next comparison. The containment guard at `:819-832` is kept
after the pick. `preview_claude_transfer` (`:155`) calls `find_live_claude_jsonl` and keeps returning the `Err`.
Any `Err` from `find_live_claude_jsonl` inside `transfer_worktree` becomes
`Failed(SessionNotDetected{ message: <the error string> })` (so the user still lands in fresh Codex).

### Entry flow for `transfer_worktree` (Claude→Codex)

Signature (`transfer.rs:166` today takes no `project` and returns `()`):

```rust
#[tauri::command]
pub async fn transfer_worktree(project: String, worktree: String, target_cli: CliKind,
    gate: State<'_, DispatchGate>, client: State<'_, SharedBackend>,
    registry: State<'_, TransferRegistry>, app: tauri::AppHandle) -> Result<TransferOutcome, String>
```
`preview_claude_transfer` becomes `(project, worktree) -> Result<ConfirmPreview, String>` (the real return
type is `ConfirmPreview`, `:101`). Registration in `host.rs:2215-2216` stays; the new commands are `04`'s.

1. `let guard = registry.begin(&project, &worktree)?` (`Err("a transfer is already in progress for this worktree")`).
2. `refuse_terminated` (`:203`), then `with_recovery_lock` (`:212`) as today around the direction branch
   (unchanged; the recovery hold now spans up to ~150 s on this path, as it spans the summary on the other).
3. Resolve the project by name (`04`; `project` is the resolved `[project].name`), the worktree entry
   (persisted `claude_session_id`, trail-only), `CodexEnv` (`02`), then emit
   `trail(Env, {exe, exe_source, version, codex_home_source, ..})`.
4. `jsonl = find_live_claude_jsonl(..)` then `trail(Import, selection_trail(..))`; `Err` →
   `ImportResult::Failed(SessionNotDetected)` (skip step 5).
5. `spawn_blocking(move || run_claude_import(&env, &cwd_s, &jsonl, cancel, &progress, &trail))`, where
   `progress` builds a `TransferProgress` (`elapsed_ms = guard.elapsed_ms()`, `deadline_ms`) and calls
   `app.emit("transfer://progress", ..)` (an `AppHandle` clone: `Fn + Send + Sync`), and `trail` is just
   `crate::transfer_registry::trail(&project, &worktree, stage, detail)` (it logs and pushes in one call; the
   registry computes `at_ms` since `begin`, callers never pass a timestamp). A `JoinError` →
   `Err("import task panicked: ..")` (guard drop releases the marker, nothing changed).
6. `Cancelled`, or `guard.cancel_flag()` set → `Ok(TransferOutcome::Cancelled)` (guard drop releases the marker).
7. `persist_under_gate(&gate, || commit_then_persist(&registry, &project, &worktree, CommitCancel::Honour, ||
   { .. })).await` (both helpers are `04`'s): `commit_then_persist` calls `registry.mark_committed(..)`
   **first** (the atomic cancel check, before persist); `Committed::Cancelled` → `Ok(Cancelled)` (nothing
   persisted; guard drop releases the marker). Otherwise the inner closure runs `progress(Persisting, None)`,
   `persist_worktree_cli(..)` per `persist_plan` (`02`) and the resume-store record; on a persist `Err`
   `commit_then_persist` has already called `registry.settle(..)`, so return `Err(..)` (an `Err` never leaves
   a marker). The gate is released when the closure returns.
8. Return `Imported{thread_id}` or `Fresh{ reason, reason_text: reason.user_copy() }` (marker held until
   `transfer_settled`). `worktrees://changed` is emitted for `Imported`, `Fresh` and `Done` (as `:195-198`
   does for any `Ok` today) and **not** for `Cancelled`.

## Tests

All Rust, in `transfer.rs`'s `mod tests` unless stated. Tests never touch the user's running herdr and never
read or write the real `$HOME`, `~/.codex` or `~/.claude`: every path is under a `tempfile::tempdir()`;
`dirs::home_dir()` is never called by tests (the pure cores take their paths as arguments). Fixtures use
placeholder names (`thread-a`, `/home/u/...`).

### Test infrastructure: the in-process fake app-server

Location: `src-tauri/src/transfer/fake_app_server.rs`, declared in `transfer.rs` as
`#[cfg(test)] mod fake_app_server;` (edition 2021: `transfer.rs` plus a `transfer/` directory is valid).
No new dependencies (`tempfile` is already a dev-dependency).

```rust
pub(super) struct ChannelReader(Receiver<Vec<u8>>);   // impl Read: blocks on recv; Ok(0) once the sender drops
pub(super) struct ChannelWriter(Sender<Vec<u8>>);     // impl Write: one send per write; flush is a no-op
pub(super) struct FakeServer {
    pub reader: ChannelReader,       // our stdout
    pub stderr: ChannelReader,       // our stderr
    pub writer: ChannelWriter,       // our stdin
    pub handle: FakeHandle,
}
pub(super) struct FakeHandle { /* Arc state */ }
impl FakeHandle {
    pub fn received(&self) -> Vec<Value>;                 // every JSON line the client wrote, in order
    pub fn teardown_grace(&self) -> Option<Duration>;     // Some(grace) once the teardown closure ran
    pub fn teardown(&self) -> Box<dyn FnOnce(Duration) + Send>;  // to pass to from_streams
}
pub(super) struct FakeOut { /* senders */ }
impl FakeOut {
    pub fn respond(&self, id: &Value, result: Value);
    pub fn respond_error(&self, id: &Value, code: i64, message: &str);
    pub fn notify(&self, method: &str, params: Value);
    pub fn server_request(&self, id: Value, method: &str);   // id may be a string
    pub fn stderr(&self, text: &str);
}
impl FakeServer {
    pub fn new(handler: impl FnMut(&Value, &FakeOut) + Send + 'static) -> FakeServer;   // per client line
    pub fn scripted(script: Script) -> FakeServer;
}
pub(super) enum Reply { Result(Value), Error(i64, &'static str), Silent }
pub(super) struct Script {
    pub initialize: Reply,
    pub import: Reply,
    pub before_import_response: Vec<(String, Value)>,   // notifications written before the response
    pub after_import_response: Vec<(String, Value)>,    // notifications written after the response
    pub on_import: Option<Box<dyn FnMut() + Send>>,     // side effect at import time (append to a file, etc.)
    pub stderr_at_start: Option<String>,
}
```
A pump thread inside `FakeServer` assembles `writer` bytes into lines, parses JSON, records them, and calls
the handler. When the client drops its writer (teardown closes stdin) the pump ends and drops the stdout
sender, so the client's reader sees EOF exactly like a real child exit. Tests build the `AppServer` with
`AppServer::from_streams(f.reader, f.stderr, f.writer, cancel, deadline, Some(f.handle.teardown()))` and
call `import_over` with an `ImportCtx` whose `codex_home` is a tempdir, `jsonl` a tempfile, `ledger_window`
about 500 ms and `ledger_poll` 20 ms, and whose `progress`/`trail` closures push into a `Mutex<Vec<..>>`.
A `Script::default_success(import_id, target)` helper builds the common happy script.

### Fake-server scenarios (`import_over_*`; each named test has one scenario and the stated assertions)

1. `import_over_success_with_target_returns_imported`: `initialize` result, import response
   `{importId:"imp-1"}`, completion `{importId:"imp-1", itemTypeResults:[{itemType:"SESSIONS",
   successes:[{target:"thread-a"}], failures:[]}]}`. Assert `Imported{thread_id:"thread-a"}`; client sent
   `initialize`, `initialized`, `externalAgentConfig/import` in that order (via `received()`); ledger file never
   created.
2. `import_over_matches_completion_that_arrives_before_response`: completion is in `before_import_response`.
   Assert `Imported{"thread-a"}` (exercises the `notif_buf` scan in `wait_notification`).
3. `import_over_ignores_completion_for_a_different_import_id`: first a completion with `importId:"imp-other"`
   and target `thread-other`, then the matching one with `thread-a`. Assert `Imported{"thread-a"}`.
4. `import_over_session_not_detected_failure_is_classified`: completion with
   `failures:[{failureStage:"detect", errorType:"session_not_detected", subErrorType:"", message:"m"}]`.
   Assert `Failed(SessionNotDetected{message:"m", ..})`; the ledger is not read (a ledger record for the path
   exists in the tempdir and must be ignored).
5. `import_over_continued_in_failure_is_skipped`: failure `errorType:"continued-in"`. Assert
   `Failed(Skipped{reason:ContinuedIn, ..})`.
6. `import_over_deferred_failure_does_not_consult_the_ledger`: failure `subErrorType:"thread_open"` plus a
   ledger record for the path. Assert `Failed(Skipped{reason:Deferred, ..})` (no stale-thread resume).
7. `import_over_empty_successes_fall_back_to_ledger_by_path`: completion with empty `successes`/`failures`;
   ledger record has `source_path` = the raw jsonl path and a *different* `content_sha256`. Assert
   `Imported{<ledger thread>}`; `progress` saw `ReadingLedger`.
8. `import_over_noop_marker_without_ledger_record_is_skipped_noop`: `COMPLETION_NOOP`, no ledger file. Assert
   `Failed(Skipped{reason:NoOp, ..})` after the (short) window.
9. `import_over_empty_completion_without_ledger_record_is_other`: `COMPLETION_EMPTY`, no ledger. Assert
   `Failed(Other{message contains "without recording a thread", ..})`.
10. `import_over_legacy_response_without_import_id_uses_ledger`: import response `{}`, completion `{}` (no
    `importId`, no items), ledger has the record under the **canonical** path while the sent path was the raw
    one. Assert `Imported`; also run once with the matching completion carrying an unrelated `importId` to
    confirm it is accepted when ours is `None`.
11. `import_over_late_ledger_flush_is_found`: completion empty; a helper thread writes the ledger 150 ms after
    the completion; window 1 s. Assert `Imported` and that the lookup took ≥ 100 ms.
12. `import_over_deadline_in_initialize_is_timeout_initialize`, `..._import_is_timeout_import`,
    `..._completion_is_timeout_completion`: `Reply::Silent` at the respective step, deadline 300 ms. Assert
    `Failed(Timeout{phase: Initialize|Import|Completion, ..})`, total elapsed < 1.5 s, that the call
    returned (not hung), and that after dropping the server `teardown_grace() == Some(Duration::ZERO)` (a
    timeout kills immediately).
13. `import_over_stderr_written_while_alive_is_in_the_failure_tail`: `stderr_at_start = "boom: bad auth"`,
    import answers `Rpc -32000`. Assert `Failed(ProtocolError{stderr_tail contains "boom: bad auth", ..})` while
    the fake is still open (the pre-fix code returned an empty tail here).
14. `import_over_string_id_server_request_gets_a_32601_reply`: before the import response the fake sends
    `server_request(json!("srv-1"), "item/approval")`. Assert `received()` contains
    `{"id":"srv-1","error":{"code":-32601,..}}` and the import still succeeds.
15. `import_over_appended_jsonl_mid_import_still_imports`: `on_import` appends a line to the jsonl file.
    Assert `Imported{"thread-a"}` (no hash comparison anywhere).
16. `import_over_cancel_while_waiting_returns_cancelled_and_tears_down`: completion never sent; a helper thread
    sets the cancel flag after 100 ms; deadline 10 s. Assert `Cancelled`; elapsed < 1 s; `progress` saw
    `Cancelling`; after dropping the server, `teardown_grace() == Some(Duration::ZERO)`.
17. `import_over_cancel_during_ledger_poll_returns_cancelled`: empty completion, no ledger, window 10 s, flag
    set after 100 ms. Assert `Cancelled` within ~500 ms.
18. `import_over_import_method_not_found_is_codex_too_old`: import answers `Rpc -32601` and a stale ledger
    record exists. Assert `Failed(CodexTooOld{version: ctx.version, message == UPGRADE_MSG, ..})`.
19. `import_over_other_rpc_error_is_protocol_error`: import answers `-32000 "boom"`. Assert `ProtocolError`
    whose message contains `-32000` and `boom`.
20. `import_over_stdout_eof_with_not_recognized_stderr_is_codex_not_found`: fake writes stderr
    `'codex' is not recognized as an internal or external command`, then drops everything. Assert
    `Failed(CodexNotFound{..})` with the text in `stderr_tail`; and a second case with an unrelated stderr line
    gives `ProtocolError`.
21. `import_over_initialize_codex_home_is_used_for_the_ledger`: `initialize` returns `codexHome` = tempdir B (the
    ledger lives there; `ctx.codex_home` points at an empty tempdir A). Assert `Imported` from B's record. A
    second case with a relative `codexHome` ignores it and uses A.
22. `import_over_forwards_progress_notifications`: `after_import_response` includes
    `externalAgentConfig/import/progress {message:"step 1"}`. Assert `progress` closure received phases in the
    order `Initializing, Importing, AwaitingCompletion` and an `AwaitingCompletion` call whose message is
    `"step 1"`.

### Real-spawn smoke (platform-gated; two tests)

Shared helper writes a fake `codex` into a tempdir and builds a `CodexEnv { exe, codex_home: tempdir/home,
path: <tempdir>:<process PATH>, version: None }`.

- Unix (`#[cfg(unix)]`, `real_spawn_smoke_unix`): a `#!/bin/sh` script (mode 0755) that echoes
  `env CODEX_HOME=$CODEX_HOME` and `env PATH=$PATH` to stderr, then loops `while read line`, replying with
  `printf` to `initialize` (`id` 1, result with `codexHome` = the env's home) and to the import (`id` 2,
  `{"importId":"imp-1"}` followed by a completion with target `thread-smoke`), and exits when stdin closes.
- Windows (`#[cfg(windows)]`, `real_spawn_smoke_windows`): `fake_codex.cmd` that runs
  `powershell -NoProfile -NonInteractive -File "%~dp0fake_codex.ps1"`; the `.ps1` does the same reads/writes
  with `[Console]::In.ReadLine()` and `[Console]::Error.WriteLine`. (Assumes PowerShell is installed, which
  is true on every supported Windows build; the test is skipped with a printed note if `powershell` is not
  found on `PATH`.)

Assertions (both): `AppServer::spawn` + `import_over` returns `Imported{"thread-smoke"}`;
`server.stderr_tail()` contains `CODEX_HOME=<tempdir home>` and the explicit `PATH` prefix (proves the env
was set on the child and the pump read stderr while the child was alive); dropping the server takes
< `TEARDOWN_GRACE` (the script exits on EOF).

- Unix only, `real_spawn_normal_finish_returns_before_teardown_unix`: a script that answers the protocol
  (import success) but then ignores stdin EOF (`sleep 60`) and prints its pid to stderr. Assert
  `run_claude_import` returns `Imported{..}` in < 1 s (the 3 s grace runs on the detached thread), and that
  `kill -0 <pid>` fails within ~6 s (the detached teardown killed the group after the grace).
- Unix only, `real_spawn_teardown_kills_a_hung_child_unix`: a script that prints `pid=$$` to stderr, ignores
  stdin EOF (`sleep 60` in the foreground after spawning a `sleep 60 &` grandchild whose pid it also prints).
  After `server.abort(); drop(server)`: elapsed < 2 s and `kill -0 <pid>` fails for both pids (process
  group kill reaped the grandchild). The Windows tree kill reuses the already-shipped
  `kill_process_tree_windows` and is covered by a manual check recorded in the PR (cancel mid-import on
  Windows; confirm no `codex`/`node` process remains).

### Unit tests (pure functions)

- `transfer_failure_user_copy_is_exact_for_every_variant`: one assertion per row of the `user_copy()` table
  (11 rows) with exact string equality, plus an assertion that no `user_copy()` string ends with `.`; a `match` over all variants without a wildcard in the test helper so
  adding a variant fails to compile until a row is added.
- `transfer_outcome_wire_shape` (the name `00-contracts.md` section 1 pins): `serde_json::to_value` of
  `Imported`, `Fresh{reason: Skipped{ContinuedIn,..}, reason_text}`, `Done`, `Cancelled`, and one `Timeout`;
  assert the exact `kind`/camelCase keys of `00-contracts.md` section 1 (this is where the serde-attribute
  check the contract asks for is made executable).
- `upstream_shape_fixtures_pin_field_names`: see "How tests pin them".
- `judge_completion_table`: one row per case, asserting the verdict:
  `COMPLETION_SUCCESS` → `Thread("thread-a")`; two successes (last non-empty wins); success with a non-string
  `target` and no failures → `LedgerFallback{false}`; `session_not_detected` in `errorType` →
  `Failed{SessionNotDetected}`; `continuedIn` in `subErrorType` → `Failed{ContinuedIn}`; `thread_open` →
  `Failed{Deferred}`; marker only in `message` ("session was continued-in elsewhere") → `ContinuedIn`; mixed
  failures (`Other` first, `session_not_detected` second) → `SessionNotDetected` (priority); unknown failure
  with `message:"m"` → `Failed{Other, "m"}`; unknown failure without message → `"<stage>/<type>"`;
  `COMPLETION_NOOP` → `LedgerFallback{true}`; `COMPLETION_EMPTY` → `LedgerFallback{false}`; no
  `itemTypeResults` → `LedgerFallback{false}`; two items with only the `SESSIONS` one carrying a target →
  `Thread`; marker words appearing only in a *different* item are ignored.
- `normalize_handles_kebab_camel_and_screaming_case`.
- `result_from_req_err_maps_every_row`: one assertion per row of the B5 table including `Closed` with and
  without the not-found stderr substrings. Replaces `rpc_minus_32601_maps_to_upgrade_message` and
  `other_rpc_error_is_not_the_upgrade_message`.
- `classify_*` (see "Existing tests"): `classify_string_id_server_request`,
  `classify_response_with_string_id_is_junk`, `classify_null_id_is_a_notification`.
- `stderr_ring_keeps_newest_lines` (push 250 lines → 200 remain, first is line 51),
  `stderr_ring_caps_total_bytes` (push 40 lines of 1 KiB → total ≤ 16 KiB, newest kept),
  `stderr_ring_truncates_a_long_line` (5 KiB line → ≤ 2 KiB, on a char boundary), and
  `stderr_pump_survives_invalid_utf8` (feed `b"ok\n\xff\xfe\nnext\n"` through a `Cursor`; tail has `ok` and
  `next`).
- `ledger_thread_for_paths_*`: `..._takes_last_and_tolerates_extra_fields` (the old sample, expects
  `...0003`), `..._matches_any_candidate_path` (raw vs `\\?\` canonical vs stripped), `..._ignores_records_without_a_thread_id`,
  `..._ignores_other_paths`, `..._tolerates_missing_records_key`, `..._needs_no_sha`.
- `ledger_source_paths_orders_raw_then_canonical_and_dedupes` (tempdir file; on Windows also asserts the
  stripped form lacks `\\?\` while the canonical form keeps it).
- `ledger_lookup_returns_not_found_after_the_window` and `ledger_lookup_stops_on_cancel`.
- `pick_jsonl_picks_the_newest_by_mtime` (set mtimes with `File::set_modified`),
  `pick_jsonl_skips_agent_prefixed_files` (an `agent-*.jsonl` newer than the real one loses),
  `pick_jsonl_returns_none_when_only_agent_files_exist`, `pick_jsonl_ignores_non_jsonl_and_directories`, and
  `selection_trail_records_persisted_vs_picked` (`persisted_differs` false when the persisted id equals the
  picked stem, true when it differs, `persisted_session_id_present:false` for `None`; the persisted id never
  influences `pick_jsonl`).
- `jsonl_under_home_projects_accepts_a_file_under_home_claude_projects`,
  `..._rejects_a_file_under_another_config_dir`, `..._rejects_when_home_is_none_or_missing`.
- `persist_plan_maps_each_import_result`: `Imported` → `Set(id)`; `Failed(any)` → `Clear` and `cli == Codex`;
  `Cancelled` → `None`; `claude_session_id: Some` → `Set`, `None` → `Keep`.
- `migration_payload_serializes_verbatim` (`:1653`) is unchanged and stays as the pin of the request body.

### Existing tests touched (`mod tests` at `:1525`)

| Existing test | Action |
|---|---|
| `ledger_match_takes_last_and_tolerates_extra_fields` (`:1626`) | rewrite as `ledger_thread_for_paths_takes_last_and_tolerates_extra_fields`; keep `LEDGER_SAMPLE`/`SAMPLE_SOURCE`, delete `SAMPLE_SHA` |
| `ledger_match_requires_all_three_of_path_sha_thread` (`:1633`) | delete; replaced by `..._ignores_other_paths` and `..._needs_no_sha` |
| `ledger_match_tolerates_missing_records_key` (`:1644`) | rename to `ledger_thread_for_paths_tolerates_missing_records_key` |
| `rpc_minus_32601_maps_to_upgrade_message` (`:1796`), `other_rpc_error_is_not_the_upgrade_message` (`:1869`) | replaced by `result_from_req_err_maps_every_row` (same assertions on the `-32601`/`-32000`/`Transport` inputs, new return type) |
| `classify_notification_and_server_request` (`:1915`) | update: `ServerRequest.id` is now `json!(3)` |
| `ledger_realpath_retains_prefix_while_cwd_encoding_strips_it` (`:1770`, windows) | unchanged (still true; `ledger_source_paths` keeps the prefixed form) |
| `claude_projects_dir_*`, `encode_project_dir_*`, `claude_session_file_helpers`, `migration_payload_serializes_verbatim`, `a_transfer_holds_its_session_out_of_recovery...`, `a_terminated_session_is_refused...` | unchanged |

Dead code to delete after the change (compile with `-D warnings` expectations): `map_import_error`,
`match_ledger_thread`, `ledger_match`, `sha256_hex`, `IMPORT_COMPLETED_TIMEOUT`, `REQUEST_TIMEOUT`, the
`content_sha256` field. `sha2` and `hex` in `src-tauri/Cargo.toml` are removed **only if** a repo-wide search
shows no other user (at planning time every `sha2::`/`hex::` use in `src-tauri/src` is in `transfer.rs`).

## Implementation order within this doc

Each step compiles and `cargo test --workspace` stays green before the next (iterate with the
`transfer::` test filter; never run gated herdr tests).

1. **Types.** `TransferOutcome`, `TransferFailure`, `SkipReason`, `TransferPhase`, `ProgressPhase`,
   `TransferProgress`, `user_copy()`, the `FieldIntent`/`CodexEnv` stubs if `02` is not in. Tests: user_copy,
   wire shapes.
2. **Framing.** `Frame::ServerRequest.id: Value`, new `classify`, `reject_server_request(&Value)`. Update and
   add `classify_*` tests.
3. **Stderr ring.** `StderrRing` and its pump plus tests (not yet wired).
4. **Generic `AppServer`.** `from_streams`, `next_frame` (cancel and deadline), new `request` /
   `wait_notification` / `handshake`, `abort`, `Drop` with the teardown closure, `ReqErr` variants; delete
   `REQUEST_TIMEOUT`/`IMPORT_COMPLETED_TIMEOUT`. Add `fake_app_server.rs`. Temporarily adapt the old
   `run_claude_import` to the new calls so it compiles. Tests: scenarios 12, 14, 16 on a thin harness.
5. **Spawn + teardown.** `AppServer::spawn(env, cwd, cancel, deadline)` with explicit env (CODEX_HOME only for
   `EnvVar`), `process_group`, per-platform teardown closure, and the detached-teardown hand-off in
   `run_claude_import`. Tests: the two real-spawn smoke tests, the hung-child test and the detached-teardown
   test.
6. **Ledger.** `ledger_source_paths`, `ledger_thread_for_paths`, `ledger_lookup`; delete the hash path;
   port the old ledger tests.
7. **JSONL.** `pick_jsonl`, `find_live_claude_jsonl(worktree_cwd)`, `selection_trail`,
   `jsonl_under_home_projects`; the callers are `preview_claude_transfer` and the entry flow. Tests for each.
8. **Judgement and import.** `judge_completion`, `result_from_req_err`, `import_over`, new
   `run_claude_import`. Tests: `judge_completion_table`, `result_from_req_err_maps_every_row`, all scenarios
   1-22, the shape-pin test.
9. **Entry flow.** New `transfer_worktree`/`preview_claude_transfer` signatures, registry begin/commit (before
   persist, `persist_under_gate`)/settle,
   `persist_plan`, outcome construction, progress and trail closures, `worktrees://changed` rule. Test:
   `persist_plan_maps_each_import_result`. Delete the dead code listed above.
10. **Review-gate manual checks** (recorded in the PR, not CI): one real transfer on Codex ≥ 0.149 comparing
    trail keys with the fixtures; Windows cancel mid-import leaves no stray `codex`/`node` process; macOS
    transfer with Codex outside the default `PATH`.

## Risks and mitigations

- **Codex protocol churn.** Parse everything as `serde_json::Value` with optional fields; detect
  capabilities by behaviour (a `-32601` reply or a missing `importId`); fixtures pin the shapes and the trail
  records key names so drift is visible remotely.
- **Upstream claims unverifiable here.** See the assumptions table; the ledger fallback keeps older versions
  and shape surprises working, and every decision is logged to the trail.
- **Windows.** `cmd /C` + `short_path` for the env-carrying child, `taskkill /T` on cancel and teardown, a
  `.cmd` + PowerShell fake for the smoke test.
- **Unix grandchildren.** The npm `codex` shim may spawn the native binary as a child; `process_group(0)` +
  `killpg` removes the orphan risk that a bare `child.kill()` had.
- **Blocking input during the import** could annoy a mid-thought user. Mitigation: Cancel is always
  available and the deadline is bounded (both in `03-frontend.md`).
- **Persisted `claude_session_id` can be stale** (it is only written by a transfer, `host.rs:1296` only reads
  it): preferring its file could import an older conversation than the one on screen. Mitigation (decided):
  the newest non-`agent-*` file always wins and the persisted id is only compared and logged
  (`persisted_differs` in the `import` trail entry).
- **Detached teardown outliving the command.** The graceful teardown thread is not joined; it only holds the
  child and its pipes. Mitigation: the kill path is the same closure as before, and a cancel or timeout never
  uses the detached path.

## Decisions this document depends on

- Complexity is large; Fracture splits along import / launch+env / frontend / diagnostics.
- A failed import always switches to Codex; the Fresh fallback starts Codex blank.
- The reporter runs Codex 0.166.0 with no env overrides: the `importId` path is available; the
  `continued-in` skip must classify as `Skipped`; `CODEX_HOME` drift is not the likely cause; PATH for the
  import child is (R7, `02`).
- #29 is treated as a likely symptom of R3 (`02`); recount after landing before closing it.
- The fake app-server is in-process (`transfer/fake_app_server.rs`), through a writer-generic `AppServer<W>`
  with stream-taking constructor `from_streams` and a separate child-spawn constructor `spawn`.
- The frontend phase machine allows cancel only before persist (this document enforces it by
  `mark_committed` returning `false` after a cancel request).
- `DispatchGate` is held only for the commit and persist step; the import is lock-free under the registry
  marker (`04`).
- An explicit `project` is threaded through the transfer IPC commands (`04`).
- `-32601` on the import method is `CodexTooOld` immediately, without consulting the ledger; a deferred or
  `continued-in` completion never falls back to the ledger; only a no-signal or no-op completion does.
- JSONL selection: the newest non-`agent-*` `.jsonl` wins; the persisted `claude_session_id` is only logged to
  the trail (it goes stale).
- AppServer teardown (close stdin, wait up to 3 s, then kill the tree) runs on a detached thread **after** the
  outcome is returned; cancel and timeout kill immediately and inline.
- `mark_committed` returns `bool` and runs before persist, atomically with the cancel check; a persist failure
  is followed by `settle`.

## Contract changes requested by this document (all applied in `00-contracts.md`; see its Changelog)

1. `TransferRegistry::mark_committed` returns `bool` and is called **before** persist, atomically with the
   cancel check (applied: section 2 "Commit order", section 4).
2. After a persist failure the backend calls `settle` so an `Err` leaves no marker (applied: section 2).
3. The preview return type is `ConfirmPreview` and `target_cli` is `CliKind` (applied; `02`, `03` match).
4. `TransferGuard` is `Send` (applied: section 4).
5. Section 1 types derive `Debug, Clone, PartialEq, Eq, Serialize`; `rename_all_fields` needs serde >= 1.0.185
   and the repo has 1.0.228 (applied: section 1).
6. `CodexEnv.codex_home` is resolved before spawn (env var, else `~/.codex`); the `initialize` `codexHome`
   overrides it for the ledger lookup only (`with_initialize_home`); `CODEX_HOME` is set on the child and the
   pane only when the source is `EnvVar` (applied in `02` section D).
7. `CodexTooOld` is immediate on `-32601` for the import method and never consults the ledger (applied in `02`).
8. The progress emitter is `Fn + Send + Sync` (applied: section 3).

## Remaining open questions

1. **Stale persisted `claude_session_id`** (resolved by decision: the newest non-`agent-*` file wins; the
   persisted id is only logged). The proposal text that says "prefer the persisted id's file" is superseded by
   this decision. If the manual check later shows the newest file is the wrong pick, revisit.
2. **Deferred/no-op marker words** (`B6`) are guesses until a real Codex payload is captured; the fixtures
   must be refreshed from the first real capture.
3. **A9** (Codex exits on stdin EOF) is assumed from the current teardown design (`:1386`); if false, every
   transfer pays the full 3 s grace.
4. **Windows smoke dependency on PowerShell**; if CI images lack it the Windows smoke test self-skips and the
   manual check carries the weight.
5. (Resolved: the graceful teardown runs on a detached thread after the outcome is returned; cancel and
   timeout kill immediately. See B3.)
