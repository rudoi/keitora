# 0077 Antigravity provider: implementation entry point

**Implementation is SEQUENTIAL.** Do the stages in order, one commit each. Do not start a stage until the
previous one is committed and its validation is green; later stages depend on symbols earlier ones add.

Read first: `plans/0077-antigravity-provider/README.md` (index, decisions and the "Open issue resolutions"
section, which wins over the earlier "Chosen" defaults), then the docs `01`-`08` named by your stage,
`facts.md` (verified CLI facts) and `loss-report.md`. Also read `AGENTS.md`.

Branch: `llm/andrew/keitora/0077-antigravity-provider` (already checked out; do not create another; do not push).

## Stages

| # | File | Complexity | Goal |
|---|---|---|---|
| 1 | `010-provider-model.md` | scaffold | Add `CliKind::Antigravity`, `ProviderSpec`/`CLI_COUNT`/`ALL`, the config struct fields the compile-driven arms need, the id-safety rename, and every Rust exhaustive-match arm so the workspace compiles and behaves correctly for the new variant. |
| 2 | `020-persistence.md` | complex | Persist per-provider config: project `ProjectRef` override, `config_edit` generic over `CliKind`, registry `ProviderIds`, and the SQLite CHECK-constraint migration. |
| 3 | `030-settings-ipc-frontend.md` | complex | Replace the fixed claude/codex settings wire shapes with provider-keyed shapes in Rust and TypeScript together, add `providers.ts`, route every frontend claude/codex ternary through it, add the glyph, and update the mock harness. |
| 4 | `040-discovery-launch-resume.md` | complex | Discover `agy`, build its launch arguments and fallback chain, make resume provider-aware, and teach the fake herdr and pane environment about Antigravity. |
| 5 | `050-trust-and-integration-gating.md` | complex | Add the shared global-write guard, workspace trust seeding for `agy`, and gating of herdr's `antigravity-cli` integration on `agy` being installed. |
| 6 | `060-hooks-and-session-cli.md` | complex | Install the global Antigravity hooks that only act inside Keitora panes, add `keitora session --stdin-payload --if-fully-idle`, and wire every install trigger. |
| 7 | `070-waiting-state.md` | complex | Treat herdr `blocked` as `waiting` for every provider via `HINT_WAITING`, `apply_reconcile`, the hint map, activity and dispatch (the one approved Claude/Codex behaviour change). |
| 8 | `080-updates-and-doctor.md` | complex | Make agent updates and the doctor provider-complete for Antigravity: version parsing, release-manifest latest version, install kind and update command, provider-keyed snapshot (Rust and TS together), doctor row. |
| 9 | `090-agent-switch.md` | complex | Guard transfer to its two pairs and add the switch-agent command and its frontend flow (menu, pending intent, reducer mode, overlay, onSwitch). |
| 10 | `100-settings-ui.md` | scaffold | Add the generic provider settings group, the default-agent picker and (only if a real sample exists) Antigravity prompt chrome. |
| 11 | `110-isolation-docs.md` | scaffold | Make dev isolation skip real-home writes, and update the README, CONTRIBUTING, Windows backend doc and build plan. |

Order follows the docs' dependency order with these adjustments: the config struct fields (doc 02) and the glyph
(doc 07) are pulled forward into stages 1 and 3, because compile-driven match arms and TypeScript
`Record<CliKind, ...>` maps need them; trust/gating (doc 03) and hooks/session CLI (doc 04) are separate stages.

## Rules

- **AGENTS.md.** No task or plan identifiers (`#0077`, "Stage N", `plans/0077-...` paths) in source code,
  comments or commit messages. Placeholder names only (`proj`, `alpha`, `/home/u`, `C:/Users/x`,
  `user@example.com`); never real usernames, machine names or paths. Conventional Commits
  `type(scope): description`, then a blank line and the trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **Green at every stage.** Each stage must leave these passing: `cargo build --workspace`,
  `cargo test --workspace`, `yarn tsc --noEmit` (no separate typecheck script exists; `yarn build` runs
  `tsc && vite build`) and `yarn test` (vitest). `yarn lint` / `yarn lint:rust` are what the commit hooks run.
- **Tests ship with the code.** New Rust logic gets `#[cfg(test)]` tests in the same commit; new TypeScript logic
  gets vitest tests in the same commit.
- **Safety.** Tests must NEVER touch the user's real home directories (`~/.gemini`, `~/.claude`, `~/.codex`), the
  running herdr daemon, or launch real agents. Use injectable paths/roots, temp dirs and the existing fakes (fake
  herdr, fake `Net`/`Proc`/`Fs`). Never read `dirs::home_dir()` in a tested path.
- **Forbidden commands.** Do NOT run herdr-gated or `#[ignore]` integration tests (anything needing
  `KEITORA_HERDR_BIN`), `yarn dev:isolated*`, `yarn dev:herdr`, `yarn tauri dev`, and never kill any keitora or
  herdr process (the user runs a real one). Avoid heavy runs beyond the validation commands.
- **Compatibility.** Claude and Codex behaviour stays byte-identical (launch args, TOML output, hook installs,
  pane env, UI) except the one approved change: herdr `blocked` now maps to `waiting` for every provider.
- **Hardened docs win.** Do not redesign. If a doc disagrees with the code, locate by symbol (line numbers are
  hints) and report the mismatch instead of inventing a new design.
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
  `git log -1 --format=%B | grep -E '#[0-9]'` must both be empty. Never commit `.harness/`.
