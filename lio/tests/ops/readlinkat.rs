#[path = "../common.rs"]
mod common;

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

use common::poll_until_recv;

#[test]
fn test_readlinkat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let target = CString::new(format!(
    "/tmp/lio_test_readlinkat_target_{}",
    std::process::id()
  ))
  .unwrap();
  let link = CString::new(format!(
    "/tmp/lio_test_readlinkat_link_{}",
    std::process::id()
  ))
  .unwrap();

  unsafe {
    libc::unlink(target.as_ptr());
    libc::unlink(link.as_ptr());
    let fd = libc::open(
      target.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "failed to create target file");
    libc::close(fd);
    assert_eq!(libc::symlink(target.as_ptr(), link.as_ptr()), 0);
  }

  let (sender, receiver) = mpsc::channel();
  api::readlinkat(&cwd, link.clone(), vec![0; 512])
    .with_lio(&mut lio)
    .send_with(sender);

  let (result, buf) = poll_until_recv(&mut lio, &receiver);
  let bytes = result.unwrap() as usize;
  assert_eq!(&buf[..bytes], target.as_bytes());

  unsafe {
    libc::unlink(link.as_ptr());
    libc::unlink(target.as_ptr());
  }
  std::mem::forget(cwd);
}

#[test]
fn test_readlinkat_missing_path_fails() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let link = CString::new(format!(
    "/tmp/lio_test_readlinkat_missing_{}",
    std::process::id()
  ))
  .unwrap();

  unsafe {
    libc::unlink(link.as_ptr());
  }

  let (sender, receiver) = mpsc::channel();
  api::readlinkat(&cwd, link, vec![0; 64]).with_lio(&mut lio).send_with(sender);

  let (result, _buf) = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "readlinkat should fail for missing path");

  std::mem::forget(cwd);
}
