//! Integration tests for the lio FFI (C API).
//!
//! These tests compile the C test programs in tests/ffi/ with both C and C++
//! compilers to verify the header works with both languages.

#![cfg(unix)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn get_lib_dir() -> PathBuf {
  let out_dir = env::var("OUT_DIR").ok();

  if let Some(out) = out_dir {
    let p = PathBuf::from(out);
    if let Some(target_dir) = p.ancestors().nth(3) {
      return target_dir.to_path_buf();
    }
  }

  let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
  let target_dir =
    PathBuf::from(&manifest_dir).parent().unwrap().join("target");

  if target_dir.join("debug").join("liblio.a").exists() {
    target_dir.join("debug")
  } else if target_dir.join("release").join("liblio.a").exists() {
    target_dir.join("release")
  } else {
    target_dir.join("debug")
  }
}

fn get_include_dir() -> PathBuf {
  let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
  PathBuf::from(&manifest_dir).join("include")
}

fn get_ffi_test_dir() -> PathBuf {
  let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
  PathBuf::from(&manifest_dir).join("tests").join("ffi")
}

fn get_c_compiler() -> &'static str {
  if Command::new("clang").arg("--version").output().is_ok() {
    "clang"
  } else {
    "gcc"
  }
}

fn get_cpp_compiler() -> &'static str {
  if Command::new("clang++").arg("--version").output().is_ok() {
    "clang++"
  } else {
    "g++"
  }
}

fn compile_test(source: &Path, output: &Path, compiler: &str) -> Output {
  let lib_dir = get_lib_dir();
  let include_dir = get_include_dir();
  let ffi_test_dir = get_ffi_test_dir();

  #[cfg(target_os = "macos")]
  {
    let static_lib = lib_dir.join("liblio.a");
    if static_lib.exists() {
      return Command::new(compiler)
        .arg(source)
        .arg("-o")
        .arg(output)
        .arg(format!("-I{}", include_dir.display()))
        .arg(format!("-I{}", ffi_test_dir.display()))
        .arg(&static_lib)
        .arg("-lpthread")
        .arg("-framework")
        .arg("CoreFoundation")
        .output()
        .expect("Failed to execute compiler");
    }
  }

  #[cfg(target_os = "linux")]
  {
    let static_lib = lib_dir.join("liblio.a");
    if static_lib.exists() {
      return Command::new(compiler)
        .arg(source)
        .arg("-o")
        .arg(output)
        .arg(format!("-I{}", include_dir.display()))
        .arg(format!("-I{}", ffi_test_dir.display()))
        .arg(&static_lib)
        .arg("-lpthread")
        .arg("-lm")
        .output()
        .expect("Failed to execute compiler");
    }
  }

  Command::new(compiler)
    .arg(source)
    .arg("-o")
    .arg(output)
    .arg(format!("-I{}", include_dir.display()))
    .arg(format!("-I{}", ffi_test_dir.display()))
    .arg(format!("-L{}", lib_dir.display()))
    .arg("-llio")
    .arg("-lpthread")
    .output()
    .expect("Failed to execute compiler")
}

fn run_test_binary(binary: &Path) -> Output {
  let lib_dir = get_lib_dir();
  let mut cmd = Command::new(binary);

  #[cfg(target_os = "macos")]
  cmd.env("DYLD_LIBRARY_PATH", &lib_dir);

  #[cfg(target_os = "linux")]
  cmd.env("LD_LIBRARY_PATH", &lib_dir);

  cmd.output().expect("Failed to execute test binary")
}

fn run_c_test(test_name: &str) {
  run_test_with_compiler(test_name, get_c_compiler(), "C");
}

fn run_cpp_test(test_name: &str) {
  run_test_with_compiler(test_name, get_cpp_compiler(), "C++");
}

fn run_test_with_compiler(test_name: &str, compiler: &str, lang: &str) {
  let ffi_test_dir = get_ffi_test_dir();
  let source = ffi_test_dir.join(format!("{}.c", test_name));

  assert!(source.exists(), "Test source not found: {}", source.display());

  let temp_dir = env::temp_dir().join("lio_ffi_tests");
  std::fs::create_dir_all(&temp_dir).expect("Failed to create temp dir");

  let output_binary = temp_dir.join(format!("{}_{}", test_name, lang));

  let compile_result = compile_test(&source, &output_binary, compiler);

  if !compile_result.status.success() {
    eprintln!("=== {} compilation failed for {} ===", lang, test_name);
    eprintln!("stdout: {}", String::from_utf8_lossy(&compile_result.stdout));
    eprintln!("stderr: {}", String::from_utf8_lossy(&compile_result.stderr));
    panic!("{} compilation failed for {}", lang, test_name);
  }

  let run_result = run_test_binary(&output_binary);
  let stdout = String::from_utf8_lossy(&run_result.stdout);
  let stderr = String::from_utf8_lossy(&run_result.stderr);

  if !stdout.is_empty() {
    println!("{}", stdout);
  }

  if !run_result.status.success() {
    eprintln!("=== {} test failed: {} ===", lang, test_name);
    if !stderr.is_empty() {
      eprintln!("stderr: {}", stderr);
    }
    panic!(
      "{} test {} failed with exit code: {:?}",
      lang,
      test_name,
      run_result.status.code()
    );
  }

  let _ = std::fs::remove_file(&output_binary);
}

/// Generate C and C++ test functions for an FFI test.
macro_rules! ffi_test {
  ($name:ident) => {
    pastey::paste! {
        #[test]
        fn [<$name _c>]() {
            run_c_test(stringify!($name));
        }

        #[test]
        fn [<$name _cpp>]() {
            run_cpp_test(stringify!($name));
        }
    }
  };
}

ffi_test!(test_lifecycle);
ffi_test!(test_timeout);
ffi_test!(test_file_ops);
ffi_test!(test_socket_ops);
ffi_test!(test_send_recv);
ffi_test!(test_error_handling);
