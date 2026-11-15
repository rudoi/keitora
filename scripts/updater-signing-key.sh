#!/usr/bin/env bash
# scripts/updater-signing-key.sh — generate the Tauri updater signing key and upload it to the repo's
# `release` environment, without the private key or its password ever being printed or typed.
#
#   bash scripts/updater-signing-key.sh
#
# It generates a random password onto the clipboard, runs `tauri signer generate` (paste it at both
# prompts), writes the new public key into src-tauri/tauri.conf.json, proves the pair works by signing
# a scratch file and checking it with scripts/check-updater-sig.ts, then uploads the key and password
# with `gh secret set` over stdin. Finally it hands you the password to back up, with the key file.
#
# Generating a new key strands every installed copy that trusts the old one (see CONTRIBUTING.md,
# "Updater signing"). Only run it when no release with the old pubkey has shipped, or to replace a
# lost key. Run it in your own terminal from the repo root, then commit tauri.conf.json.
set -euo pipefail

[[ -f src-tauri/tauri.conf.json ]] || { echo "ERROR: run from the repo root" >&2; exit 1; }
command -v gh >/dev/null || { echo "ERROR: gh is not installed" >&2; exit 1; }
command -v pbcopy >/dev/null || { echo "ERROR: needs pbcopy (macOS)" >&2; exit 1; }
REPO="${REPO:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}"
gh api "repos/$REPO/environments/release" >/dev/null 2>&1 ||
  { echo "ERROR: $REPO has no 'release' environment; create it first (CONTRIBUTING.md, \"Releases\")" >&2; exit 1; }

DIR="$HOME/keitora-updater"
KEY="$DIR/keitora-updater.key"
mkdir -m 700 "$DIR" || { echo "ERROR: $DIR already exists; move it away and re-run" >&2; exit 1; }

DONE=0
cleanup() {
  pbcopy </dev/null
  rm -f "$DIR/probe" "$DIR/probe.sig"
  # A key that never reached GitHub is useless; don't leave it lying around.
  if [[ "$DONE" != 1 ]]; then
    if [[ -f "$KEY" ]]; then rm -P "$KEY"; fi
    rm -rf "$DIR"
  fi
}
trap cleanup EXIT

PW="$(openssl rand -base64 33 | tr -d '\n')"
printf '%s' "$PW" | pbcopy
echo "==> A random password is on your clipboard (not shown). Paste it at both prompts below."
yarn -s tauri signer generate -w "$KEY"
pbcopy </dev/null
[[ -s "$KEY" && -s "$KEY.pub" ]] || { echo "ERROR: tauri signer did not write $KEY(.pub)" >&2; exit 1; }

# The public key is public: write it into the config the app embeds.
PUB="$(tr -d '\n' <"$KEY.pub")" node -e '
  const fs = require("node:fs");
  const p = "src-tauri/tauri.conf.json";
  const s = fs.readFileSync(p, "utf8");
  const re = /("pubkey"\s*:\s*)"[^"]*"/;
  if (!re.test(s)) { console.error("ERROR: no updater pubkey in " + p); process.exit(1); }
  fs.writeFileSync(p, s.replace(re, (_, k) => k + JSON.stringify(process.env.PUB)));
'
echo "==> Wrote the new pubkey to src-tauri/tauri.conf.json"

# Prove the key, password and committed pubkey agree before anything is uploaded. The key and password
# reach tauri through the environment, never argv.
printf 'probe\n' >"$DIR/probe"
TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$PW" \
  yarn -s tauri signer sign "$DIR/probe" >/dev/null
yarn -s tsx scripts/check-updater-sig.ts "$DIR/probe.sig"

gh secret set TAURI_SIGNING_PRIVATE_KEY --env release --repo "$REPO" <"$KEY"
printf '%s' "$PW" | gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --env release --repo "$REPO"
DONE=1
echo "==> Uploaded TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD (release env)"

cat <<EOF

Back up the key now. Losing it strands every installed copy.
  - the key file (encrypted with the password): $KEY
  - the password: on your clipboard now; save it in your password manager.
EOF
printf '%s' "$PW" | pbcopy
unset PW
read -r -p "Press Enter once both are backed up (the clipboard is then cleared)... "
pbcopy </dev/null
read -r -p "Delete the local key file now? [y/N] " ans
if [[ "$ans" == [yY]* ]]; then
  rm -P "$KEY"
  rm -rf "$DIR"
  echo "==> Deleted $DIR"
else
  echo "==> Kept $DIR; delete it once it is backed up offline."
fi
echo "==> Next: commit src-tauri/tauri.conf.json (the new pubkey)."
