# 005 — Claude Code sub-view history loss

**Goal:** act on the spike's finding for "← back from a workflow view leaves the
input box at the top of an empty pane".

**Prerequisites:** 001.

## Implementation (branch on the finding)

- **Upstream (Claude Code emits `ESC[3J` / clears and repaints; ghostty erases
  history):**
  - no code change;
  - add a short note in `docs/build-plan.md`, replacing the stale "scrollback
    intact" claims with the actual model: herdr owns history, and an app's
    clear-scrollback erases it;
  - record a follow-up task with `./keitora task new` if a herdr-side option
    would help.
- **Keitora (a resize/repaint or frame coalescing during the switch drops
  content):** fix it, with a regression test in `frameCoalescer` / writer tests.

## Validation

Tests for any code change. For the docs-only path, `git diff` review.

**Commit:** `docs(terminal): document herdr-owned scrollback model`, or
`fix(terminal): …` if code changed.
