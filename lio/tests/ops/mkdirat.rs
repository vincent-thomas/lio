#[path = "../common.rs"]
mod common;

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

use common::poll_until_recv;

#[test]
fn test_mkdirat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path =
    CString::new(format!("/tmp/lio_test_mkdirat_{}", std::process::id()))
      .unwrap();

  unsafe {
    libc::rmdir(path.as_ptr());
  }

  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, path.clone(), 0o755).with_lio(&mut lio).send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("mkdirat should succeed");

  let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
  let stat_result = unsafe { libc::stat(path.as_ptr(), st.as_mut_ptr()) };
  assert_eq!(stat_result, 0, "directory should exist");
  let st = unsafe { st.assume_init() };
  assert_eq!(st.st_mode & libc::S_IFMT, libc::S_IFDIR);

  unsafe {
    libc::rmdir(path.as_ptr());
  }
  std::mem::forget(cwd);
}

#[test]
fn test_mkdirat_existing_path_fails() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path = CString::new(format!(
    "/tmp/lio_test_mkdirat_existing_{}",
    std::process::id()
  ))
  .unwrap();

  unsafe {
    libc::mkdir(path.as_ptr(), 0o755);
  }

  let (sender, receiver) = mpsc::channel();
  api::mkdirat(&cwd, path.clone(), 0o755).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "mkdirat should fail for existing path");

  unsafe {
    libc::rmdir(path.as_ptr());
  }
  std::mem::forget(cwd);
}
