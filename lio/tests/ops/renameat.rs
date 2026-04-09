#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

use common::poll_until_recv;

#[test]
fn test_renameat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let old_path = CString::new(format!(
    "/tmp/lio_test_renameat_old_{}.txt",
    std::process::id()
  ))
  .unwrap();
  let new_path = CString::new(format!(
    "/tmp/lio_test_renameat_new_{}.txt",
    std::process::id()
  ))
  .unwrap();

  unsafe {
    let fd = libc::open(
      old_path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "failed to create old file");
    libc::close(fd);
    libc::unlink(new_path.as_ptr());
  }

  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, old_path.clone(), &cwd, new_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("renameat should succeed");

  let old_exists = unsafe { libc::access(old_path.as_ptr(), libc::F_OK) == 0 };
  let new_exists = unsafe { libc::access(new_path.as_ptr(), libc::F_OK) == 0 };
  assert!(!old_exists, "old path should be gone");
  assert!(new_exists, "new path should exist");

  unsafe {
    libc::unlink(new_path.as_ptr());
  }
  std::mem::forget(cwd);
}

#[test]
fn test_renameat_missing_source_fails() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let old_path = CString::new(format!(
    "/tmp/lio_test_renameat_missing_{}.txt",
    std::process::id()
  ))
  .unwrap();
  let new_path = CString::new(format!(
    "/tmp/lio_test_renameat_target_{}.txt",
    std::process::id()
  ))
  .unwrap();

  unsafe {
    libc::unlink(old_path.as_ptr());
    libc::unlink(new_path.as_ptr());
  }

  let (sender, receiver) = mpsc::channel();
  api::renameat(&cwd, old_path, &cwd, new_path)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "renameat should fail for missing source");

  std::mem::forget(cwd);
}
