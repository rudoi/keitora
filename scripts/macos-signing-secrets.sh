#!/usr/bin/env bash
# scripts/macos-signing-secrets.sh — upload the macOS signing identity to the repo's `release`
# environment for release.yml, without the key or its password ever being printed or typed.
#
#   bash scripts/macos-signing-secrets.sh <SHA-1>
#
# <SHA-1> is the full 40-character hash from `security find-identity -v -p codesigning`.
# The script generates a random export password and puts it on the clipboard, you export that one
# identity from Keychain Access into a private folder, and the script verifies the .p12 is the
# identity you named, uploads it with `gh secret set` (values go over stdin, never argv or output),
# sets the matching variables, then shreds the file and clears the clipboard.
#
# Run it in your own terminal: it waits for you to finish the Keychain Access export.
# Re-run it whenever the certificate is renewed.
set -euo pipefail

SHA1="${1:-}"
if ! [[ "$SHA1" =~ ^[0-9A-Fa-f]{40}$ ]]; then
  echo "usage: bash scripts/macos-signing-secrets.sh <40-character SHA-1>" >&2
  echo "       (from: security find-identity -v -p codesigning)" >&2
  exit 1
fi
SHA1="$(printf '%s' "$SHA1" | tr '[:lower:]' '[:upper:]')"

[[ "$(uname)" == Darwin ]] || { echo "ERROR: run this on the Mac that holds the identity" >&2; exit 1; }
command -v gh >/dev/null || { echo "ERROR: gh is not installed" >&2; exit 1; }
REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"

# The identity must be valid here (chain and expiry), or CI's identical check would fail anyway.
LINE="$(security find-identity -v -p codesigning | grep -F "$SHA1" || true)"
[[ -n "$LINE" ]] || { echo "ERROR: no valid code-signing identity with SHA-1 $SHA1 in your keychains" >&2; exit 1; }
NAME="$(printf '%s' "$LINE" | sed -E 's/^[^"]*"(.*)"$/\1/')"
echo "==> Identity: $NAME"
echo "==> Repo:     $REPO (environment: release)"

gh api "repos/$REPO/environments/release" >/dev/null 2>&1 ||
  { echo "ERROR: $REPO has no 'release' environment; create it first (CONTRIBUTING.md, \"Releases\")" >&2; exit 1; }

# A short, fixed path so it is easy to type in the save dialog; created private, refused if it exists.
WORK="$HOME/keitora-signing-export"
mkdir -m 700 "$WORK" || { echo "ERROR: $WORK already exists; remove it and re-run" >&2; exit 1; }
P12="$WORK/signing.p12"
cleanup() {
  if [[ -f "$P12" ]]; then rm -P "$P12"; fi
  rm -rf "$WORK"
  pbcopy </dev/null
}
trap cleanup EXIT

PW="$(openssl rand -base64 33 | tr -d '\n')"
printf '%s' "$PW" | pbcopy

cat <<EOF

A random export password is on your clipboard (it is not shown). In Keychain Access:
  1. login keychain -> My Certificates
  2. right-click "$NAME" -> Export...
  3. Save As: signing.p12, format "Personal Information Exchange (.p12)",
     folder: ~/keitora-signing-export  (press Cmd-Shift-G in the save dialog and type it)
  4. paste the clipboard into both password fields, then allow the keychain access prompt.
Exporting from the GUI exports only this identity; \`security export\` would export every one.

EOF
open -a "Keychain Access" || true
read -r -p "Press Enter once signing.p12 is saved... "
pbcopy </dev/null

[[ -s "$P12" ]] || { echo "ERROR: $P12 not found" >&2; exit 1; }

# The password goes to openssl through the environment, not argv. /usr/bin/openssl (LibreSSL) reads the
# Keychain export format without OpenSSL 3's -legacy flag.
export P12_PASS="$PW"
GOT="$(/usr/bin/openssl pkcs12 -in "$P12" -nokeys -clcerts -passin env:P12_PASS 2>/dev/null |
  /usr/bin/openssl x509 -noout -fingerprint -sha1 2>/dev/null | sed 's/.*=//; s/://g' || true)"
unset P12_PASS
if [[ -z "$GOT" ]]; then
  echo "ERROR: could not open the .p12 (wrong password or format)" >&2; exit 1
fi
if [[ "$GOT" != "$SHA1" ]]; then
  echo "ERROR: the .p12 holds certificate $GOT, not $SHA1; export the right identity" >&2; exit 1
fi
echo "==> .p12 verified against $SHA1"

base64 -i "$P12" | gh secret set MACOS_CERTIFICATE_P12 --env release --repo "$REPO"
printf '%s' "$PW" | gh secret set MACOS_CERTIFICATE_PASSWORD --env release --repo "$REPO"
unset PW
gh variable set MACOS_SIGNING_SHA1 --body "$SHA1" --repo "$REPO"
gh variable set MACOS_SIGNING_IDENTITY --body "$NAME" --repo "$REPO"

echo "==> Done: secrets MACOS_CERTIFICATE_P12, MACOS_CERTIFICATE_PASSWORD (release env);"
echo "    variables MACOS_SIGNING_SHA1, MACOS_SIGNING_IDENTITY. The .p12 is shredded on exit."
