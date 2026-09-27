#!/usr/bin/env bash
# One-time setup: create a stable self-signed code-signing certificate
# ("SenClaw Dev") in the login keychain and trust it for code signing.
#
# Why: `make app-install` used to leave the app ad-hoc signed. Ad-hoc
# signatures change on every build, so macOS TCC (Screen Recording, etc.)
# silently drops its grant after each reinstall — the toggle stays ON in
# System Settings but access is denied. Signing every build with the SAME
# certificate gives the app a stable code-signing identity, so TCC grants
# survive rebuilds/reinstalls.
#
# Usage: scripts/macos_make_signing_cert.sh
#   - macOS will show ONE password dialog when trusting the cert, and ONE
#     "codesign wants to sign" keychain dialog on first build — click
#     "Always Allow" there.
set -euo pipefail

CN="SenClaw Dev"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -v -p codesigning | grep -q "$CN"; then
  echo "identity '$CN' already exists — nothing to do"
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> generating self-signed code-signing certificate '$CN' (10 years)"
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -days 3650 \
  -subj "/CN=$CN" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning"

# OpenSSL 3 defaults the PKCS#12 MAC to PBMAC1/SHA-256, which macOS's
# `security import` cannot verify — it fails with "MAC verification failed
# during PKCS12 import (wrong password?)", pointing at the password rather than
# the algorithm. Pin the legacy SHA-1/3DES trio that Keychain understands.
openssl pkcs12 -export -out "$TMP/cert.p12" \
  -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -passout pass:senclaw \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1

echo "==> importing into login keychain"
security import "$TMP/cert.p12" -k "$KEYCHAIN" -P senclaw -T /usr/bin/codesign

echo "==> trusting for code signing (a macOS password dialog will appear)"
security add-trusted-cert -p codeSign -k "$KEYCHAIN" "$TMP/cert.pem"

echo "==> done:"
security find-identity -v -p codesigning | grep "$CN" || {
  echo "identity not visible yet — if codesign later fails, open Keychain Access,"
  echo "find '$CN', set Trust > Code Signing = Always Trust."
}
echo "Now rebuild + reinstall: make app-build app-install"
echo "After the FIRST reinstall, re-grant Screen Recording once; it will stick from then on."
