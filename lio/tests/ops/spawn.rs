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

#[test]
fn test_spawn_basic() {
  let mut lio = Lio::new(64).unwrap();
  let path = CString::new("/bin/sh").unwrap();
  let argv = vec![
    CString::new("sh").unwrap(),
    CString::new("-c").unwrap(),
    CString::new("exit 0").unwrap(),
  ];

  let (sender, receiver) = mpsc::channel();
  api::spawn(path, argv, None).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  let pid = result.unwrap();
  assert!(pid.as_raw() > 0);

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
