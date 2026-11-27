# 0078-03 hook broker (`keitora session gate`, Stop capture, hook installers)

Part of `plans/0078-cross-worktree-driving/`. See `00-overview.md` for goal and glossary.

**Dependencies:**
- 01 task store (referred to here by role; exact method names are 01's): drive lookup by driven
  worktree (`driver`, `paused`, `escalate_after_s`), insert prompt, read prompt status, compare-and-set
  a prompt from `pending` to a terminal status, record a reply (store `last_message` and bump `turn`
  atomically), session-state write (with the withdrawal rule), `PRAGMA data_version`.
- 02 CLI: `gate` is a reserved first argument of `keitora session`; the "dispatch `Cmd::Session` before
  the tracker backend is opened" change lives there. `answer`/`release`/pause in 02 are what unblock the
  gate. `--escalate-after` must be validated to 1..=540 by 02 (see Singletonize decisions).
- External: Claude and Codex hook installers (`src-tauri/src/hooks.rs`, `src-tauri/src/codex_hooks.rs`).
  Codex hooks need trust (`--dangerously-bypass-hook-trust`, `src-tauri/src/backend/herdr/agent.rs`);
  changed hook content re-triggers the Codex trust review for sessions not launched by Keitora.
- Gated by spikes (a) and (b) in 00. See "Spikes (manual verification owed)".

## Why at the hook

Claude Code and Codex both run a `PermissionRequest` hook that can return allow or deny and can block
for minutes (600s default timeout). If the hook returns nothing, the normal interactive prompt appears;
that "no decision" path is how escalation works. Today `keitora session` ignores stdin and takes only a
`state` (`crates/keitora-cli/src/main.rs`: `Cmd::Session { state }` at line 35, handled at lines
268-275 as `session_upsert(&me, &state, current, None)`, with `me = cli_actor()` =
`keitora_tasks::current_worktree()`, `crates/keitora-tasks/src/config.rs:2944`).

## Code layout (so the logic is pure and testable)

New modules in `crates/keitora-cli/src/`, declared `mod gate; mod stop_capture;` in `main.rs`, tests in
the same files under `#[cfg(test)]`. `keitora-cli` already depends on `serde_json`; add nothing else.

- `gate.rs` holds everything below except real I/O, which is injected:
  - `trait GateStore` (implemented for `keitora_tasks::LocalStore` in `gate.rs`; a fake in tests) with
    exactly these methods, each a thin call into 01:
    `drive_for(worktree) -> Result<Option<DriveInfo>>` where
    `DriveInfo { driver: String, paused: bool, escalate_after_s: u32 }`;
    `session_state(worktree) -> Result<Option<String>>`;
    `insert_prompt(NewPrompt) -> Result<i64>`;
    `prompt(id) -> Result<PromptRow>` (`status`, `decision_json`, `reason`, `decided_by`);
    `escalate_if_pending(id, decided_by, reason) -> Result<()>` (compare-and-set; no-op if already
    terminal);
    `set_session_state(worktree, state) -> Result<()>`;
    `data_version() -> Result<i64>`.
  - `trait Clock { fn now(&self) -> Instant; fn sleep(&self, d: Duration); }` (real = `Instant::now`
    and `thread::sleep`; the fake advances a virtual counter).
- Pure functions, all unit-tested: `parse_payload`, `route`, `summarize`, `truncate_input`,
  `outcome_from_row`, `render`, `read_bounded` (generic over `Read`).
- `fn run_gate<S: GateStore, C: Clock>(store: &S, clock: &C, provider: Provider, payload: &GatePayload)
  -> Option<String>` returns the exact stdout text (`None` = print nothing). It never returns an error:
  every `Err` inside becomes escalate (see Fail-safe).
- `pub fn gate_main(provider: Provider) -> !` is the only function that touches stdin, stdout and the
  process exit. It prints the string from `run_gate` (if any) with `print!` plus a flush, then calls
  `std::process::exit(0)` on every path. It ignores the global `--json` flag and never uses `emit_ok`.

## CLI surface this doc needs from `main.rs` (coordinate with 02)

```
Session {
    /// idle | busy | waiting | offline | gate
    state: String,
    #[arg(long)] provider: Option<String>,   // gate only: claude | codex
    #[arg(long)] stdin: bool,                // idle only (Stop): capture the reply from the payload
    #[arg(long)] failed: bool,               // idle only (StopFailure): "turn ended with an error"
}
```

- `run()` routes `state == "gate"` first, right after `Cli::parse()` and before
  `resolve()`/`open_backend`/`LocalStore::open` (their `?` would turn a missing `keitora.toml` into
  exit 1 and an error line). `gate_main` does its own `resolve()` + `LocalStore::open(&config.db_path)`
  and treats any failure as "no decision".
- `session idle --stdin|--failed` keeps today's path (`resolve`, `open_backend`, `LocalStore::open`) and
  adds the reply capture below before the existing `session_upsert`.
- `gate` is not in `SESSION_STATES`; the old CLI therefore reads it as an invalid state (see "Older CLI").
- The existing test at `main.rs:1225` (`Cmd::Session { state } => ...`) becomes
  `Cmd::Session { state, .. }`.

## Hook entries and exact command strings

`BIN` is the resolved binary (`resolve_keitora_bin`, extended-prefix stripped; on Windows Claude uses
forward slashes and Codex keeps native backslashes, exactly as the current `hook_command`s do).
`GATE_HOOK_TIMEOUT_S = 600` (seconds) is a fixed const, so the installed file is identical across drives.

### Claude (`.claude/settings.local.json`; POSIX shell on every platform: `/bin/sh`, Git Bash on Windows)

| Event | `matcher` | `command` | `timeout` |
|---|---|---|---|
| `PermissionRequest` | `""` (all tools) | `[ -x "BIN" ] && "BIN" session gate --provider claude 2>/dev/null \|\| true # keitora:session-hook` | 600 |
| `PreToolUse` | `AskUserQuestion` | same command string as above | 600 |
| `Stop` | `""` | `[ -x "BIN" ] && { "BIN" session idle --stdin 2>/dev/null \|\| "BIN" session idle >/dev/null 2>&1; } \|\| true # keitora:session-hook` | default |
| `StopFailure` | `""` | `[ -x "BIN" ] && { "BIN" session idle --failed 2>/dev/null \|\| "BIN" session idle >/dev/null 2>&1; } \|\| true # keitora:session-hook` | default |

(`\|` above is table escaping for `|`.) The other four `HOOK_MAP` entries (`UserPromptSubmit`,
`Notification`/`permission_prompt`, `SessionStart`, `SessionEnd`) are unchanged. Gate hook object shape:
`{"type":"command","command":"…","timeout":600}` inside `{"matcher":"…","hooks":[ … ]}`.

Output preservation: the gate rows redirect only fd 2 (`2>/dev/null`), so stdout reaches Claude. The
shell passes its stdin (the hook payload pipe) to the child unchanged. `|| true` forces exit 0 whatever
the child did. The `Stop` rows leave stdout unredirected, harmless because the `--stdin`/`--failed` path
prints nothing (it must not call `emit_ok`); they fall back to the old plain `session idle` (all streams
silenced) so an older installed CLI still records idle and a session can never stick at `busy`.

### Codex (`.codex/hooks.json`; no `matcher`: `{"hooks":[{"type":"command","command":…,"timeout":600}]}`)

`timeout` is added to the `PermissionRequest` hook object only. Other Codex events (`SessionStart`,
`UserPromptSubmit`, `SessionEnd`) are unchanged.

| Event | Windows (PowerShell) | Unix (`/bin/sh`) |
|---|---|---|
| `PermissionRequest` (replaces `session waiting`) | `if (Test-Path -LiteralPath "BIN") { & "BIN" session gate --provider codex 2>$null; if ($LASTEXITCODE -ne 0) { & "BIN" session waiting *> $null } }; exit 0 # keitora:codex-session-hook` | `[ -x "BIN" ] && { "BIN" session gate --provider codex 2>/dev/null \|\| "BIN" session waiting >/dev/null 2>&1; } \|\| true # keitora:codex-session-hook` |
| `Stop` | `if (Test-Path -LiteralPath "BIN") { & "BIN" session idle --stdin 2>$null; if ($LASTEXITCODE -ne 0) { & "BIN" session idle *> $null } }; exit 0 # keitora:codex-session-hook` | `[ -x "BIN" ] && { "BIN" session idle --stdin 2>/dev/null \|\| "BIN" session idle >/dev/null 2>&1; } \|\| true # keitora:codex-session-hook` |

- Only stderr is discarded (`2>$null` / `2>/dev/null`), so stdout reaches Codex. The current Codex
  command's `*> $null` discards every stream and must not be used on the gate or `--stdin` invocations.
- PowerShell: `& "BIN" …` is a native call, which should inherit the hook's stdin; that is spike (b)'s
  first question. If it does not, switch the gate and `Stop` rows to the pipeline form
  `$input | & "BIN" session gate --provider codex 2>$null` (likewise for `idle --stdin`); `$input`
  re-encodes the stream as text, fine for JSON (the gate's own output is ASCII only).
- `; exit 0` is required on Windows: a PowerShell command run exits with the last native exit code, and
  an old CLI's clap error is exit 2, which Codex could read as a block. The new gate always exits 0.
- The `session waiting` fallback keeps the rail's `waiting` state working when the installed CLI predates
  `gate` (the old CLI rejects `gate` and writes nothing).

## Gate: stdin parsing

Reading (`read_bounded`, pure over `impl Read`; real use in `gate_main`):
- If stdin is a terminal (`std::io::IsTerminal`), skip the read and treat the payload as empty, so a hook
  run by hand cannot hang.
- Otherwise read on a helper thread into a buffer capped at **1 MiB** (`Read::take`) and wait with
  `mpsc::Receiver::recv_timeout(Duration::from_secs(2))`. On timeout or read error, use what was read so
  far if it parses, else treat as empty. `gate_main` ends in `process::exit(0)`, which abandons a thread
  still blocked in `read`.
- The `Stop` capture uses a 64 KB cap and a 1s timeout (below).

Parsing, `parse_payload(raw: &[u8]) -> Result<GatePayload, GateParseError>`: a UTF-8 JSON object, else
`Err`. Fields, same names for both providers:

| Field | Type | Used for |
|---|---|---|
| `hook_event_name` | string | must be `PermissionRequest` or `PreToolUse`; selects the route |
| `tool_name` | string | route, `drive_prompts.tool_name`, summary; absent or empty gives `Err` |
| `tool_input` | object | stored (truncated) and summarised; absent means `{}`. For `AskUserQuestion` it must carry `questions`: an array of objects each with a string `question` and an `options` array of objects with a string `label`, else `Err` |
| `session_id`, `transcript_path`, `cwd`, `permission_mode`, `tool_use_id`, `turn_id`, `model`, `permission_suggestions` | any | parsed if present, never required, not stored in v1 |

Unknown fields are ignored. Identity is **not** read from the payload: the worktree is
`current_worktree()` (git toplevel basename of the process cwd), the same identity the existing `session`
hooks use; the project is `keitora_tasks::resolve()` (registry match, cwd walk to `keitora.toml`,
`$KEITORA_PROJECT`), landing on the project's single `tasks.db`.

Routing, `route(provider, &payload) -> Route` (`Ignore | Permission | Question`):

| provider | `hook_event_name` | `tool_name` | Route |
|---|---|---|---|
| claude | `PermissionRequest` | `AskUserQuestion` | `Ignore` (brokered once, by `PreToolUse`) |
| claude | `PermissionRequest` | anything else | `Permission` |
| claude | `PreToolUse` | `AskUserQuestion` | `Question` |
| claude | `PreToolUse` | anything else | `Ignore` |
| codex | `PermissionRequest` | any | `Permission` |
| any other combination | | | `Ignore` |

`Ignore` prints nothing. For Codex, `Ignore` and not-driven also write `waiting`
(`set_session_state(me, "waiting")`), preserving what the replaced `session waiting` entry did.

## Gate: algorithm (`run_gate`)

1. `route`. `Ignore`: (Codex only: write `waiting`), return `None`.
2. `store.drive_for(me)`. `None` (not driven): (Codex only: write `waiting`), return `None`. This is the
   fast path, one SQLite read.
3. Escalate immediately (return `None`; Codex also writes `waiting`; insert no row) if `drive.paused`, or
   the driver's session is offline (`session_state(driver)` is `None` or `Some("offline")`).
4. Insert the prompt: `driven = me`, `driver = drive.driver`, `provider`, `kind` (`permission` for
   `Route::Permission`, `question` for `Route::Question`), `tool_name`,
   `tool_input = truncate_input(..)`, `summary = summarize(..)`, `hook_pid = process::id()`. Then
   `set_session_state(me, "waiting")`.
5. Poll (below) until the row is terminal or the deadline passes.
6. Map the row with `outcome_from_row`, then `render`. When the outcome is a driver decision (`Allow`,
   `Deny`, `Answer`), first call `set_session_state(me, "busy")` (best effort; a failure is ignored and the
   decision is still printed). For an escalation leave the state `waiting` (the native dialog is showing).

### Polling loop

- Deadline: `start + min(drive.escalate_after_s, 540)` seconds, with `escalate_after_s` read from the
  `drives` row at step 2 (01's column; 02/01 default 300). The 540 cap keeps the gate inside the 600s
  hook timeout.
- Tick every **500 ms** (`clock.sleep`), one connection for the whole run (`LocalStore::open` already
  sets WAL and `busy_timeout = 3000`, `crates/keitora-tasks/src/sqlite_backend.rs:56-58`).
- Each tick: `v = store.data_version()` (`PRAGMA data_version`; it changes only when *another*
  connection commits, which is the case because the answerer is a separate process). Run the real
  `SELECT` (`store.prompt(id)`) when `v` differs from the last seen value, and unconditionally every 20
  ticks (10s) as a safety net. The initial `v` is read right after the insert, and one `SELECT` runs
  immediately after the insert (the answer may predate the first tick). No read transaction is held
  between ticks.
- On each real `SELECT`, also check whether to keep waiting: `drive_for(me)` is `None`, has a different
  `driver`, or is `paused` (drive ended or stolen), or the driver's `session_state` is offline. Then
  `escalate_if_pending(id, "system", reason)` and stop.
- Deadline reached: `escalate_if_pending(id, "timeout", "no driver decision in {n}s")`, then **re-read
  the row once**. If the driver decided in that instant the compare-and-set lost, the row is terminal with
  the driver's decision, and that decision is honoured. Otherwise the row is `escalated`.
- Terminal statuses (`allowed|denied|answered|escalated|expired|withdrawn`) end the loop. `withdrawn` and
  `expired` mean the hook's world changed (Esc, agent exited, 01's stale rule): print nothing.
- Up to 3 consecutive `Err` from `data_version`/`prompt` are tolerated (retried next tick); the 4th ends the
  loop as escalate (best-effort `escalate_if_pending`, whose failure is ignored).
- A hook killed by the provider (timeout, Esc, session exit) dies without cleanup; its `pending` row is
  later `withdrawn` by the withdrawal rule (01) or read as `expired` by 01's stale rule (older than its `timeout_s` + 30 s, always inside the 600s hook timeout).

### Decision handling, `outcome_from_row(kind, row, payload) -> Outcome`

`Outcome = Allow | Deny{message} | Answer{answers} | Escalate`.

| Row `status` | `kind = permission` | `kind = question` |
|---|---|---|
| `allowed` | `Allow` | `Escalate` (no answers; unusable) |
| `denied` | `Deny{message}` | `Deny{message}` |
| `answered` | `Escalate` | `Answer{answers}` if `decision_json` is `{"answers":{<question text>:<label>,…}}` and the keys are exactly the payload's `question` strings (every question answered, no extras, values non-empty); else `Escalate` |
| `escalated`, `expired`, `withdrawn`, `pending` (deadline), any unknown string | `Escalate` | `Escalate` |

`message` is `Denied by driver {driver}: {reason}` (`Denied by driver {driver}` when the reason is empty),
truncated to 500 bytes on a char boundary. For multi-select questions the driver supplies the labels
joined with `, ` in one value; a value equal to no option label is passed through as the user's free-text
"Other" answer. Partial answers make the whole outcome `Escalate` (never partially applied).

## Exact output per decision, `render(provider, route, &Outcome, &payload) -> Option<String>`

One line of JSON on stdout, **ASCII only** (every non-ASCII char written as `\uXXXX` so PowerShell and Git
Bash codepages cannot corrupt it; `serde_json` does not do this by default, so the renderer post-escapes),
exit code always 0. `""` means nothing is printed.

| Outcome | Claude `PermissionRequest` | Claude `PreToolUse` (`AskUserQuestion`) | Codex `PermissionRequest` |
|---|---|---|---|
| `Allow` | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` | not produced (escalate) | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}` |
| `Deny{m}` | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"<m>"}}}` | `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"<m>"}}` | `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"<m>"}}}` |
| `Answer{a}` | not produced (escalate) | `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"Answered by driver","updatedInput":{"questions":<original tool_input.questions>,"answers":{"<question text>":"<selected option label>",…}}}}` | not produced (escalate) |
| `Escalate` | `""` (native dialog) | `""` (native question UI; see decision 2) | `""` (native approval) |

`updatedInput` replaces the whole tool input, so it carries the original `questions` array verbatim (the
renderer copies it from the parsed payload held in memory, never from the DB) plus `answers`: an object
mapping each question's full `question` text to the selected option's `label` string. Codex's output is
the same `hookSpecificOutput` shape per its hooks docs; spike (b) confirms.

## Fail-safe: every error path ends in escalate (no decision), never allow

`Allow` is produced in exactly one place: `outcome_from_row` seeing `status == "allowed"` and
`kind == permission` in the row the **driver's CLI** wrote. The gate never writes `allowed`, `denied` or
`answered`. Every path below ends with no output (the dialog appears) and, where a row exists, a
best-effort `escalated` write.

| Error path | Result |
|---|---|
| binary missing or not executable (`[ -x ]` / `Test-Path` false) | command does nothing, exit 0, no output |
| CLI older than `gate` (clap rejects `--provider`, or reads `gate` as an invalid state) | non-zero exit, no stdout; `\|\| true` / `; exit 0` makes it exit 0 (Codex also runs the old `session waiting`) |
| `resolve()` or `LocalStore::open` fails (for example no `keitora.toml`) | `gate_main` swallows it, no output, exit 0 |
| stdin is a terminal, empty, read error, 2s timeout, non-JSON, not an object, missing `hook_event_name` or `tool_name`, malformed `questions` | no output |
| unknown provider string, unknown event, unrouted combination | no output |
| `drive_for` fails (DB locked beyond `busy_timeout`, schema missing) | no output (driven state unknown: normal behaviour) |
| `insert_prompt` or the `waiting` write fails | no output; any row is left for the withdrawal rule |
| 4th consecutive poll error | escalate, no output |
| `decision_json` malformed, unknown `status`, decision kind does not match prompt kind, answer keys do not match, missing answers | `Escalate` |
| deadline passed; driver offline; drive released, stolen or paused; human Take over; row `withdrawn`/`expired` | no output |
| `render` cannot serialise | no output |
| panic anywhere | `run_gate` runs under `std::panic::catch_unwind(AssertUnwindSafe(..))`; a caught panic prints nothing; an uncaught one exits non-zero, neutralised by `\|\| true` / `; exit 0` |
| stdout write error | ignored, exit 0 |
| provider hook timeout (600s) or Esc | treated by the provider as a non-blocking error: the dialog shows |

A unit test asserts the single-source rule: for every non-`allowed` status and every error input,
`run_gate` never returns text containing `"allow"`.

## Older CLI (no `gate` subcommand)

Hook files are rewritten when the Keitora app starts, but the hook binary is the PATH `keitora`
(`resolve_keitora_bin`), which can be older than the app. An older binary given
`session gate --provider claude` exits 2 from clap (unknown `--provider`) with nothing on stdout; even
without the flag it would read `gate` as an invalid state (`session_upsert` error, exit 1, no write). The
shell wrapper turns either into exit 0 with empty stdout (`|| true` for Claude, `; exit 0` for Codex),
which both providers read as "no decision". `Stop`/`StopFailure` fall back to plain `session idle`.
`keitora drive` (02) checks that the driven worktree's hook file has the gate entry (a command containing
`session gate`) and warns; driver guidance says to upgrade the CLI. That warning cannot detect an old
binary, only an old hook file.

## Withdrawal and state writes

- Any session-state write for a worktree other than `waiting` (`busy`, `idle`, `offline`) marks its
  still-pending prompts `withdrawn`. The rule is owned by 01 and applied in 01's session-state setter, not
  here. `waiting` does not withdraw, so the gate's own `waiting` write and Claude's `Notification` hook do
  not clear a pending prompt.
- The gate writes `busy` only after its own row is terminal, so the withdrawal it triggers can touch only
  other, stale pending prompts.
- A pending prompt older than its `timeout_s` + 30 s (01's stale rule; always under the 600s hook timeout, because `timeout_s` <= 540) is treated as `expired` by any reader (01).

## Stop capture (`session idle --stdin` / `--failed`)

Used by `Stop` (`--stdin`) and `StopFailure` (`--failed`) only; `SessionStart` still runs plain
`session idle` and does not bump `turn`.

Sequence, in `run()`'s existing Session branch before `session_upsert`:
1. `--failed`: `last_message = "turn ended with an error"`; no stdin read.
2. `--stdin`: `read_bounded` (**64 KB cap, 1s timeout**, same terminal check as the gate), parse JSON,
   take `last_assistant_message` if it is a non-empty string.
3. Else fallback: if the payload has a `transcript_path` string ending in `.jsonl` that is a regular file,
   call `stop_capture::last_assistant_text(path)`. The provider is detected from the line shapes (below),
   so `idle --stdin` needs no `--provider`.
4. Else `last_message = ""`.
5. Truncate to 32 KB (32768 bytes including a trailing `\n[truncated]` marker, cut on a char boundary),
   call 01's record-reply API (stores `last_message`, `turn = turn + 1`, `updated_at` in one transaction),
   then the existing `session_upsert(me, "idle", …)`. Reply first, state second, so anything that sees
   `idle` already sees the new turn.
6. A failure in steps 2-4 (bad JSON, unreadable transcript) yields an empty message and continues: the
   `turn` bump and `idle` write still happen. If the DB write fails the process exits non-zero, which
   triggers the command's plain `session idle` fallback.

`stop_capture::last_assistant_text` is pure over a `&str` tail plus a thin file reader. No existing local
code parses transcript *contents*: `src-tauri/src/transfer.rs` and `backend/herdr/resume.rs` only locate
and validate transcript files (`find_live_claude_jsonl`, `claude_session_file_exists`,
`encoded_cwd_dir_name`) and live in `src-tauri`, which the CLI cannot depend on. So this is a small new
parser. The reader seeks to the last 256 KB, drops the first (possibly partial) line, and walks lines in
reverse; a line that fails to parse is skipped.
- **Claude** (`~/.claude/projects/<encoded cwd>/<session>.jsonl`): lines are JSON objects with `type`.
  Take the trailing run of `type == "assistant"` lines (stop at the first `user` line) and concatenate, in
  file order, every `message.content[]` block with `type == "text"` (`text` field), joined with `\n`.
  Ignore `tool_use` and `thinking` blocks.
- **Codex** (`rollout-*.jsonl`): the last line with `type == "response_item"`, `payload.type == "message"`,
  `payload.role == "assistant"`: concatenate `payload.content[]` blocks of type `output_text` (`text`). If
  none, the last `type == "event_msg"` line with `payload.type == "agent_message"`: `payload.message`.
- The implementer confirms both line schemas against one real transcript from each CLI while writing the
  tests, then commits minimal scrubbed inline fixtures (placeholder text only).

## Hook installation (`hooks.rs`, `codex_hooks.rs`)

Changes, with exact names:
- `hooks.rs`: add `const GATE_HOOK_TIMEOUT_S: u64 = 600;` and
  `const CLAUDE_GATE_MAP: [(&str, &str); 2] = [("PermissionRequest", ""), ("PreToolUse", "AskUserQuestion")];`.
  Replace `fn hook_command(task_bin, state)` with a version taking a `HookCmd` enum
  (`State(&str)`, `Stop`, `StopFailure`, `Gate`) so every command string in the tables above is built in
  one place. `merged_hooks_settings` keeps looping `HOOK_MAP` (its `Stop`/`StopFailure` rows now use
  `HookCmd::Stop`/`StopFailure`) and adds a loop over `CLAUDE_GATE_MAP` building
  `{"matcher": m, "hooks":[{"type":"command","command":…,"timeout":GATE_HOOK_TIMEOUT_S}]}`.
- `codex_hooks.rs`: `CODEX_HOOK_MAP` keeps its shape, but its `PermissionRequest` row now uses the gate
  command and `Stop` uses `--stdin`. Extend `hook_command` the same way (Windows PowerShell and unix forms
  in the table above) and add `timeout` to the `PermissionRequest` hook object only. Keep a small
  platform-independent builder helper so both shapes are testable on any OS.
- `is_ours` in both files already matches the current markers (`keitora:session-hook`,
  `keitora:codex-session-hook`), which every new command carries; do not rely on the bare
  `/keitora session ` substring (the quoted gate command does not match it). `arr.retain(!is_ours)` then
  `push` keeps the merge idempotent and path-change safe, and preserves a user's own `PermissionRequest`
  or `PreToolUse` hooks on the same event.
- Existing tests that compare to `hook_command(bin, "idle")` for `Stop`, and the Codex
  `PermissionRequest` -> `waiting` expectation, are updated to the new commands.

Reinstall for existing worktrees is automatic. `install_hooks_for` / `install_codex_hooks_for` rewrite a
worktree's file whenever the merged JSON differs from the file (and skip when equal, keeping `git status`
clean). They run at app start and on project activation (`hooks::install_active_session_hooks`,
`src-tauri/src/host.rs:514` and `:1134`), on config reload (`hooks::install_session_hooks`,
`host.rs:2403`), and for a new worktree (`worktree_create`, `host.rs:558-565`). Claude hot-reloads the
settings file. A running Codex session reads hooks at launch, so it needs a `reset`/restart (and, for
sessions not launched by Keitora, re-trust) to pick up the gate; `keitora drive` warns when the driven
worktree's file lacks the gate entry (02).

## Spikes (manual verification owed)

Neither can be unit tested. Both must be run by hand and the result recorded in the PR before this stage
is called proven; until then only the pure logic is covered by tests.

1. **Spike (a), Claude.** (i) A `PermissionRequest` command hook that prints nothing and exits 0 leaves the
   normal permission dialog showing. (ii) A `PreToolUse` hook on `AskUserQuestion` returning
   `permissionDecision: allow` with `updatedInput{questions, answers}` answers the question with no UI,
   and printing nothing shows the normal question UI. (iii) Bypass/auto-accept sessions never fire
   `PermissionRequest`. (iv) A blocking hook of about 60s does not freeze the pane.
2. **Spike (b), Codex on Windows.** (i) `PermissionRequest` and `Stop` hooks actually fire in a headless
   pane (the original spike never saw them: the account hit its usage limit). (ii) stdin reaches
   `& "BIN" session gate …` through PowerShell (else use the `$input |` form) and stdout passes through to
   Codex. (iii) The hook-object key is `timeout` in seconds; if it is ignored, the 600s default equals our
   value, so behaviour is unchanged. (iv) The `hookSpecificOutput.decision` shape is accepted for allow
   and deny.

**Fallback design if spike (a)(i) fails** (no output does not yield the dialog): broker through
`PreToolUse` instead of `PermissionRequest`. Drop the Claude `PermissionRequest` entry; add a `PreToolUse`
entry with matcher `Bash|Edit|Write|MultiEdit|NotebookEdit|WebFetch|mcp__.*` (plus the existing
`AskUserQuestion` entry), both running the same `gate`. `Route` gains `PreToolUseTool` (kind
`permission`). Output mapping for it: allow -> `permissionDecision: "allow"`, deny -> `"deny"` +
`permissionDecisionReason`, escalate -> `"ask"` (forces the native dialog). Known degradation: `PreToolUse`
fires for calls that would not have prompted (already-allowed commands), so the driver sees extra prompts,
and a driver `allow` bypasses the user's own ask rules for those tools; driver guidance notes this. If
(ii) fails independently, question brokering is dropped for Claude (always escalate) and 02's `--choice`
is documented accordingly.

## Risks owned here

1. Claude `PermissionRequest` no-output behaviour (spike (a)); fallback above.
2. Blocking hooks hide the dialog while a prompt is brokered; mitigated by the rail state, Take over (05)
   and the bounded broker timeout (cap 540s).
3. Codex hooks are less proven here (spike (b)): stdin/stdout through PowerShell, event firing, trust
   re-review after a hook-content change.
4. Secrets in `tool_input` land in `tasks.db` (truncated, purged by 01 after 24h).

## Tests (same change, `#[cfg(test)]`; pure functions with fakes; no real DB, no herdr)

`gate.rs` (fake `GateStore` with scripted statuses and a fake `Clock` that advances virtual time):
- `parse_payload`: Claude `PermissionRequest` Bash payload; Claude `PreToolUse` `AskUserQuestion`
  payload; Codex `PermissionRequest` payload; unknown fields ignored; missing `tool_name`, non-object,
  invalid UTF-8, empty input and malformed `questions` each give `Err`.
- `route`: every row of the routing table, including claude `PermissionRequest` + `AskUserQuestion` =
  `Ignore` and claude `PreToolUse` + `Bash` = `Ignore`.
- `read_bounded`: stops at the cap; returns partial data when the reader blocks past the timeout (a reader
  that never returns); terminal case skipped.
- `summarize` (Bash first line capped at 200 chars, Edit/Write path, WebFetch url, `mcp__` name, fallback
  compact JSON) and `truncate_input` (string leaves over 1024 bytes cut with a marker; a value still over
  8192 bytes collapses to `{"_truncated": "<first 8000 bytes>"}`; cuts on char boundaries, including
  multibyte input).
- `outcome_from_row`: the full table above, including `allowed` on a question, `answered` on a permission,
  wrong and partial answer keys, malformed `decision_json`, unknown status.
- `render`: every non-empty cell of the output table asserted byte for byte, including the
  `AskUserQuestion` `updatedInput{questions, answers}` shape (original `questions` preserved, answers keyed
  by question text), message truncation at 500 bytes, ASCII-only escaping of non-ASCII input.
- `run_gate` flow: not driven (no output, no insert; Codex writes `waiting`), paused, driver offline,
  driven allow/deny/answer, human `escalated`, timeout with no decision (virtual 300s, and the 540s cap
  when `escalate_after_s = 9999`), timeout race (compare-and-set loses, driver decision wins), drive
  released or stolen mid-wait, `withdrawn` row, 3 transient errors then recovery vs a 4th error
  escalating, `data_version` unchanged means no `SELECT` between safety ticks (count calls on the fake),
  `busy` written only on driver decisions.
- Fail-safe property: every error input and every non-`allowed` status gives `None` (or the render of a
  non-allow outcome), never text containing `"allow"`.

`stop_capture.rs`: `last_assistant_message` wins; Claude transcript fallback (multi-block assistant
message, trailing run stops at a `user` line, `tool_use` skipped, partial first line dropped); Codex
rollout fallback (`output_text`, `agent_message` fallback); non-`.jsonl` path rejected; 32 KB truncation on
a char boundary with marker; `--failed` fixed message; empty or invalid payload gives an empty message.

`hooks.rs` / `codex_hooks.rs`: gate entries present on `PermissionRequest` and `PreToolUse`
(`AskUserQuestion` matcher) with `timeout: 600`; `Stop`/`StopFailure` use `--stdin`/`--failed` with the
fallback chain; every new command carries its marker and `is_ours` replaces an older install (including a
pre-change Codex `session waiting` entry and a pre-change Claude `Stop` entry); idempotent re-merge; a
user's own `PermissionRequest` hook is preserved; gate and `--stdin` commands discard only stderr (assert
no `>/dev/null 2>&1` or `*> $null` on those invocations, only on the fallbacks); the Codex Windows string
ends `; exit 0 # keitora:codex-session-hook`.

## Singletonize decisions

1. **Hook timeout is a fixed 600s** for both providers and the broker cap is 540s, replacing "broker
   timeout + 30s". The timeout is per file, `escalate_after_s` is per drive, and one file serves any
   drive; 600 equals both providers' documented default, so an ignored or misnamed `timeout` key is
   harmless. 02 must validate `--escalate-after` to 1..=540.
2. **Question escalation prints nothing**, not `permissionDecision: ask` (this changes the earlier table).
   `ask` would add a tool-permission dialog before the question UI; no output on `PreToolUse` is the
   defined "continue normally" and shows the native question. Spike (a)(ii) confirms.
3. **Gate stdin cap is 1 MiB**, not 64 KB, because a large `Write` input would otherwise fail to parse and
   silently skip brokering; 64 KB applies to the `Stop` capture only. Stored `tool_input` is truncated to
   8 KB.
4. **Provider is passed as `--provider`** on the gate command instead of being inferred from the payload.
5. **Old-CLI safety nets**: `Stop`/`StopFailure` and the Codex `PermissionRequest` commands chain a
   fallback to the old plain `session idle` / `session waiting`, so a stale CLI never leaves a session
   stuck at `busy` or loses Codex's `waiting`; the Codex PowerShell form ends in `; exit 0`.
6. **Identity from the process cwd**, not payload `cwd`, matching the existing `session` hooks.
7. **ASCII-only JSON output** to avoid PowerShell/Git Bash encoding corruption.
8. **Pure logic lives in `crates/keitora-cli/src/gate.rs` and `stop_capture.rs`** behind `GateStore` and
   `Clock` traits implemented over 01's `LocalStore` API, because the CLI crate cannot depend on
   `src-tauri` and the logic needs fake-driven tests.
9. **Transcript parsing is new code**; nothing local parses transcript contents to reuse.
10. **Timeout escalation is a compare-and-set followed by one re-read**, so a decision landing in the same
    instant is honoured rather than lost.
11. **`waiting` writes never withdraw prompts** (only `busy`/`idle`/`offline` do), so the gate's own write
    and Claude's `Notification` hook cannot clear a pending prompt; 01 must implement it that way.
