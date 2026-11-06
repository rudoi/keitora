# 0069 — Harden Claude to Codex transfer: implementation entry point

Read this file first, then the stage file you were assigned. Stage files are `010-…md` to `110-…md` in this
directory. Implement exactly one stage per run, commit it, stop.

## Source documents

- Proposal: `plans/0069-codex-transfer-hardening.md`. Where it conflicts with the five docs below, the docs win
  (for example JSONL selection is "newest non-agent file by mtime", persisted session id only logged).
- `00-contracts.md` — shared types, IPC signatures, `transfer://progress`, `TransferRegistry`, launch signals, trail.
- `01-import.md` — outcome/cancel, protocol-driven import, `AppServer`, ledger, JSONL selection.
- `02-launch-and-env.md` — persistence intents, `LaunchPolicy`, strict launch, `CodexEnv`.
- `03-frontend.md` — phase machine, overlay, outcomes, vitest.
- `04-scoping-locking-diagnostics.md` — by-name project lookup, registry, gate narrowing, diagnostics.
- `AGENTS.md` — commit, identifier and placeholder-name rules.

Each doc has an "Implementation order within this doc" section. Stages honour them; a stage file names the
steps it covers. When a stage file and a doc disagree on a detail, the doc wins; on ordering, the stage file wins.

## Branch

`llm/andrew/keitora/0069-codex-transfer-hardening` (already checked out). One commit per stage, in order.

## Stages

| # | File | Summary | Routing |
|---|---|---|---|
| 010 | `010-foundations-storage-and-codex-env.md` | keitora-tasks: by-name lookup, field-intent writer, resume replace, Codex discovery; `codex_env.rs` | scaffold (sonnet) |
| 020 | `020-contracts-and-registry.md` | Outcome/progress types, `TransferRegistry` + trail + diagnostics, managed state, three new command stubs | complex (opus) |
| 030 | `030-import-transport.md` | Framing, stderr ring, generic `AppServer`, in-process fake app-server | complex (opus) |
| 040 | `040-import-completion.md` | Spawn/teardown, ledger, JSONL selection, completion judgement, `import_over`, `run_claude_import` | complex (opus) |
| 050 | `050-launch-foundations.md` | `Default` regression tests first, `LaunchPolicy`, args/env variants, fake herdr extension | scaffold (sonnet) |
| 060 | `060-strict-launch.md` | Strict chain and verify, resume-store replace/clear, trail buffer, `spawn_pty(transfer)` | complex (opus) |
| 070 | `070-backend-wiring.md` | `transfer_worktree` outcome, project threading, gate narrowing, `dispatch_apply` refusal, progress | complex (opus) |
| 080 | `080-frontend-core.md` | `ipc.ts` types/wrappers, `resumeNotice`, `laneState`, `transferPhase` + vitest | complex (opus) |
| 090 | `090-frontend-wiring.md` | `Toast`, `TransferOverlay`, `TerminalPane`, `Rail`, `App`, harness mocks | complex (opus) |
| 100 | `100-stale-exit.md` | Stale-exit repro tests, backend check, conditional exit generation | scaffold (sonnet), complex only if `gen` is needed |
| 110 | `110-testing.md` | Full workspace run, lint/build, grep gates, review-gate checklist | scaffold (sonnet) |

## Parallelism

Default is strictly sequential in the order above. Two safe exceptions (disjoint files, separate worktrees,
merged in numeric order before 070 and 090 respectively):

- Group A (`030`, `040`) and Group B (`050`, `060`) touch disjoint files (`src-tauri/src/transfer.rs` and
  `transfer/` versus `backend/herdr/*`, `agent.rs`, `env.rs`, `host.rs`). Both need `010` and `020` first. `070` needs both.
- `080` touches only files under `src/`; it needs only the contracts in `00-contracts.md`
  (and `020` for the Rust shapes it mirrors), so it can run alongside `030` to `070`. `090` needs `080` and `070`.

Every other pair conflicts on `transfer.rs`, `host.rs`, `lib.rs` or `App.tsx`: do not parallelise.

## Test rigor

- Every stage that adds Rust logic ships `#[cfg(test)]` tests in the same commit. Frontend logic ships vitest tests
  in the same commit. Untested new logic is not done.
- Per-stage validation is only: `cargo check -p <crate>` plus the targeted `cargo test -p <crate> <filter>` for
  that stage's tests (crates: `keitora-tasks`, and `keitora` for `src-tauri`; use `--lib` for the latter).
  Frontend stages add `yarn test` and `npx tsc --noEmit`.
- Do NOT run `cargo test --workspace` per stage. It runs once, in stage 110, because the dev machine is under load.
- Test time-based logic with the `*_at` twins and a fixed base `Instant`; never subtract from an `Instant`.

## Hard rules (restated; violating any is a failed stage)

1. Never run the app (`yarn dev*`, `tauri dev`, any built binary).
2. Never run herdr or any gated herdr test (anything needing `KEITORA_HERDR_BIN`). Herdr behaviour is tested only
   through `backend/herdr/fake.rs` or pure functions. Tests never touch the real `$HOME`, `~/.codex`, `~/.claude`
   or the user config; use temp dirs and `KEITORA_CONFIG_HOME`.
3. Never kill processes by image name (no `taskkill /IM`, `pkill codex`, `killall`). Kill by PID only, and only
   processes a test itself spawned.
4. No plan or task identifiers in code, comments, test names or commit messages: no `#NNNN`, no "Stage N", no
   `plans/…` paths, no `0069`. Explain what and why on its own terms.
5. Placeholder names only in fixtures, paths, docs and commit text: `proj-a`, `proj-b`, `other-proj`,
   `alpha`/`bravo`/`wt`, `thread-a`, `/Users/me`, `/home/u`, `C:/Users/x`. No real project, game, repo, worktree,
   person or machine names.
6. Commit messages are plain Conventional Commits (`type(scope): description`) and end with the session's
   `Co-Authored-By:` trailer. Before committing run
   `git grep -nE '#[0-9]{3,4}' -- <changed files>` and `git log -1 --format=%B | grep -E '#[0-9]'`; both must be empty.
7. Do not edit anything the stage file lists under "Must not touch". Doc-only plan files are not edited by stages.

## Stage file format

Goal, Prerequisites, Docs/sections to implement, Files touched, Implementation, Tests to add, Validation, Commit message.
Each stage must leave the tree compiling with its own tests green.
