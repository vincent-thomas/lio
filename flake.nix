{
  description = "A Rust project development environment using Nix flakes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
    }:
    {
      overlays.default = final: prev: {
        lio = self.packages.${final.system}.default;
      };
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # OS-grouped test definitions
        # Each OS has: auto (host arch), x86_64, aarch64 variants
        mkLinuxTest = arch: {
          description = "Test on Linux ${arch} via QEMU VM";
          runtimeInputs = with pkgs; [
            qemu
            curl
            cdrtools
          ];
          script = ''
            export LIO_VM_ARCH="${arch}"
            export QEMU_EFI_AARCH64="${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd"
            ${builtins.readFile ./vm/linux/run.sh}
          '';
        };

        mkWindowsTest = arch: {
          description = "Test on Windows ${arch} via QEMU VM (IOCP backend)";
          runtimeInputs = with pkgs; [
            qemu
            curl
            dosfstools
            mtools
            gnutar
            gzip
          ];
          script = ''
            export LIO_VM_ARCH="${arch}"
            export OVMF_CODE="${pkgs.qemu}/share/qemu/edk2-x86_64-code.fd"
            export OVMF_VARS="${pkgs.qemu}/share/qemu/edk2-i386-vars.fd"
            ${builtins.readFile ./vm/windows/run.sh}
          '';
        };

        mkFreebsdTest = arch: {
          description = "Test on FreeBSD ${arch} via QEMU VM (kqueue backend)";
          runtimeInputs = with pkgs; [
            qemu
            curl
            cdrtools
          ];
          script = ''
            export LIO_VM_ARCH="${arch}"
            export QEMU_EFI_AARCH64="${pkgs.qemu}/share/qemu/edk2-aarch64-code.fd"
            ${builtins.readFile ./vm/freebsd/run.sh}
          '';
        };

        mkIllumosTest = {
          description = "Test on illumos/OpenIndiana via QEMU VM (event ports backend)";
          runtimeInputs = with pkgs; [
            qemu
            curl
            cdrtools
            zstd
          ];
          script = builtins.readFile ./vm/illumos/run.sh;
        };

        osTests = {
          # Auto-detect host arch
          linux = mkLinuxTest "auto";
          windows = mkWindowsTest "auto";
          freebsd = mkFreebsdTest "auto";
          illumos = mkIllumosTest;

          # Explicit arch variants
          linux-x86_64 = mkLinuxTest "x86_64";
          linux-aarch64 = mkLinuxTest "aarch64";
          windows-x86_64 = mkWindowsTest "x86_64";
          freebsd-x86_64 = mkFreebsdTest "x86_64";
          freebsd-aarch64 = mkFreebsdTest "aarch64";

          # Native test on current platform
          native = {
            description = "Test natively on current platform";
            runtimeInputs = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              stdenv.cc.cc
              clang
              cargo-nextest
            ];
            script = ''
              echo "=== Native test on $(uname -s) $(uname -m) ==="
              cargo nextest run --all-features --release
            '';
          };
        };

        mkOsTest =
          name: cfg:
          pkgs.writeShellApplication {
            name = "test-${name}";
            runtimeInputs = cfg.runtimeInputs;
            text = ''
              set -e
              echo "=== ${cfg.description} ==="
              ${cfg.script}
            '';
          };

        testPackages = pkgs.lib.mapAttrs mkOsTest osTests;

        apps = pkgs.lib.mapAttrs (name: pkg: {
          type = "app";
          program = "${pkg}/bin/test-${name}";
        }) testPackages;
      in
      {
        inherit apps;
        packages = {
          default = import ./default.nix { inherit pkgs; };
        }
        // testPackages;

        devShells =
          let
            sharedEnvVars = {
              nativeBuildInputs = with pkgs; [
                (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
                stdenv.cc.cc
                gnumake
                cargo-nextest

                clang
              ];
              RUST_BACKTRACE = "1";
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            };
          in
          {
            ci = pkgs.mkShell (
              sharedEnvVars
              // {
                packages = with pkgs; [
                  cargo-deny
                  cargo-hack
                  cargo-release
                ];
              }
            );
            default = pkgs.mkShell sharedEnvVars;
          };
      }
    );
}
