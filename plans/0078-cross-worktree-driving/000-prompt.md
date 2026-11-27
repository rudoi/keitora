# 0078 cross-worktree driving: implementation entry point

Read `00-overview.md` first, then the subsystem document each stage names. Also read
`AGENTS.md` and `loss-report.md`. Where `00-overview.md` and a subsystem document differ, the
owning subsystem document wins (see "Cross-doc reconciliation" in the overview).

Docs (all in `plans/0078-cross-worktree-driving/`):

| Doc | Subsystem |
|---|---|
| `00-overview.md` | goal, scope, paradigm, decisions, risks, glossary |
| `01-task-store.md` | `crates/keitora-tasks` schema, API, state machines, retention |
| `02-cli.md` | `crates/keitora-cli` driver verbs, exit codes, identity |
| `03-hook-broker.md` | `keitora session gate`, Stop capture, hook installers |
| `04-app-runner.md` | `src-tauri` request runner, Tauri commands, window events, activity |
| `05-ui.md` | attention model, rail, banner, menu |

Branch: `llm/andrew/keitora/0078-cross-worktree-driving` (already checked out; do not create
another).

## Execution model

Implementation is **sequential**: do the stages in order, one commit each. Do not start a
stage until the previous one is committed and its validation is green. Later stages use
symbols the earlier stages introduce. Each stage is sized for one sub-agent working in one
pass over roughly one subsystem. Each stage doc summarises the work and points at the exact
subsystem-doc sections that hold the full contract; read those sections before coding.

## Stages

| # | File | Goal |
|---|---|---|
| 1 | `001-task-store.md` | keitora-tasks: drive DDL, model types, `LocalStore` drive API, state machines, retention, missing-table tolerance |
| 2 | `002-hook-gate.md` | Pure gate and Stop-capture logic in keitora-cli, plus `keitora session gate` and `session idle --stdin/--failed` wiring |
| 3 | `003-cli-driver-verbs.md` | keitora-cli driver verbs: drive, release, drives, send, wait, read, prompts, answer, reset, swap |
| 4 | `004-hook-installers.md` | `hooks.rs` and `codex_hooks.rs` write the gate, Stop and failure hook entries |
| 5 | `005-app-runner.md` | `src-tauri/src/drive.rs` runner, Tauri commands, `session_revision`, session rows, activity fields |
| 6 | `006-frontend-state-and-rail.md` | ipc types, `drive.ts`, attention, laneState, `bg_notifier.rs` port, rail badges, chips, menu |
| 7 | `007-banner-and-window-events.md` | `DriveBanner`, `drive://restart` and `drive://swap` listeners, transfer drive mode and report effect |
| 8 | `008-docs.md` | README driving section, CLI help text, final full-workspace verification |

## Test rigor

- Every stage that adds Rust logic ships `#[cfg(test)]` tests **in the same commit**. Every
  stage that adds frontend logic ships vitest tests in the same commit. Untested new logic is
  not done.
- Per stage, run `cargo test -p <crate>` for each touched crate (`keitora-tasks`,
  `keitora-cli`, and the app package `keitora` in `src-tauri`), plus `cargo check` for the
  same. For stages touching the frontend also run `yarn test` and `npx tsc --noEmit`. Stages
  touching both sides run all of them.
- After stage 8, run `cargo test --workspace`, `yarn test`, `yarn lint` and `npx tsc --noEmit`;
  all must be green.
- **No test may** contact a running herdr daemon, touch the real Keitora home or the real
  home directory, or spawn a real agent. Use temp dirs, temp-file SQLite, and fakes
  (`GateStore`, `Clock`, injected paths and homes). Never read `dirs::home_dir()` in a tested
  path.
- **Never** run gated herdr tests (anything requiring `KEITORA_HERDR_BIN`), `yarn dev:isolated`,
  or `yarn dev:herdr`. Avoid heavy runs beyond the commands above.
- Spikes named in the overview and `03-hook-broker.md` are manual verification owed after
  implementation; they do not gate any commit.

## Rules from AGENTS.md (repeated)

- No plan or task identifiers in source code or commit messages: no `#0078`, no `Stage N`, no
  `plans/0078-...` paths in comments. Comments explain what and why on their own terms. The
  plan docs say "01", "03" and so on; never carry those labels into code.
- Placeholder names only: driver `alpha`, driven `bravo`/`charlie`, project `my-game` or
  `proj`, users `me`/`x`, hosts `example.com`. No real project, person, company or machine
  names in fixtures, comments or commit text.
- Commits: plain Conventional Commits `type(scope): description`, no trailing task tag. End
  each message with the session's configured trailer:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
  `git log -1 --format=%B | grep -E '#[0-9]'` must both be empty.
- Do not push. Do not `git add` anything under `.harness/` (local-only mocks).
