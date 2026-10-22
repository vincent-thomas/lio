#![allow(clippy::expect_fun_call)]
#![cfg(feature = "high")]

#[path = "common.rs"]
mod common;

use common::{path_to_cstring, poll_recv};
use lio::{Lio, fs};
use std::path::{Path, PathBuf};

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

/// Creates a symbolic link to a directory.
///
/// Only on Unix: on Windows, directory links need a privilege or Developer
/// Mode, and junctions take a different API, so the symlink tests below are
/// Unix-only.
#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) {
  std::os::unix::fs::symlink(target, link).expect("symlink failed");
}

/// A directory outside the tree under test, holding one file.
#[cfg(unix)]
fn outside_dir(name: &str) -> PathBuf {
  let dir = temp_path(name);
  std::fs::create_dir(&dir).unwrap();
  std::fs::write(dir.join("keep.txt"), b"keep").unwrap();
  dir
}

#[cfg(unix)]
fn assert_outside_intact(dir: &Path) {
  assert!(dir.is_dir(), "{} must survive", dir.display());
  assert_eq!(
    std::fs::read(dir.join("keep.txt")).expect("file outside the tree"),
    b"keep"
  );
}

fn remove_dir_all(lio: &mut Lio, path: &Path) -> std::io::Result<()> {
  let mut remove =
    fs::remove_dir_all(path_to_cstring(path)).with_lio(lio).send();
  poll_recv(lio, &mut remove)
}

#[test]
#[cfg_attr(miri, ignore = "needs a real backend")]
fn fs_remove_dir_removes_empty_directory() {
  let mut lio = Lio::new(64).unwrap();
  let dir = temp_path("fs_remove_dir");
  std::fs::create_dir(&dir).unwrap();

  let mut remove = fs::remove_dir(path_to_cstring(&dir)).with_lio(&lio).send();
  poll_recv(&mut lio, &mut remove).expect("remove_dir");

  assert!(!dir.exists());
}

#[test]
#[cfg_attr(miri, ignore = "needs a real backend")]
fn fs_remove_dir_all_removes_nested_directories() {
  let mut lio = Lio::new(64).unwrap();
  let dir = temp_path("fs_remove_dir_all");
  std::fs::create_dir(&dir).unwrap();
  std::fs::create_dir_all(dir.join("nested").join("deeper")).unwrap();
  std::fs::create_dir(dir.join("empty")).unwrap();
  std::fs::write(dir.join("root.txt"), b"root").unwrap();
  std::fs::write(dir.join("nested").join("mid.txt"), b"mid").unwrap();
  std::fs::write(dir.join("nested").join("deeper").join("leaf.txt"), b"leaf")
    .unwrap();

  remove_dir_all(&mut lio, &dir).expect("remove_dir_all");

  assert!(!dir.exists());
}

#[test]
#[cfg_attr(miri, ignore = "needs a real backend")]
fn fs_remove_dir_all_rejects_a_regular_file() {
  let mut lio = Lio::new(64).unwrap();
  let file = temp_path("fs_remove_dir_all_file");
  std::fs::write(&file, b"file").unwrap();

  let err = remove_dir_all(&mut lio, &file).expect_err("not a directory");

  assert_eq!(err.kind(), std::io::ErrorKind::NotADirectory);
  assert!(file.exists(), "a file passed as the path is left alone");
  std::fs::remove_file(&file).ok();
}

// Symlink tests are Unix-only: see `symlink_dir`.
#[cfg(unix)]
#[test]
#[cfg_attr(miri, ignore = "needs a real backend")]
fn fs_remove_dir_all_does_not_follow_directory_symlinks() {
  let mut lio = Lio::new(64).unwrap();
  let dir = temp_path("fs_remove_dir_all_symlink_root");
  let outside = outside_dir("fs_remove_dir_all_symlink_target");
  std::fs::create_dir_all(dir.join("nested")).unwrap();
  std::fs::write(dir.join("nested").join("inner.txt"), b"inner").unwrap();
  symlink_dir(&outside, &dir.join("linked-dir"));
  symlink_dir(&outside, &dir.join("nested").join("deep-link"));

  remove_dir_all(&mut lio, &dir).expect("remove_dir_all symlink");

  assert!(!dir.exists());
  assert_outside_intact(&outside);
  std::fs::remove_dir_all(&outside).ok();
}

// Symlink tests are Unix-only: see `symlink_dir`.
#[cfg(unix)]
#[test]
#[cfg_attr(miri, ignore = "needs a real backend")]
fn fs_remove_dir_all_on_a_symlink_removes_only_the_link() {
  let mut lio = Lio::new(64).unwrap();
  let outside = outside_dir("fs_remove_dir_all_root_link_target");
  let link = temp_path("fs_remove_dir_all_root_link");
  symlink_dir(&outside, &link);

  remove_dir_all(&mut lio, &link).expect("remove_dir_all on a symlink");

  assert!(
    std::fs::symlink_metadata(&link).is_err(),
    "the link itself must be removed"
  );
  assert_outside_intact(&outside);
  std::fs::remove_dir_all(&outside).ok();
}
