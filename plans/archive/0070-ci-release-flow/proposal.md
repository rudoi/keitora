# 0070 — CI release flow + CI workflow

## Goal

Cutting a release becomes one command on any machine — `yarn release --patch` — and GitHub Actions
builds and publishes every platform's installer. Today `yarn release` builds only the platform it runs
on and uploads that one file, so a release needs a Windows run *and* a Mac run. Alongside it, add a CI
workflow so every PR and every push to `main` is built and tested on Windows, macOS and Linux. Keitora
has no `.github/` directory at all today.

The model is kitwright (`D:/repos/kitwright`): `scripts/release.ts` bumps, commits, tags and pushes; the
tag push runs `.github/workflows/release.yml`, which builds and publishes. Nothing local publishes.

## Decisions already made (user)

- **macOS signing in CI:** the self-signed `Keitora Local Signing` identity
  (`scripts/macos-local-signing-cert.sh`), exported once from the user's Mac as a `.p12` and stored as
  a secret. No notarization in CI (a self-signed certificate cannot be notarized).
- **macOS architecture:** Apple Silicon only (`aarch64`), matching v0.3.0's `Keitora_0.3.0_aarch64.dmg`.
  `macos-latest` runners are arm64, so this is a native build.
- **Windows:** stays the NSIS `Keitora_X.Y.Z_x64-setup.exe`, unsigned, as today.
- **No in-app updater**, so no minisign/updater key (kitwright's `bundle-overlay.ts` and `.sig` handling
  are not ported).
- **Push straight to `main`:** the `main` ruleset now has an admin "always" bypass (same as kitwright),
  so the version commit can be pushed directly (the push prints `remote: Bypassed rule violations`).
  Non-admins still need a PR.
- **Toolchain pinned**, **clippy warnings are errors**, **old per-platform upload flow removed**
  (see Resolved questions).

## Scope

**In**

1. `scripts/release.ts` + `scripts/release.lib.ts` rewritten to kitwright's shape: bump → commit
   `chore(release): vX.Y.Z` → tag → one atomic push of `main` and the tag. No local build, no
   `gh release` calls. Preconditions refuse: a dirty tree, a branch other than `main`, a branch behind
   `origin/main`, an existing tag, the four version files disagreeing, or the latest CI run for
   `HEAD` not having passed. `--dry-run` previews from any branch.
2. `.github/workflows/release.yml`: on a `v*` tag push, build Windows and macOS in parallel and publish
   one GitHub release with both installers. Also runs build-only (unsigned, no publish) on PRs touching
   release files, and build-only on `workflow_dispatch` and a weekly schedule (which also keep the
   release cache warm).
3. `.github/workflows/ci.yml`: fmt, clippy, `cargo test --workspace`, and a frontend job (biome,
   `yarn build` = `tsc && vite build`, `yarn test`). Rust runs on `windows-latest`, `macos-latest`,
   `ubuntu-latest`.
4. `.github/actions/setup-rust` and `.github/actions/setup-node-yarn` composite actions (kitwright's,
   adapted).
5. `rust-toolchain.toml` pinning `1.96.1` with `rustfmt`, `clippy`.
6. Docs: `CONTRIBUTING.md` "Releases" and "Signed macOS builds" rewritten (new flow, the one-time
   certificate export + secrets + environment setup, what the CI build is and is not; `yarn release`
   no longer picks a macOS signing mode); `README.md` line 33 ("no prebuilt downloads yet") and the
   install bullets (lines ~85–90) point to GitHub Releases and explain the macOS "Open Anyway" /
   Windows SmartScreen first-run prompts.
7. Fixing whatever the first macOS/Linux CI runs surface (compile errors, lints, tests) — this is the
   first time Unix code compiles anywhere.

**Out**

- Developer ID signing / notarization, Windows code signing (the workflow leaves room for them).
- A universal or Intel macOS build; a Linux installer.
- Release candidates (`X.Y.Z-rc.N`).
- Bumping `crates/*` versions — they are versioned independently (0.1.0), as in kitwright.
- Repository settings beyond the already-done admin bypass: required status checks on `main`, the
  `release` environment's protection rules and a `v*` tag ruleset are documented and **offered at the
  review gate**, not applied silently.
- Changing `scripts/build-dmg.sh` beyond what CI needs (it stays the local signed-build path).

## Architecture

```
yarn release --patch          (local, any OS, on an up-to-date, clean, CI-green main)
  ├─ bump package.json, src-tauri/tauri.conf.json, src-tauri/Cargo.toml, Cargo.lock
  ├─ git commit "chore(release): vX.Y.Z"  →  git tag vX.Y.Z
  └─ git push --atomic origin refs/heads/main refs/tags/vX.Y.Z
        on failure: nothing left the machine; delete the local tag, undo the commit, print why

tag push vX.Y.Z  →  release.yml
  ├─ build-windows (windows-latest): tag==version → yarn tauri:bundle → upload setup.exe
  ├─ build-macos   (macos-latest, environment: release on tag runs):
  │      tag==version → import + trust self-signed identity (tags only; required) →
  │      bash scripts/build-dmg.sh --sign-only (builds, signs, verifies DR) → upload dmg
  │      [PR/dispatch/schedule: yarn tauri:bundle, ad-hoc, build-only]
  └─ publish (ubuntu; push-to-tag only; needs both):
         gh release create vX.Y.Z <both files> --generate-notes --verify-tag
         (or gh release upload --clobber when re-running a tag)

PR / push main  →  ci.yml
  ├─ fmt (ubuntu)
  ├─ rust-os matrix (windows, macos, ubuntu): clippy -D warnings + cargo test
  ├─ rust (aggregate check; ubuntu informational until promoted)
  └─ frontend (ubuntu): biome ci src, yarn build, yarn test
```

## Technical design

### Release script (`scripts/release.ts`, `scripts/release.lib.ts`)

- Port kitwright's pure logic and tests, simplified to plain `X.Y.Z` (rc support removed from the
  `Version` type, `parseVersion`, `formatVersion`, `bumpVersion`, `compareVersions` and `parseArgs`
  help; `isPrerelease`/`bundleVersion*`/`*Overlay` not ported): `parseArgs`, `bumpVersion`,
  `compareVersions`, `preconditionProblems(RepoState, tag)`, `commitMessage`, `tagFor`,
  `startLine`/`doneLines` (say "installers", plural), `needsShell`, `setPackageJsonVersion`,
  `setCargoPackageVersion` (scoped to `[package]`), `setCargoLockVersion`, `getCargoPackageVersion`.
- **New for Keitora:** `src-tauri/tauri.conf.json` carries the app version, so add
  `getTauriConfVersion`/`setTauriConfVersion` (JSON-aware read, top-level `version` only, formatting
  preserved) and a `getCargoLockVersion(lock, "keitora")`. `BUMPED_FILES` = `package.json`,
  `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml`, `Cargo.lock`. All four must agree before a
  bump (kitwright checks two). Lock entry is `name = "keitora"` (other members are `keitora-cli`,
  `keitora-tasks` — the exact-name match must not touch them).
- **Atomic push** (deviation from kitwright, simpler and safer): one
  `git push --atomic origin refs/heads/main refs/tags/vX.Y.Z`. Either both refs land or neither, so
  kitwright's two-step `pushSteps`/`pushRecovery` collapses into one failure path: delete the local
  tag, `git reset --soft HEAD~1` + restore the bumped files, print the remote's error. Full refspecs
  avoid a branch/tag name clash.
- **CI-green precondition:** `RepoState` gains `ciStatus` for `HEAD` (= `origin/main`, enforced by the
  behind/branch checks), read with `gh run list --workflow ci.yml --commit <sha> --json
  conclusion,status`. Refuse unless the newest run concluded `success`; `--skip-ci-check` overrides
  with a warning (for the very first release before CI has history, or when Actions is down). In
  `--dry-run`, problems are printed, not thrown.
- **Behaviour changes, intended:** no `--overwrite`; no "fill the current release" no-flag mode (a bump
  flag or `--set` is required unless `--dry-run`); a non-increasing `--set` hard-fails (today it asks
  for confirmation).
- Remove the now-dead `tauriArchToken`, `platformArtifactPattern`, `assetMatches`, `decideUpload`
  and their tests (only `release.ts`/`release.lib.test.ts` use them). `release.ts` stops importing
  `scripts/signing.mjs`; `signing.mjs` and `signing.test.ts` stay (used by `stage-sidecars.mjs`).
- Note: `scripts/` is not covered by `biome.json` (`files.includes` = `src/**`) or `tsconfig.json`
  (`include` = `src`), so vitest is the only guard for this code — tests must cover every pure function,
  including version setters against real Keitora file shapes (CRLF-tolerant).

### release.yml

- **Triggers:** `push: tags: ["v*"]`; `workflow_dispatch`; `schedule` weekly (main only);
  `pull_request` filtered to `.github/workflows/release.yml`, `.github/actions/**`,
  `src-tauri/tauri*.json`, `src-tauri/entitlements.plist`, `scripts/stage-sidecars.mjs`,
  `scripts/signing.mjs`, `scripts/build-dmg.sh`. Never `pull_request_target`.
- `concurrency: release-${{ github.ref }}`, `cancel-in-progress: false`. Top-level
  `permissions: contents: read`; only `publish` has `contents: write`.
- `env: HUSKY: 0, CARGO_INCREMENTAL: 0, CARGO_TERM_COLOR: always`.
- Both build jobs check `GITHUB_REF_NAME == v<package.json version>` on tag runs.
- **Windows:** `yarn tauri:bundle`. Find exactly one
  `target/release/bundle/nsis/Keitora_*_x64-setup.exe`. NSIS tooling downloads at build time; a flake
  is handled by re-running the job.
- **macOS, tag runs (`startsWith(github.ref, 'refs/tags/v')`):** job runs in GitHub Environment
  `release` (secrets live there). Signing is **required**: a missing secret fails the job — an ad-hoc
  release would have a different designated requirement and reset every user's Screen Recording grant.
  - Import + trust step (`timeout-minutes: 5`, the only step that sees the secrets):
    ```bash
    set -euo pipefail
    KC="$RUNNER_TEMP/keitora-signing.keychain-db"; KC_PASS="$(openssl rand -hex 24)"
    printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$RUNNER_TEMP/id.p12"
    security create-keychain -p "$KC_PASS" "$KC"
    security set-keychain-settings -lut 21600 "$KC"
    security unlock-keychain -p "$KC_PASS" "$KC"
    security import "$RUNNER_TEMP/id.p12" -k "$KC" -f pkcs12 -P "$MACOS_CERTIFICATE_PASSWORD" -x -T /usr/bin/codesign
    rm -f "$RUNNER_TEMP/id.p12"
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KC_PASS" "$KC" >/dev/null
    security list-keychains -d user -s "$KC" $(security list-keychains -d user | tr -d '"')
    security find-certificate -c "$IDENTITY" -p "$KC" > "$RUNNER_TEMP/id.pem"
    sudo security authorizationdb write com.apple.trust-settings.admin allow
    sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$RUNNER_TEMP/id.pem"
    security find-identity -v -p codesigning "$KC" | grep -F "$MACOS_SIGNING_SHA1"
    ```
    `IDENTITY` comes from repo variable `MACOS_SIGNING_IDENTITY` (default `Keitora Local Signing`);
    `MACOS_SIGNING_SHA1` is a repo variable (not secret) pinning the exact certificate, so a wrongly
    exported identity fails loudly instead of resetting grants. Trust is required: `codesign` refuses an
    untrusted self-signed root, and `-x` makes the key non-extractable. Codesign trust in the user
    domain would raise a GUI prompt, so the admin domain with the relaxed authorizationdb rule is used
    (runner is ephemeral; no restore needed).
  - Build: `bash scripts/build-dmg.sh --sign-only` with `APPLE_SIGNING_IDENTITY="$IDENTITY"`. It stages
    the sidecar, runs `tauri build` with the bundle overlay, and verifies the dmg's copy of the app
    (mounted read-only — Tauri deletes `bundle/macos/Keitora.app` after making the dmg), including the
    `identifier "com.keitora.dispatch"` designated-requirement check. `APPLE_CERTIFICATE` is **never**
    set: Tauri's importer only recognises Apple-issued name prefixes and would reject this identity.
  - Cleanup (`if: always()`): `security delete-keychain "$KC"`.
- **macOS, non-tag runs** (PR, dispatch, schedule): no environment, no secrets, `yarn tauri:bundle`
  ad-hoc (the existing `stage-sidecars.mjs` warning prints). Fork PRs therefore succeed unsigned.
- Find exactly one `target/release/bundle/dmg/Keitora_*_aarch64.dmg`; upload artifacts
  (`retention-days: 14`).
- **Publish:** `if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')` (a manual
  dispatch on a tag never publishes); download both artifacts, assert both files, then
  `gh release create "$TAG" … --title "$TAG" --generate-notes --verify-tag`, or
  `gh release upload --clobber` when the release exists (re-run).
- **Caches:** `cargo-release-<os>-…` key. Tag runs only restore (a tag-scoped cache is unreachable from
  other tags, so saving it is wasted quota). `schedule`/`workflow_dispatch` runs on `main` save it,
  after trimming bundle output.

### ci.yml

- Mirrors kitwright's `ci.yml` minus the herdr-pin and gallery steps.
- Triggers `pull_request` and `push: branches: [main]`; concurrency `ci-${{ github.ref }}` with
  `cancel-in-progress: ${{ github.event_name == 'pull_request' }}` so main runs (which save caches)
  are never cancelled.
- `env: HUSKY: 0, CARGO_INCREMENTAL: 0, CARGO_TERM_COLOR: always, CARGO_PROFILE_DEV_DEBUG: 0` (debug
  info off keeps three OS debug caches inside the 10 GB repo quota; flags are not put in `RUSTFLAGS`,
  which would rebuild every dependency between steps).
- `fmt` (ubuntu): `rustup toolchain install` then `cargo fmt --all --check`.
- `rust-os` matrix, `fail-fast: false`: Linux installs `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev
  librsvg2-dev libdbus-1-dev libxdo-dev libssl-dev build-essential pkg-config`; then
  `cargo clippy --workspace --all-targets --locked -- -D warnings` (the workspace already denies
  warnings via `[workspace.lints]`; the flag makes it explicit) and `cargo test --workspace --locked`.
  Cache trimmed of workspace crate artifacts before save; save only on `main`.
- `rust` aggregate job (`needs: [fmt, rust-os]`, `if: always()`), `REQUIRED_LEGS: "windows-latest
  macos-latest"`; `ubuntu-latest` is informational until promoted by adding it to the list.
- `frontend` (ubuntu): `yarn biome ci src` (`biome.json` already scopes to `src`), `yarn build`,
  `yarn test`.
- `#[ignore]`d tests are never run: the real-herdr tests (also gated on `KEITORA_HERDR_BIN`) and the
  `exec.rs` helper-child tests (run as children by other tests).

### Composite actions

- `setup-rust`: `rustup toolchain install` (reads `rust-toolchain.toml`), restore cargo cache keyed
  `<prefix>-<os>-<hash(rust-toolchain.toml)>-<hash(Cargo.lock)>`; saving left to callers.
- `setup-node-yarn`: Node 22 (match `package.json` engines if set), yarn cache restore, `yarn install
  --frozen-lockfile`, save only on `main`.

### Action policy

Only GitHub-authored `actions/*` (checkout, setup-node, cache, upload/download-artifact), versions
mirrored from kitwright where they work today, so no third-party action ever sees a secret.

## Dependencies

- Admin bypass on the `main` ruleset — **done** (2026-11-14).
- **User, one-time, on the Mac** (documented in CONTRIBUTING; needed before the first tag, not for the
  PR): export `Keitora Local Signing` via Keychain Access → login → My Certificates → right-click →
  Export… as `.p12` (the GUI exports just that identity; `security export -t identities` would export
  every identity), check it with `/usr/bin/openssl pkcs12 -in k.p12 -nokeys | grep subject`, then
  `base64 -i k.p12 | gh secret set MACOS_CERTIFICATE_P12 --env release`,
  `gh secret set MACOS_CERTIFICATE_PASSWORD --env release`,
  `gh variable set MACOS_SIGNING_SHA1 --body <sha1 from security find-identity>`, `rm -P k.p12`.
- **Offered at review gate (repo settings):** create Environment `release` limited to `v*` tags;
  a tag ruleset so only admins create `v*` tags; required status checks (`rust`, `frontend`) on `main`.

## Risks

- **First macOS/Linux compile and test run ever.** Unix-only code (notably the transfer work) has never
  compiled; tests using `dirs::home_dir()` or spawning `herdr` (`env.rs`) may behave differently on
  runners. Fixes are in scope; iterate against CI logs (no local Mac). Linux stays informational if it
  needs more than small fixes.
- **Codesign trust on macOS 15 runners.** The authorizationdb + `sudo add-trusted-cert` pattern comes
  from GitHub's own runner images, but some report the authorizationdb write failing on Sequoia. It can
  only be proven on a run with the secret (first `workflow_dispatch`-on-tag rehearsal or first tag).
  Fallback if it fails: sign outside Tauri with `rcodesign` from the `.p12` (no trust needed) — a
  follow-up, not built now.
- **Signing key value.** Anyone with the key can build an app that inherits users' Screen Recording
  grants. Mitigations: key only in the `release` environment, used only on tag runs, imported
  non-extractable, never exposed to PR code; tag creation restricted to admins (offered).
- **Gatekeeper on downloads.** A self-signed, unnotarized dmg is blocked on first open; users allow it
  once under Privacy & Security → Open Anyway. Documented in README.
- **Tag pushed, build failed.** The tag exists with no release. Recovery: fix and re-run failed jobs on
  the tag (publish is idempotent via `--clobber`), or fix forward with a new patch version. Documented.
- **Flaky runners:** NSIS download, `hdiutil` "Resource busy". Recovery: re-run failed jobs.
- **Cache quota (10 GB).** Debug info off, workspace artifacts trimmed, PRs and tags never save.

## Resolved questions (refine, 2026-11-14)

1. **Toolchain pin:** yes — `rust-toolchain.toml`. Version adjusted from the proposed 1.92.0 to
   **1.96.1** during the automated refine pass: commit `c5db355` already satisfied clippy 1.96 lints,
   so a contributor builds on ≥1.96 and pinning 1.92 would downgrade them; 1.96.1 is installed locally.
2. **clippy strictness:** yes — `-D warnings` on every leg ("should be warnings as errors for sure").
3. **Old flow:** removed — no per-platform upload mode.

## Refinement log

- **Cycle 1 (user):** accepted defaults; warnings as errors confirmed; requested a thorough automated
  refine pass, then auto-advance to the review gate.
- **Cycle 1 (automated: reference check + adversarial CI/signing review):** toolchain 1.96.1; atomic
  push replaces two-step push + recovery; CI-green precondition with `--skip-ci-check`; tag runs require
  signing (no ad-hoc release); secrets in a `release` environment, tag runs only; full keychain/trust
  sequence with SHA-1 pin; verify via `build-dmg.sh --sign-only` (Tauri deletes the `.app`); never set
  `APPLE_CERTIFICATE`; publish only on tag push; release cache saved from main (schedule/dispatch), not
  tags; CI debug info off + cancel only PR runs; JSON-aware `tauri.conf.json` setter and 4-file
  agreement; full Linux apt list; README/CONTRIBUTING sections named; repo-settings hardening offered
  at review gate rather than applied.

## Implementation reference (singletonized)

Exact values an implementer must use; nothing here is open.

- **Files created:** `rust-toolchain.toml`, `.github/actions/setup-rust/action.yml`,
  `.github/actions/setup-node-yarn/action.yml`, `.github/workflows/ci.yml`,
  `.github/workflows/release.yml`.
- **Files rewritten:** `scripts/release.ts`, `scripts/release.lib.ts`, `scripts/release.lib.test.ts`.
- **Files edited:** `CONTRIBUTING.md` (§ Releases, § Signed macOS builds; one line in § macOS
  permissions only if it references `yarn release`), `README.md` (line ~33 and install bullets ~85–90).
- **Untouched:** `scripts/build-dmg.sh`, `scripts/signing.mjs`, `scripts/signing.test.ts`,
  `scripts/stage-sidecars.mjs`, `src-tauri/tauri*.json`, `package.json` scripts (`release` stays
  `tsx scripts/release.ts`).
- **Action versions** (as kitwright uses today): `actions/checkout@v7`, `actions/setup-node@v7`
  (`node-version: 22`; the repo pins no engines), `actions/cache/restore@v6`, `actions/cache/save@v6`,
  `actions/upload-artifact@v7`, `actions/download-artifact@v8`.
- **Secret/variable names:** environment `release` secrets `MACOS_CERTIFICATE_P12` (base64 `.p12`),
  `MACOS_CERTIFICATE_PASSWORD`; repo variables `MACOS_SIGNING_SHA1` (required on tag runs) and
  `MACOS_SIGNING_IDENTITY` (optional, default `Keitora Local Signing`). Missing secret or SHA-1 on a tag
  run → the macOS job fails with a message naming the missing item and pointing at CONTRIBUTING.
- **Artifact names:** `keitora-windows` (the `setup.exe`), `keitora-macos` (the `.dmg`).
- **Schedule:** `cron: "17 6 * * 1"` (weekly, Monday 06:17 UTC); scheduled and dispatch runs build
  unsigned and only save the release cache when `github.ref == 'refs/heads/main'`.
- **Release script CLI:** `--patch | --minor | --major | --set X.Y.Z`, `--dry-run`, `--skip-ci-check`,
  `--help/-h`. Exit 2 on usage errors, 1 on refused/failed release.
- **CI check semantics:** newest `ci.yml` run whose `headSha` == `HEAD`: `completed` + `success` → OK;
  in progress → refuse ("CI still running for <sha>"); failure/cancelled → refuse; none → refuse ("no
  CI run for <sha>"). `gh` missing or unauthenticated → refuse with a hint to pass `--skip-ci-check`.
- **Atomic push failure:** nothing reached the remote. Undo locally without risking user work:
  `git tag -d vX.Y.Z`, `git reset --soft HEAD~1`, then `git checkout HEAD -- <BUMPED_FILES>` (the tree
  was clean before the bump, so those files carry only the bump). Never `reset --hard`. Print the
  remote's error and "nothing was pushed".
