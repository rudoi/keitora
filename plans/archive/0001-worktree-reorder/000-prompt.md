# Worktree reorder — implementation entry point

Proposal: `plans/0001-worktree-reorder.md`. Branch: `llm/anna/keitora/0001-worktree-reorder`.

Stages, sequential; each stage's tests pass before the next starts:

1. `001-task-store.md` — `reorder_entries`, order-keeping `set_project_worktrees`.
2. `002-host.md` — `reorder_worktrees` command and its registration.
3. `003-frontend.md` — ipc, pure order helpers, lane drag in the rail, CSS.

Test rigor: every new Rust function has `#[cfg(test)]` coverage in the same commit; every new pure
TypeScript function has a `*.test.ts` case per specified input. `cargo test --workspace` and
`yarn test` pass after each stage. Code and commit messages carry no plan or task identifiers.
