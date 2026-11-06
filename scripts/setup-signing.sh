#!/bin/bash
# One-time: creates a personal code-signing certificate ("Lanes Local Signing")
# in your login keychain. Builds signed with it keep the same identity, so macOS
# remembers the Accessibility permission across rebuilds.
set -euo pipefail
NAME="Lanes Local Signing"
if security find-certificate -c "$NAME" >/dev/null 2>&1; then
  echo "Certificate \"$NAME\" already exists."
  exit 0
fi
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout "$TMP/key.pem" -out "$TMP/cert.pem" -subj "/CN=$NAME" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null
PASS=$(openssl rand -hex 16)
# macOS's keychain only reads the older PKCS#12 encryption.
openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -name "$NAME" \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -out "$TMP/lanes.p12" -passout "pass:$PASS"
security import "$TMP/lanes.p12" -k "$HOME/Library/Keychains/login.keychain-db" \
  -P "$PASS" -T /usr/bin/codesign
echo "Created \"$NAME\" in your login keychain."
