# Phase 5: reply text fallbacks

Part of 0080; see `00-overview.md`.

## Purpose

When a turn completes (detected by herdr) and no Stop hook reply row exists, still return the
agent's reply. Today `read --last` returns nothing in that case.

## Verified current code

All paths under `D:/repos/keitora`.

| Symbol | Where | Fact that matters |
|---|---|---|
| `last_assistant_text_from_tail`, `claude_reply`, `codex_reply`, `last_assistant_text`, `truncate_reply`, `TRANSCRIPT_TAIL` (256 KiB), `REPLY_MAX` (32 KiB), `capture_message` | `crates/keitora-cli/src/stop_capture.rs` | Pure string parsers, lenient (bad lines skipped). `claude_reply` = trailing run of `assistant` lines back to the last `user` line (including a `tool_result` user line), `text` blocks only, joined with `\n`. `codex_reply` = last assistant `response_item` message `output_text` blocks, else the last `event_msg`/`agent_message`. They are **in the CLI crate**, which the app (`src-tauri`) does not depend on (`src-tauri/Cargo.toml` depends only on `keitora-tasks`). |
| `claude_projects_dir()` (honours `CLAUDE_CONFIG_DIR`), `encoded_cwd_dir_name(worktree)`, `claude_session_file_exists(projects, encoded, id)`, `validate_jsonl_in_dir`, `find_live_claude_jsonl`, `pick_jsonl` | `src-tauri/src/transfer.rs` | The Claude transcript locator pieces. `pick_jsonl` = newest-mtime `*.jsonl`, skipping `agent-*.jsonl` (subagent transcripts), and deliberately ignores any persisted id (stale after `/clear`/`--resume`). `claude_projects_dir` is `pub(crate)`; `pick_jsonl`, `validate_jsonl_in_dir` and `find_live_claude_jsonl` are private. |
| `codex_home_dir(...)`, `CodexEnv.codex_home` | `src-tauri/src/codex_env.rs` | `CODEX_HOME` when set and non-empty, else `~/.codex`. Rollouts are under `<codex_home>/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` (research, unverified on disk). |
| `SessionReply`, `session_reply_record(worktree, text) -> turn`, `session_reply_get` | `crates/keitora-tasks/src/drive_store.rs`, `drive.rs` | `record` bumps `turn` on every call. `read --last` (`read_last_output` in `crates/keitora-cli/src/drive.rs`) prints `last_message` from this row. `SESSION_REPLY_MAX` 32 KiB. |
| Claude launch args | `src-tauri/src/backend/herdr/agent.rs` | Keitora passes `--resume <id>`, `--continue` or nothing. It does **not** pass `--session-id` today, so "the transcript identified via `--session-id`" is not available from existing launches. The persisted `claude_session_id` in `ProviderIds` is written only by a transfer and is stale otherwise. |
| `agent_session` | herdr `agent list` (`research-herdr.md`) | `{source, agent, kind: id|path, value}`. Claude's herdr hook reports the transcript path (`--agent-session-path`) as well as the id. Hook-dependent, like everything under herdr's integrations; a cross-check, not a root of trust. |

Corrections to earlier plan text:
- The Claude transcript is **not** identified by `--session-id` (Keitora doesn't pass it).
  Resolution order is below. If Phase 4 (`05-hook-placement.md`) ever adds a launch-time
  `--session-id`, step 3 of the locator activates with no other change.
- The parsers must move out of the CLI crate before the app can use them (Decision R1).

## Design

Reply collection order (defined in `03-turns-and-cli.md`):
1. A Stop-hook reply recorded for this delivery (`session_replies.turn` moved; `03` T5).
2. **The transcript (this phase).**
3. None, with `reply_source: "none"` and a hint to use `keitora read`.

The transcript is used only for reply text after a herdr- or hook-detected completion, never as
a completion signal. Fixtures are captured, scrubbed copies (Spike 5), with synthetic stand-ins
until then.

### Module layout

- **`crates/keitora-tasks/src/transcript.rs` (new, pure).** Moves `truncate_reply`,
  `text_blocks`, `non_empty`, `claude_reply`, `codex_reply`, the constants and
  `last_assistant_text_from_tail` out of `stop_capture.rs` (which then `use`s them, so the Stop
  hook's behaviour and its existing tests are unchanged), and adds the turn-aware readers:
  ```rust
  pub struct TurnReply { pub text: String, pub outcome: TurnOutcome }
  pub enum TurnOutcome { Complete, Aborted }
  pub enum Provider { Claude, Codex }
  /// `lines`: parsed JSON objects of the transcript tail, file order.
  /// `delivered_at`: unix ms of the delivery being resolved (`Baseline.delivered_at_ms`;
  /// for a human turn, the ledger-move time, `03-turns-and-cli.md` T5 step 4).
  pub fn reply_for_turn(provider: Provider, lines: &[Value], delivered_at: i64) -> Option<TurnReply>;
  pub fn read_tail(path: &Path, max_bytes: u64) -> std::io::Result<TailRead>; // { text, truncated_front }
  ```
- **`src-tauri/src/transcript.rs` (new, side effects).** `locate_claude(...)`,
  `locate_codex(...)`, a small `TranscriptIndex` cache and `read_reply(provider, worktree_path,
  agent_session, delivered_at) -> Option<TurnReply>`. It uses `transfer.rs` helpers: make
  `claude_projects_dir`, `encoded_cwd_dir_name`, `claude_session_file_exists`, `pick_jsonl` and
  `validate_jsonl_in_dir` `pub(crate)` (no behaviour change).
- **Called by the engine's reply collector** (`03-turns-and-cli.md`), through a trait
  `TranscriptSource` so tests inject a
  fake and the fake backend never touches disk. `ReplyQuery { provider, worktree_path,
  agent_session: Option<AgentSessionRef>, launch_session_id: Option<String> /* None until a
  launch-time id exists */, delivered_at_ms }`. The trait returns the miss reason too:
  `fn reply(&self, q: &ReplyQuery) -> Result<TurnReply, &'static str>` (`not_found`,
  `not_this_turn`, `parse_failed`, `no_provider_support`), which the engine records in its
  completion ring. Phase 2 ships a `NoTranscript` impl returning `Err("no_provider_support")`.

### File location, per provider

**Claude.**
1. `agent_session` with `kind == path`, or herdr's reported transcript path: accept only if
   `validate_jsonl_in_dir(<claude projects>/<encoded-cwd>, path)` passes (an existing `.jsonl`
   directly inside this worktree's project dir).
2. `agent_session` with `kind == id` (`source` starts `herdr:claude`) and the id passes
   `keitora_tasks::is_safe_session_id`: `<claude projects>/<encoded-cwd>/<id>.jsonl`, checked
   with `claude_session_file_exists`.
3. A launch-time id, when Phase 4 provides one (`ReplyQuery.launch_session_id`); same path rule
   as 2.
4. Fallback: `pick_jsonl(<project dir>)` (newest non-`agent-*` `.jsonl`) **and** its mtime
   `>= delivered_at - 5 s`. Safe because Keitora runs one Claude agent per worktree; `/clear`
   and `--resume` switch files, so this runs afresh on every collection and never caches a
   fallback pick.
- `<claude projects>` is `claude_projects_dir()` (honours the app's `CLAUDE_CONFIG_DIR`; if the
  pane's value differs the locator misses and the reply is `none`, with finding input
  `transcript_not_found`).
- The result passes the containment guard in `find_live_claude_jsonl` (canonical path under the
  canonical projects dir, `.jsonl` extension); factor that guard into one function both use.

**Codex.**
1. `agent_session` `kind == id` (thread id, `source` `herdr:codex`): find
   `rollout-*-<id>.jsonl` under `<codex_home>/sessions`. Search today's then yesterday's
   `YYYY/MM/DD` dirs, then walk `sessions/*/*/*/` newest first, at most 400 `read_dir` calls
   (a resumed thread appends to its **original** dated file). Cache `id -> path` in
   `TranscriptIndex`; drop the entry when the file disappears.
2. `agent_session` `kind == path`: accept if the canonical path is under `<codex_home>/sessions`
   and the name matches `rollout-*.jsonl`.
3. Fallback: the newest `rollout-*.jsonl` in today's and yesterday's dirs with mtime
   `>= delivered_at - 5 s` whose first parsed line is `session_meta` with `payload.cwd` equal
   to the worktree path (compare after `strip_extended_prefix_str`, `/`-normalised, ASCII
   case-insensitive on Windows). A match without `cwd` in `session_meta` is rejected (a wrong
   worktree's reply is worse than none).
- `<codex_home>` is `codex_env::codex_home_dir` / `CodexEnv.codex_home`.
- Ids are validated (`is_safe_session_id`) before they become part of a glob or path.

**Antigravity (built in s27b; S5-agy confirmed the mapping).**

- **Locator:** herdr's agy `agent_session` (kind `id`, source `herdr:antigravity_cli`) names
  exactly one file, `~/.gemini/antigravity-cli/conversations/<id>.db` (a per-conversation
  SQLite file in WAL mode). The id must pass `is_safe_session_id`. `ANTIGRAVITY_CLI_CONFIG_DIR`
  stands for `.gemini/config` and is not applied (unverified for conversations; the resume code
  uses the same fixed root for `brain/`). The path is a plain join, so no index cache. No
  newest-mtime or cwd fallback.
- **Read:** read-only (`SQLITE_OPEN_READ_ONLY` through the existing `-wal`/`-shm` while agy has
  the file open; `immutable=1` when there is no `-wal`, so nothing is created next to it), busy
  timeout 200 ms, the last 500 `steps` rows by `idx`.
- **Parser rules** (from the captured file): table `steps(idx, step_type, status, metadata,
  step_payload, ...)`, blobs are protobuf. `step_type` 14 = user input, 15 = planner response,
  other values = tool steps; `status` 3 = done. `metadata` field 1 = creation time
  `{1: seconds, 2: nanos}`. Planner text = `step_payload` field 20 -> field 1 (field 8 holds the
  same text; fallback). The turn starts at the last user input created at or after
  `delivered_at - 2 s` (R3); the reply is the last planner response after it with text, only
  when that response is done. Anything else -> no reply.
- **Result:** `reply_source: "transcript"` (R4 unchanged; no new value). The agy caveat stays on
  every herdr-detected agy completion (background tasks may still run).
- **Fallback when unmapped** (no `agent_session`, id matches no file, unreadable or ambiguous
  data): `reply_source: "none"` with the hint `keitora read`, plus the existing agy caveat.

### Parse rules

Read at most the last `TRANSCRIPT_TAIL` = 256 KiB (`read_tail`), drop the first line when the
read started mid-file, skip lines that fail to parse or aren't JSON objects (as today). If no
line at or after `delivered_at` is found in the tail, re-read the last 2 MiB once, then give up
(`None`). Timestamps are the per-line `timestamp` field (RFC 3339), parsed to unix ms; a line
without one is treated as "unknown" and never selects a turn start by itself.

**Claude (`reply_for_turn(Claude, ..)`).**
1. Find the last `user` line that is a prompt (`message.content` is a string, or an array with
   any `text` block and no `tool_result`) with `timestamp >= delivered_at - 2000 ms`. None found
   -> `None` (the transcript doesn't show this delivery yet).
2. From that line to the end, collect `assistant` lines. Ignore lines with `isSidechain: true`
   (subagent traffic, if it appears inline; verify in Spike 5).
3. Reply = `text` blocks only (no `thinking`, `tool_use`), in file order, of the **trailing run**
   of assistant lines (the existing `claude_reply` rule: back to the last `user` line, tool
   results included), joined with `\n`; empty/whitespace -> `None`.
4. `TurnOutcome::Complete`. A transcript whose last line is a `user` line (the turn was
   interrupted before any assistant text) yields `None`; the reply is `none` and `wait` still
   reports `turn_finished`.
5. `stop_reason` is **not** required (Claude streams a block per line and the final line's
   `stop_reason` may be `null`). The engine already decided the turn finished.

**Codex (`reply_for_turn(Codex, ..)`).**
1. Find the last `event_msg` line with `payload.type == "task_started"` and `timestamp >=
   delivered_at - 2000 ms`. Else the last `user_message` `event_msg` at or after `delivered_at`.
   None -> `None`.
2. Look after it for the first `event_msg` with `payload.type` of:
   - `task_complete` -> `Complete`. Reply = `payload.last_agent_message` when present and
     non-empty (name per third-party parsers; confirm in Spike 5), else `codex_reply` over the
     lines between the turn start and this line (assistant `response_item` `output_text`
     blocks, else the last `agent_message`).
   - `turn_aborted` -> `Aborted`, reply text = whatever assistant text exists in the span,
     possibly empty (name unverified).
3. No terminal event yet -> `None` (the collector retries, below). A `task_complete` older than
   the turn start is never used.

**Output.** Text is passed through `truncate_reply` (32 KiB cap, `\n[truncated]` marker). Never
return text from before `delivered_at`.

### Timing inside the collector

The collector is the engine's (`03-turns-and-cli.md` T5 step 4, which owns the timing). On a
completion detected at `t0`, it is evaluated on every engine ingest (hot cadence, about 1 s):
- each ingest: the hook first (`session_replies.turn` past the value at detection, or
  `>= awaiting_turn` for a driver turn);
- from `t0 + 1 s` (`TRANSCRIPT_AFTER`), one transcript lookup is started on
  `spawn_blocking`, never on the status loop's tick; its result is used on the next ingest, and
  a hook reply seen on that same ingest still wins;
- at `t0 + 3 s` (`REPLY_WAIT`) with neither: `reply_source: "none"`.

### `reply_source` values

`hook` (a Stop hook row for that delivery), `transcript` (this phase), `none`. The set is used
in `drive_turns.reply_source` (CHECK constraint in `03-turns-and-cli.md` T2), the `wait`
`turn_finished`/`human_turn` JSON (T8), `read --last` (T10) and `keitora status`. The engine
does **not** write a transcript reply into `session_replies` (hooks own that table, D8 and T11):
a new `read --last` already returns it through `drive_turn_latest`, and writing it would bump
`session_replies.turn`, which the collector and the legacy `wait` path read as "a Stop hook
replied". Older CLIs on PATH keep seeing hook replies only.

### Diagnostics hooks

The collector reports `completion_without_stop` inputs (`07-self-diagnostics.md`): per
worktree, a ring of the last 10 completions with `{signal, reply_source}`, plus the reason a
transcript lookup failed (`not_found`, `not_this_turn`, `parse_failed`, `no_provider_support`).

## Fixtures (Spike 5 requirements)

Location: `crates/keitora-tasks/tests/fixtures/transcripts/{claude,codex}/`. Naming:
`<case>.captured.jsonl` (from Spike 5) or `<case>.synthetic.jsonl` (hand-written from the line
shapes in `stop_capture.rs` tests, used until the capture exists; the test name carries
`_synthetic`). Each fixture has a sibling `<case>.expect.json`: `{ "delivered_at": "...",
"reply": "..." | null, "outcome": "complete|aborted" }`.

Cases (both providers unless noted): plain turn; tool turn (tool_use/tool_result, reply is the
final message); denied permission; interrupt (Codex `turn_aborted`, Claude trailing `user`
line); `/clear` (new file; the old one must not be picked by fallback 4); compaction;
resumed thread (Codex: appended to an older dated file); two turns in one file (the reply is
the second's); torn last line; CRLF line endings; a line over 256 KiB (forces the re-read);
Claude assistant message with `thinking` + `text` blocks; Claude `isSidechain` lines (if they
occur inline); Codex rollout without `cwd` in `session_meta`.

Scrub rules (placeholders only, per `AGENTS.md`): paths `/Users/me/proj` and `C:/Users/x/proj`,
worktree names `alpha`/`bravo`, session ids fixed fakes `00000000-0000-4000-8000-00000000000N`,
prompts replaced by short neutral text (`reply with ok`), no tokens, emails or hostnames.
Spike 5 must also confirm: Claude's final-line `stop_reason`, the `timestamp` field on every
line, the Codex `task_complete`/`turn_aborted` payload names (and `last_agent_message`),
whether `session_meta.cwd` exists, and that `agent_session` for Claude carries a path.

## Files touched (as referenced)

- `crates/keitora-tasks/src/transcript.rs` (new), `crates/keitora-tasks/src/lib.rs` (export),
  `crates/keitora-cli/src/stop_capture.rs` (re-use; behaviour and tests unchanged)
- `src-tauri/src/transcript.rs` (new), `src-tauri/src/transfer.rs` (visibility only),
  `src-tauri/src/codex_env.rs` (read-only use)
- the engine's reply collector from `03-turns-and-cli.md`
- fixtures and tests as above

## Reports fixed

B (`read --last` empty).

## Regression tests

- **Pure (`keitora-tasks`):** one test per fixture case and provider against its `.expect.json`;
  "reply is never from before `delivered_at`"; torn/CRLF/oversize lines; the moved parsers keep
  every existing `stop_capture` test passing unchanged (run before and after the move).
- **Locator (`src-tauri`, temp dirs, no real home):** Claude steps 1-4 in order, containment
  rejection of a path outside the project dir, `agent-*.jsonl` skipped, fallback rejected when
  mtime is older than `delivered_at - 5 s`; Codex id search in today/yesterday/older dated dirs,
  cache invalidation, `cwd` mismatch rejected, id with path separators rejected.
- **Collector** (in `03-turns-and-cli.md` T12, `transcript_reply_used_after_1s_when_no_hook`): a
  fake `TranscriptSource` and fake clock: hook wins when it arrives before the transcript result
  is used; transcript used from `t0 + 1 s` otherwise; `none` at `t0 + 3 s`; `session_replies` is
  never written by the engine.
- **Repro (RED first, `00-overview.md` "Reproductions"):** `repro_b_codex_rollout_fixture_yields_the_reply`
  (`keitora-tasks`, pure): the captured Codex rollout fixture through the reply parser; written
  failing before the parsers move, green with them. The Stop-payload fixtures (null/string
  `last_assistant_message`) are owned by `05-hook-placement.md` section 9.
- **B:** a Codex delivery with no Stop hook and a captured rollout: `wait` returns
  `turn_finished` with `reply_source: "transcript"` and the text; `read --last` returns it.

## Dependencies

- `03-turns-and-cli.md`: completion detection, the reply collection order, `delivered_at`.
- `02-engine-core.md`: `AgentObs.agent_session` (the value) plus `agent_session_kind` and
  `agent_session_source` (02 S4), from which the engine builds
  `AgentSessionRef { source, kind, value }` for `ReplyQuery`; `View`/`StatusSession` keep only
  the value.
- Spike 5 (`spikes.md`) gates this phase's captured fixtures; the synthetic fixtures let the
  parsers and locators land first. If Claude's final line lacks a reliable `end_turn`, nothing
  changes because the transcript is never a completion signal.
- `07-self-diagnostics.md` records "completion without Stop" when this fallback is used.

## Singletonize decisions

- **R1. Parsers move to `keitora-tasks::transcript`;** `stop_capture.rs` consumes them. The app
  and the CLI share one implementation.
- **R2. No `--session-id` dependency.** Claude resolution: herdr path, herdr id, launch id
  (when Phase 4 adds one), newest-mtime fallback with a freshness bound. Codex: herdr id, herdr
  path, cwd-matched newest rollout.
- **R3. Turn selection by timestamp** (`delivered_at - 2 s` for clock/flush skew), not by "last
  line of the file", so a stale transcript can never be returned as this turn's reply.
- **R4. `reply_source` is `hook | transcript | none` everywhere:** the `drive_turns` CHECK
  (03 T2), the `wait` JSON (03 T8), `read --last` (03 T10) and `keitora status` (07). No other
  value exists.
- **R5. Timing.** Hook preferred; the transcript lookup starts at `t0 + 1 s` on a blocking task
  and its result is used on the next ingest; total budget stays 3 s; reads happen off the
  status-loop thread. 03 T5 step 4 is the normative text.
- **R6. `read --last` compatibility** through `drive_turns` (03 T10), not by writing
  `session_replies`; the engine never writes that table (03 T11, D8).
- **R7. agy transcript fallback is spike-gated (S5-agy, s27b).** Until the spike confirms an
  `agent_session` id to file mapping, agy has no transcript fallback: `reply_source: none` plus
  the `keitora read` hint. This is a documented limitation, not a permanent design.
