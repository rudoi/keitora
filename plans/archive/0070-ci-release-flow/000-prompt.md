# 0070 — implementation entry point

Proposal (source of truth): `plans/0070-ci-release-flow.md` — read it fully, especially
"Technical design" and "Implementation reference (singletonized)".
Reference implementation: `D:/repos/kitwright` (`.github/`, `scripts/release*.ts`).
Branch: `llm/andrew/keitora/0070-ci-release-flow`.

## Stages

| # | File | Depends on | Model |
|---|---|---|---|
| 1 | `001-ci-foundation.md` — toolchain pin, composite actions, `ci.yml` | — | sonnet |
| 2 | `002-release-script.md` — `release.ts` / `release.lib.ts` / tests | — | opus |
| 3 | `003-release-workflow.md` — `release.yml` | 1 (actions) | sonnet |
| 4 | `004-docs.md` — CONTRIBUTING + README | 2, 3 | sonnet |

Stages 1 and 2 are independent and run in parallel; 3 after 1; 4 last.

## Rules

- Conventional Commits, no plan/task IDs in code or commits, placeholder names only (AGENTS.md).
- Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Never run the app, herdr, `dev:isolated`, or gated herdr tests. Avoid full `cargo build`/`cargo test`
  storms on this machine; Rust is barely touched, so `cargo fmt --all --check` is enough locally and
  CI does the rest.
- Test rigor: every pure function in `release.lib.ts` has vitest coverage (`yarn test` green).
  Workflows are validated by YAML parse + review locally and by real CI runs on the PR.
