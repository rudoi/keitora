#!/usr/bin/env bash
# scripts/macos-local-signing-cert.sh — create a self-signed code-signing identity for
# local Keitora builds, for contributors without an Apple Development / Developer ID
# certificate.
#
# Why: an ad-hoc signed Keitora.app has a designated requirement equal to its cdhash, so
# macOS forgets privacy grants (Screen Recording) on every rebuild. Signing with any real
# identity, even a self-signed one, pins the requirement to the bundle id plus
# certificate, which stays the same across rebuilds.
#
# Idempotent: if the identity already exists, it only prints it. Changes the login
# keychain and may raise macOS authorization prompts (trusting the certificate).
# The certificate is for this machine only; Gatekeeper will still flag copies elsewhere.
set -euo pipefail

NAME="Keitora Local Signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script is for macOS only." >&2
  exit 1
fi

identities() { security find-identity -v -p codesigning 2>/dev/null || true; }

print_export() {
  echo ""
  echo "Use it for bundles with:"
  echo "  export APPLE_SIGNING_IDENTITY=\"$1\""
}

existing=$(identities)
apple=$(printf '%s\n' "$existing" | grep -oE '"(Apple Development|Developer ID Application): [^"]+"' || true)
if [[ -n "$apple" ]]; then
  echo "Apple code-signing identities are already available; prefer one of these:"
  printf '  %s\n' "$apple"
  echo ""
fi

if printf '%s\n' "$existing" | grep -qF "\"$NAME\""; then
  echo "\"$NAME\" already exists."
  print_export "$NAME"
  exit 0
fi

# Imported by an earlier run whose trust step failed or was cancelled: never import a
# second certificate with the same name (codesign would call the name ambiguous).
if security find-certificate -c "$NAME" "$KEYCHAIN" >/dev/null 2>&1; then
  echo "\"$NAME\" is in the keychain but not trusted for code signing yet."
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  security find-certificate -c "$NAME" -p "$KEYCHAIN" >"$tmp/cert.pem"
  echo "==> Trusting the certificate for code signing (macOS may ask you to authorize)..."
  security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$tmp/cert.pem"
  if identities | grep -qF "\"$NAME\""; then
    echo "==> \"$NAME\" is now trusted."
    print_export "$NAME"
    exit 0
  fi
  echo "ERROR: \"$NAME\" is still not a valid identity. Delete it in Keychain Access and rerun." >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

cat >"$tmp/openssl.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $NAME
[ext]
basicConstraints = critical,CA:false
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
subjectKeyIdentifier = hash
EOF

echo "==> Creating a 10-year self-signed code-signing certificate..."
# Long-lived on purpose: a new certificate changes the designated requirement, which
# would invalidate privacy grants again.
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -config "$tmp/openssl.cnf" -keyout "$tmp/key.pem" -out "$tmp/cert.pem" 2>/dev/null

# A transient password for the PKCS#12 bundle; it only lives for this import.
p12pass=$(openssl rand -hex 16)
legacy=()
# OpenSSL 3+ defaults to a PBE that macOS `security` cannot import.
if openssl version | grep -qE '^OpenSSL ([3-9]|[1-9][0-9])\.'; then legacy=(-legacy); fi
openssl pkcs12 -export ${legacy[@]+"${legacy[@]}"} -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
  -name "$NAME" -passout "pass:$p12pass" -out "$tmp/identity.p12"

echo "==> Importing into the login keychain..."
security import "$tmp/identity.p12" -k "$KEYCHAIN" -P "$p12pass" -T /usr/bin/codesign >/dev/null

echo "==> Trusting the certificate for code signing (macOS may ask you to authorize)..."
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$tmp/cert.pem"

# Let codesign use the key without a keychain prompt on every build. This needs the
# login keychain password; an empty one works non-interactively.
if ! security set-key-partition-list -S apple-tool:,apple: -s -l "$NAME" -k "" "$KEYCHAIN" >/dev/null 2>&1; then
  echo ""
  echo "note: could not update the key's access list without your keychain password."
  echo "      To stop codesign prompting on every build, run:"
  echo "        security set-key-partition-list -S apple-tool:,apple: -s -l \"$NAME\" -k <login-password> \"$KEYCHAIN\""
fi

if ! identities | grep -qF "\"$NAME\""; then
  echo "ERROR: \"$NAME\" was imported but is not listed as a valid code-signing identity." >&2
  echo "       Check its trust settings in Keychain Access." >&2
  exit 1
fi

echo "==> Created \"$NAME\"."
print_export "$NAME"
