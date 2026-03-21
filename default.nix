{ pkgs }:
let
  cargoToml = builtins.fromTOML (builtins.readFile ./lio/Cargo.toml);
  pname = cargoToml.package.name;
  version = cargoToml.package.version;
  description = cargoToml.package.description;

in
pkgs.rustPlatform.buildRustPackage {
  inherit pname version;

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = with pkgs; [
    (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
    gnumake
    cargo-nextest
    clang
    llvmPackages.libclang
    llvmPackages.clang
  ] ++ lib.optionals stdenv.isLinux [
    linuxHeaders
    glibc.dev
  ];

  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

  buildPhase = ''
    runHook preBuild
    ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
      export C_INCLUDE_PATH=${pkgs.linuxHeaders}/include
      export BINDGEN_EXTRA_CLANG_ARGS="-I${pkgs.llvmPackages.clang}/resource-root/include -I${pkgs.glibc.dev}/include"
    ''}
    make cbuild
    runHook postBuild
  '';

  checkPhase = ''
    runHook cargoCheckHook
    ./scripts/test-ffi.sh
  '';

  installPhase = ''
    mkdir -p $out/lib $out/include $out/lib/pkgconfig
    cp lio/include/lio.h $out/include/
    cp target/release/liblio${pkgs.stdenv.hostPlatform.extensions.sharedLibrary} $out/lib/
    cp target/release/liblio${pkgs.stdenv.hostPlatform.extensions.staticLibrary} $out/lib/

    # Fix install name on macOS
    ${pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
      install_name_tool -id $out/lib/liblio${pkgs.stdenv.hostPlatform.extensions.sharedLibrary} $out/lib/liblio${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}
    ''}

    # Generate pkg-config file
    cat > $out/lib/pkgconfig/${pname}.pc << EOF
    prefix=$out
    libdir=$out/lib
    includedir=$out/include

    Name: ${pname}
    Description: ${description}
    Version: ${version}
    Libs: -L$out/lib -l${pname} -Wl,-rpath,$out/lib
    Libs.private: -lpthread ${if pkgs.stdenv.isLinux then "-ldl" else ""}
    Cflags: -I$out/include
    EOF
  '';

  meta = with pkgs.lib; {
    inherit description;
    license = licenses.mit;
    platforms = platforms.unix;
  };
}
