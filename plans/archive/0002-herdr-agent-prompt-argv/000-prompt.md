# 0002 — Dispatch prompts reach the agent through herdr: implementation entry point

Read the proposal first: `plans/0002-herdr-agent-prompt-argv.md` (goal, root cause, scope,
design). Also read `AGENTS.md` (no plan/task identifiers or "Stage N" in code or commit
messages; placeholder names only in fixtures, comments and commit text).

## Branch

`llm/anna/keitora/0002-herdr-agent-prompt-argv` (already checked out). One commit per stage.

## Stages

| # | File | Summary |
|---|------|---------|
| 1 | `001-agent-prompt-argv.md` | `agent_prompt_args` builder without `--`, its unit test, the `agent_prompt` doc comment, and the dash-leading probe removed from the gated live test. Rust only. |

## Sequencing

1. One stage; sequential.

## Test-rigor expectations

1. `agent_prompt_args` ships with its `#[cfg(test)]` test in the same commit.
2. Tests never spawn or touch a real herdr; run no gated (`#[ignore]`) test and do not run the app.
3. Before committing: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
   `git log -1 --format=%B | grep -E '#[0-9]'` are both empty.
