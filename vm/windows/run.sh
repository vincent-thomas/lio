#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="${LIO_PROJECT_ROOT:-$(pwd)}"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/lio-vm"

# Configuration
VM_NAME="lio-windows"
VM_MEMORY="4G"
VM_CPUS="4"
# Windows Server 2022 Evaluation (180-day trial, auto-downloadable)
ISO_URL="https://go.microsoft.com/fwlink/p/?LinkID=2195280&clcid=0x409&culture=en-us&country=US"
ISO_FILE="$CACHE_DIR/windows-server-2022.iso"
BASE_IMAGE="$CACHE_DIR/windows-base.qcow2"
PROVISIONED_FILE="$CACHE_DIR/${VM_NAME}-provisioned.qcow2"
DISK_FILE="$CACHE_DIR/${VM_NAME}-disk.qcow2"
FLOPPY_FILE="$CACHE_DIR/autounattend.img"

SHELL_MODE=false
PROVISION_MODE=false
CLEAN_MODE=false
INSTALL_MODE=false

SSH_USER="lio"
SSH_PASS="lio"
SSH_PORT="2223"

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Test lio on Windows VM with IOCP backend.

Options:
  --install           Force fresh Windows installation (takes ~30 min)
  --provision         Force re-provisioning (reinstall Rust)
  --clean             Remove cached images and start fresh
  --shell             Drop to interactive mode instead of running tests
  --memory SIZE       VM memory (default: $VM_MEMORY)
  --cpus N            Number of CPUs (default: $VM_CPUS)
  -h, --help          Show this help

First run will download Windows Server 2022 Evaluation (~5GB) and install it.
Subsequent runs use cached provisioned image.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --install) INSTALL_MODE=true; shift ;;
        --provision) PROVISION_MODE=true; shift ;;
        --clean) CLEAN_MODE=true; shift ;;
        --shell) SHELL_MODE=true; shift ;;
        --memory) VM_MEMORY="$2"; shift 2 ;;
        --cpus) VM_CPUS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1"; usage; exit 1 ;;
    esac
done

setup_firmware() {
    mkdir -p "$CACHE_DIR"
    # Use BIOS mode (simpler, more compatible)
    echo "Using BIOS mode"
    FIRMWARE_OPTS=()
}

check_accel() {
    if [[ -e /dev/kvm ]] && [[ -r /dev/kvm ]] && [[ -w /dev/kvm ]]; then
        echo "Using KVM acceleration"
        ACCEL_OPTS=(-accel kvm -cpu host)
    elif [[ "$(uname)" == "Darwin" ]] && qemu-system-x86_64 -accel help 2>&1 | grep -q hvf; then
        echo "Using HVF acceleration (macOS)"
        ACCEL_OPTS=(-accel hvf -cpu host)
    else
        echo "Warning: No hardware acceleration available, VM will be very slow"
        ACCEL_OPTS=(-cpu qemu64)
    fi
}

download_iso() {
    mkdir -p "$CACHE_DIR"

    if [[ ! -f "$ISO_FILE" ]]; then
        echo "Downloading Windows Server 2022 Evaluation ISO (~5GB)..."
        echo "This is a one-time download."
        curl -L -o "$ISO_FILE.tmp" "$ISO_URL"
        mv "$ISO_FILE.tmp" "$ISO_FILE"
    else
        echo "Using cached Windows ISO"
    fi
}

create_autounattend() {
    echo "Creating autounattend floppy image..."

    local autounattend_xml="$CACHE_DIR/autounattend.xml"
    local setup_ps1="$CACHE_DIR/setup.ps1"

    # Autounattend.xml for unattended Windows installation
    cat > "$autounattend_xml" <<'XMLEOF'
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
    <settings pass="windowsPE">
        <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
            <SetupUILanguage>
                <UILanguage>en-US</UILanguage>
            </SetupUILanguage>
            <InputLocale>en-US</InputLocale>
            <SystemLocale>en-US</SystemLocale>
            <UILanguage>en-US</UILanguage>
            <UserLocale>en-US</UserLocale>
        </component>
        <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
            <DiskConfiguration>
                <Disk wcm:action="add">
                    <CreatePartitions>
                        <CreatePartition wcm:action="add">
                            <Order>1</Order>
                            <Extend>true</Extend>
                            <Type>Primary</Type>
                        </CreatePartition>
                    </CreatePartitions>
                    <ModifyPartitions>
                        <ModifyPartition wcm:action="add">
                            <Order>1</Order>
                            <PartitionID>1</PartitionID>
                            <Format>NTFS</Format>
                            <Label>Windows</Label>
                            <Active>true</Active>
                        </ModifyPartition>
                    </ModifyPartitions>
                    <DiskID>0</DiskID>
                    <WillWipeDisk>true</WillWipeDisk>
                </Disk>
            </DiskConfiguration>
            <ImageInstall>
                <OSImage>
                    <InstallTo>
                        <DiskID>0</DiskID>
                        <PartitionID>1</PartitionID>
                    </InstallTo>
                    <InstallFrom>
                        <MetaData wcm:action="add">
                            <Key>/IMAGE/INDEX</Key>
                            <Value>2</Value>
                        </MetaData>
                    </InstallFrom>
                </OSImage>
            </ImageInstall>
            <UserData>
                <AcceptEula>true</AcceptEula>
                <ProductKey>
                    <Key></Key>
                    <WillShowUI>Never</WillShowUI>
                </ProductKey>
            </UserData>
        </component>
    </settings>
    <settings pass="specialize">
        <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
            <ComputerName>lio-windows</ComputerName>
            <TimeZone>UTC</TimeZone>
        </component>
        <component name="Microsoft-Windows-TerminalServices-LocalSessionManager" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
            <fDenyTSConnections>false</fDenyTSConnections>
        </component>
    </settings>
    <settings pass="oobeSystem">
        <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
            <OOBE>
                <HideEULAPage>true</HideEULAPage>
                <HideLocalAccountScreen>true</HideLocalAccountScreen>
                <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
                <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
                <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
                <ProtectYourPC>3</ProtectYourPC>
                <SkipMachineOOBE>true</SkipMachineOOBE>
                <SkipUserOOBE>true</SkipUserOOBE>
            </OOBE>
            <UserAccounts>
                <AdministratorPassword>
                    <Value>lio</Value>
                    <PlainText>true</PlainText>
                </AdministratorPassword>
                <LocalAccounts>
                    <LocalAccount wcm:action="add">
                        <Name>lio</Name>
                        <Group>Administrators</Group>
                        <Password>
                            <Value>lio</Value>
                            <PlainText>true</PlainText>
                        </Password>
                    </LocalAccount>
                </LocalAccounts>
            </UserAccounts>
            <AutoLogon>
                <Enabled>true</Enabled>
                <Username>lio</Username>
                <Password>
                    <Value>lio</Value>
                    <PlainText>true</PlainText>
                </Password>
                <LogonCount>999</LogonCount>
            </AutoLogon>
            <FirstLogonCommands>
                <SynchronousCommand wcm:action="add">
                    <Order>1</Order>
                    <CommandLine>powershell -ExecutionPolicy Bypass -File A:\setup.ps1</CommandLine>
                    <Description>Run setup script</Description>
                </SynchronousCommand>
            </FirstLogonCommands>
        </component>
    </settings>
</unattend>
XMLEOF

    # PowerShell setup script - enables SSH and installs Rust
    cat > "$setup_ps1" <<'PSEOF'
# Enable OpenSSH Server
Write-Host "=== Installing OpenSSH Server ===" -ForegroundColor Green
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Start-Service sshd
Set-Service -Name sshd -StartupType 'Automatic'

# Configure SSH to use PowerShell as default shell
New-ItemProperty -Path "HKLM:\SOFTWARE\OpenSSH" -Name DefaultShell -Value "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe" -PropertyType String -Force

# Firewall rule for SSH
New-NetFirewallRule -Name sshd -DisplayName 'OpenSSH Server (sshd)' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22

# Install Visual C++ Build Tools (required for Rust)
Write-Host "=== Installing Visual Studio Build Tools ===" -ForegroundColor Green
$vsUrl = "https://aka.ms/vs/17/release/vs_BuildTools.exe"
$vsInstaller = "$env:TEMP\vs_buildtools.exe"
Invoke-WebRequest -Uri $vsUrl -OutFile $vsInstaller
Start-Process -FilePath $vsInstaller -ArgumentList "--quiet", "--wait", "--norestart", "--nocache", "--add", "Microsoft.VisualStudio.Workload.VCTools", "--includeRecommended" -Wait

# Install Rust
Write-Host "=== Installing Rust ===" -ForegroundColor Green
$rustupUrl = "https://win.rustup.rs/x86_64"
$rustupInstaller = "$env:TEMP\rustup-init.exe"
Invoke-WebRequest -Uri $rustupUrl -OutFile $rustupInstaller
Start-Process -FilePath $rustupInstaller -ArgumentList "-y" -Wait

# Add Rust to system PATH
$env:Path = [System.Environment]::GetEnvironmentVariable("Path","Machine") + ";" + [System.Environment]::GetEnvironmentVariable("Path","User")
[Environment]::SetEnvironmentVariable("Path", $env:Path + ";C:\Users\lio\.cargo\bin", "Machine")

# Install cargo-nextest
Write-Host "=== Installing cargo-nextest ===" -ForegroundColor Green
$env:Path += ";C:\Users\lio\.cargo\bin"
& "C:\Users\lio\.cargo\bin\cargo.exe" install cargo-nextest

# Create marker file
New-Item -Path "C:\Users\lio\.provisioned" -ItemType File -Force

Write-Host "=== Provisioning complete ===" -ForegroundColor Green

# Shutdown after provisioning
shutdown /s /t 10
PSEOF

    # Create floppy image with autounattend.xml using mtools (no sudo needed)
    rm -f "$FLOPPY_FILE"

    # Create a FAT12 floppy image
    dd if=/dev/zero of="$FLOPPY_FILE" bs=1024 count=1440 2>/dev/null

    if command -v mkfs.fat &>/dev/null; then
        mkfs.fat -F 12 "$FLOPPY_FILE"
    elif command -v newfs_msdos &>/dev/null; then
        newfs_msdos -F 12 "$FLOPPY_FILE"
    else
        echo "Error: No FAT filesystem tool available (mkfs.fat or newfs_msdos)"
        exit 1
    fi

    # Copy files using mtools (no mounting required)
    if command -v mcopy &>/dev/null; then
        mcopy -i "$FLOPPY_FILE" "$autounattend_xml" ::autounattend.xml
        mcopy -i "$FLOPPY_FILE" "$setup_ps1" ::setup.ps1
    else
        echo "Error: mtools (mcopy) not found"
        exit 1
    fi

    rm -f "$autounattend_xml" "$setup_ps1"
}

run_vm_install() {
    echo "Starting Windows installation..."
    echo "This will take approximately 20-30 minutes."
    echo "The VM will shutdown automatically when done."

    # Simple configuration for Windows install
    local qemu_cmd=(
        qemu-system-x86_64
        "${ACCEL_OPTS[@]}"
        "${FIRMWARE_OPTS[@]}"
        -m "$VM_MEMORY"
        -smp "$VM_CPUS"
        -hda "$BASE_IMAGE"
        -cdrom "$ISO_FILE"
        -boot d
        -drive "file=$FLOPPY_FILE,format=raw,index=0,if=floppy"
        -netdev "user,id=net0"
        -device "e1000,netdev=net0"
        -vga std
        -nographic
    )

    "${qemu_cmd[@]}"
}

run_vm() {
    local disk="$1"
    local extra_args=("${@:2}")

    local qemu_cmd=(
        qemu-system-x86_64
        "${ACCEL_OPTS[@]}"
        "${FIRMWARE_OPTS[@]}"
        -m "$VM_MEMORY"
        -smp "$VM_CPUS"
        -hda "$disk"
        -netdev "user,id=net0,hostfwd=tcp::${SSH_PORT}-:22"
        -device "e1000,netdev=net0"
        -vga std
        "${extra_args[@]}"
    )

    "${qemu_cmd[@]}"
}

wait_for_ssh() {
    echo "Waiting for VM to boot and SSH to become available..."
    local retries=60
    while ! ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -o UserKnownHostsFile=/dev/null \
        -p "$SSH_PORT" "${SSH_USER}@localhost" "echo ok" 2>/dev/null; do
        ((retries--)) || { echo "Timeout waiting for SSH"; return 1; }
        echo "Waiting for SSH... ($retries attempts left)"
        sleep 5
    done
    echo "SSH is ready!"
}

run_ssh() {
    ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -p "$SSH_PORT" "${SSH_USER}@localhost" "$@"
}

run_scp() {
    scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -P "$SSH_PORT" "$@"
}

run_tests() {
    echo "Copying source to VM..."
    local src_archive="$CACHE_DIR/lio-src.tar.gz"
    tar -czf "$src_archive" -C "$PROJECT_ROOT" \
        --exclude=target \
        --exclude=.git \
        --exclude='*.qcow2' \
        .

    run_scp "$src_archive" "${SSH_USER}@localhost:C:/Users/${SSH_USER}/lio-src.tar.gz"

    echo "=== Running tests ==="
    run_ssh <<'REMOTE_SCRIPT'
cd C:\Users\lio
Remove-Item -Recurse -Force lio -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path lio -Force
tar -xzf lio-src.tar.gz -C lio
cd lio
$env:Path += ";C:\Users\lio\.cargo\bin"
cargo nextest run -p lio --all-features --release
REMOTE_SCRIPT

    local result=$?
    rm -f "$src_archive"
    return $result
}

# Main logic
setup_firmware
check_accel

if $CLEAN_MODE; then
    echo "Cleaning cached Windows images..."
    rm -f "$BASE_IMAGE" "$PROVISIONED_FILE" "$DISK_FILE" "$FLOPPY_FILE"
    echo "Done. ISO preserved at $ISO_FILE"
    echo "Run without --clean to reinstall."
    exit 0
fi

# Install Windows if needed
# Check if base image exists and is valid (> 1GB means Windows was installed)
base_image_valid() {
    [[ -f "$BASE_IMAGE" ]] && [[ $(stat -f%z "$BASE_IMAGE" 2>/dev/null || stat -c%s "$BASE_IMAGE" 2>/dev/null || echo 0) -gt 1073741824 ]]
}

if $INSTALL_MODE || ! base_image_valid; then
    echo "=== Windows Installation Required ==="
    download_iso

    echo "Creating disk image..."
    rm -f "$BASE_IMAGE"
    qemu-img create -f qcow2 "$BASE_IMAGE" 60G

    create_autounattend
    run_vm_install

    echo ""
    echo "=== Windows installation complete ==="
fi

# Provision if needed
if $PROVISION_MODE || [[ ! -f "$PROVISIONED_FILE" ]]; then
    echo "=== Provisioning Windows VM ==="
    rm -f "$PROVISIONED_FILE"
    qemu-img create -f qcow2 -b "$BASE_IMAGE" -F qcow2 "$PROVISIONED_FILE" 60G

    # Boot and wait for provisioning to complete
    run_vm "$PROVISIONED_FILE" -nographic &
    vm_pid=$!

    # Wait for SSH, then wait for .provisioned file
    if wait_for_ssh; then
        echo "Waiting for provisioning to complete..."
        while run_ssh "Test-Path C:\\Users\\lio\\.provisioned" 2>/dev/null | grep -q "False"; do
            sleep 10
        done
        echo "Provisioning complete!"
    fi

    wait "$vm_pid" 2>/dev/null || true
    echo ""
    echo "=== Provisioning complete ==="
fi

# Create snapshot for this test run
echo "Creating test snapshot..."
rm -f "$DISK_FILE"
qemu-img create -f qcow2 -b "$PROVISIONED_FILE" -F qcow2 "$DISK_FILE" 60G

if $SHELL_MODE; then
    echo "Launching Windows VM in interactive mode..."
    echo "SSH: ssh -p $SSH_PORT $SSH_USER@localhost (password: $SSH_PASS)"
    echo "Press Ctrl-A X to exit QEMU"
    run_vm "$DISK_FILE" -display sdl
else
    echo "Starting Windows VM for tests..."
    run_vm "$DISK_FILE" -nographic -daemonize

    if wait_for_ssh; then
        run_tests
        TEST_RESULT=$?
    else
        TEST_RESULT=1
    fi

    echo "Shutting down VM..."
    run_ssh "shutdown /s /t 0" 2>/dev/null || true
    sleep 5

    # Kill any remaining QEMU process
    pkill -f "qemu.*${DISK_FILE}" 2>/dev/null || true

    rm -f "$DISK_FILE"
    exit $TEST_RESULT
fi

rm -f "$DISK_FILE"
