#[path = "../common.rs"]
mod common;

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

use common::poll_until_recv;

#[test]
fn test_linkat_hard_link_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let source =
    CString::new(format!("/tmp/lio_test_linkat_source_{}", std::process::id()))
      .unwrap();
  let link =
    CString::new(format!("/tmp/lio_test_linkat_link_{}", std::process::id()))
      .unwrap();

  unsafe {
    libc::unlink(source.as_ptr());
    libc::unlink(link.as_ptr());
    let fd = libc::open(
      source.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "failed to create source file");
    libc::close(fd);
  }

  let (sender, receiver) = mpsc::channel();
  api::linkat(
    &cwd,
    source.clone(),
    &cwd,
    link.clone(),
    api::ops::LinkKind::Hard,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("hard link should succeed");

  let mut st1 = std::mem::MaybeUninit::<libc::stat>::uninit();
  let mut st2 = std::mem::MaybeUninit::<libc::stat>::uninit();
  assert_eq!(unsafe { libc::stat(source.as_ptr(), st1.as_mut_ptr()) }, 0);
  assert_eq!(unsafe { libc::stat(link.as_ptr(), st2.as_mut_ptr()) }, 0);
  let st1 = unsafe { st1.assume_init() };
  let st2 = unsafe { st2.assume_init() };
  assert_eq!(st1.st_ino, st2.st_ino);

  unsafe {
    libc::unlink(link.as_ptr());
    libc::unlink(source.as_ptr());
  }
  std::mem::forget(cwd);
}

#[test]
fn test_linkat_soft_link_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let target = CString::new(format!(
    "/tmp/lio_test_symlinkat_target_{}",
    std::process::id()
  ))
  .unwrap();
  let link = CString::new(format!(
    "/tmp/lio_test_symlinkat_link_{}",
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
  }

  let (sender, receiver) = mpsc::channel();
  api::linkat(
    &cwd,
    target.clone(),
    &cwd,
    link.clone(),
    api::ops::LinkKind::Soft,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("symbolic link should succeed");

  let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
  assert_eq!(unsafe { libc::lstat(link.as_ptr(), st.as_mut_ptr()) }, 0);
  let st = unsafe { st.assume_init() };
  assert_eq!(st.st_mode & libc::S_IFMT, libc::S_IFLNK);

  unsafe {
    libc::unlink(link.as_ptr());
    libc::unlink(target.as_ptr());
  }
  std::mem::forget(cwd);
}
