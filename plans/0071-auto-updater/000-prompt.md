# Implementation entry point: in-app auto updater

- **Proposal (source of truth):** `plans/0071-auto-updater/proposal.md`. Read it in full before any stage.
- **Branch:** `llm/andrew/keitora/0071-auto-updater`
- **Execution:** sequential. Each stage is built and tested before the next one starts.

## Stages
1. `001-tauri-bump.md`: bump tauri to ≥ 2.12 and the JS packages to match. No behaviour change.
2. `002-settings.md`: the `UpdatesConfig` user setting, its DTO, and the toggle command.
3. `003-updater-core.md`: the pure release assessment and the HTTPS release source.
4. `004-updater-runtime.md`: the Updater state machine, plugin wiring, commands, menu, config, and
   the signing pubkey.
5. `005-frontend.md`: updates logic, IPC wrappers, the provider, the banner, the settings section,
   and harness mocks.
6. `006-release-ci.md`: the bundle overlay, the key-id check, `build-dmg.sh`, and `release.yml`.
7. `007-docs.md`: CONTRIBUTING "Releases" and the build-plan status.

## Rules for every stage
- **Test rigor:** new Rust logic ships with `#[cfg(test)]` tests in the same commit, and new TS logic
  with vitest tests. No test touches the network, the plugin's install, or herdr.
- **Validation:**
  - `cargo clippy --workspace --all-targets -- -D warnings` (lints deny warnings) and
    `cargo test --workspace`;
  - `yarn test` and `yarn build`, when the frontend or scripts change.
  - Run cargo commands **one at a time**, never in parallel; the machine is shared with real work.
- **Never** run the app against the real herdr, run gated herdr tests (`KEITORA_HERDR_BIN`), or run
  `dev:herdr`.
- **Commits:** plain Conventional Commits with **no** task or plan identifiers, ending with
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **Public-repo hygiene:** placeholder names only (AGENTS.md). Never commit or print the private
  signing key or its password.
