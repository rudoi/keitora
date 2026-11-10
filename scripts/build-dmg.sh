#!/usr/bin/env bash
# scripts/build-dmg.sh — Build a signed (and normally notarized) Keitora .dmg
# Run from the repo root. Requires env vars set before invoking.
#
#   bash scripts/build-dmg.sh              # sign + notarize (notarization vars required)
#   bash scripts/build-dmg.sh --sign-only  # sign with a stable identity, skip notarization
set -euo pipefail

SIGN_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --sign-only) SIGN_ONLY=1 ;;
    *) echo "ERROR: unknown argument: $arg" >&2; exit 1 ;;
  esac
done

# ── Validate required env vars ────────────────────────────────────────────────
: "${APPLE_SIGNING_IDENTITY:?Set APPLE_SIGNING_IDENTITY to the full code-signing identity name}"
if [[ "$APPLE_SIGNING_IDENTITY" == "-" ]]; then
  # Ad-hoc signing gives a cdhash designated requirement, so macOS privacy grants
  # (Screen Recording) would not survive an upgrade.
  echo "ERROR: APPLE_SIGNING_IDENTITY=- is ad-hoc; name a real identity" >&2
  exit 1
fi
if [[ "$SIGN_ONLY" == 1 ]]; then
  # Make sure Tauri does not try to notarize with stray variables.
  unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH
else
  # Notarization: set either Flow A or Flow B vars before running.
  # Flow A: APPLE_ID, APPLE_PASSWORD, APPLE_TEAM_ID
  # Flow B: APPLE_API_KEY, APPLE_API_ISSUER, APPLE_API_KEY_PATH
  # A set counts only when all three variables are non-empty (same rule as signing.mjs).
  if ! [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]] &&
    ! [[ -n "${APPLE_API_KEY:-}" && -n "${APPLE_API_ISSUER:-}" && -n "${APPLE_API_KEY_PATH:-}" ]]; then
    echo "ERROR: Set all of APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID (Flow A)" >&2
    echo "       or all of APPLE_API_KEY/APPLE_API_ISSUER/APPLE_API_KEY_PATH (Flow B)" >&2
    echo "       or pass --sign-only to skip notarization" >&2
    exit 1
  fi
fi

# ── Build, sign (and notarize) via Tauri ────────────────────────────────────────
# `tauri build` runs `beforeBuildCommand` ("yarn build") itself, so the frontend
# is compiled here — no separate `yarn build` needed.
# Stage the CLI sidecar and build with the bundle overlay so the signed
# .dmg ships the `externalBin` sidecar (plain `tauri build` would omit them).
echo "==> Staging sidecars..."
yarn bundle-cli
echo "==> Running tauri build (frontend + sign [+ notarize], with sidecars)..."
yarn tauri build --config src-tauri/tauri.bundle.conf.json

# ── Locate outputs ────────────────────────────────────────────────────────────
# This is a Cargo workspace, so the bundle lands in the repo-root target/, not
# src-tauri/target/.
# Newest first, so a dmg left over from an earlier build is never the one verified.
DMG=$(ls -t target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)

if [[ -z "$DMG" ]]; then
  echo "ERROR: No .dmg found under target/release/bundle/dmg/" >&2
  exit 1
fi

echo "==> Produced DMG: $DMG"

# Tauri deletes target/release/bundle/macos/Keitora.app once the dmg is built, so check
# the app that actually ships: mount the dmg read-only and verify the copy inside it.
MNT=$(mktemp -d)
hdiutil attach -nobrowse -readonly -mountpoint "$MNT" "$DMG" >/dev/null
trap 'hdiutil detach -quiet "$MNT" || true; rmdir "$MNT" 2>/dev/null || true' EXIT
APP="$MNT/Keitora.app"

# ── Verify code signature on .app ─────────────────────────────────────────────
echo "==> Verifying code signature..."
codesign --verify --deep --strict --verbose=2 "$APP"

# The designated requirement must name the bundle id, not a cdhash: TCC stores grants
# against it, and a cdhash changes on every build.
REQ=$(codesign -dr - "$APP" 2>&1)
if [[ "$REQ" != *'identifier "com.keitora.dispatch"'* ]]; then
  echo "ERROR: designated requirement is not stable:" >&2
  echo "$REQ" >&2
  exit 1
fi
echo "==> Designated requirement is stable"

# Gatekeeper and the staple only pass for notarized builds.
if [[ "$SIGN_ONLY" == 0 ]]; then
  # ── Verify Gatekeeper acceptance of .app ────────────────────────────────────
  echo "==> Verifying Gatekeeper acceptance..."
  spctl --assess --type exec --verbose "$APP"

  # ── Verify notarization staple on .dmg ──────────────────────────────────────
  echo "==> Verifying notarization staple..."
  xcrun stapler validate "$DMG"
fi

echo ""
echo "Build and verification complete."
echo "DMG: $DMG"
