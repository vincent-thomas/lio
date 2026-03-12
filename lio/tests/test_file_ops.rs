//! Tests for file operations: fsync, linkat, symlink, nop, and openat fixes.

mod common;

use common::{TempFile, poll_recv, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::sync::mpsc;

// ============================================================================
// Nop tests
// ============================================================================

#[test]
fn test_nop_basic() {
  let mut lio = Lio::new(64).unwrap();

  let mut recv = api::nop().with_lio(&mut lio).send();
  let result = poll_recv(&mut lio, &mut recv);

  result.expect("nop should complete successfully");
}

#[test]
fn test_nop_concurrent_100() {
  let mut lio = Lio::new(256).unwrap();

  let (sender, receiver) = mpsc::channel();

  // Submit 100 nop operations
  for _ in 0..100 {
    api::nop().with_lio(&mut lio).send_with(sender.clone());
  }

  // Wait for all to complete
  for i in 0..100 {
    let result = poll_until_recv(&mut lio, &receiver);
    result.expect(&format!("nop {} should complete", i));
  }
}

// ============================================================================
// Openat tests (fixed flag combinations)
// ============================================================================

#[test]
fn test_openat_create_file() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("openat_create");

  // Use AT_FDCWD as a Resource
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // CORRECT flags: O_CREAT | O_WRONLY | O_TRUNC (not O_RDONLY which conflicts)
  let (sender, receiver) = mpsc::channel();
  api::openat(
    &cwd,
    temp.path.clone(),
    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver).expect("Failed to create file");

  // Verify fd is valid
  assert!(fd.as_fd().as_raw_fd() >= 0, "File descriptor should be valid");

  // Don't drop cwd - it's AT_FDCWD which shouldn't be closed
  std::mem::forget(cwd);
}

#[test]
fn test_openat_read_existing() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("openat_read");

  // Create file first using libc
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "Failed to create test file");
    libc::write(fd, b"test data".as_ptr() as *const _, 9);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Open for reading
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, temp.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to open file for reading");

  assert!(fd.as_fd().as_raw_fd() >= 0, "File descriptor should be valid");

  std::mem::forget(cwd);
}

#[test]
fn test_openat_read_write() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("openat_rdwr");

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Open for read/write with create
  let (sender, receiver) = mpsc::channel();
  api::openat(
    &cwd,
    temp.path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver)
    .expect("Failed to open file for read/write");

  assert!(fd.as_fd().as_raw_fd() >= 0, "File descriptor should be valid");

  std::mem::forget(cwd);
}

#[test]
fn test_openat_nonexistent() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path =
    CString::new("/tmp/lio_test_nonexistent_12345_abcdef.txt").unwrap();

  // Try to open non-existent file without O_CREAT
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, path, libc::O_RDONLY).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);

  assert!(result.is_err(), "Should fail to open non-existent file");
  let err = result.unwrap_err();

  assert_eq!(err.raw_os_error(), Some(libc::ENOENT), "Should be ENOENT");

  std::mem::forget(cwd);
}

// ============================================================================
// Fsync tests
// ============================================================================

#[test]
fn test_fsync_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fsync_basic");

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create and open file
  let (sender, receiver) = mpsc::channel();
  api::openat(
    &cwd,
    temp.path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver).expect("Failed to create file");

  // Write some data
  let data = b"test data for fsync".to_vec();
  let (sender_write, receiver_write) = mpsc::channel();
  api::write(&fd, data).with_lio(&mut lio).send_with(sender_write);

  poll_until_recv(&mut lio, &receiver_write).0.expect("Failed to write");

  // Fsync
  let (sender_fsync, receiver_fsync) = mpsc::channel();
  api::fsync(&fd).with_lio(&mut lio).send_with(sender_fsync);

  let result = poll_until_recv(&mut lio, &receiver_fsync);
  result.expect("fsync should succeed");

  std::mem::forget(cwd);
}

#[test]
fn test_fsync_on_readonly() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fsync_readonly");

  // Create file first
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"data".as_ptr() as *const _, 4);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Open read-only
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, temp.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender);

  let fd = poll_until_recv(&mut lio, &receiver).expect("Failed to open file");

  // Fsync on read-only fd - behavior varies by filesystem, just ensure no crash
  let (sender_fsync, receiver_fsync) = mpsc::channel();
  api::fsync(&fd).with_lio(&mut lio).send_with(sender_fsync);

  let _result = poll_until_recv(&mut lio, &receiver_fsync);
  // Result may be Ok or Err depending on filesystem, just don't crash

  std::mem::forget(cwd);
}

// ============================================================================
// Linkat tests
// ============================================================================

#[test]
fn test_linkat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("linkat_source");
  let link_path = CString::new(format!(
    "/tmp/lio_test_linkat_link_{}.txt",
    std::process::id()
  ))
  .unwrap();

  // Create source file
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"link test".as_ptr() as *const _, 9);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create hard link
  let (sender, receiver) = mpsc::channel();
  api::linkat(&cwd, temp.path.clone(), &cwd, link_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("linkat should succeed");

  // Verify link exists by checking st_nlink
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    let ret = libc::stat(temp.path.as_ptr(), &mut stat);
    assert_eq!(ret, 0, "stat should succeed");
    assert!(stat.st_nlink >= 2, "Should have at least 2 links");

    // Cleanup link
    libc::unlink(link_path.as_ptr());
  }

  std::mem::forget(cwd);
}

#[test]
fn test_linkat_cross_directory() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("linkat_cross_src");

  // Create a subdirectory
  let subdir =
    CString::new(format!("/tmp/lio_test_subdir_{}", std::process::id()))
      .unwrap();
  unsafe {
    libc::mkdir(subdir.as_ptr(), 0o755);
  }

  let link_path = CString::new(format!(
    "/tmp/lio_test_subdir_{}/link.txt",
    std::process::id()
  ))
  .unwrap();

  // Create source file
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"cross dir".as_ptr() as *const _, 9);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create hard link in subdirectory
  let (sender, receiver) = mpsc::channel();
  api::linkat(&cwd, temp.path.clone(), &cwd, link_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("linkat cross directory should succeed");

  // Cleanup
  unsafe {
    libc::unlink(link_path.as_ptr());
    libc::rmdir(subdir.as_ptr());
  }

  std::mem::forget(cwd);
}

// ============================================================================
// Symlinkat tests
// ============================================================================

#[test]
fn test_symlinkat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("symlink_target");
  let link_path =
    CString::new(format!("/tmp/lio_test_symlink_{}.txt", std::process::id()))
      .unwrap();

  // Create target file
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"symlink target".as_ptr() as *const _, 14);
    libc::close(fd);
  }

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create symlink
  let (sender, receiver) = mpsc::channel();
  api::symlinkat(&cwd, temp.path.clone(), link_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("symlinkat should succeed");

  // Verify it's a symlink
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    let ret = libc::lstat(link_path.as_ptr(), &mut stat);
    assert_eq!(ret, 0, "lstat should succeed");
    assert!(
      (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK,
      "Should be a symlink"
    );

    // Cleanup
    libc::unlink(link_path.as_ptr());
  }

  std::mem::forget(cwd);
}

#[test]
fn test_symlinkat_dangling() {
  let mut lio = Lio::new(64).unwrap();

  let target = CString::new("/tmp/lio_nonexistent_target_12345").unwrap();
  let link_path = CString::new(format!(
    "/tmp/lio_test_dangling_symlink_{}.txt",
    std::process::id()
  ))
  .unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };

  // Create dangling symlink (target doesn't exist)
  let (sender, receiver) = mpsc::channel();
  api::symlinkat(&cwd, target, link_path.clone())
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("symlinkat to nonexistent target should succeed");

  // Verify it's a symlink (lstat should work even if target doesn't exist)
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    let ret = libc::lstat(link_path.as_ptr(), &mut stat);
    assert_eq!(ret, 0, "lstat should succeed on dangling symlink");
    assert!(
      (stat.st_mode & libc::S_IFMT) == libc::S_IFLNK,
      "Should be a symlink"
    );

    // Cleanup
    libc::unlink(link_path.as_ptr());
  }

  std::mem::forget(cwd);
}
