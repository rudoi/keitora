# 0080 spike results

Run on Windows (herdr 0.9.5, Claude Code, Codex CLI, Antigravity CLI 1.5.18), 2026-11-27.

The spikes ran through a scripted driver, not the dev GUI. The driver:
- started its **own** headless herdr server, using the launcher's `herdrDevEnv` (every inherited
  `HERDR_*` stripped);
- created its own workspaces and panes;
- drove agents only through that server's CLI.

Before every herdr command it re-checked that the server's socket was inside the spike home and
the pid was the one it started. Raw captures stay under the spike home and are never committed.
Paths below are placeholders.

## S0 isolation gate: **pass** (four sessions)

- Pre-check (`isolationVerdict` with the stripped env): ok, with no server reachable.
- Stripped inherited names (names only): `HERDR_BIN_PATH`, `HERDR_ENV`, `HERDR_PANE_ID`,
  `HERDR_SOCKET_PATH`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`.
- **`herdr status server --json` has no pid field.** On Windows the socket is a pointer file
  holding `<pid>:<nonce>`. The new server's pid (read from the pointer file, only when inside
  the spike home) was a `herdr.exe` that was not in the baseline.
- Teardown stopped only the isolated server (`mayStopOnTeardown`). In every run all baseline
  real-herdr pids stayed alive and the real socket's mtime was unchanged.
- A first gate attempt aborted on the missing pid field. The abort path stopped only the
  isolated server, so that is safe.

## S1 `agent list` semantics (claude, codex, agy)

| Observation | claude | codex | agy |
|---|---|---|---|
| `interactive_ready` after start | true | true | true |
| `agent_session` (kind / source) | id / `herdr:claude` | id / `herdr:codex` | id / `herdr:antigravity_cli` |
| prompt → `working` seen | ~0.38 s | ~0.38–0.7 s | ~0.37–1.2 s |
| status after a finished turn | `idle` (or `done`) | `done` | `done` |
| `completion_seq` after a turn | = `state_change_seq` at that idle | same | same |
| after `/clear` | `completion_seq` null | changes | unchanged |
| two prompts back to back | the second `agent prompt` accepted (code 0); final `completion_seq` set | final idle, `completion_seq` **null** | final idle, `completion_seq` **null** |
| `blocked` observed | no (no permission dialog appeared) | no | no |

**`completion_seq` classification: stamp, not a counter.**
- It is the server-wide `state_change_seq` value at which the agent's latest completed-work idle
  transition happened. The sequence is shared across agents: the numbers continue from one
  agent to the next.
- It can be **null** while the agent is idle: after `/clear` (claude), and after back-to-back
  prompts (codex, agy).
- `state_change_seq` rises by 2 per prompt→working→idle cycle.

**Codex without `--dangerously-bypass-hook-trust`:** the turn worked, but `agent_session` never
appeared. herdr's own Codex session hook did not run.

**agy background work:** after `start a background task …`, the status went
`working → done → working` while the background task ran. herdr reports a finished turn before
the background work ends.

**Approvals:** no provider showed a permission dialog in this setup, including file writes and a
shell `curl`. So `blocked` detection by herdr is **unverified** here.

## S3 Claude `--settings` hooks: **works**

- Claude started with `--settings <file>`. Hooks from that file fired for `SessionStart`,
  `UserPromptSubmit` and `Stop`.
- A project `.claude/settings.local.json` `Stop` hook **also** fired, so the two merge rather
  than replace each other. The order seen was project `Stop`, then launch `Stop`.
- Claude's first launch in a new folder shows a trust dialog whose default is "No, exit".
  When the folder's project settings contain hooks, the dialog warns about them. `agent start`
  fails with `agent_not_ready` while that dialog is up, and herdr does not register the agent.
  Keitora-launched worktrees are normally already trusted. A fresh worktree is not.

## S3b agy hooks in a scratch config dir: **not run**

A scratch `ANTIGRAVITY_CLI_CONFIG_DIR` would have no agy login.

## S5 transcript capture: **all three map by herdr's session id**

- **claude:** `<claude projects>/<slug>/<session-id>.jsonl`. Found by id under the spike's
  project slug only.
- **codex:** `<codex sessions>/YYYY/MM/DD/rollout-…-<session-id>.jsonl`. Found by filename.
- **agy:** the files named by the session id are:
  - `<agy config>/antigravity-cli/conversations/<session-id>.db` (a per-session SQLite DB);
  - `brain/<session-id>` (a directory);
  - `annotations/<session-id>.pbtxt`;
  - `presence/<session-id>.lock`.

  Found by name match only; no other file was opened.
- Raw copies are under `<spike home>/spike5/raw*` and are not committed. s27 and s27b scrub them
  into fixtures.

## S2 push: **not run** (not blocking; polling stays final, D2)

## Decisions taken

1. **S0 passed.** Live verification of the build is possible later through the same driver.
2. **Turn-finished predicate (s12):**
   - Keep the disjunction: settled `idle` or `done`, plus any of:
     - `completion_seq` present and different from the baseline;
     - `state_change_seq` advanced by at least 2 since delivery;
     - `working` seen after delivery.
   - `completion_seq` **must** be treated as a nullable stamp. Never compare it as a per-agent
     counter or require it to rise.
   - Null after back-to-back prompts and `/clear` means the `state_change_seq` and
     working-seen clauses are required, not optional.
3. **Settled idle** treats `idle` and `done` alike. Claude often reports `idle` straight away.
4. **Stall window:** `working` appears within about 1.2 s of delivery for all three providers.
   The "no activity since delivery" stall can use the planned grace window, and 1 s polling is
   enough.
5. **Codex trust bypass (s21):** required for herdr's session id. Pass it on every launch step,
   which confirms the priority.
6. **Claude hooks (s23): branch A**, launch `--settings` file plus the double-fire guard.
   Add a finding when a worktree's Claude trust dialog blocks a start (`agent_not_ready`).
7. **agy reader (s27b): build it.** Map via `agent_session` to
   `conversations/<session-id>.db`. The SQLite schema is to be inspected from the raw capture in
   s27b. Ambiguous parsing falls back to `reply_source: none`.
8. **agy background work:** document that `turn_finished` can arrive while agy background tasks
   still run. The engine does not try to wait them out.
9. **`blocked` is unverified:** keep the conservative branch. A herdr `blocked` is a hint only.
   Brokered and escalated prompts come from gate rows.
