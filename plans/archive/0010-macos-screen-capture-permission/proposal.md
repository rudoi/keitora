# 0010 — macOS screen-capture permission never sticks for agent sessions

## Goal

An agent session (Claude or Codex in a Keitora pane) can run `screencapture` on macOS
after the user grants **Screen & System Audio Recording** once. The grant should survive
Keitora rebuilds and upgrades, and the user should not see a permission prompt on every
capture.

## Symptom

Keitora and herdr are both switched on under System Settings → Privacy & Security →
Screen & System Audio Recording. Even so, every `screencapture` from an agent session
shows a permission prompt, and the capture never succeeds.

## Research findings

Observed on macOS 26.3 with an installed `yarn tauri:bundle` build.

### 1. TCC charges the *responsible process*, not the caller

macOS checks the Screen Recording grant (TCC service `kTCCServiceScreenCapture`) against
the **responsible process** of the caller. That is normally the app bundle at the root
of the launch chain. A command-line tool started from a shell borrows the grant of the
app that started that shell. Live process chain for an agent session:

```
screencapture → zsh → claude → fish → herdr server (ppid 1) ⇐ spawned by Keitora.app
```

- The `herdr server` is started by Keitora (`RealHerdr::spawn_server`,
  `src-tauri/src/backend/herdr/real.rs`, through `backend::launch::spawn_detached`).
  It detaches and is reparented to launchd (ppid 1). Detaching does **not** reset
  responsibility on macOS, so the responsible app stays the **Keitora instance that
  started the server**.
- The server outlives Keitora. Sessions persist and Keitora adopts the running server on
  its next launch (`backend/herdr/boot.rs`, `BootClass::Adopt`). So the responsible
  process can be an *older* Keitora build than the one now installed. In the observed
  case the server was started about 6 minutes before the running Keitora.
- If the server was ever started by hand from a terminal (`herdr server`), the
  responsible app is that terminal emulator, not Keitora.
- `claude` is properly signed (hardened runtime, Anthropic team ID), but it is not the
  responsible process, so its own signature does not matter here.

**Verified** with `sudo launchctl procinfo <claude-pid>` on a live agent session:
`responsible path = /Applications/Keitora.app/Contents/MacOS/keitora-app`. The herdr
server those sessions run under started at 09:16. The only Keitora running started at
09:23. So the grant is checked against an **earlier Keitora instance**, the one that
spawned the server. Combined with finding 2, that earlier build can never match a grant.
(The reported responsible pid was reused by an unrelated process, so the path is the
reliable field.)

### 2. The installed Keitora.app has no stable code identity (primary cause)

```
$ codesign -dv /Applications/Keitora.app
Identifier=keitora_app-<hash>          ← not the bundle id
flags=0x20002(adhoc,linker-signed)
$ codesign -dr - /Applications/Keitora.app
designated => cdhash H"…"               ← identity = this exact binary
(no Contents/_CodeSignature — Info.plist and resources are not sealed)
```

`tauri.conf.json` has `"signingIdentity": null`. As a result `yarn tauri:bundle`
produces an app carrying only the linker's ad-hoc signature. TCC stores each grant with
the client's *designated requirement*. For an ad-hoc binary that requirement is a raw
cdhash, and every rebuild produces a new one. The toggle in System Settings therefore
points at a binary that no longer exists. The running app never matches it, so TCC
prompts again and denies the capture. The code identifier is also `keitora_app-<hash>`
rather than `com.keitora.dispatch`, because the bundle is not sealed. This is the
textbook "the switch is on but it keeps asking" failure.

The herdr binary (installed in `~/.local/bin`) is ad-hoc and linker-signed in the same
way, so a grant to herdr is also invalidated by every herdr upgrade. Once finding 1 is
confirmed, the herdr entry in System Settings is a red herring: herdr is not the
responsible process for agent sessions.

### 3. Screen Recording grants only take effect after a relaunch

macOS applies a new Screen Recording grant only to processes started after it. That is
why System Settings offers "Quit & Reopen". The long-lived herdr server and every shell
under it keep their old attribution and state. So even a correct grant will not reach
existing sessions until the herdr server, and therefore every pane, restarts.

### What already exists

- `scripts/build-dmg.sh` signs *and notarizes* with `APPLE_SIGNING_IDENTITY`. It requires
  notarization credentials, so it is not the everyday local build. The release flow
  (`scripts/release.ts`) calls it on macOS. Day-to-day installs come from
  `yarn tauri:bundle`, which is unsigned.
- A local keychain can hold usable signing identities (Apple Development or Developer ID).
  Contributors without an Apple account have none.
- `src-tauri/entitlements.plist` is empty, and hardened runtime is already enabled in
  config. It only takes effect once a real identity signs the bundle.

## Scope

**In**

1. **Stable local signing for `yarn tauri:bundle`.**
   - With `signingIdentity: null` (kept committed: the repo is public and CI has no
     credentials), the Tauri bundler falls back to `APPLE_SIGNING_IDENTITY`. It signs the
     main binary and the `externalBin` sidecar with `--options runtime` and
     `entitlements.plist`. Notarization only runs when a complete set of `APPLE_ID` /
     `APPLE_PASSWORD` / `APPLE_TEAM_ID` or `APPLE_API_*` variables is present, so leaving
     those unset gives a sign-only build. A login-keychain identity needs no
     `APPLE_CERTIFICATE` import.
   - `APPLE_SIGNING_IDENTITY=-` (ad-hoc) is **not** a fix: it still yields a cdhash
     designated requirement. The warning below treats it like no identity.
   - Fallback for contributors without an Apple certificate:
     `scripts/macos-local-signing-cert.sh` creates a self-signed code-signing identity,
     "Keitora Local Signing", once. A self-signed leaf yields a designated requirement
     pinned to identifier plus certificate, which is stable across rebuilds.
   - Verify the sealed app at `target/release/bundle/macos/Keitora.app` (workspace root,
     not `src-tauri/target`):
     - `codesign --verify --deep --strict` passes,
     - `codesign -dr -` shows `identifier "com.keitora.dispatch"`, not `cdhash`.
2. **Unsigned-bundle warning.** `scripts/stage-sidecars.mjs` already runs first in
   `tauri:bundle` on every platform. On `darwin` only, it warns when
   `APPLE_SIGNING_IDENTITY` is unset, empty or `-`. The warning says that Screen
   Recording grants won't survive the next build and points to the docs. It never fails
   the build. The decision is a pure exported function in a new `scripts/signing.mjs`.
3. **Sign-only releases.**
   - A pure `macBuildMode(env)` in `scripts/signing.mjs` returns one of:
     - `"notarized"`: identity set plus a complete Apple ID or API-key set,
     - `"signed-only"`: identity set and no notarization variables at all,
     - `"partial-notarization"` (error): some notarization variables set, but no
       complete set,
     - `"no-identity"` (error): identity unset, empty or `-`.
   - `scripts/release.ts` `build()` uses it on macOS. `notarized` runs
     `build-dmg.sh` unchanged. `signed-only` runs `build-dmg.sh --sign-only` and prints a
     loud warning: the uploaded DMG is signed but not notarized, so downloaders must
     right-click → Open the first time. The error modes abort with a clear message.
   - `build-dmg.sh --sign-only` skips the notarization variable check and the `stapler`
     step. It keeps `codesign --verify --deep --strict` and adds the `codesign -dr -`
     identifier check. `spctl` is reported but does not fail, since it rejects
     non-notarized builds.
   - Unit tests in `scripts/signing.test.ts` cover every mode: Apple ID set, API key
     set, each partial set, none, no identity, `-`, plus the warning predicate across
     platforms.
4. **Diagnostics script** `scripts/macos-capture-doctor.sh` (read-only, no sudo). It
   reports, flagging the first problem:
   - **App signature:** how `/Applications/Keitora.app` is signed (or a path given as
     argument), using `codesign -dr -`. A cdhash requirement is a failure, a stable
     identifier is OK.
   - **Signing identities:** what `security find-identity -v -p codesigning` offers, as
     a hint for which `APPLE_SIGNING_IDENTITY` to use. It prints names only.
   - **herdr server:** the process found by an exact argv match on `herdr server` (pane
     bridges also run `herdr`), its start time (`ps -o lstart=`), and its parent.
   - **Stale server:** whether the server started *before the installed app binary's
     mtime*, meaning an older build spawned it and the server must be restarted.
     Comparing against the running Keitora's start time would flag the normal adopt case
     (`boot.rs`, `BootClass::Adopt`) as a false positive.
   - **Optional confirmation:** the `sudo launchctl procinfo <pid> | grep responsible`
     command to run by hand.
5. **Docs.**
   - A "macOS permissions" section in `CONTRIBUTING.md`, after "Signed macOS builds".
     It covers:
     - which entry to grant (Keitora, not herdr or the terminal),
     - why ad-hoc builds lose the grant,
     - how to pick or create a signing identity,
     - the one-time re-grant: quit Keitora, stop the herdr server (this ends live panes,
       which resume later), `tccutil reset ScreenCapture com.keitora.dispatch`, relaunch,
       grant, then run the doctor,
     - that `yarn tauri dev` builds are ad-hoc and out of scope.
   - `CONTRIBUTING.md`'s release section: document sign-only mode.
   - `README.md`: a short pointer in the install section.
   - Fix the stale `src-tauri/target/release/bundle/...` paths in `README.md` and
     `CONTRIBUTING.md`. Bundles land in the workspace-root `target/`.

**Out**

- Notarizing local builds.
- Development builds (`yarn tauri dev`, `target/debug`). They are always ad-hoc, and a
  herdr server spawned by one is attributed to that build. This is documented and
  flagged by the doctor's signature check, but not fixed.
- Code-signing herdr. It is third-party, and it is not the responsible process once the
  server is started by a signed Keitora.
- Disclaiming responsibility when spawning herdr (`responsibility_spawnattrs_setdisclaim`,
  a private API). That would move the grant onto the unsigned herdr binary.
- Any in-app UI or Rust change.

## Architecture / Technical design

- **`scripts/signing.mjs`:** pure functions only, following the `dev-herdr.mjs` +
  `dev-herdr.test.ts` pattern:
  - `isStableIdentity(id)`,
  - `unsignedBundleWarning(platform, env)`, returning the warning string or `null`,
  - `macBuildMode(env)`.

  `stage-sidecars.mjs` and `release.ts` (through `tsx`) import it.
- **Self-signed certificate script** (`scripts/macos-local-signing-cert.sh`):
  - It is idempotent: it exits 0 with the identity name if `find-identity -v -p
    codesigning` already lists "Keitora Local Signing". It also prints any Apple
    identities it finds, which are preferred when present.
  - It runs `openssl req -x509` with `keyUsage=critical,digitalSignature`,
    `extendedKeyUsage=critical,codeSigning` and a 10-year lifetime, so the designated
    requirement stays stable.
  - It exports a PKCS#12 file. Under OpenSSL 3 it adds `-legacy`, because macOS
    `security` cannot import the modern PBE.
  - It then runs `security import … -k login.keychain-db -T /usr/bin/codesign`,
    `security add-trusted-cert -p codeSign` (this raises a GUI auth prompt, which the
    script documents), and `security set-key-partition-list -S apple-tool:,apple: -s`
    so `codesign` does not prompt on every build.
  - Temporary files are written under `mktemp -d` and removed afterwards. Nothing is
    committed, and the script never echoes secrets.
- **Hardened runtime:** this is the first time it is actually applied. It does not
  restrict separately signed children (`claude`, `herdr`). Add entitlements only for a
  concrete failure, as the plist comments prescribe; `disable-library-validation` is
  the likeliest candidate. A smoke test on the signed bundle covers:
  - a pane spawn,
  - the `keitora-cli` sidecar,
  - clipboard image paste,
  - a screenshot.
- **Rejected in Refine:** a Keitora-side check at boot that logs a warning when the
  bundle's designated requirement is a bare cdhash (see Decisions).
- **Placeholders:** docs and tests use the existing `Jane Smith (AB12CD34EF)` style for
  identity names. Real identity strings from the dev machine are never pasted.

## Dependencies

- A code-signing identity in the login keychain (Apple certificate or the self-signed
  fallback).
- `tccutil`, `codesign` and `security` (all ship with macOS).

## Risks

- **Hardened runtime regressions.** This is the first time hardened runtime is actually
  applied, and it could break something the ad-hoc build tolerated (for example dylib
  loading). Mitigation: smoke-test a full session (spawn, transfer, paste image,
  screenshot) on the signed bundle before staging.
- **Self-signed trust.** A self-signed leaf may need explicit trust before `codesign`
  will use it, and Gatekeeper will still flag it when the app is copied to another
  machine. That is acceptable, since the certificate is for local use only.
- **Restarting the herdr server** ends live panes, which are then resumed. The re-grant
  doc must say this plainly.
- **Recurring macOS prompts.** Recent macOS versions periodically re-confirm apps that
  use legacy screen-capture paths. Some prompts may remain even with a stable identity.
  That is expected OS behaviour and separate from this bug.

## Decisions (Refine)

1. **Responsible process:** Keitora (verified, see finding 1).
2. **Local signing identity:** use `APPLE_SIGNING_IDENTITY` (Apple Development or
   Developer ID) when it is set. Otherwise fall back to the self-signed local certificate
   from `scripts/macos-local-signing-cert.sh`. Both are stable across rebuilds.
3. **No in-app unsigned-build warning.** A build-time warning from `tauri:bundle` plus
   the doctor script are enough. There is no Rust change in this work.
4. **`yarn release` on macOS signs without notarizing** when notarization credentials
   are missing, and warns that notarization was skipped.

## Complexity

**Small.** One shared `.mjs` module with tests, two new shell scripts, edits to
`stage-sidecars.mjs`, `release.ts` and `build-dmg.sh`, and docs. No Rust changes. Recommend skipping Fracture, Singletonize and Loss check.
