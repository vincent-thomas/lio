#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="${LIO_PROJECT_ROOT:-$(pwd)}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/lio-vm"

# OpenIndiana only supports x86_64
VM_ARCH="x86_64"
QEMU_BIN="qemu-system-x86_64"

# Configuration
VM_NAME="lio-illumos-${VM_ARCH}"
VM_MEMORY="4G"
VM_CPUS="4"
OI_VERSION="20251026"

IMAGE_URL="https://dlc.openindiana.org/isos/hipster/${OI_VERSION}/OI-hipster-cloudimage.img.zst"
IMAGE_FILE="$CACHE_DIR/openindiana-${OI_VERSION}.qcow2"
PROVISIONED_FILE="$CACHE_DIR/${VM_NAME}-provisioned.qcow2"
DISK_FILE="$CACHE_DIR/${VM_NAME}-disk.qcow2"
SEED_FILE="$CACHE_DIR/${VM_NAME}-seed.img"

SHELL_MODE=false
PROVISION_MODE=false
CLEAN_MODE=false

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Test lio on illumos/OpenIndiana VM with event ports backend.

Options:
  --provision         Force re-provisioning (reinstall Rust)
  --clean             Remove cached images and start fresh
  --shell             Drop to shell instead of running tests
  --memory SIZE       VM memory (default: $VM_MEMORY)
  --cpus N            Number of CPUs (default: $VM_CPUS)
  -h, --help          Show this help

Auto-provisions on first run if needed.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --provision) PROVISION_MODE=true; shift ;;
        --clean) CLEAN_MODE=true; shift ;;
        --shell) SHELL_MODE=true; shift ;;
        --memory) VM_MEMORY="$2"; shift 2 ;;
        --cpus) VM_CPUS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1"; usage; exit 1 ;;
    esac
done

check_qemu() {
    if ! command -v "$QEMU_BIN" &>/dev/null; then
        echo "Error: $QEMU_BIN not found"
        echo "Install QEMU: brew install qemu (macOS) or apt install qemu-system (Linux)"
        exit 1
    fi
}

check_accel() {
    local host_arch host_os
    host_arch=$(uname -m)
    host_os=$(uname -s)

    # Normalize host arch
    case "$host_arch" in
        x86_64|amd64) host_arch="x86_64" ;;
        arm64|aarch64) host_arch="aarch64" ;;
    esac

    case "$host_os" in
        Darwin)
            if [[ "$host_arch" == "x86_64" ]] && sysctl -n kern.hv_support 2>/dev/null | grep -q 1; then
                echo "HVF acceleration available"
                ACCEL_OPTS=(-accel hvf -cpu host)
            else
                echo "TCG emulation (x86_64 VM on $host_arch Mac)"
                ACCEL_OPTS=(-accel tcg -cpu max)
            fi
            ;;
        Linux)
            if [[ "$host_arch" == "x86_64" ]] && [[ -e /dev/kvm ]] && [[ -r /dev/kvm ]] && [[ -w /dev/kvm ]]; then
                echo "KVM acceleration available"
                ACCEL_OPTS=(-enable-kvm -cpu host)
            else
                echo "TCG emulation (x86_64 VM on $host_arch Linux)"
                ACCEL_OPTS=(-accel tcg -cpu max)
            fi
            ;;
        *)
            echo "TCG emulation (unknown platform)"
            ACCEL_OPTS=(-accel tcg -cpu max)
            ;;
    esac
}

download_image() {
    mkdir -p "$CACHE_DIR"

    if [[ ! -f "$IMAGE_FILE" ]]; then
        echo "Downloading OpenIndiana ${OI_VERSION} cloud image..."
        curl -L -o "$IMAGE_FILE.zst" "$IMAGE_URL"
        echo "Extracting image..."
        zstd -d "$IMAGE_FILE.zst" -o "$IMAGE_FILE"
        rm -f "$IMAGE_FILE.zst"
    fi
}

create_provision_seed() {
    echo "Creating provisioning seed image..."
    local meta_data="$CACHE_DIR/meta-data"
    local user_data="$CACHE_DIR/user-data"

    cat > "$meta_data" <<EOF
instance-id: ${VM_NAME}-provision
local-hostname: ${VM_NAME}
EOF

    # OpenIndiana uses cloud-init
    # Install Rust via rustup (x86_64 is supported)
    cat > "$user_data" <<'CLOUDINIT'
#cloud-config

users:
  - name: lio
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    groups: [root]
    lock_passwd: false

runcmd:
  - echo "=== illumos version ==="
  - uname -a
  - cat /etc/release
  - echo "=== Installing packages ==="
  - pkg install -v build-essential git pkg-config
  - echo "=== Installing Rust ==="
  - su - lio -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
  - touch /export/home/lio/.provisioned
  - echo "=== Provisioning complete ==="
  - poweroff
CLOUDINIT

    mkisofs -output "$SEED_FILE" -volid cidata -joliet -rock "$user_data" "$meta_data" 2>/dev/null
    rm -f "$meta_data" "$user_data"
}

# shellcheck disable=SC2329
create_source_disk() {
    echo "Creating source disk image..."
    local src_tar="$CACHE_DIR/lio-src.tar"
    local src_disk="$CACHE_DIR/lio-src-illumos.img"

    # Create tarball of source (excluding target dir)
    tar -cf "$src_tar" -C "$PROJECT_ROOT" --exclude='target' --exclude='.git' .

    # Create a 100MB disk and write tar to it
    dd if=/dev/zero of="$src_disk" bs=1M count=100 2>/dev/null
    dd if="$src_tar" of="$src_disk" conv=notrunc 2>/dev/null
    rm -f "$src_tar"

    SOURCE_DISK="$src_disk"
}

create_test_seed() {
    echo "Creating test seed image..."
    local meta_data="$CACHE_DIR/meta-data"
    local user_data="$CACHE_DIR/user-data"

    cat > "$meta_data" <<EOF
instance-id: ${VM_NAME}-test-$(date +%s)
local-hostname: ${VM_NAME}
EOF

    # illumos: extract source from secondary disk (raw tar)
    cat > "$user_data" <<'CLOUDINIT'
#cloud-config

runcmd:
  - echo "=== Extracting source from disk ==="
  - rm -rf /export/home/lio/lio
  - mkdir -p /export/home/lio/lio
  - dd if=/dev/vdb bs=1M 2>/dev/null | tar -xf - -C /export/home/lio/lio
  - chown -R lio:staff /export/home/lio/lio
  - echo "=== Building staticlib for FFI tests ==="
  - su - lio -c 'source ~/.cargo/env && cd ~/lio && cargo build -p lio --all-features --release'
  - echo "=== Running lib tests ==="
  - su - lio -c 'source ~/.cargo/env && cd ~/lio && cargo test -p lio --all-features --lib --release --no-fail-fast' || true
  - echo "=== Running integration tests ==="
  - su - lio -c 'source ~/.cargo/env && cd ~/lio && cargo test -p lio --all-features --test "*" --release --no-fail-fast' || true
  - echo "=== Tests complete ==="
  - poweroff
CLOUDINIT

    mkisofs -output "$SEED_FILE" -volid cidata -joliet -rock "$user_data" "$meta_data" 2>/dev/null
    rm -f "$meta_data" "$user_data"
}

create_shell_seed() {
    echo "Creating shell seed image..."
    local meta_data="$CACHE_DIR/meta-data"
    local user_data="$CACHE_DIR/user-data"

    cat > "$meta_data" <<EOF
instance-id: ${VM_NAME}-shell-$(date +%s)
local-hostname: ${VM_NAME}
EOF

    cat > "$user_data" <<'CLOUDINIT'
#cloud-config

runcmd:
  - echo "Shell mode - source disk at /dev/vdb"
CLOUDINIT

    mkisofs -output "$SEED_FILE" -volid cidata -joliet -rock "$user_data" "$meta_data" 2>/dev/null
    rm -f "$meta_data" "$user_data"
}

run_vm() {
    local disk="$1"
    local capture_output="${2:-false}"
    echo "Starting illumos VM (x86_64)..."
    echo "Project root: $PROJECT_ROOT"

    local qemu_cmd=(
        "$QEMU_BIN"
        "${ACCEL_OPTS[@]}"
        -m "$VM_MEMORY"
        -smp "$VM_CPUS"
        -drive "file=$disk,format=qcow2,if=virtio"
        -cdrom "$SEED_FILE"
        -netdev "user,id=net0"
        -device "virtio-net-pci,netdev=net0"
        -nographic
    )

    # Attach source disk if it exists (for test runs)
    if [[ -n "${SOURCE_DISK:-}" ]] && [[ -f "$SOURCE_DISK" ]]; then
        qemu_cmd+=(-drive "file=$SOURCE_DISK,format=raw,if=virtio,readonly=on")
    fi

    if [[ "$capture_output" == "true" ]]; then
        "${qemu_cmd[@]}" | tee "$VM_OUTPUT_FILE"
    else
        "${qemu_cmd[@]}"
    fi
}

# Main logic
check_qemu
check_accel

if $CLEAN_MODE; then
    echo "Cleaning cached images..."
    rm -f "$IMAGE_FILE" "$PROVISIONED_FILE" "$DISK_FILE" "$SEED_FILE" "$CACHE_DIR/lio-src-illumos.img"
    echo "Done. Run again to download fresh images."
    exit 0
fi

download_image

provision() {
    echo "=== Provisioning ==="
    rm -f "$PROVISIONED_FILE" "$SEED_FILE"

    echo "Creating fresh disk for provisioning..."
    qemu-img create -f qcow2 -b "$IMAGE_FILE" -F qcow2 "$PROVISIONED_FILE" 20G

    create_provision_seed

    echo "Booting VM to install Rust and dependencies..."
    echo "(This will take a few minutes)"
    run_vm "$PROVISIONED_FILE"

    echo ""
    echo "=== Provisioning complete ==="
}

# Auto-provision if needed, or force if --provision flag
if $PROVISION_MODE || [[ ! -f "$PROVISIONED_FILE" ]]; then
    provision
fi

# Create a snapshot for this test run (preserves provisioned image)
echo "Creating test snapshot..."
rm -f "$DISK_FILE"
qemu-img create -f qcow2 -b "$PROVISIONED_FILE" -F qcow2 "$DISK_FILE" 20G

VM_OUTPUT_FILE="$CACHE_DIR/vm-output-illumos.log"

if $SHELL_MODE; then
    create_shell_seed
    create_source_disk
    echo "Launching VM in interactive mode..."
    echo "Login: lio"
    echo "Source disk at /dev/vdb (extract: dd if=/dev/vdb | tar -xf - -C ~/lio)"
    echo "Press Ctrl-A X to exit QEMU"
    run_vm "$DISK_FILE" false
    exit_code=0
else
    create_source_disk
    create_test_seed
    echo "Running tests..."
    run_vm "$DISK_FILE" true

    # Check for test failures in the output
    if grep -q "test result: FAILED" "$VM_OUTPUT_FILE" 2>/dev/null; then
        echo ""
        echo "=== TESTS FAILED ==="
        exit_code=1
    elif grep -q "error: test failed" "$VM_OUTPUT_FILE" 2>/dev/null; then
        echo ""
        echo "=== TESTS FAILED ==="
        exit_code=1
    elif grep -q "error\[E" "$VM_OUTPUT_FILE" 2>/dev/null; then
        echo ""
        echo "=== COMPILATION FAILED ==="
        exit_code=1
    else
        echo ""
        echo "=== ALL TESTS PASSED ==="
        exit_code=0
    fi
    rm -f "$VM_OUTPUT_FILE"
fi

# Clean up test snapshot and source disk
rm -f "$DISK_FILE" "${SOURCE_DISK:-}"
exit $exit_code
