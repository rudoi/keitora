# 0072 — implementation entry point

Proposal: `proposal.md` (read it first — it is the design).
Branch: `llm/andrew/keitora/0072-rail-project-mismatch`.

Stages run **sequentially**; each is validated (its tests run green) and committed before
the next.

| # | File | Area |
|---|---|---|
| 001 | `001-registry-write-safety.md` | keitora-tasks + CLI |
| 002 | `002-tagged-worktrees-and-registry-watcher.md` | src-tauri |
| 003 | `003-ordered-loads.md` | `src/lib/useLiveResource.ts`, `loadProject` |
| 004 | `004-tag-guards-and-self-heal.md` | `src/lib/ipc.ts`, `warmProjects.ts`, `App.tsx` |
| 005 | `005-rail-active-by-key.md` | `src/lib/railGroups.ts` |

## Test rigor
- New Rust logic ships with `#[cfg(test)]` coverage in the same commit.
- New TS logic ships with vitest coverage in the same commit (pure helpers preferred).
- Never run gated herdr tests (`KEITORA_HERDR_BIN`) or a non-isolated dev app. Run the
  narrowest cargo test (`-p <crate>`) per stage; one full `cargo test --workspace` in
  Testing.
- No real project/user names in fixtures — `proj`, `other-proj`, `alpha`, `bravo`.
- Commit messages: plain Conventional Commits, no task/stage ids.
