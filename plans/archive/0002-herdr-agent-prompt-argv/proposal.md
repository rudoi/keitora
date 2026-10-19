# 0002 — Dispatch prompts reach the agent through herdr

## Goal

1. A board dispatch into an idle worktree with a live agent submits every dispatch step to
   that agent, and `dispatch_log` records `dispatched` with an empty rationale.
2. The Codex-to-Claude handoff prompt (`inject_handoff_summary` in `src-tauri/src/transfer.rs`) reaches
   the agent: it calls the same `write_line`.

## Symptom

1. Every dispatch into a herdr-backed session reserves the task and sends nothing to the agent.
2. `dispatch_log` records `queued` with the rationale
   `herdr exited with exit code: 2: unknown option: <first dispatch step>`.
3. The board shows the task reserved to the worktree; the session shows no input.

## Root cause

1. `RealHerdr::agent_prompt` (`src-tauri/src/backend/herdr/real.rs`) runs
   `herdr agent prompt <pane_id> -- <text>`.
2. herdr's `agent_prompt` (`src/cli/agent.rs`, identical in v0.9.3 and v0.9.5) takes
   `args[0]` as the target and `args[1]` as the text, unchecked, then parses every later
   argument as `--wait` / `--until` / `--timeout`, and rejects any other with
   `unknown option: <arg>` and exit code 2.
3. herdr therefore takes `--` as the prompt text and rejects the real text as an unknown
   option. `--` is a separator only for `herdr agent start` (`position(|arg| arg == "--")`).
4. `dispatch_apply` (`src-tauri/src/dispatch.rs`) maps the failed first step to
   `WriteLineOutcome::Error` with nothing sent, and records `queued`.
5. The comment in the gated live test in `src-tauri/src/backend/herdr/mod.rs`
   ("herdr 0.9.3 rejects a leading-dash TEXT even after `--`") describes the same misread:
   herdr rejected the text because it followed `--`, not because it begins with a dash.

## Scope

### In

1. `agent_prompt` runs `herdr agent prompt <pane_id> <text>`.
2. A pure argv builder `agent_prompt_args(pane_id, text)` in `real.rs`, matching
   `pane_process_info_args`, with a unit test asserting the exact four-element argv for plain
   text and for dash-leading text, matching `pane_process_info_passes_the_pane_flag`.
3. The `agent_prompt` doc comment states the corrected argv.
4. The gated live test `spawn_agent_launches_lists_and_prompts_gated` in `mod.rs` keeps its
   empty-text submit-only check and removes the dash-leading probe, its `dash_probe` output,
   and the doc-comment and inline-comment text describing the probe: after the fix that probe
   submits a real prompt to a live agent.
5. The gated live test is renamed `spawn_agent_launches_and_lists_gated` (code review).
6. A new gated live test `agent_prompt_argv_is_accepted_by_herdr_gated` runs `agent_prompt`
   with dash-leading text against a plain shell pane and asserts herdr answers with
   `agent_not_found` or `agent_not_ready`, not a usage exit (code review).
7. `RealHerdr::run` reads a `{"error":{…}}` body from stderr when the call exits non-zero
   with no JSON on stdout: herdr 0.9.5 reports `agent prompt` errors there with exit code 1,
   so `agent_blocked` / `agent_not_ready` reached `agent_prompt_error_from` as an `Exit`
   error with no code and the line was never queued (found by the gated live test,
   2026-10-19). A pure `json_result` holds the decision, with unit tests.

### Out

1. Showing the `reserved — could not inject` reason on the board.

## Technical design

1. `agent_prompt_args(pane_id: &str, text: &str) -> [&str; 4]` returns
   `["agent", "prompt", pane_id, text]`; `agent_prompt` passes it to `self.run`.
2. Dash-leading text needs no escaping: herdr reads `args[1]` positionally before any option
   parsing.
3. `agent_start` keeps its `--` separator; herdr's `agent_start` parser splits on `--`.
4. `json_result(output)` returns the stdout JSON value, or `Api` for an `error` member on
   stdout; `Null` for a success with no stdout; `Api` for an `error` member on the stderr of a
   failed call; otherwise `Exit`.

## Verification

1. `cargo test --workspace` and `yarn test` pass, including the new argv test.
2. Rebuild and reinstall the app; dispatch a reserved task into an idle worktree with a live
   Claude session.
3. `dispatch_log` has a new `dispatched` row with an empty rationale, and the session's pane
   shows each dispatch step submitted.
4. The earlier `queued` rows do not block the re-dispatch: `dispatch_reserve` returns
   `AlreadyDispatched` only after a `dispatched` row.

## Complexity

1. Small: three files in one subsystem. Fracture, Singletonize and Loss check are skipped.
