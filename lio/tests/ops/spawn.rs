#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::{Lio, api};
use std::{ffi::CString, sync::mpsc};

use common::poll_until_recv;

/// A shell command that exits successfully, as `(program, argv)`.
fn exit_zero_command() -> (CString, Vec<CString>) {
  let cstr = |s: &str| CString::new(s).unwrap();
  #[cfg(unix)]
  return (cstr("/bin/sh"), vec![cstr("sh"), cstr("-c"), cstr("exit 0")]);
  #[cfg(windows)]
  return (
    cstr(r"C:\Windows\System32\cmd.exe"),
    vec![cstr("cmd"), cstr("/C"), cstr("exit 0")],
  );
}

#[test]
fn test_spawn_basic() {
  let mut lio = Lio::new(64).unwrap();
  let (path, argv) = exit_zero_command();

  let (sender, receiver) = mpsc::channel();
  api::spawn(path, argv, None).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  let pid = result.unwrap();
  assert!(pid.as_raw() > 0);

  // Reap the child so it does not linger as a zombie.
  #[cfg(unix)]
  // SAFETY: `pid` is our child and is waited on exactly once.
  unsafe {
    libc::waitpid(pid.as_raw() as libc::pid_t, std::ptr::null_mut(), 0);
  }
}

#[test]
fn test_spawn_missing_path_fails() {
  let mut lio = Lio::new(64).unwrap();
  let path = CString::new("/definitely/missing/spawn-binary").unwrap();
  let argv = vec![CString::new("spawn-binary").unwrap()];

  let (sender, receiver) = mpsc::channel();
  api::spawn(path, argv, None).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "spawn should fail for a missing executable");
}
