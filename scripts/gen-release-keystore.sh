#!/usr/bin/env bash
# Generate a persistent release signing keystore.
# Output: release-signing/nairobi-release.jks
#
# Preferred: `./build.sh keystore` — runs this inside the builder container, so
# no host JDK is needed. You may also run it directly on the host if you have a
# JDK (`keytool`) installed.
#
# Required env:
#   NAIROBI_KEYSTORE_PASSWORD   keystore password
# Optional:
#   NAIROBI_KEY_ALIAS           key alias (default: nairobi)
#   NAIROBI_KEY_PASSWORD        key password (default: = keystore password)

set -euo pipefail
# Resolve the repo root so this works both inside the container (cwd /work) and
# when invoked directly from the host as ./scripts/gen-release-keystore.sh.
cd "$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

OUT_DIR=release-signing
KEYSTORE="$OUT_DIR/nairobi-release.jks"
ALIAS="${NAIROBI_KEY_ALIAS:-nairobi}"
STOREPASS="${NAIROBI_KEYSTORE_PASSWORD:?set NAIROBI_KEYSTORE_PASSWORD}"
KEYPASS="${NAIROBI_KEY_PASSWORD:-$STOREPASS}"

mkdir -p "$OUT_DIR"
if [ -f "$KEYSTORE" ]; then
    echo "error: $KEYSTORE already exists (refusing to overwrite)" >&2
    exit 1
fi

keytool -genkeypair -v \
    -keystore "$KEYSTORE" \
    -alias "$ALIAS" \
    -keyalg RSA -keysize 4096 -validity 10000 \
    -storepass "$STOREPASS" -keypass "$KEYPASS" \
    -dname "CN=nairobi2, OU=nairobi2, O=nairobi2, C=KE"

# Hand the keystore back to the host user (rootful Docker creates it as root).
if [ -n "${CHOWN_UID:-}" ]; then
    chown -R "${CHOWN_UID}:${CHOWN_GID:-$CHOWN_UID}" "$OUT_DIR" 2>/dev/null || true
fi

echo "==> Created $KEYSTORE (alias=$ALIAS)."
echo "    Keep this file AND the passwords safe — they are required to ship"
echo "    updates that Android will accept over an installed build."
