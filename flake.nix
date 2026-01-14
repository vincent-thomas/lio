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
        inherit (nixpkgs) lib;
      in
      {
        packages.default = import ./default.nix { inherit pkgs; };

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
              # LD_LIBRARY_PATH = "${pkgs.stdenv.cc.cc.lib}/lib";
            };
          in
          {
            ci = pkgs.mkShell (
              sharedEnvVars
              // {
                packages = with pkgs; [
                  cargo-deny
                  cargo-hack
                ];
              }
            );
            default = pkgs.mkShell sharedEnvVars;
          };
      }
    );
}
