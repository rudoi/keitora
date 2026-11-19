# 0074 — implementation entry point

Proposal: `proposal.md` (read it fully, including "Refinement").
Branch: `llm/andrew/keitora/0074-terminal-link-reach`.

Stages (sequential; each is tested before the next):
1. `001-backend-outside-files.md` — drop worktree containment in `src-tauri/src/terminal_links.rs`.
2. `002-frontend-logical-lines.md` — one link provider joining herdr-repainted rows.
3. `003-docs.md` — build-plan entry.

Test rigor: Rust changes ship with `#[cfg(test)]` coverage; TS logic with vitest cases.
Use only `cargo test -p keitora` scoped to `terminal_links` and `yarn test` for the touched
files while iterating (the user's machine is loaded — no herdr contact, no app runs).
No plan/task ids in code or commit messages.
