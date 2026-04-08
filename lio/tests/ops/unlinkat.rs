#[path = "../common.rs"]
mod common;

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

use common::poll_until_recv;

#[test]
fn test_unlinkat_file() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path = CString::new(format!(
    "/tmp/lio_test_unlinkat_file_{}.txt",
    std::process::id()
  ))
  .unwrap();

  unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "failed to create test file");
    libc::close(fd);
  }

  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, path.clone(), 0).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("unlinkat should succeed");

  let exists = unsafe { libc::access(path.as_ptr(), libc::F_OK) == 0 };
  assert!(!exists, "file should be removed");

  std::mem::forget(cwd);
}

#[test]
fn test_unlinkat_directory() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path =
    CString::new(format!("/tmp/lio_test_unlinkat_dir_{}", std::process::id()))
      .unwrap();

  unsafe {
    let rc = libc::mkdir(path.as_ptr(), 0o755);
    assert_eq!(rc, 0, "failed to create test directory");
  }

  let (sender, receiver) = mpsc::channel();
  api::unlinkat(&cwd, path.clone(), libc::AT_REMOVEDIR)
    .with_lio(&mut lio)
    .send_with(sender);

  poll_until_recv(&mut lio, &receiver)
    .expect("unlinkat AT_REMOVEDIR should succeed");

  let exists = unsafe { libc::access(path.as_ptr(), libc::F_OK) == 0 };
  assert!(!exists, "directory should be removed");

  std::mem::forget(cwd);
}
