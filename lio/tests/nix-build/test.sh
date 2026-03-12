#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
OUT_DIR="$PROJECT_ROOT/target/nix-test"

mkdir -p "$OUT_DIR"

echo "==> Building nix package..."
nix build -L

RESULT_DIR="$(readlink -f "$PROJECT_ROOT/result")"
export PKG_CONFIG_PATH="$RESULT_DIR/lib/pkgconfig"

echo "==> Verifying pkg-config..."
pkg-config --exists lio
echo "    version: $(pkg-config --modversion lio)"

echo "==> Compiling test (dynamic)..."
gcc "$SCRIPT_DIR/test_pkgconfig.c" $(pkg-config --cflags --libs lio) -o "$OUT_DIR/test_dynamic"

echo "==> Running test (dynamic)..."
"$OUT_DIR/test_dynamic"

echo "==> Compiling test (static)..."
LIO_LIBDIR=$(pkg-config --variable=libdir lio)
gcc "$SCRIPT_DIR/test_pkgconfig.c" $(pkg-config --cflags lio) "$LIO_LIBDIR/liblio.a" -lpthread -o "$OUT_DIR/test_static"

echo "==> Verifying static link..."
if otool -L "$OUT_DIR/test_static" 2>/dev/null | grep -q liblio.dylib; then
    echo "FAIL: liblio.dylib found in dynamic deps"
    exit 1
elif ldd "$OUT_DIR/test_static" 2>/dev/null | grep -q liblio; then
    echo "FAIL: liblio found in dynamic deps"
    exit 1
fi

echo "==> Running test (static)..."
"$OUT_DIR/test_static"

echo "==> All tests passed"
