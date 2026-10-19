# 001 — `herdr agent prompt` argv without `--`

## Goal

1. `RealHerdr::agent_prompt` runs `herdr agent prompt <pane_id> <text>`.

## Prerequisites

1. Proposal `plans/0002-herdr-agent-prompt-argv.md` read.

## Implementation

1. `src-tauri/src/backend/herdr/real.rs`: add
   `fn agent_prompt_args<'a>(pane_id: &'a str, text: &'a str) -> [&'a str; 4]` returning
   `["agent", "prompt", pane_id, text]`, placed beside `pane_process_info_args`, with a doc
   comment stating that herdr reads the text as the second positional argument and has no
   `--` separator for `agent prompt`.
2. `RealHerdr::agent_prompt` calls `self.run(CallClass::Quick, &agent_prompt_args(pane_id, text))`.
3. The `agent_prompt` doc comment reads `herdr agent prompt <pane_id> <text>`.
4. Test in `real.rs` `mod tests`, beside `pane_process_info_passes_the_pane_flag`:
   `agent_prompt_args("w1:p2", "go")` equals `["agent", "prompt", "w1:p2", "go"]`;
   `agent_prompt_args("w1:p2", "-x")` equals `["agent", "prompt", "w1:p2", "-x"]`.
5. `src-tauri/src/backend/herdr/mod.rs`, `spawn_agent_launches_lists_and_prompts_gated`:
   remove the `probe` call to `write_line` with the `-`-leading text, the `dash_probe={probe:?}`
   field of the `eprintln!`, the inline comment above the probe, and the `-`-leading prompt
   clause of the doc comment's `write_line` bullet. The empty-text `write_line` call and its
   assertion stay.

## Validation

1. `cargo test -p keitora --lib backend::herdr::real` passes, including the new test.
2. `cargo test --workspace` and `yarn test` pass.
3. `cargo clippy -p keitora --all-targets` reports no new warning in the changed lines.
4. `git grep -n '"--", text' -- src-tauri` is empty.

## Commit message

`fix(herdr): send dispatch prompts as herdr's text argument`
