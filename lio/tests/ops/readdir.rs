#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use common::poll_until_recv;
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::sync::mpsc;

#[test]
fn test_readdir_basic() {
  let mut lio = Lio::new(64).unwrap();

  let dir = std::env::temp_dir().join(format!(
    "lio_test_readdir_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();
  std::fs::write(dir.join("file.txt"), b"hello").unwrap();
  std::fs::create_dir(dir.join("nested")).unwrap();

  let dir_cstr = CString::new(dir.as_os_str().as_bytes()).unwrap();
  let dir_fd = unsafe {
    libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "failed to open temp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let (sender, receiver) = mpsc::channel();
  api::readdir(&dir_res, lio::api::ReadDirBuf::with_capacity(4096, 32))
    .with_lio(&mut lio)
    .send_with(sender);

  let buf =
    poll_until_recv(&mut lio, &receiver).expect("readdir should succeed");

  let names: Vec<Vec<u8>> =
    buf.iter().map(|entry| entry.name.to_vec()).collect();
  assert!(
    names.iter().any(|name| name.as_slice() == b"file.txt"),
    "readdir should return regular files"
  );
  assert!(
    names.iter().any(|name| name.as_slice() == b"nested"),
    "readdir should return subdirectories"
  );
  assert!(
    names
      .iter()
      .all(|name| name.as_slice() != b"." && name.as_slice() != b".."),
    "readdir should omit dot entries"
  );
  assert_eq!(buf.result.entries, names.len());
  assert!(buf.result.eof, "small directory should fit in one batch");

  std::fs::remove_file(dir.join("file.txt")).ok();
  std::fs::remove_dir(dir.join("nested")).ok();
  std::fs::remove_dir(&dir).ok();
}

#[test]
fn test_readdir_empty_directory() {
  let mut lio = Lio::new(64).unwrap();
  let dir = std::env::temp_dir().join(format!(
    "lio_test_readdir_empty_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();

  let dir_cstr = CString::new(dir.as_os_str().as_bytes()).unwrap();
  let dir_fd = unsafe {
    libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "failed to open temp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let (sender, receiver) = mpsc::channel();
  api::readdir(&dir_res, lio::api::ReadDirBuf::with_capacity(4096, 8))
    .with_lio(&mut lio)
    .send_with(sender);

  let buf =
    poll_until_recv(&mut lio, &receiver).expect("readdir should succeed");
  assert_eq!(buf.result.entries, 0);
  assert!(buf.iter().next().is_none());
  assert!(buf.result.eof);

  std::fs::remove_dir(&dir).ok();
}

#[test]
fn test_readdir_small_entries_capacity_truncates() {
  let mut lio = Lio::new(64).unwrap();
  let dir = std::env::temp_dir().join(format!(
    "lio_test_readdir_small_entries_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();
  for i in 0..6 {
    std::fs::write(dir.join(format!("file_{i}.txt")), b"x").unwrap();
  }

  let dir_cstr = CString::new(dir.as_os_str().as_bytes()).unwrap();
  let dir_fd = unsafe {
    libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "failed to open temp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let (sender, receiver) = mpsc::channel();
  api::readdir(&dir_res, lio::api::ReadDirBuf::with_capacity(4096, 2))
    .with_lio(&mut lio)
    .send_with(sender);

  let buf =
    poll_until_recv(&mut lio, &receiver).expect("readdir should succeed");
  assert_eq!(buf.result.entries, 2);
  assert!(
    !buf.result.eof,
    "small entries capacity should require more batches"
  );

  for i in 0..6 {
    std::fs::remove_file(dir.join(format!("file_{i}.txt"))).ok();
  }
  std::fs::remove_dir(&dir).ok();
}

#[test]
fn test_readdir_small_raw_buffer_requires_continuation() {
  let mut lio = Lio::new(64).unwrap();
  let dir = std::env::temp_dir().join(format!(
    "lio_test_readdir_small_raw_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();
  let expected_count = 8usize;
  for i in 0..expected_count {
    std::fs::write(
      dir.join(format!("long_file_name_{i}_abcdefghijklmnopqrstuvwxyz.txt")),
      b"x",
    )
    .unwrap();
  }

  let dir_cstr = CString::new(dir.as_os_str().as_bytes()).unwrap();
  let dir_fd = unsafe {
    libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "failed to open temp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let mut buf = lio::api::ReadDirBuf::with_capacity(192, 32);
  let mut names = std::collections::BTreeSet::new();
  let mut rounds = 0usize;

  loop {
    let (sender, receiver) = mpsc::channel();
    api::readdir(&dir_res, buf).with_lio(&mut lio).send_with(sender);
    buf = poll_until_recv(&mut lio, &receiver).expect("readdir should succeed");
    rounds += 1;
    for entry in buf.iter() {
      names.insert(entry.name.to_vec());
    }
    if buf.result.eof {
      break;
    }
  }

  assert!(rounds > 1, "small raw scratch should require multiple rounds");
  assert_eq!(names.len(), expected_count);
  for i in 0..expected_count {
    assert!(names.contains(
      format!("long_file_name_{i}_abcdefghijklmnopqrstuvwxyz.txt").as_bytes()
    ));
  }

  for i in 0..expected_count {
    std::fs::remove_file(
      dir.join(format!("long_file_name_{i}_abcdefghijklmnopqrstuvwxyz.txt")),
    )
    .ok();
  }
  std::fs::remove_dir(&dir).ok();
}

#[test]
fn test_readdir_continuation_collects_all_entries() {
  let mut lio = Lio::new(64).unwrap();
  let dir = std::env::temp_dir().join(format!(
    "lio_test_readdir_continue_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();
  let expected_count = 6usize;
  for i in 0..expected_count {
    std::fs::write(dir.join(format!("file_{i}.txt")), b"x").unwrap();
  }

  let dir_cstr = CString::new(dir.as_os_str().as_bytes()).unwrap();
  let dir_fd = unsafe {
    libc::open(dir_cstr.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
  };
  assert!(dir_fd >= 0, "failed to open temp directory");
  let dir_res = unsafe { Resource::from_raw_fd(dir_fd) };

  let mut buf = lio::api::ReadDirBuf::with_capacity(256, 2);
  let mut names = std::collections::BTreeSet::new();

  loop {
    let (sender, receiver) = mpsc::channel();
    api::readdir(&dir_res, buf).with_lio(&mut lio).send_with(sender);
    buf = poll_until_recv(&mut lio, &receiver).expect("readdir should succeed");

    for entry in buf.iter() {
      names.insert(entry.name.to_vec());
    }

    if buf.result.eof {
      break;
    }
  }

  assert_eq!(names.len(), expected_count);
  for i in 0..expected_count {
    assert!(names.contains(format!("file_{i}.txt").as_bytes()));
  }

  for i in 0..expected_count {
    std::fs::remove_file(dir.join(format!("file_{i}.txt"))).ok();
  }
  std::fs::remove_dir(&dir).ok();
}

#[test]
fn test_readdir_non_directory_fd_fails() {
  let mut lio = Lio::new(64).unwrap();
  let path = std::env::temp_dir().join(format!(
    "lio_test_readdir_regular_file_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::write(&path, b"hello").unwrap();

  let file_cstr = CString::new(path.as_os_str().as_bytes()).unwrap();
  let file_fd = unsafe { libc::open(file_cstr.as_ptr(), libc::O_RDONLY) };
  assert!(file_fd >= 0, "failed to open temp file");
  let file_res = unsafe { Resource::from_raw_fd(file_fd) };

  let (sender, receiver) = mpsc::channel();
  api::readdir(&file_res, lio::api::ReadDirBuf::with_capacity(4096, 8))
    .with_lio(&mut lio)
    .send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "readdir should fail on a regular file");

  std::fs::remove_file(&path).ok();
}
