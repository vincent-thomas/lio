# VM-Based Cross-Platform Testing

This directory contains scripts for testing lio on different platforms using QEMU VMs.

## Prerequisites

- QEMU (`qemu-system-x86_64`) installed on your host
- Sufficient disk space for VM images (~5-20GB per platform)
- For best performance: KVM enabled on Linux hosts

## Quick Start

```bash
# Test on Linux (io_uring backend)
make vm-linux

# Test on Windows (IOCP backend)
make vm-windows

# Test on FreeBSD (kqueue backend)
make vm-freebsd

# Test on all platforms
make vm-all
```

## Detailed Usage

### Linux VM

Tests the io_uring backend on a recent Linux kernel.

```bash
./vm/linux/run.sh [options]

Options:
  --kernel VERSION    Test with specific kernel version (default: latest)
  --keep              Keep VM running after tests
  --shell             Drop to shell instead of running tests
```

The Linux VM uses Ubuntu cloud images with cloud-init for automatic provisioning.

### Windows VM

Tests the IOCP backend on Windows.

```bash
./vm/windows/run.sh [options]

Options:
  --keep              Keep VM running after tests
  --shell             Drop to interactive session
```

**Note:** Windows VMs require a pre-built image or evaluation ISO. On first run,
the script will guide you through setup.

### FreeBSD VM

Tests the kqueue backend on FreeBSD (non-Apple implementation).

```bash
./vm/freebsd/run.sh [options]

Options:
  --keep              Keep VM running after tests
  --shell             Drop to shell instead of running tests
```

## Directory Structure

```
vm/
├── README.md              # This file
├── run.sh                 # Main entry point
├── linux/
│   ├── run.sh             # Linux VM launcher
│   └── cloud-init.yaml    # Auto-provision config
├── windows/
│   └── run.sh             # Windows VM launcher
└── freebsd/
    └── run.sh             # FreeBSD VM launcher
```

## Image Cache

VM images are cached in `~/.cache/lio-vm/`:

```
~/.cache/lio-vm/
├── ubuntu-24.04-cloud.qcow2
├── freebsd-14.qcow2
└── windows-eval.qcow2
```

To clear the cache: `rm -rf ~/.cache/lio-vm`

## How It Works

1. **Image Download**: On first run, downloads cloud images (Ubuntu, FreeBSD) or
   prompts for Windows ISO location
2. **Provisioning**: Uses cloud-init (Linux/FreeBSD) or manual setup (Windows)
   to install Rust toolchain
3. **Source Mounting**: Project source is shared via virtio-9p (Linux/FreeBSD)
   or copied into VM (Windows)
4. **Test Execution**: Runs `cargo nextest run` inside the VM
5. **Results**: Exit code and test output are returned to the host

## Troubleshooting

### "KVM not available"

The VM will run without KVM but significantly slower. To enable KVM on Linux:

```bash
# Check if KVM is available
ls /dev/kvm

# Add user to kvm group if needed
sudo usermod -aG kvm $USER
```

### "Permission denied" mounting source

Ensure QEMU has read access to the project directory.

### Windows VM won't start

Windows VMs are more complex. Consider using a pre-built QEMU Windows image
or the Windows Subsystem for Linux (WSL) for testing.
