# 0075 — implementation entry point

Proposal: `plans/0075-update-line-pane-shell/proposal.md` (read it first; it is the design).
Branch: `llm/<user>/keitora/0075-update-line-pane-shell`.

Stages (sequential — each builds and tests before the next):

1. `001-model-and-backend.md` — shell model + rendering in `agent_updates::model`, and the
   `pane_shell` / `run_line` backend methods (herdr + fakes).
2. `002-start-run-and-dialog.md` — `AgentUpdates::start_run`, `agent_update_run` wiring,
   wire-field split, frontend dialog/TerminalPane changes, doc comments.

Test rigor: every new function ships with `#[cfg(test)]` coverage in the same commit
(AGENTS.md). Never touch the running herdr: tests use `FakeBackend` / the herdr fake ops
only; no gated real-herdr tests, no `yarn dev:*`. Commit messages are plain Conventional
Commits with no task/plan identifiers, ending with the Co-Authored-By trailer.
