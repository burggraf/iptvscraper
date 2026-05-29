#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_NAME="iptvscraper"
DIST_DIR="$ROOT_DIR/dist"
DIST_BIN="$DIST_DIR/${APP_NAME}-linux-x64"
IMAGE="${RUST_DOCKER_IMAGE:-rust:1-bookworm}"

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker is required for Linux x64 cross-build" >&2
  exit 1
fi

if ! docker info >/dev/null 2>&1; then
  echo "error: docker daemon is not running; start Docker/OrbStack and retry" >&2
  exit 1
fi

mkdir -p "$DIST_DIR"

docker run --rm \
  --platform linux/amd64 \
  -e HOST_UID="$(id -u)" \
  -e HOST_GID="$(id -g)" \
  -v "$ROOT_DIR:/work" \
  -w /work \
  "$IMAGE" \
  bash -lc "
    set -euo pipefail
    export PATH=\"/usr/local/cargo/bin:\$PATH\"
    apt-get update >/dev/null
    apt-get install -y --no-install-recommends ca-certificates cmake perl pkg-config binutils >/dev/null
    export CARGO_TARGET_DIR=/tmp/iptvscraper-target
    export RUSTFLAGS=\"-C strip=symbols\"
    cargo build --release --locked
    cp \"\$CARGO_TARGET_DIR/release/$APP_NAME\" \"/work/dist/$APP_NAME-linux-x64\"
    strip \"/work/dist/$APP_NAME-linux-x64\" 2>/dev/null || true
    chown \"\$HOST_UID:\$HOST_GID\" \"/work/dist/$APP_NAME-linux-x64\"
  "

BYTES=$(wc -c < "$DIST_BIN" | tr -d ' ')
printf 'Linux x64 release binary: %s\nSize: %s bytes\n' "$DIST_BIN" "$BYTES"
file "$DIST_BIN" || true
