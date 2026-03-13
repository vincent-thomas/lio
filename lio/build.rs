fn main() {
  cfg_aliases::cfg_aliases! {
      linux: { target_os = "linux" },
      macos: { target_os = "macos" },
      apple: { target_vendor = "apple" },
      kqueue: { any(
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "visionos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
      )
    }
  }

  #[cfg(feature = "unstable_ffi")]
  generate_c_bindings();
}

#[cfg(feature = "unstable_ffi")]
fn generate_c_bindings() {
  // Re-run if ffi.rs changes
  println!("cargo:rerun-if-changed=src/ffi.rs");
  println!("cargo:rerun-if-changed=cbindgen.toml");

  let crate_dir =
    std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
  let out = std::path::PathBuf::from(&crate_dir).join("include/lio.h");

  let config = cbindgen::Config::from_file(
    std::path::PathBuf::from(&crate_dir).join("cbindgen.toml"),
  )
  .expect("failed to read cbindgen.toml");

  let bindings = cbindgen::Builder::new()
    .with_crate(&crate_dir)
    .with_config(config)
    .generate()
    .expect("cbindgen failed");

  // Write to a temporary string so we can strip stray blank lines that
  // cbindgen emits for opaque types it can't represent in C.
  let mut raw = Vec::new();
  bindings.write(&mut raw);
  let cleaned = String::from_utf8(raw)
    .expect("cbindgen output is not UTF-8")
    .lines()
    .fold((String::new(), false), |(mut acc, was_blank), line| {
      let is_blank = line.trim().is_empty();
      if !(is_blank && was_blank) {
        acc.push_str(line);
        acc.push('\n');
      }
      (acc, is_blank)
    })
    .0;
  std::fs::write(&out, cleaned).expect("failed to write lio.h");
}
