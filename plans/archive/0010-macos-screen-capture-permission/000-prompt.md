# 0010 — implementation entry point

Proposal: `proposal.md` (read it first, especially
Scope and Architecture).
Branch: `llm/andrew/keitora/0010-macos-screen-capture-permission`.

Implementation is **sequential**: finish and validate each stage before starting the
next. Every stage that adds logic ships its vitest coverage in the same commit, and
`yarn test` must stay green. There are no Rust changes; still run `cargo test --workspace`
once in Testing as a guard. Commit messages follow AGENTS.md: plain Conventional
Commits, no task ids. Docs and tests use placeholder identity names only.

| # | Stage | Commit |
|---|---|---|
| 001 | Pure signing helpers + tests | `feat(scripts): signing mode helpers for macOS bundles` |
| 002 | Unsigned-bundle warning | `feat(bundle): warn when a macOS bundle is built unsigned` |
| 003 | Sign-only releases | `feat(release): sign without notarizing when credentials are missing` |
| 004 | Self-signed local identity script | `feat(scripts): create a local code-signing identity` |
| 005 | Screen-capture doctor | `feat(scripts): macOS screen-capture permission doctor` |
| 006 | Docs | `docs(macos): screen recording permissions and signed local builds` |
