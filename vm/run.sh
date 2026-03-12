#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
    cat <<EOF
Usage: $(basename "$0") <platform> [options]

Platforms:
  linux     Test io_uring backend on Linux
  windows   Test IOCP backend on Windows
  freebsd   Test kqueue backend on FreeBSD
  all       Test on all platforms

Options:
  --keep    Keep VM running after tests
  --shell   Drop to shell instead of running tests
  -h        Show this help

Examples:
  $(basename "$0") linux
  $(basename "$0") windows --shell
  $(basename "$0") all
EOF
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

PLATFORM="$1"
shift

case "$PLATFORM" in
    linux)
        exec "$SCRIPT_DIR/linux/run.sh" "$@"
        ;;
    windows)
        exec "$SCRIPT_DIR/windows/run.sh" "$@"
        ;;
    freebsd)
        exec "$SCRIPT_DIR/freebsd/run.sh" "$@"
        ;;
    all)
        echo "=== Testing on Linux ==="
        "$SCRIPT_DIR/linux/run.sh" "$@"
        echo ""
        echo "=== Testing on FreeBSD ==="
        "$SCRIPT_DIR/freebsd/run.sh" "$@"
        echo ""
        echo "=== Testing on Windows ==="
        "$SCRIPT_DIR/windows/run.sh" "$@"
        echo ""
        echo "=== All platforms passed ==="
        ;;
    -h|--help)
        usage
        exit 0
        ;;
    *)
        echo "Error: Unknown platform '$PLATFORM'"
        usage
        exit 1
        ;;
esac
