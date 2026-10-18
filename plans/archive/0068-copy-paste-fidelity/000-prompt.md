# 0068 — Terminal copy/paste fidelity: implementation entry point

Read the proposal first: `plans/0068-copy-paste-fidelity.md` (goal, root causes, scope,
decisions). This directory holds the ordered implementation stages. Each stage file has
Goal, Prerequisites, Implementation, Validation and the Commit message.

Also read `AGENTS.md` (no plan/task identifiers or "Stage N" in code or commit messages;
placeholder names only in fixtures, comments and commit text).

## Branch

`llm/andrew/keitora/0068-copy-paste-fidelity` (already checked out). One commit per stage.

## Stages (run in this order)

| # | File | Summary |
|---|------|---------|
| 1 | `001-backend-paste.md` | `PtyBackend::paste`, `pty_paste` command, `WriteJob::Paste`/`Run::Paste` (never coalesced), backend-owned bracketing + marker stripping, size caps, CLI-fallback single `send-text`, per-session paste-refused event. Rust only. |
| 2 | `002-frontend-paste.md` | `ptyPaste` IPC + refusal event wrapper, `sendPaste` in `TerminalPane.tsx` replacing every `term.paste`, native paste intercepted on all platforms, refusal toasts, stale-comment fixes. |
| 3 | `003-copy-selection.md` | Pure `selectionToText` (`src/lib/selectionText.ts`) and its wiring into Ctrl+C and a capture-phase `copy` listener, Alt+drag fallback. |

## Sequencing

- Stage 2 depends on stage 1 (it calls `pty_paste` and listens for the event stage 1
  emits). Run it after stage 1.
- Stage 3 is logically independent of stages 1 and 2 (no shared types or IPC) and could
  be done first or in parallel by another agent. BUT stages 2 and 3 both edit
  `src/components/TerminalPane.tsx` (different regions: paste handlers vs. the Ctrl+C
  branch and a new `copy` listener). Run them sequentially (2 then 3) in this branch to
  avoid conflicts; if run in parallel, expect a trivial textual merge.
- Do not start a stage until the previous stage's Validation is green and committed.

## Test-rigor expectations

- Every new Rust logic unit ships with `#[cfg(test)]` coverage **in the same commit**
  (pure helpers, writer behaviour, backend method, command mapping). Untested new logic
  is not done.
- Frontend pure helpers get vitest tests in the same commit. vitest runs in the plain
  node environment (no jsdom), so keep logic in pure functions over small fakes (see
  `src/lib/bufferText.test.ts`'s `fakeBuffer`); the DOM wiring in `TerminalPane.tsx`
  is covered by the owed manual smoke checks, not unit tests.
- Tests must NEVER spawn or touch a real herdr. Use the existing doubles: the writer's
  scripted `SessionIo` test doubles (`Script`, `Hold`) in `backend/herdr/writer.rs`,
  `fake::FakeHerdr` + `typing_backend` / `fake_backend` helpers in `backend/herdr/mod.rs`
  tests, `FakeBackend` in `backend/fake.rs`, and the `CaptureSink` `EmitSink` for events.
- Do not run the app, `yarn dev:*`, or any gated herdr test. Run only the targeted
  commands listed in each stage's Validation (the crate is `keitora`; lib target
  `keitora_lib`). Finish each stage with the repo-wide checks it names.
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
  `git log -1 --format=%B | grep -E '#[0-9]'` must both be empty for source and commits.
  Commits end with the configured `Co-Authored-By:` trailer.

## Smoke checks owed after all stages (real app, by the user)

Multi-line paste into Claude Code and into a plain shell; image paste and file drop still
attach; macOS/Linux native paste and middle-click; copy of wrapped prose, separator lines
and a blank-row-heavy block; Ctrl+C with no selection still interrupts; the doubled
blank-line symptom (see proposal, Copy section).
