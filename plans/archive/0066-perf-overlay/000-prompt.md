# 0066 perf overlay — implementation entry point

Proposal (source of truth for every contract): `plans/0066-perf-overlay.md`.
Branch: `llm/andrew/keitora/0066-perf-overlay`. Implementation is **sequential**: each stage
builds, has its tests written and passing, and is committed before the next starts.

| # | Stage | Model |
|---|---|---|
| 001 | herdr `pane process-info` plumbing | sonnet |
| 002 | perf core: types, attribution, history, lease (pure) | opus |
| 003 | root cache, sampler, herdr source, commands + wiring | opus |
| 004 | frontend data layer: IPC, view-model, hook, format, shortcut | sonnet |
| 005 | overlay UI, rail button, App wiring, CSS, harness check | sonnet |

## Rules for every stage
- Follow `AGENTS.md`: no plan/task ids or "Stage N" in code or commit messages; placeholder
  names only (`proj`, `alpha`, `C:/Users/x/...`) in tests and fixtures.
- New Rust logic ships with `#[cfg(test)]` tests in the same commit; new TS logic with vitest.
- **Never contact a real herdr**: no herdr CLI, no gated tests (`KEITORA_HERDR_BIN`), no
  `yarn tauri dev`, no `yarn dev:isolated*`. Tests use `FakeHerdr` / pure functions.
- The machine is loaded: run only the targeted test filter for the module you touched (check the
  package name in `src-tauri/Cargo.toml`), plus `cargo check`. One full
  `cargo test --workspace` + `yarn test` happens in Testing.
- Commit message is given per stage; end it with the session's `Co-Authored-By` trailer.
