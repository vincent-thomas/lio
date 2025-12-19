#!/usr/bin/env bash
set -e

# Determine project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TARGET_DIR="$PROJECT_ROOT/target/release"

echo $TARGET_DIR

echo "Building the Nix package..."
nix build -L

echo ""
echo "Setting PKG_CONFIG_PATH..."
PKGCONFIG_DIR="$(readlink -f $PROJECT_ROOT/result)/lib/pkgconfig"

which pkg-config

# Nix's pkg-config wrapper uses architecture-specific variables
# Detect if we're in a nix environment and set the appropriate variable
if [ -n "${NIX_PKG_CONFIG_WRAPPER_TARGET_TARGET_aarch64_unknown_linux_gnu:-}" ]; then
    export PKG_CONFIG_PATH_aarch64_unknown_linux_gnu="$PKGCONFIG_DIR"
    echo "PKG_CONFIG_PATH_aarch64_unknown_linux_gnu=$PKG_CONFIG_PATH_aarch64_unknown_linux_gnu"
elif [ -n "${NIX_PKG_CONFIG_WRAPPER_TARGET_TARGET_x86_64_unknown_linux_gnu:-}" ]; then
    export PKG_CONFIG_PATH_x86_64_unknown_linux_gnu="$PKGCONFIG_DIR"
    echo "PKG_CONFIG_PATH_x86_64_unknown_linux_gnu=$PKG_CONFIG_PATH_x86_64_unknown_linux_gnu"
elif [ -n "${NIX_PKG_CONFIG_WRAPPER_TARGET_TARGET_x86_64_apple_darwin:-}" ]; then
    export PKG_CONFIG_PATH_x86_64_apple_darwin="$PKGCONFIG_DIR"
    echo "PKG_CONFIG_PATH_x86_64_apple_darwin=$PKG_CONFIG_PATH_x86_64_apple_darwin"
elif [ -n "${NIX_PKG_CONFIG_WRAPPER_TARGET_TARGET_aarch64_apple_darwin:-}" ]; then
    export PKG_CONFIG_PATH_aarch64_apple_darwin="$PKGCONFIG_DIR"
    echo "PKG_CONFIG_PATH_aarch64_apple_darwin=$PKG_CONFIG_PATH_aarch64_apple_darwin"
else
    # Fallback to standard PKG_CONFIG_PATH for non-nix environments
    echo "Falling back, no nix env"
    export PKG_CONFIG_PATH="$PKGCONFIG_DIR"
    echo "PKG_CONFIG_PATH=$PKG_CONFIG_PATH"
fi
echo ""
echo "Testing pkg-config queries..."
echo "1. Check if lio.pc is found:"
pkg-config --libs lio
pkg-config --exists lio && echo "   ✓ lio.pc found" || echo "   ✗ lio.pc not found"

echo ""
echo "2. Package version:"
pkg-config --modversion lio

echo ""
echo "3. Compiler flags:"
pkg-config --cflags lio

echo ""
echo "4. Linker flags:"
pkg-config --libs lio

echo ""
echo "5. Static linker flags:"
pkg-config --libs --static lio

echo ""
echo "6. All variables:"
pkg-config --print-variables lio

echo ""
echo "Compiling test program with pkg-config (dynamic linking)..."
gcc $SCRIPT_DIR/test_pkgconfig.c $(pkg-config --cflags --libs lio) -o $TARGET_DIR/test_pkgconfig

echo ""
echo "Running test program (dynamic)..."

$TARGET_DIR/test_pkgconfig

echo ""
echo "Compiling test program with static lio library..."
# On macOS, we can't fully statically link, but we can statically link our library
# We need to extract the library path and link directly to the .a file
LIO_LIBDIR=$(pkg-config --variable=libdir lio)
gcc $SCRIPT_DIR/test_pkgconfig.c $(pkg-config --cflags lio) $LIO_LIBDIR/liblio.a -lpthread -o $TARGET_DIR/test_pkgconfig_static

echo ""
echo "Verifying static linking of lio library..."
# Check that lio is not in the dynamic dependencies
if otool -L $TARGET_DIR/test_pkgconfig_static 2>/dev/null | grep -q liblio.dylib; then
    echo "   ✗ Binary is dynamically linking liblio.dylib"
    otool -L $TARGET_DIR/test_pkgconfig_static
    exit 1
elif ldd $TARGET_DIR/test_pkgconfig_static 2>/dev/null | grep -q liblio; then
    echo "   ✗ Binary is dynamically linking liblio"
    ldd $TARGET_DIR/test_pkgconfig_static
    exit 1
else
    echo "   ✓ lio library is statically linked"
fi

echo ""
echo "Running test program (static)..."

$TARGET_DIR/test_pkgconfig_static

echo ""
echo "✓ All tests passed!"
