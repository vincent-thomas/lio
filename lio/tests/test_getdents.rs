//! Tests for directory iteration (getdents).

mod common;

use common::poll_until_recv;
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::collections::HashSet;
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;

fn cwd_resource() -> Resource {
  unsafe { Resource::from_raw_fd(libc::AT_FDCWD) }
}

/// RAII wrapper for temporary directory cleanup
struct TempDir {
  path: CString,
}

impl TempDir {
  fn new(name: &str) -> Self {
    let path = CString::new(format!(
      "/tmp/lio_test_dir_{}_{}",
      name,
      std::process::id()
    ))
    .expect("Failed to create CString path");
    // Create the directory
    unsafe {
      libc::mkdir(path.as_ptr(), 0o755);
    }
    Self { path }
  }

  fn subpath(&self, name: &str) -> CString {
    let path_str = self.path.to_str().unwrap();
    CString::new(format!("{}/{}", path_str, name)).unwrap()
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    // Remove all contents first
    let path_str = self.path.to_str().unwrap();
    let _ = std::fs::remove_dir_all(path_str);
  }
}

#[test]
fn test_getdents_basic() {
  let mut lio = Lio::new(64).unwrap();
  let dir = TempDir::new("getdents_basic");

  // Create some files
  let file1 = dir.subpath("file1.txt");
  let file2 = dir.subpath("file2.txt");
  let subdir = dir.subpath("subdir");

  unsafe {
    let fd = libc::creat(file1.as_ptr(), 0o644);
    libc::close(fd);
    let fd = libc::creat(file2.as_ptr(), 0o644);
    libc::close(fd);
    libc::mkdir(subdir.as_ptr(), 0o755);
  }

  // Open directory
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(
    &cwd_resource(),
    dir.path.clone(),
    libc::O_RDONLY | libc::O_DIRECTORY,
  )
  .with_lio(&mut lio)
  .send_with(sender_open);
  let dir_fd = poll_until_recv(&mut lio, &receiver_open).unwrap();

  // Read entries
  let (sender_dents, receiver_dents) = mpsc::channel();
  api::getdents(&dir_fd, vec![0u8; 4096])
    .with_lio(&mut lio)
    .send_with(sender_dents);
  let (result, _buf, entries) = poll_until_recv(&mut lio, &receiver_dents);
  let bytes_read = result.expect("getdents failed");

  assert!(bytes_read > 0, "Should have read some bytes");
  assert!(!entries.is_empty(), "Should have parsed some entries");

  // Collect entry names
  let names: HashSet<_> =
    entries.iter().map(|e| e.name.to_string_lossy().to_string()).collect();

  // Should have . and ..
  assert!(names.contains("."), "Should have '.' entry");
  assert!(names.contains(".."), "Should have '..' entry");

  // Should have our files
  assert!(names.contains("file1.txt"), "Should have file1.txt");
  assert!(names.contains("file2.txt"), "Should have file2.txt");
  assert!(names.contains("subdir"), "Should have subdir");
}

#[test]
fn test_getdents_entry_types() {
  let mut lio = Lio::new(64).unwrap();
  let dir = TempDir::new("getdents_types");

  // Create a file and a directory
  let file = dir.subpath("file.txt");
  let subdir = dir.subpath("subdir");

  unsafe {
    let fd = libc::creat(file.as_ptr(), 0o644);
    libc::close(fd);
    libc::mkdir(subdir.as_ptr(), 0o755);
  }

  // Open directory
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(
    &cwd_resource(),
    dir.path.clone(),
    libc::O_RDONLY | libc::O_DIRECTORY,
  )
  .with_lio(&mut lio)
  .send_with(sender_open);
  let dir_fd = poll_until_recv(&mut lio, &receiver_open).unwrap();

  // Read entries
  let (sender_dents, receiver_dents) = mpsc::channel();
  api::getdents(&dir_fd, vec![0u8; 4096])
    .with_lio(&mut lio)
    .send_with(sender_dents);
  let (result, _buf, entries) = poll_until_recv(&mut lio, &receiver_dents);
  result.expect("getdents failed");

  // Find entries by name
  let file_entry =
    entries.iter().find(|e| e.name.to_string_lossy() == "file.txt");
  let dir_entry = entries.iter().find(|e| e.name.to_string_lossy() == "subdir");

  // Check types
  if let Some(e) = file_entry {
    assert!(e.is_file(), "file.txt should be a file");
  }
  if let Some(e) = dir_entry {
    assert!(e.is_dir(), "subdir should be a directory");
  }
}

#[test]
fn test_getdents_empty_directory() {
  let mut lio = Lio::new(64).unwrap();
  let dir = TempDir::new("getdents_empty");

  // Open directory (empty, just has . and ..)
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(
    &cwd_resource(),
    dir.path.clone(),
    libc::O_RDONLY | libc::O_DIRECTORY,
  )
  .with_lio(&mut lio)
  .send_with(sender_open);
  let dir_fd = poll_until_recv(&mut lio, &receiver_open).unwrap();

  // Read entries
  let (sender_dents, receiver_dents) = mpsc::channel();
  api::getdents(&dir_fd, vec![0u8; 4096])
    .with_lio(&mut lio)
    .send_with(sender_dents);
  let (result, _buf, entries) = poll_until_recv(&mut lio, &receiver_dents);
  result.expect("getdents failed");

  // Should have at least . and ..
  let names: Vec<_> =
    entries.iter().map(|e| e.name.to_string_lossy().to_string()).collect();

  assert!(names.contains(&".".to_string()), "Should have '.'");
  assert!(names.contains(&"..".to_string()), "Should have '..'");
}
