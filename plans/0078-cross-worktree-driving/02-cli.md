# 0078-02 CLI (`crates/keitora-cli`)

Part of `plans/0078-cross-worktree-driving/`. See `00-overview.md` for goal and glossary.

**Dependencies:**
- 01 task store: every verb is a thin layer over its `LocalStore` methods (named below).
- 03 hook broker: shares the `keitora session` entry point and identity resolution. `drive` warns when
  the driven worktree's hook file lacks the gate entry (03 defines the entry).
- 04 app runner: `send`, `read`, `reset`, `swap` only enqueue `drive_requests`; the app applies them.
  `answer`, `prompts`, `wait`, `drives`, `drive`, `release` are DB-only.

Placeholder names only: driver `alpha`, driven `bravo`/`charlie`, runtime provider names `claude`,
`codex`. No task or plan identifiers in code or commits.

## Rules common to all verbs

- Top-level verbs, no `drive attach`.
- The CLI never touches herdr or panes directly (same rule as `task dispatch`,
  `crates/keitora-cli/src/main.rs:667-672`). Pane-affecting verbs enqueue a request and poll its row.
- The caller (`me`) is `keitora_tasks::current_worktree()` (`crates/keitora-tasks/src/config.rs:2944`,
  git toplevel basename, `"unknown"` outside git). `me == "unknown"` is exit 6 for every drive verb.
- Every verb except `drive` and `drives` requires `me` to be the target's current driver
  (`LocalStore::drive_get(target)`: none -> exit 3 "@bravo is not driven"; other driver -> exit 3
  "@bravo is driven by @x, not you"). The store re-checks atomically; a race that slips past surfaces
  as exit 4.
- `--json` is the existing global flag (`Cli.json`, `main.rs:21`); it works on every verb. JSON is
  printed with `serde_json::to_string_pretty` like `cmd_task_dispatch`.
- Target worktree names are validated against `Config.worktrees` when that list is non-empty
  (as `cmd_task_dispatch` does): unknown name -> exit 6 `unknown worktree "x" — must be one of: ...`.
- `send`, `read`, `reset`, `swap` first require the project to be registered with the app (the same
  check `cmd_task_dispatch` runs: `UserConfig::load()` then `project_ref_by_root(root)`; message
  `unregistered_message(root)`), exit 6. Extract the check into a shared fn used by both.

## Identity and project resolution

`keitora_tasks::resolve()` (`config.rs:1928`) order is: `$KEITORA_PROJECT` -> registry match on cwd
-> cwd walk to `keitora.toml` (up to 8 levels) -> `NotFound`. It lands on the project's single
`tasks.db` for every worktree. `current_worktree()` is the git toplevel basename, correct from a
subdirectory. The same identity the existing `session` hooks rely on.

**Dispatch order in `run()` (`main.rs:240`).** Today `run()` resolves the config, then opens
`open_backend` and `LocalStore`, then matches `cli.cmd`. New order:

1. `Cmd::Config` (unchanged, before `resolve()`).
2. `resolve()` + `Config::from_project` (unchanged).
3. **Before** `open_backend`: `Cmd::Session { .. }` and every drive verb open only
   `LocalStore::open(&config.db_path)` and return. Drive verbs never need the task backend (a GitHub
   backend must not be contacted to answer a prompt).
4. Everything else (`Cmd::Task`) opens the backend as today.

`gate` is a reserved first argument of the `Session` command (03 owns its behaviour); 02 only
guarantees the early dispatch so the gate and hooks stay cheap.

## clap structure

New file `crates/keitora-cli/src/drive.rs` (`mod drive;` in `main.rs`) owns the arg structs and the
handlers. `Cmd` in `main.rs` gains ten top-level variants, each a single `#[derive(clap::Args)]`
struct from `drive.rs` (except `Drives`, a unit variant):

```rust
enum Cmd {
    Task(TaskCmd), Session { state: String }, Config(ConfigCmd),   // existing
    /// Bind this worktree as the driver of another
    Drive(drive::DriveArgs),
    /// End a drive binding
    Release(drive::ReleaseArgs),
    /// List drive bindings in this project
    Drives,
    /// Send a prompt to a driven worktree
    Send(drive::SendArgs),
    /// Block until a driven worktree finishes its turn, needs a decision, or stalls
    Wait(drive::WaitArgs),
    /// Read a driven worktree's pane tail or last reply
    Read(drive::ReadArgs),
    /// List pending brokered prompts
    Prompts(drive::PromptsArgs),
    /// Decide a brokered prompt
    Answer(drive::AnswerArgs),
    /// Start a fresh conversation in a driven worktree, optionally on another provider
    Reset(drive::ResetArgs),
    /// Switch a driven worktree's provider, carrying context (needs a human confirm)
    Swap(drive::SwapArgs),
}
```

`Drive`'s `about`/`long_about` carries the driver guidance (see the last section), which is what
`keitora drive --help` prints.

### Args (exact)

```rust
pub struct DriveArgs {
    pub worktree: String,
    /// Seconds before an unanswered prompt escalates to the human (1..=540; default 300)
    #[arg(long, value_name = "SECS")]
    pub escalate_after: Option<i64>,
    /// Take over a worktree already driven by someone else
    #[arg(long)]
    pub force: bool,
}
pub struct ReleaseArgs {
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub worktree: Option<String>,
    #[arg(long)]
    pub all: bool,
}
pub struct SendArgs {
    pub worktree: String,
    /// The prompt text; "-" reads it from stdin (max 32768 bytes)
    pub text: String,
    /// If the agent is busy, keep the request pending until idle (up to 10 minutes) instead of refusing
    #[arg(long)]
    pub queue: bool,
    /// Block until the app applies or refuses the request (not until the turn finishes: use `wait`)
    #[arg(long)]
    pub wait: bool,
    /// With --wait: give up after this many seconds (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 60, requires = "wait")]
    pub timeout: u64,
}
pub struct WaitArgs {
    #[arg(required = true)]
    pub worktrees: Vec<String>,            // one or more
    /// Give up after this many seconds (exit 2). 0 = wait forever (the default)
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    pub timeout: u64,
    /// Report `stalled` when a busy worktree has had no hook activity this long (min 10)
    #[arg(long, value_name = "SECS", default_value_t = 120)]
    pub stall_after: u64,
}
pub struct ReadArgs {
    pub worktree: String,
    /// Pane tail lines (1..=2000)
    #[arg(long, default_value_t = 200, conflicts_with = "last")]
    pub lines: i64,
    /// Print the last finished turn's message from the database instead of reading the pane
    #[arg(long)]
    pub last: bool,
    /// Seconds to wait for the app (exit 2)
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    pub timeout: u64,
}
pub struct PromptsArgs { pub worktree: Option<String> }
pub struct AnswerArgs {
    pub worktree: String,
    pub id: i64,
    #[arg(value_enum)]
    pub decision: AnswerDecision,          // allow | deny | answer | escalate
    #[arg(long)]
    pub reason: Option<String>,
    /// For question prompts: QUESTION=ANSWER; QUESTION is the full question text or its 1-based index
    #[arg(long = "choice", value_name = "Q=A")]
    pub choices: Vec<String>,
}
pub struct ResetArgs {
    pub worktree: String,
    #[arg(long, value_name = "PROVIDER")]
    pub to: Option<String>,
    #[arg(long)] pub wait: bool,
    #[arg(long, value_name = "SECS", default_value_t = 60, requires = "wait")] pub timeout: u64,
}
pub struct SwapArgs {
    pub worktree: String,
    #[arg(long, value_name = "PROVIDER")]
    pub to: String,                        // required
    #[arg(long)] pub wait: bool,
    #[arg(long, value_name = "SECS", default_value_t = 300, requires = "wait")] pub timeout: u64,
}
```

Notes on the args:
- The proposal lists `answer ... allow|deny|escalate`; its decision table also has **`answer`** (for
  `AskUserQuestion`), so the enum has all four.
- Provider values (`--to`) are a free `String` matching `^[a-z][a-z0-9_-]{0,31}$` (else exit 4). The
  CLI does **not** hardcode `claude`/`codex`: a third provider must work for `reset`. The app refuses
  unknown or unsupported providers at apply time (exit 5 with the app's reason). `swap` supports only
  the pairs the transfer engine supports; the app refuses others the same way.
- `text == "-"` reads stdin to end (bounded at 32768 bytes; larger -> exit 4). An empty text is exit 4.
- `release` with neither a worktree nor `--all` is a clap usage error (exit 2).
- `--wait` is a boolean flag here (not `task dispatch`'s `--wait <secs>`), per the proposal's
  signature; the bound is the separate `--timeout`.

## Exit codes (all drive verbs)

Implemented as `struct ExitError { code: i32, msg: String }` (implements `std::error::Error`),
returned inside the existing `Box<dyn Error>`. `main()` (`main.rs:233`) prints `error: {msg}` to
stderr as today, then exits with `e.downcast_ref::<ExitError>().map(|x| x.code).unwrap_or(1)`.
`keitora_tasks::Error::Conflict(m)` from the store maps to `ExitError { 4, m }`; any other store
error is exit 1.

| Code | Meaning | Examples |
|---|---|---|
| 0 | Success (event delivered, request applied, nothing to do for `release --all`) | |
| 1 | Unexpected error (IO, SQLite, JSON, anything unclassified) | DB locked past the busy timeout |
| 2 | Timeout, **no state change** | `wait --timeout` elapsed; `send/reset/swap --wait` or `read` timed out. Also clap usage errors (clap's fixed exit code, unchanged from today; a timeout always prints a `timeout` line/event on **stdout**, usage errors print only to stderr) |
| 3 | Caller is not the driver | target not driven; driven by someone else |
| 4 | Rejected by a rule or invalid input | one-level/self/already-driven (drive); drive paused; prompt not pending, wrong kind, wrong driver; `escalate` without `--reason`; bad `--choice`; bad `--to`; empty/oversize text; `wait` with nothing to wait for |
| 5 | The app refused the request, or it ended `refused` | agent busy (no `--queue`); project not open in Keitora; reset during a transfer; swap cancelled or unsupported pair; request expired |
| 6 | Not found / unavailable | unknown worktree name; prompt id not found; project not registered with the app; caller is `"unknown"` |

When a request ends `refused` the verb still prints its normal result (JSON object or human line,
including the reason) to stdout, then exits 5.

## Verbs

### `keitora drive <wt> [--escalate-after S] [--force]`

1. Validate `<wt>` (exit 6). Call `local.drive_bind(me, wt, escalate_after, force)`; a `Conflict` -> exit 4
   (the message already names the rule).
2. Hook check (warning only, never an error): read `<Config::worktree_path(wt)>/.claude/settings.local.json`
   and `<...>/.codex/hooks.json` as text; if at least one exists and **none** contains the substring
   `session gate`, add the warning `"@bravo's hook file has no gate entry; the installed keitora may
   be too old to broker prompts — upgrade it and restart the app, or prompts will reach the human
   directly"`. If neither file exists, no warning (`hook_gate_installed: null`).
3. Output.

JSON (`--json`):
```json
{"driver":"alpha","driven":"bravo","since":"2026-01-01 10:00:00","escalate_after_s":300,
 "paused":false,"forced_from":null,"escalated_prompts":0,"hook_gate_installed":true,
 "warnings":[]}
```
`hook_gate_installed` is `true|false|null`. `forced_from` is the previous driver on a steal.
Human: `  alpha now drives @bravo (escalate after 300s)` plus ` (took over from @x; N prompt(s) escalated)`
on a steal, then each warning as `  warning: ...`.

### `keitora release <wt> | --all`

`release <wt>`: authorize as above (exit 3), then `drive_release(me, wt)`. `--all`:
`drive_release_all(me)`; an empty result is success (`released: []`).
JSON: `{"released":[{"driven":"bravo","escalated_prompts":1}]}`.
Human: `  Released @bravo (1 pending prompt escalated to you)` per worktree, or `  Nothing to release`.

### `keitora drives`

Open to any caller. For each `drives_list()` row (`ORDER BY driven`) join: session state from
`sessions_list()` (`null` if no session row), pending prompt count from `drive_prompts_pending(None,
Some(driven))`, and `turn` from `session_reply_get`.
JSON:
```json
{"drives":[{"driver":"alpha","driven":"bravo","since":"...","paused":false,
  "escalate_after_s":300,"forced_from":null,"awaiting_turn":4,"turn":3,
  "state":"busy","pending_prompts":0}]}
```
Human, one line per drive: `  alpha -> bravo  busy  prompts:0  turn:3 (awaiting 4)  [paused]`; or
`  No drives`.

### `keitora send <wt> <text> [--queue] [--wait [--timeout S]]`

1. Authorize (3); app-registered check (6); read stdin if `text == "-"`; validate size (4).
2. `drive_request_add("send", wt, me, {"text": text}, queue)` (a paused drive -> 4).
3. Without `--wait`: print the pending result and exit 0. With `--wait`: poll
   `drive_request_get(id)` every **200 ms** (same as `DISPATCH_WAIT_POLL`, `main.rs:665`) until
   `is_resolved()` or the timeout.
   - Timeout while `pending`: `drive_request_cancel(id, "no running Keitora app picked it up within Ns")`
     and exit 2. Timeout while `running`/`pending_confirm`: leave it, exit 2, and say the request
     is still in flight.
4. `applied` -> exit 0. `refused` -> print result, exit 5.

`applied` result is `{"awaiting_turn": N}`; N is the turn number that `wait` will deliver.
JSON: `{"request_id":12,"status":"applied","reason":"","queued":false,"awaiting_turn":4}`. Without
`--wait` the status is `"pending"` and `awaiting_turn` is `null` (the app sets it on apply).
Human: `  Sent to @bravo (turn 4)`, or `  Requested send to @bravo (request 12); the running Keitora app applies it`,
or `  Refused send to @bravo (<reason>)`.

### `keitora wait <wt>... [--timeout S] [--stall-after S]`

Exact semantics:

- **Setup.** Each listed worktree is authorized (exit 3 on the first failure, listing it). Duplicates
  are removed, order is kept. `--stall-after` below 10 is exit 4. Initial check: unless at least one
  listed worktree has `drives.awaiting_turn IS NOT NULL` **or** a pending prompt, exit 4
  `nothing to wait for: no send is outstanding on bravo, charlie` (no hang).
- **Polling.** Every 500 ms read `data_version()` on one held connection; run the real evaluation
  (below) only when it changed, **or** when 5 s have passed since the last evaluation (so `stalled`,
  which depends on time, still fires with no DB change), and always once at start. Never hold a read
  transaction open between polls. Use `std::thread::sleep` as the existing waits do.
- **One evaluation**, per listed worktree `w` in argument order, producing at most one candidate
  event each:
  1. `prompt`: `drive_prompts_pending(Some(me), Some(w))` non-empty -> the oldest (lowest id).
  2. `turn_finished`: `drive_get(w).awaiting_turn = Some(a)` and
     `session_reply_get(w).turn >= a` (missing reply row counts as turn 0).
  3. `stalled`: `awaiting_turn` is `Some`, the session state is `busy`, and
     `idle = now - max(last_heartbeat, wait_started) >= stall_after`, where `idle` comes from
     `session_idle_secs(w)` clamped to `elapsed_since_wait_start` (`min(session_idle_secs,
     secs_since_this_call_started)`). The clamp guarantees at least `stall_after` seconds pass inside
     each call, so a driver that re-waits after a `stalled` is not told again immediately.
- **Selection across worktrees.** Among all candidates pick, in order: any `prompt` (a prompt blocks
  the driven agent), else any `turn_finished`, else any `stalled`; ties broken by argument order.
  Exactly **one event per call**; the call then exits 0.
- **On `turn_finished`** call `drive_ack_turn(w, a)` before printing so the same turn is never
  delivered twice. `prompt` and `stalled` are not acked (a prompt stays pending until answered; an
  unanswered prompt is re-delivered by the next `wait`).
- **A drive disappearing** mid-wait (released by the driver or the human, or stolen): exit 3
  `@bravo is no longer driven by you`. A drive that is paused keeps waiting (prompts won't appear;
  turns still complete).
- **Timeout** (`--timeout` > 0 elapsed): print the `timeout` event (below) on stdout, exit 2, change
  no state.

Event JSON (one object, pretty-printed; `event` discriminates):

```json
{"event":"turn_finished","worktree":"bravo","turn":4,"last_message":"...","updated_at":"..."}
{"event":"prompt","worktree":"bravo","prompt":{ /* prompt object, see `prompts` */ }}
{"event":"stalled","worktree":"bravo","state":"busy","idle_s":125}
{"event":"timeout","waited_s":90,"worktrees":["bravo","charlie"]}
```

Human output: `  @bravo finished turn 4` followed by the `last_message` on following lines;
`  @bravo needs a decision: prompt 7 (permission, Bash: rm -rf build)`;
`  @bravo looks stalled (busy, no activity for 125s); try: keitora read bravo`;
`  timed out after 90s`.

`stalled` is also the mitigation for Codex `request_user_input`, which no hook sees (herdr reports the
agent as busy): after `stalled`, the driver runs `keitora read <wt>`. v1 adds no distinct state.
Heartbeats come from whatever the `keitora session` hooks write, so a single very long tool call can
produce a false `stalled`; the driver reads and re-waits.

### `keitora read <wt> [--lines N | --last] [--timeout S]`

- `--last`: DB-only, no app needed (skips the registered check). `session_reply_get(wt)`; none ->
  exit 6 `no finished turn recorded for @bravo`.
  JSON: `{"worktree":"bravo","source":"last_message","text":"...","truncated":false,"turn":4,"updated_at":"..."}`.
- Default: `--lines` must be 1..=2000 (exit 4). `drive_request_add("read", wt, me, {"lines": N}, false)`,
  poll as `send --wait` does with `--timeout` (default 30; exit 2 on timeout, cancelling a still-`pending`
  row). The applied `result` is the pane tail text (the app truncates to the last 64 KB, 04).
  JSON: `{"worktree":"bravo","source":"pane","text":"...","truncated":false,"lines":200}`;
  `truncated` is `result.len() >= 65536`. A `refused` request (pane not open, project closed) -> exit 5.
- Human: the raw text on stdout, nothing else.

### `keitora prompts [<wt>]`

For the caller's drives (`drives_by_driver(me)`), or just `<wt>` (authorized, exit 3). Lists
`drive_prompts_pending(Some(me), wt)`, oldest first. Prompt object (shared with `wait`'s `prompt`
event):

```json
{"id":7,"driven":"bravo","kind":"permission","tool_name":"Bash","summary":"rm -rf build",
 "tool_input":"{\"command\":\"rm -rf build\"}","age_s":42,"timeout_s":300,
 "created_at":"...","provider":"claude",
 "questions":null}
```
For `kind = "question"` and a `tool_input` that parses as JSON with a `questions` array, `questions` is
`[{"index":1,"question":"Which db?","header":"DB","multi_select":false,"options":["pg","sqlite"]}]`
(`options` are the `label`s); otherwise `null`. JSON output: `{"prompts":[ ... ]}`.
Human, one line each: `  #7 @bravo permission Bash: rm -rf build  (42s ago)`; `  No pending prompts`.

### `keitora answer <wt> <id> allow|deny|answer|escalate [--reason R] [--choice Q=A]...`

1. Authorize (3). `drive_prompt_get(id)`: missing, or `driven != wt` -> exit 6 `no prompt 7 for @bravo`.
2. Build the `PromptDecision` (01):
   - `allow`: no flags allowed (`--choice` -> 4).
   - `deny`: optional `--reason` (default `denied by driver`).
   - `escalate`: `--reason` required and non-empty (exit 4 otherwise).
   - `answer`: kind must be `question`; at least one `--choice`.
     Parse each `--choice` at its **first** `=` (key, value; both non-empty after trim, else 4). If the
     key is an integer N, map to the Nth question's text (1-based); else it must equal a question's
     `question` text exactly. The prompt's `tool_input` must parse as JSON with `questions`
     (else exit 4 `cannot read this question; use escalate`). Every question must be answered
     (missing ones are listed, exit 4); a repeated key keeps the last. Values pass through verbatim
     (for multi-select, the driver joins labels with `, `). Result:
     `Answer { answers: { "<question text>": "<value>" } }`.
3. `drive_prompt_decide(id, me, &decision)`; `Conflict` -> exit 4 (already decided, paused, wrong
   kind...). Success exit 0.

JSON: `{"id":7,"status":"allowed","decided_by":"driver:alpha","reason":""}`.
Human: `  Prompt 7 allowed`.

### `keitora reset <wt> [--to PROVIDER] [--wait [--timeout S]]`

Authorize, registered check, validate `--to` (4), `drive_request_add("reset", wt, me, {"to": ...}, false)`,
poll as `send`. Default timeout 60 s (a restart takes several seconds).
JSON: `{"request_id":13,"status":"applied","reason":"","provider":"codex"}` (`provider` is the result's
`provider` when applied, else the requested `--to`, else `null`). Human: `  Reset @bravo on codex`,
`  Requested reset of @bravo (request 13)`, `  Refused reset of @bravo (<reason>)`.

### `keitora swap <wt> --to PROVIDER [--wait [--timeout S]]`

As `reset` with kind `swap`. The request can sit in `pending_confirm` while the human accepts or
cancels the transfer confirm in the Keitora window; `--wait` keeps polling through it (default timeout
300 s). JSON status is one of `pending|running|pending_confirm|applied|refused`; human line
`  @bravo is waiting for the human to confirm the swap` for `pending_confirm`. A cancelled confirm ends
`refused`, exit 5. Timeout while `pending_confirm`: exit 2 and the request is left (the confirm may
still be answered).

### `keitora wake <wt> [--to PROVIDER] [--fresh] [--wait [--timeout S]]`

Authorize, registered check, validate `--to` (4), then `drive_request_add("wake", wt, me,
{"fresh": bool, "to"?: ...}, false)` and poll as `reset`. `wake` is its own kind so an app that
predates it refuses the row as unknown instead of running a reset (an earlier draft filed a
`reset` with `"mode": "wake"`, which such an app ran as a reset). The app decides whether the
agent is awake: an awake target is applied with `already_awake: true` (a no-op).
JSON: `{"request_id","status","reason","provider","already_awake","timed_out"}`; `already_awake` is
a boolean only when the wake is `applied`, `null` while pending or when refused. Human:
`  Woke @bravo on claude`, `  @bravo is already awake`, `  Requested wake of @bravo (request 13)`,
`  Refused wake of @bravo (<reason>)`.

`send` adds `"wake": true` to its payload unless `--no-wake`: the app wakes a stopped agent first and
delivers once it is idle (queued semantics).

Possible later additions (not v1): `keitora stop <wt>`, `keitora launch <wt>`, `keitora interrupt <wt>`.

## Driver guidance

Help text of `keitora drive --help` plus a README section ("driving other worktrees"):
- the loop: `send` -> `wait` -> `answer`/`read` -> repeat (use `--timeout` below your tool's own
  command timeout and re-wait on exit 2),
- the escalation contract: escalate destructive, credential, external-side-effect, scope-changing and ambiguous
  prompts, and always give a reason,
- driving doesn't elevate the driver (a driven agent's output is the driver's input; the driver stays
  bounded by its own permission prompts),
- upgrade the CLI: the hook binary is the PATH `keitora`, so an older installed CLI silently never
  brokers (03),
- the exit codes table above (so an agent can branch on 2, 3, 4, 5).

## Tests

In `crates/keitora-cli/src/drive.rs` `#[cfg(test)]`, plus store tests in 01. Pure functions are
extracted so most tests need no DB; DB tests use a temp `LocalStore`.

Parsing (like `parses_task_dispatch_and_watch`, `main.rs:1236`):
- `parses_drive_verbs`: every verb parses with its flags and defaults (`wait --timeout` default 0,
  `--stall-after` 120, `read --lines` 200, `swap` timeout 300).
- `release_requires_worktree_or_all_but_not_both`
- `send_timeout_requires_wait`
- `swap_requires_to`

Exit codes and authorization:
- `non_driver_is_refused_with_exit_3` (for send, wait, read, prompts, answer, reset, swap, release).
- `unknown_worktree_is_exit_6`
- `one_level_and_self_rejection_surface_as_exit_4`
- `drive_force_steal_reports_previous_driver_and_escalated_count`
- `drive_escalate_after_out_of_range_is_exit_4` (0 and 541 refused by the store's `Conflict`, 1 and 540 accepted).
- `store_conflict_maps_to_exit_4_other_errors_to_1` (`ExitError` mapping fn, pure).
- `paused_drive_refuses_send_answer_reset_swap_but_allows_read_wait` (exit 4 vs success).

`wait` event selection (pure `fn select_event(&[WaitInput], stall_after, elapsed) -> Option<WaitEvent>`):
- `wait_prefers_prompt_over_turn_over_stalled`
- `wait_turn_finished_requires_turn_ge_awaiting_and_missing_reply_is_zero`
- `wait_stale_idle_before_turn_bump_does_not_finish` (turn < awaiting).
- `wait_stalled_only_when_busy_and_awaiting_and_clamped_to_call_start`
- `wait_multiple_worktrees_picks_first_in_argument_order_within_a_class`
- `wait_acks_turn_so_it_is_not_redelivered` (DB test: second call with only that worktree exits 4 nothing to wait for).
- `wait_nothing_outstanding_is_exit_4`
- `wait_timeout_prints_event_and_exits_2_without_state_change` (use a tiny `--timeout`; assert `awaiting_turn` and prompts unchanged).
- `wait_released_mid_wait_is_exit_3` (release from a second connection while waiting).
- `wait_skips_select_when_data_version_unchanged` (counter on the evaluation fn).

`answer`:
- `answer_choice_parses_first_equals_and_index_or_text_key`
- `answer_requires_every_question_and_rejects_unknown_key`
- `answer_escalate_without_reason_is_exit_4`
- `answer_allow_on_question_and_answer_on_permission_are_exit_4`
- `answer_unparseable_question_input_suggests_escalate`

Requests:
- `send_enqueues_request_and_wait_resolves_applied_with_awaiting_turn` (resolve the row from a second connection as the app would).
- `send_wait_timeout_cancels_pending_request_and_exits_2`
- `send_refused_prints_reason_then_exit_5`
- `send_dash_reads_stdin_and_enforces_size_limit` (reader injected).
- `read_last_needs_no_app_and_missing_reply_is_exit_6`
- `swap_pending_confirm_is_polled_through_to_applied_or_refused`
- `reset_provider_validation_accepts_unknown_but_wellformed_names` (a third provider name passes the CLI).

Output:
- `drives_json_joins_state_prompts_and_turn`
- `prompt_object_parses_question_options` (and is `null` for malformed input).
- `human_lines_for_each_verb` (snapshot the human strings above).
- `gate_hook_check_warns_only_when_files_exist_without_session_gate`

## Singletonize decisions

1. **Exit code 2 collides with clap's usage-error exit code.** The proposal fixes timeout = 2, so it
   stays 2, and clap's usage errors keep exit 2 (changing them via `try_parse` would alter every
   existing verb). Disambiguation: a timeout always writes a `timeout` line/event to **stdout**; usage
   errors write only to stderr.
2. **Full exit table** (0/1/2/3/4/5/6 above) is new; the proposal only specified 2. It deliberately
   differs from `task dispatch`, which exits 1 for every failure; the existing verbs are unchanged.
3. **`--wait` is a flag plus `--timeout`**, not `--wait <secs>` as in `task dispatch`, to match the
   proposal's `[--wait]`. Defaults: 60 s send/reset, 300 s swap (human confirm), 30 s read.
4. **`answer` accepts `answer`** as a fourth decision (the proposal's signature omits it but its
   decision table needs it).
5. **`send` text `-` reads stdin** and **`read --last`** exist: small additions so multi-line prompts
   are possible without shell quoting hazards, and so a driver can re-fetch a reply already acked by
   `wait`. Neither touches the app.
6. **`wait` acks the turn** it delivers (`drive_ack_turn`, 01) and refuses to start with nothing
   outstanding (exit 4) instead of hanging. `awaiting_turn` is `NULL` until a `send` is applied, so
   a prompt can still be delivered for a worktree the human is typing into.
7. **Stalled timer** is clamped to the call start so repeated `wait` calls don't spin on one stall.
8. **Provider names are not validated against a fixed list in the CLI** (third provider, `agy`); only
   shape is checked, the app decides support.
9. **Drive verbs bypass `open_backend`**, like `session`; they only need the local store.
10. **No `pause`/`resume` verb**: pause is a human rail action (05) backed by `drive_set_paused` (01).
11. **`--escalate-after` is 1..=540** (default 300) because 03 fixes the hook timeout at 600 s and caps its broker at 540 s; the range is enforced by 01's `drive_bind` and surfaces as exit 4.
12. **The hook check** reads the two known hook files for the text `session gate` because the CLI
    crate can't link `src-tauri` (where the installers live); 03 must keep that literal in its gate
    command.
