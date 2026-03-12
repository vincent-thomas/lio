//! Additional tests for openat operation.
//!
//! Note: Basic openat tests are in test_file_ops.rs. This file contains
//! additional scenarios and Linux-specific tests.

mod common;

use common::{TempFile, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::mpsc;

#[test]
fn test_openat_with_directory_fd() {
  let mut lio = Lio::new(64).unwrap();

  // Open /tmp directory
  let tmp_path = CString::new("/tmp").unwrap();
  let dir_fd = unsafe {
    libc::open(tmp_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "Failed to open /tmp directory");

  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let (sender, receiver) = mpsc::channel();

  // Open file relative to directory fd
  let file_path =
    CString::new(format!("lio_test_openat_dirfd_{}.txt", std::process::id()))
      .unwrap();
  api::openat(
    &dir_res,
    file_path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to open file with directory fd");

  assert!(fd.as_fd().as_raw_fd() >= 0, "File descriptor should be valid");

  // Cleanup
  let full_path = CString::new(format!(
    "/tmp/lio_test_openat_dirfd_{}.txt",
    std::process::id()
  ))
  .unwrap();
  unsafe {
    libc::unlink(full_path.as_ptr());
  }
}

#[test]
fn test_openat_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let (sender, receiver) = mpsc::channel();

  let mut temps = Vec::new();

  // Submit multiple openat operations
  for i in 0..10 {
    let temp = TempFile::new(&format!("openat_concurrent_{}", i));
    api::openat(
      &cwd,
      temp.path.clone(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    )
    .with_lio(&mut lio)
    .send_with(sender.clone());
    temps.push(temp);
  }

  // Collect all results
  let mut fds = Vec::new();
  for i in 0..10 {
    let fd = poll_until_recv(&mut lio, &receiver)
      .expect(&format!("Failed to open file {}", i));
    assert!(fd.as_fd().as_raw_fd() >= 0);
    fds.push(fd);
  }

  // All fds should be unique
  let raw_fds: Vec<_> = fds.iter().map(|f| f.as_fd().as_raw_fd()).collect();
  for i in 0..raw_fds.len() {
    for j in i + 1..raw_fds.len() {
      assert_ne!(raw_fds[i], raw_fds[j], "File descriptors should be unique");
    }
  }

  std::mem::forget(cwd);
}

#[test]
fn test_openat_append_mode() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("openat_append");

  // Create file with initial data
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"Hello".as_ptr() as *const libc::c_void, 5);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Open in append mode
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, temp.path.clone(), libc::O_WRONLY | libc::O_APPEND)
    .with_lio(&mut lio)
    .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to open in append mode");

  // Write more data (should append)
  unsafe {
    libc::write(
      fd.as_fd().as_raw_fd(),
      b"World".as_ptr() as *const libc::c_void,
      5,
    );
  }

  // Verify content
  unsafe {
    let read_fd = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    let mut buf = vec![0u8; 20];
    let n = libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 20);
    libc::close(read_fd);
    assert_eq!(n, 10);
    assert_eq!(&buf[..10], b"HelloWorld");
  }

  std::mem::forget(cwd);
}

#[test]
fn test_openat_excl_flag() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path = CString::new(format!(
    "/tmp/lio_test_openat_excl_{}.txt",
    std::process::id()
  ))
  .unwrap();

  // First create should succeed
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, path.clone(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR)
    .with_lio(&mut lio)
    .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver)
    .expect("First O_EXCL create should succeed");
  drop(fd);

  // Second create with O_EXCL should fail (file exists)
  let (sender2, receiver2) = mpsc::channel();
  api::openat(&cwd, path.clone(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR)
    .with_lio(&mut lio)
    .send_with(sender2);

  let result = poll_until_recv(&mut lio, &receiver2);
  assert!(result.is_err(), "O_EXCL should fail when file exists");

  // Cleanup
  unsafe {
    libc::unlink(path.as_ptr());
  }

  std::mem::forget(cwd);
}

#[test]
fn test_openat_permission_denied() {
  // Skip if running as root (root can read anything)
  if unsafe { libc::getuid() } == 0 {
    return;
  }

  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("openat_perm");

  // Create file with no permissions
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o000,
    );
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Try to open - should fail with permission denied
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, temp.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "Should fail with permission denied");

  // Restore permissions for cleanup
  unsafe {
    libc::chmod(temp.path.as_ptr(), 0o644);
  }

  std::mem::forget(cwd);
}

#[test]
fn test_openat_directory() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path = CString::new("/tmp").unwrap();

  // Open directory with O_DIRECTORY
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, path.clone(), libc::O_RDONLY | libc::O_DIRECTORY)
    .with_lio(&mut lio)
    .send_with(sender);

  let fd =
    poll_until_recv(&mut lio, &receiver).expect("Failed to open directory");
  assert!(fd.as_fd().as_raw_fd() >= 0);

  std::mem::forget(cwd);
}

#[test]
fn test_openat_directory_flag_on_file() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("openat_dir_flag");

  // Create a regular file
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Try to open file with O_DIRECTORY - should fail
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, temp.path.clone(), libc::O_RDONLY | libc::O_DIRECTORY)
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "O_DIRECTORY on regular file should fail");

  std::mem::forget(cwd);
}
