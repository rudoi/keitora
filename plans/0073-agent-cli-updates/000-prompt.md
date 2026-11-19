# 0073 — Agent CLI updates: implementation entry point

The proposal at `plans/0073-agent-cli-updates/proposal.md` is the source of truth for the design.
Read it in full before starting any stage, especially **Technical design** and
**Implementation notes (singletonized)**.

Branch: `llm/andrew/keitora/0073-agent-cli-updates`.

Stages run **in order**. Each one must compile, pass its tests and be committed before the
next one starts.

| # | File | Summary |
|---|---|---|
| 1 | `001-reserved-session-key.md` | Reserved `SessionKey`, kept out of the KeyMap listings and the live set |
| 2 | `002-agent-start-outcome.md` | `AgentStart { Ready, NotReady }` passed through HerdrOps (pure refactor) |
| 3 | `003-codex-update-flag.md` | `-c check_for_update_on_startup=false` prefix and the update-screen matcher |
| 4 | `004-update-prompt-detection.md` | `AgentUpdatePrompt` error, checks on the default and strict paths, prompt event |
| 5 | `005-settings.md` | Agent fields in `[updates]`, host settings command, onboarding guard |
| 6 | `006-agent-updates-core.md` | `agent_updates/` module: versions, install kind, assessment, scheduler, run and commands |
| 7 | `007-frontend.md` | IPC, model, provider, banner, dialog, TerminalPane, settings, harness |

## Test rigor
- New Rust logic ships with `#[cfg(test)]` tests in the same commit (see AGENTS.md).
- Pure functions get table tests. These are `install_kind`, `update_command`, `assess`,
  the version parsers, `update_prompt_cli`, `is_reserved`, sentinel parsing and failure
  hints.
- Backend behaviour is tested through scripted `pane_read` / `agent_start` results on
  `FakeHerdr`.
- New frontend logic (`src/lib/agentUpdates.ts`, the new `spawnFailedCopy` case) gets
  vitest tests.
- Each stage must pass the app crate's tests (`cargo test --manifest-path src-tauri/Cargo.toml`).
  From stage 7, `yarn test`, `npx tsc --noEmit` and `yarn build` must pass too.
- Tests must **never** contact the real herdr daemon or the network. Use fakes and
  injected fetchers or probers.
- Do **not** run `yarn dev:isolated*`, gated herdr tests, `yarn tauri dev`, or the real
  `codex` / `claude` CLIs from tests.

## Commit rules
- Use plain Conventional Commits, with no task numbers and no stage references (see
  AGENTS.md).
- End every message with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- The pre-commit hook runs rustfmt, biome and cargo check. Fix whatever it reports and
  never skip it.
