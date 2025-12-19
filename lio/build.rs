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

  // #[cfg(target_os = "linux")]
  // build_liburing();
}

// #[cfg(target_os = "linux")]
// fn build_liburing() {
//   use std::{
//     env, fs,
//     path::{Path, PathBuf},
//     process::Command,
//   };
//
//   let project = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
//     .canonicalize()
//     .unwrap();
//   let liburing = project.join("liburing");
//
//   let configured_include = configure(&liburing);
//
//   let src = liburing.join("src");
//
//   cc::Build::new()
//     .file(src.join("setup.c"))
//     .file(src.join("queue.c"))
//     .file(src.join("syscall.c"))
//     .file(src.join("register.c"))
//     .include(src.join("include"))
//     .include(configured_include)
//     .flag("-include")
//     .flag("fcntl.h")
//     .flag("-include")
//     .flag("sys/wait.h")
//     .extra_warnings(false)
//     .compile("uring");
//
//   let bindings = bindgen::Builder::default()
//     .header("liburing_wrapper.h")
//     .clang_arg(format!("-I{}/src/include", liburing.display()))
//     .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
//     .generate()
//     .unwrap();
//
//   let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
//
//   bindings.write_to_file(out_path.join("bindings.rs")).unwrap();
//
//   fn configure(liburing: &Path) -> PathBuf {
//     let out_dir =
//       PathBuf::from(env::var("OUT_DIR").unwrap()).canonicalize().unwrap();
//     fs::copy(liburing.join("configure"), out_dir.join("configure")).unwrap();
//     fs::create_dir_all(out_dir.join("src/include/liburing")).unwrap();
//     Command::new("./configure")
//       .current_dir(&out_dir)
//       .output()
//       .expect("configure script failed");
//     out_dir.join("src/include")
//   }
// }
