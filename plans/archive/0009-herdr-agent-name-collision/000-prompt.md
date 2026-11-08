# 0009 — implementation entry point

Proposal: `plans/0009-herdr-agent-name-collision.md` (read it first; it is the design).
Branch: `llm/andrew/keitora/0009-herdr-agent-name-collision`.

Stages run **sequentially**: each one builds on the previous, and each must pass
`cargo test -p keitora-app` (crate in `src-tauri`; or `cargo test --workspace`) and
`cargo clippy --workspace --all-targets -- -D warnings` before its commit.

| # | Stage | Proposal section |
|---|---|---|
| 001 | herdr error bodies on stderr | A |
| 002 | AgentInfo name/cwd, agent helpers, fake fidelity | B, C, G |
| 003 | name-based eviction + `agent_name_taken` recovery | E, F |
| 004 | agent-aware reconcile | D |

Test rigor: every new pure helper gets unit tests. Each behaviour change gets a
backend-level test against the fake herdr that **fails without the change**.
Follow `AGENTS.md`: no task/stage ids in code or commits, and placeholder names
only in fixtures (`/Users/me/…`, `proj`, `alpha`).
