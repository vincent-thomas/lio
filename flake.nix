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
      in
      {
        packages = {
          default = import ./default.nix { inherit pkgs; };
          test = pkgs.writeShellApplication {
            name = "lio-test";
            text =
              (
                if pkgs.stdenv.hostPlatform.isLinux then
                  ''
                    export C_INCLUDE_PATH=${pkgs.linuxHeaders}/include
                    export LIBCLANG_PATH=${pkgs.llvmPackages.libclang.lib}/lib
                    export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.llvmPackages.clang}/resource-root/include -I${pkgs.glibc.dev}/include"
                  ''
                else
                  ""
              )
              + builtins.readFile ./scripts/test.sh;
            runtimeInputs =
              with pkgs;
              [
                (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
                jq
                cargo-nextest
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.llvmPackages.libclang.lib
                pkgs.llvmPackages.clang
                pkgs.glibc.dev
              ];
          };
          test-ffi = pkgs.writeShellApplication {
            name = "lio-test-ffi";
            text =
              (
                if pkgs.stdenv.hostPlatform.isLinux then
                  ''
                    export C_INCLUDE_PATH=${pkgs.linuxHeaders}/include
                    export LIBCLANG_PATH=${pkgs.llvmPackages.libclang.lib}/lib
                    export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.llvmPackages.clang}/resource-root/include -I${pkgs.glibc.dev}/include"
                  ''
                else if pkgs.stdenv.hostPlatform.isDarwin then
                  ''
                    export CC=clang
                    export CXX=clang++
                    export LIBRARY_PATH=${pkgs.libiconv}/lib${"$"}{LIBRARY_PATH:+:${"$"}LIBRARY_PATH}
                    export CPATH=${pkgs.libiconv}/include${"$"}{CPATH:+:${"$"}CPATH}
                  ''
                else
                  ""
              )
              + builtins.readFile ./scripts/test-ffi.sh;
            runtimeInputs =
              with pkgs;
              [
                (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
                pkg-config
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                clang
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                gcc
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.llvmPackages.libclang.lib
                pkgs.llvmPackages.clang
                pkgs.glibc.dev
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                pkgs.libiconv
              ];
          };
          test-uring = pkgs.writeShellApplication {
            name = "lio-test-uring";
            text =
              (
                if pkgs.stdenv.hostPlatform.isLinux then
                  ''
                    export C_INCLUDE_PATH=${pkgs.linuxHeaders}/include
                    export LIBCLANG_PATH=${pkgs.llvmPackages.libclang.lib}/lib
                    export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.llvmPackages.clang}/resource-root/include -I${pkgs.glibc.dev}/include"
                  ''
                else
                  ""
              )
              + builtins.readFile ./scripts/test-lio-uring.sh;
            runtimeInputs =
              with pkgs;
              [
                (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.llvmPackages.libclang.lib
                pkgs.llvmPackages.clang
                pkgs.glibc.dev
              ];
          };
          lint = pkgs.writeShellApplication {
            name = "lio-lint";
            text = builtins.readFile ./scripts/lint.sh;
            runtimeInputs = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              cargo-deny
            ];
          };
          lint-full = pkgs.writeShellApplication {
            name = "lio-lint-full";
            text = builtins.readFile ./scripts/lint-full.sh;
            runtimeInputs = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              cargo-deny
              cargo-hack
            ];
          };
        };
      }
    );
}
