# 0067 — implementation entry point

Proposal: `plans/0067-ui-fixes-and-suggest-removal.md` (read it first; it holds the verified
file:line references). Branch: `llm/andrew/keitora/0067-ui-fixes-and-suggest-removal`.

Stages run **sequentially**; each is validated before the next starts.

1. `001-rail-label-and-z-layers.md` — lowercase `settings`; shared z-layer scale, quit topmost.
2. `002-remove-dispatch-suggestions.md` — delete the suggest feature, keep manual dispatch.
3. `003-history-read-backend.md` — `pane_read` lines arg, `PtyBackend::read_history`,
   `pty_read_history` command + IPC wrapper, Rust tests via `FakeHerdr`.
4. `004-history-search-frontend.md` — `historySearch.ts` (pure, vitest-covered), rewritten
   `TerminalSearch`, Ctrl/Cmd+F native-find suppression.

## Test rigor
- New Rust logic ships with `#[cfg(test)]` coverage; new TS logic ships with vitest tests.
- Every stage: `cargo test --workspace` (when Rust changed), `yarn test`, `npx tsc --noEmit`,
  `yarn lint` must be green.
- **Never touch the running herdr**: no gated herdr tests, no `dev:herdr`/`dev:isolated`.
  Rust tests use `FakeHerdr` only.
- Commit messages: plain Conventional Commits, no task numbers / stage references.
