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
            nativeBuildInputs = with pkgs; [
              (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
              gcc
              gnumake
              pkg-config-unwrapped

              cargo-nextest
            ];
          in
          {
            ci = pkgs.mkShell {
              inherit nativeBuildInputs;
              packages = [
                cargo-deny
                cargo-audit
                cargo-hack
              ];
            };
            default = pkgs.mkShell {
              buildInputs = nativeBuildInputs;
              RUST_BACKTRACE = "1";
            };
          };
      }
    );
}
