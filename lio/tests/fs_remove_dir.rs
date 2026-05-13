#![allow(clippy::expect_fun_call)]
#![cfg(all(feature = "high", unix))]

#[path = "common.rs"]
mod common;

use common::poll_recv;
use lio::{Lio, fs};
use std::{
  ffi::CString,
  os::unix::{ffi::OsStrExt, fs::symlink},
  path::{Path, PathBuf},
};

fn path_to_cstring(path: &Path) -> CString {
  CString::new(path.as_os_str().as_bytes()).expect("path cstring")
}

fn temp_path(name: &str) -> PathBuf {
  std::env::temp_dir().join(format!(
    "lio_test_{}_{}_{}",
    name,
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ))
}

#[test]
fn fs_remove_dir_removes_empty_directory() {
  let mut lio = Lio::new(64).unwrap();
  let dir = temp_path("fs_remove_dir");
  std::fs::create_dir(&dir).unwrap();

  let mut remove = fs::remove_dir(path_to_cstring(&dir)).with_lio(&lio).send();
  poll_recv(&mut lio, &mut remove).expect("remove_dir");

  assert!(!dir.exists());
}

#[test]
fn fs_remove_dir_all_removes_nested_directories() {
  let mut lio = Lio::new(64).unwrap();
  let dir = temp_path("fs_remove_dir_all");
  std::fs::create_dir(&dir).unwrap();
  std::fs::create_dir_all(dir.join("nested").join("deeper")).unwrap();
  std::fs::write(dir.join("root.txt"), b"root").unwrap();
  std::fs::write(dir.join("nested").join("deeper").join("leaf.txt"), b"leaf")
    .unwrap();

  let mut remove =
    fs::remove_dir_all(path_to_cstring(&dir)).with_lio(&lio).send();
  poll_recv(&mut lio, &mut remove).expect("remove_dir_all");

  assert!(!dir.exists());
}

#[test]
fn fs_remove_dir_all_does_not_follow_directory_symlinks() {
  let mut lio = Lio::new(64).unwrap();
  let dir = temp_path("fs_remove_dir_all_symlink_root");
  let target = temp_path("fs_remove_dir_all_symlink_target");
  std::fs::create_dir(&dir).unwrap();
  std::fs::create_dir(&target).unwrap();
  std::fs::write(target.join("target.txt"), b"target").unwrap();
  symlink(&target, dir.join("linked-dir")).unwrap();

  let mut remove =
    fs::remove_dir_all(path_to_cstring(&dir)).with_lio(&lio).send();
  poll_recv(&mut lio, &mut remove).expect("remove_dir_all symlink");

  assert!(!dir.exists());
  assert!(target.exists());
  assert!(target.join("target.txt").exists());

  std::fs::remove_file(target.join("target.txt")).ok();
  std::fs::remove_dir(&target).ok();
}
