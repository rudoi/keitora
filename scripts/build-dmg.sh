#!/usr/bin/env bash
# scripts/build-dmg.sh — Build a signed (and normally notarized) Keitora .dmg
# Run from the repo root. Requires env vars set before invoking.
#
#   bash scripts/build-dmg.sh              # sign + notarize (notarization vars required)
#   bash scripts/build-dmg.sh --sign-only  # sign with a stable identity, skip notarization
#
# KEITORA_BUNDLE_OVERLAY (optional): path to an extra tauri config merged last, as written by
# scripts/bundle-overlay.ts. Release CI uses it to turn on signed updater artifacts.
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
  # A set counts only when all three variables are non-empty.
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
# A tarball from an earlier build must never be mistaken for this one's.
TARBALL=target/release/bundle/macos/Keitora.app.tar.gz
rm -f "$TARBALL" "$TARBALL.sig"
echo "==> Running tauri build (frontend + sign [+ notarize], with sidecars)..."
yarn tauri build --config src-tauri/tauri.bundle.conf.json ${KEITORA_BUNDLE_OVERLAY:+--config "$KEITORA_BUNDLE_OVERLAY"}

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

# ── Notarize and staple the .dmg ──────────────────────────────────────────────
# Tauri notarizes and staples the .app, but not the .dmg it wraps it in. Submitting the dmg
# lets Gatekeeper accept the download itself, and stapling it lets that check pass offline.
if [[ "$SIGN_ONLY" == 0 ]]; then
  if [[ -n "${APPLE_API_KEY:-}" ]]; then
    NOTARY_AUTH=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
  else
    NOTARY_AUTH=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
  fi
  echo "==> Notarizing the dmg (waits for Apple)..."
  # --wait exits 0 even when Apple rejects the submission, so check the status it reports.
  if ! NOTARY_OUT=$(xcrun notarytool submit "$DMG" "${NOTARY_AUTH[@]}" --wait --timeout 30m --output-format json); then
    echo "$NOTARY_OUT" >&2
    echo "ERROR: notarytool submit failed (credentials, network, or timeout)" >&2
    exit 1
  fi
  read -r NOTARY_ID NOTARY_STATUS < <(printf '%s' "$NOTARY_OUT" |
    node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log((j.id||"-")+" "+(j.status||"-"))})')
  echo "==> Notarization $NOTARY_ID: $NOTARY_STATUS"
  if [[ "$NOTARY_STATUS" != "Accepted" ]]; then
    # The log names every file Apple rejected and why.
    xcrun notarytool log "$NOTARY_ID" "${NOTARY_AUTH[@]}" || true
    echo "ERROR: notarization of $DMG was $NOTARY_STATUS" >&2
    exit 1
  fi
  xcrun stapler staple "$DMG"
fi

# Verify the app that actually ships in the dmg: mount it read-only and check the copy inside.
# Without the `app` bundle target Tauri deletes target/release/bundle/macos/Keitora.app once the
# dmg is built; with it (the updater overlay adds it) the .app stays, and Tauri also packs it into
# Keitora.app.tar.gz for the in-app updater, which is checked below too.
MNT=$(mktemp -d)
TARBALL_DIR=""
cleanup() {
  hdiutil detach -quiet "$MNT" || true
  rmdir "$MNT" 2>/dev/null || true
  if [[ -n "$TARBALL_DIR" ]]; then rm -rf "$TARBALL_DIR"; fi
}
trap cleanup EXIT
hdiutil attach -nobrowse -readonly -mountpoint "$MNT" "$DMG" >/dev/null
APP="$MNT/Keitora.app"

# The signature must verify, and the designated requirement must name the bundle id, not a
# cdhash: TCC stores grants against it, and a cdhash changes on every build.
verify_app() {
  local app="$1" req
  echo "==> Verifying code signature of $app..."
  codesign --verify --deep --strict --verbose=2 "$app"
  req=$(codesign -dr - "$app" 2>&1)
  if [[ "$req" != *'identifier "com.keitora.dispatch"'* ]]; then
    echo "ERROR: designated requirement of $app is not stable:" >&2
    echo "$req" >&2
    exit 1
  fi
  echo "==> Designated requirement is stable"
}

# ── Verify code signature on .app ─────────────────────────────────────────────
verify_app "$APP"

# ── Verify the updater tarball's .app ─────────────────────────────────────────
# An in-app update installs the .app from this tarball, so it must carry the same stable
# signature. It exists only when the overlay turned on updater artifacts (stale ones were
# removed before the build), and then it is required.
if [[ -n "${KEITORA_BUNDLE_OVERLAY:-}" ]] && grep -q '"createUpdaterArtifacts":true' "$KEITORA_BUNDLE_OVERLAY" && [[ ! -f "$TARBALL" ]]; then
  echo "ERROR: updater artifacts were requested but $TARBALL was not built" >&2
  exit 1
fi
if [[ -f "$TARBALL" ]]; then
  TARBALL_DIR=$(mktemp -d)
  tar -xzf "$TARBALL" -C "$TARBALL_DIR"
  verify_app "$TARBALL_DIR/Keitora.app"
  echo "==> Updater tarball: $TARBALL"
fi

# Gatekeeper and the staple only pass for notarized builds.
if [[ "$SIGN_ONLY" == 0 ]]; then
  # ── Verify Gatekeeper acceptance of .app ────────────────────────────────────
  echo "==> Verifying Gatekeeper acceptance..."
  spctl --assess --type exec --verbose "$APP"

  # ── Verify notarization staples ─────────────────────────────────────────────
  echo "==> Verifying notarization staples..."
  xcrun stapler validate "$DMG"
  xcrun stapler validate "$APP"
fi

echo ""
echo "Build and verification complete."
echo "DMG: $DMG"
