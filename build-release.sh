#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

APP_NAME="iptvscraper"
TARGET_DIR="$ROOT_DIR/target/release"
DIST_DIR="$ROOT_DIR/dist"
BIN="$TARGET_DIR/$APP_NAME"
DIST_BIN="$DIST_DIR/$APP_NAME"

mkdir -p "$DIST_DIR"

# Size-focused release build. Cargo profile handles most size tuning;
# RUSTFLAGS is extra safety for toolchains that support symbol stripping.
export RUSTFLAGS="${RUSTFLAGS:-} -C strip=symbols"

if [[ "${CARGO_LOCKED:-0}" == "1" ]]; then
  cargo build --release --locked
else
  cargo build --release
fi

cp "$BIN" "$DIST_BIN"

# strip may still remove platform-specific symbol data after copy.
if command -v strip >/dev/null 2>&1; then
  strip "$DIST_BIN" 2>/dev/null || true
fi

# Optional executable compression. Install upx for smallest single-file footprint.
if command -v upx >/dev/null 2>&1 && [[ "$(uname -s)" != "Darwin" ]]; then
  upx --best --lzma "$DIST_BIN" >/dev/null 2>&1 || true
fi

BYTES=$(wc -c < "$DIST_BIN" | tr -d ' ')
printf 'Release binary: %s\nSize: %s bytes\n' "$DIST_BIN" "$BYTES"
