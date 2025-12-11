fn main() {
  cfg_aliases::cfg_aliases! {
      linux: { target_os = "linux" },
      macos: { target_os = "macos" },
      apple: { target_vendor = "apple" }
  }

  #[cfg(feature = "unstable_ffi")]
  {
    let bindings = cbindgen::generate(".").unwrap();

    std::fs::create_dir_all("./include").unwrap();

    let file = std::fs::OpenOptions::new()
      .create(true)
      .write(true)
      // .truncate(true)
      .open("./include/lio.h")
      .unwrap();

    bindings.write(file);
  }
}
