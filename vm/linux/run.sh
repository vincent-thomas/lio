#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="${LIO_PROJECT_ROOT:-$(pwd)}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/lio-vm"

# Determine target architecture
if [[ "${LIO_VM_ARCH:-auto}" == "auto" ]]; then
    case "$(uname -m)" in
        x86_64|amd64) VM_ARCH="x86_64" ;;
        arm64|aarch64) VM_ARCH="aarch64" ;;
        *) echo "Unknown host arch: $(uname -m)"; exit 1 ;;
    esac
else
    VM_ARCH="${LIO_VM_ARCH}"
fi

# Configuration
VM_NAME="lio-linux-${VM_ARCH}"
VM_MEMORY="4G"
VM_CPUS="4"
# Ubuntu 25.10 has kernel 6.11+ for io_uring BIND/LISTEN
UBUNTU_VERSION="25.10"

# Set arch-specific image URL
case "$VM_ARCH" in
    x86_64)
        QEMU_BIN="qemu-system-x86_64"
        IMAGE_URL="https://cloud-images.ubuntu.com/releases/${UBUNTU_VERSION}/release/ubuntu-${UBUNTU_VERSION}-server-cloudimg-amd64.img"
        ;;
    aarch64)
        QEMU_BIN="qemu-system-aarch64"
        IMAGE_URL="https://cloud-images.ubuntu.com/releases/${UBUNTU_VERSION}/release/ubuntu-${UBUNTU_VERSION}-server-cloudimg-arm64.img"
        ;;
esac

IMAGE_FILE="$CACHE_DIR/ubuntu-${UBUNTU_VERSION}-${VM_ARCH}-cloud.qcow2"
PROVISIONED_FILE="$CACHE_DIR/${VM_NAME}-provisioned.qcow2"
DISK_FILE="$CACHE_DIR/${VM_NAME}-disk.qcow2"
SEED_FILE="$CACHE_DIR/${VM_NAME}-seed.img"

SHELL_MODE=false
PROVISION_MODE=false
CLEAN_MODE=false

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Test lio on Linux VM with io_uring backend.

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
        exit 1
    fi
}

check_accel() {
    local host_arch host_os same_arch
    host_arch=$(uname -m)
    host_os=$(uname -s)

    # Normalize host arch to match VM_ARCH format
    case "$host_arch" in
        x86_64|amd64) host_arch="x86_64" ;;
        arm64|aarch64) host_arch="aarch64" ;;
    esac

    same_arch=false
    [[ "$host_arch" == "$VM_ARCH" ]] && same_arch=true

    case "$host_os" in
        Darwin)
            if $same_arch && sysctl -n kern.hv_support 2>/dev/null | grep -q 1; then
                echo "HVF acceleration available ($VM_ARCH native)"
                ACCEL_OPTS=(-accel hvf -cpu host)
            else
                echo "TCG emulation ($VM_ARCH VM on $host_arch Mac)"
                ACCEL_OPTS=(-accel tcg -cpu max)
            fi
            ;;
        Linux)
            if $same_arch && [[ -e /dev/kvm ]] && [[ -r /dev/kvm ]] && [[ -w /dev/kvm ]]; then
                echo "KVM acceleration available"
                ACCEL_OPTS=(-enable-kvm -cpu host)
            else
                echo "TCG emulation ($VM_ARCH VM on $host_arch Linux)"
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
        echo "Downloading Ubuntu ${UBUNTU_VERSION} cloud image..."
        curl -L -o "$IMAGE_FILE.tmp" "$IMAGE_URL"
        qemu-img convert -O qcow2 "$IMAGE_FILE.tmp" "$IMAGE_FILE"
        rm "$IMAGE_FILE.tmp"
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

    cat > "$user_data" <<'CLOUDINIT'
#cloud-config

network:
  version: 2
  ethernets:
    enp0s2:
      dhcp4: true
      optional: true

bootcmd:
  - systemctl mask systemd-networkd-wait-online.service

users:
  - name: lio
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    groups: [sudo]
    lock_passwd: false
    passwd: $6$rounds=4096$salt$xDxBz7R5vqzQbPwfqJvYTvQVqVvXqVvYTvQVqVvXqVvYTvQVqVvXqVvYTvQVqVvX

runcmd:
  - echo "=== Kernel version ==="
  - uname -r
  - echo "=== Updating packages ==="
  - apt-get update
  - DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential clang curl git pkg-config libssl-dev rsync
  - echo "=== Installing Rust ==="
  - su - lio -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
  - touch /home/lio/.provisioned
  - echo "=== Provisioning complete ==="
  - poweroff
CLOUDINIT

    mkisofs -output "$SEED_FILE" -volid cidata -joliet -rock "$user_data" "$meta_data" 2>/dev/null
    rm -f "$meta_data" "$user_data"
}

create_test_seed() {
    echo "Creating test seed image..."
    local meta_data="$CACHE_DIR/meta-data"
    local user_data="$CACHE_DIR/user-data"

    cat > "$meta_data" <<EOF
instance-id: ${VM_NAME}-test-$(date +%s)
local-hostname: ${VM_NAME}
EOF

    cat > "$user_data" <<'CLOUDINIT'
#cloud-config

network:
  version: 2
  ethernets:
    enp0s2:
      dhcp4: true
      optional: true

bootcmd:
  - systemctl mask systemd-networkd-wait-online.service

runcmd:
  - mkdir -p /mnt/lio
  - mount -t 9p -o trans=virtio,version=9p2000.L lio-src /mnt/lio
  - echo "=== Copying source ==="
  - rsync -aL --exclude=target /mnt/lio/ /home/lio/lio/
  - chown -R lio:lio /home/lio/lio
  - echo "=== Building staticlib for FFI tests ==="
  - su - lio -c 'source ~/.cargo/env && cd ~/lio && cargo build -p lio --all-features --release'
  - echo "=== Running lib tests ==="
  - su - lio -c 'source ~/.cargo/env && cd ~/lio && cargo test -p lio --all-features --lib --release --no-fail-fast' || true
  - echo "=== Running integration tests ==="
  - su - lio -c 'source ~/.cargo/env && cd ~/lio && cargo test -p lio --all-features --test "*" --release --no-fail-fast' || true
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

network:
  version: 2
  ethernets:
    enp0s2:
      dhcp4: true
      optional: true

bootcmd:
  - systemctl mask systemd-networkd-wait-online.service

runcmd:
  - mkdir -p /mnt/lio
  - mount -t 9p -o trans=virtio,version=9p2000.L lio-src /mnt/lio || true

write_files:
  - path: /etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin lio --noclear %I 115200 $TERM
    permissions: '0644'
CLOUDINIT

    mkisofs -output "$SEED_FILE" -volid cidata -joliet -rock "$user_data" "$meta_data" 2>/dev/null
    rm -f "$meta_data" "$user_data"
}

run_vm() {
    local disk="$1"
    local capture_output="${2:-false}"
    echo "Starting Linux VM ($VM_ARCH)..."
    echo "Project root: $PROJECT_ROOT"

    local qemu_cmd=(
        "$QEMU_BIN"
        "${ACCEL_OPTS[@]}"
        -m "$VM_MEMORY"
        -smp "$VM_CPUS"
    )

    # Arch-specific options
    case "$VM_ARCH" in
        aarch64)
            qemu_cmd+=(
                -M virt
                -bios "$QEMU_EFI_AARCH64"
                -drive "file=$disk,format=qcow2,if=virtio"
                -drive "file=$SEED_FILE,format=raw,if=virtio"
            )
            ;;
        x86_64)
            qemu_cmd+=(
                -drive "file=$disk,format=qcow2"
                -cdrom "$SEED_FILE"
            )
            ;;
    esac

    qemu_cmd+=(
        -netdev "user,id=net0,hostfwd=tcp::2222-:22"
        -device "virtio-net-pci,netdev=net0"
        -virtfs "local,path=$PROJECT_ROOT,mount_tag=lio-src,security_model=none,id=lio-src"
        -nographic
    )

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
    rm -rf "$CACHE_DIR"
    echo "Done. Run with --provision to create new image."
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

VM_OUTPUT_FILE="$CACHE_DIR/vm-output.log"

if $SHELL_MODE; then
    create_shell_seed
    echo "Launching VM in interactive mode..."
    echo "Login: lio / lio"
    echo "Mount source: already at /mnt/lio"
    echo "Press Ctrl-A X to exit QEMU"
    run_vm "$DISK_FILE" false
    exit_code=0
else
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

# Clean up test snapshot
rm -f "$DISK_FILE"
exit $exit_code
