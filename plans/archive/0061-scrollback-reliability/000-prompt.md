# 0061 — Terminal scrollback reliability: implementation entry point

- **Proposal:** [proposal.md](proposal.md) (design, verified herdr protocol, risks)
- **Spike findings:** [spike-findings.md](spike-findings.md) (source-only research)
- **Branch:** `llm/andrew/keitora/0061-scrollback-reliability`
- **Order:** sequential. Each stage builds, tests and commits before the next.

## Stages

| File | Stage | Depends on |
|---|---|---|
| [001-spike.md](001-spike.md) | Source-only spike: lines/MB, `ESC[3J`, Claude Code / Codex modes | — |
| [002-backend-scroll.md](002-backend-scroll.md) | `terminal.scroll` through the writer queue + `pty_scroll` | — |
| [003-frontend-wheel.md](003-frontend-wheel.md) | Wheel/keyboard → `ptyScroll`; shrink xterm history; diagnostics | 002 |
| [004-history-depth.md](004-history-depth.md) | New default herdr scrollback limit | 001 |
| [005-subview-history.md](005-subview-history.md) | Claude Code sub-view history loss: fix ours / record upstream | 001 |
| [006-live-pill.md](006-live-pill.md) | "↓ live" indicator from `PaneInfo.scroll` | 002, 003 |

## Hard constraints (all stages)

- **Never touch the user's running herdr daemon.** No `herdr` CLI calls, no app
  launch (`yarn tauri dev`, `dev:herdr`, `dev:isolated*`), no gated herdr tests
  (`KEITORA_HERDR_BIN`). All herdr behaviour is exercised through `FakeHerdr` /
  mocked `HerdrOps`.
- Machine is loaded: prefer `cargo check -p keitora` while iterating. Run the
  full non-gated `cargo test --workspace` once per stage that changes Rust.
- No plan/task identifiers in code or commit messages (`AGENTS.md`). Placeholder
  names only in fixtures.

## Test rigor

New Rust logic ships with `#[cfg(test)]` coverage in the same commit. New
frontend logic in `src/lib` ships with vitest coverage. `cargo test --workspace`,
`yarn test` and `yarn tsc --noEmit` stay green. Live smoke tests (real wheel in
real panes) are owed to the user and are listed in the PR test plan.
