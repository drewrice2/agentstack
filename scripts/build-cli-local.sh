#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need cargo
need rustc

if command -v sha256sum >/dev/null 2>&1; then
  SHA256_CMD=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
  SHA256_CMD=(shasum -a 256)
else
  echo "missing required command: sha256sum or shasum" >&2
  exit 1
fi

VERSION="$(cargo metadata --no-deps --format-version 1 \
  | sed -n 's/.*"name":"agentstack","version":"\([^"]*\)".*/\1/p' \
  | head -1)"
if [ -z "$VERSION" ]; then
  echo "failed to read agentstack package version" >&2
  exit 1
fi

HOST="$(rustc -vV | awk '/^host:/ { print $2 }')"
if [ -z "$HOST" ]; then
  echo "failed to detect rustc host triple" >&2
  exit 1
fi

echo "version: $VERSION"
echo "host:    $HOST"

cargo build --release --bin agentstack

OUT_DIR="$ROOT/dist/agentstack-$VERSION-$HOST"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
cp "$ROOT/target/release/agentstack" "$OUT_DIR/agentstack"
(cd "$OUT_DIR" && "${SHA256_CMD[@]}" agentstack > SHA256SUMS)

"$OUT_DIR/agentstack" --version
echo "artifact:  $OUT_DIR/agentstack"
echo "checksum:  $OUT_DIR/SHA256SUMS"
echo "install:   install -m 0755 \"$OUT_DIR/agentstack\" \"\${HOME}/.local/bin/agentstack\""
