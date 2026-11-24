# 0076 dependency setup: implementation entry point

Proposal: `plans/0076-dependency-setup/proposal.md`. Read it fully before any stage. Its
"Implementation notes" section wins wherever it differs from earlier text. Also read
`AGENTS.md`.

Branch: `llm/andrew/keitora/0076-dependency-setup` (already checked out; do not create
another).

## Execution model

Implementation is **sequential**: do the stages in order, one commit each. Do not start a
stage until the previous one is committed and its validation is green. Stages depend on
symbols the earlier stages introduce.

## Stages

| # | File | Goal |
|---|---|---|
| 1 | `001-config-foundations.md` | keitora-tasks: `set_herdr_bin`, root-parameterised release lister, reusable version sorter, injectable home |
| 2 | `002-deps-core.md` | Pure `deps` logic: manifest + `select_release`, floor/`FloorState`/`FloorSlot`, `InstallLayout`, tier choice, `after_failure` |
| 3 | `003-installers.md` | `Proc`/`Fs` traits and fakes, tier 2 download/unpack, tier 1 script runner, manual/browse validation |
| 4 | `004-backend-integration.md` | Floor slot fed by the status loop, `require_protocol` on pane binding, `refresh_integration`, trait additions |
| 5 | `005-deps-service.md` | `DepsService`, events, install mutex, upgrade orchestration, prompt, doctor, Tauri commands, `deps::startup` |
| 6 | `006-frontend.md` | `ipc.ts` block, `deps.ts`, `DepsProvider`, dialogs, onboarding step, rail card, Settings section |
| 7 | `007-docs.md` | README install section, CONTRIBUTING discovery notes |

## Test rigor

- Every stage that adds Rust logic ships `#[cfg(test)]` tests **in the same commit**.
  Untested new logic is not done.
- Per stage, run `cargo test -p <crate>` for each touched crate (`keitora-tasks` for
  stage 1; the `src-tauri` package for stages 2-5; check the package name in
  `src-tauri/Cargo.toml`). Also `cargo check` the touched crate.
- At the end (after stage 7), run workspace `cargo test`, `yarn test`, and
  `npx tsc --noEmit`; all must be green.
- **No test may** spawn powershell or sh, touch the real `~/.herdr` or the real home
  directory, or contact a running herdr. Use fakes (`Net`, `Proc`, `Fs`, `FakeHerdr`) and
  temp dirs only. Inject home/root paths; never read `dirs::home_dir()` in tested paths.
- **Never** run gated herdr tests (anything requiring `KEITORA_HERDR_BIN` to be set),
  `yarn dev:isolated`, or `yarn dev:herdr`. Avoid heavy runs beyond the commands above.
- The single `#[ignore]` end-to-end test (local HTTP server, fake zip + manifest) is
  written but not run by default and must bind loopback only.

## Rules from AGENTS.md (repeated)

- No plan or task identifiers anywhere in source code or commit messages: no `#0076`,
  no "Stage N", no `plans/0076-...` paths in comments. Comments explain what and why on
  their own terms.
- Placeholder names only: projects `proj`, `my-game`; worktrees `main`, `alpha`; users
  `me`, `x`; hosts `example.com`. No real project, person, company, or machine names in
  fixtures, comments, or commit text. The trimmed manifest fixture may keep herdr's real
  public URLs and release versions (they are the subject of the feature), but no local
  paths or usernames.
- Commits: plain Conventional Commits `type(scope): description`, no trailing task tag.
  End each message with the configured trailer:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`
  (use whatever trailer the session is configured with).
- Before each commit: `git grep -nE '#[0-9]{3,4}' -- <changed files>` and
  `git log -1 --format=%B | grep -E '#[0-9]'` must both be empty.
- Do not push.

## Verification outside the commits

`.harness/` scenarios are local-only and uncommitted; stage 6 describes them as a
verification step. Do not `git add` anything under `.harness/`.
