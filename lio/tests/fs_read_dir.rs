#![allow(clippy::expect_fun_call)]
#![cfg(feature = "high")]

#[path = "common.rs"]
mod common;

use common::poll_recv;
use lio::{
  Lio,
  fs::{self, ReadDir},
};
use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;

#[test]
fn fs_read_dir_yields_entries_one_by_one() {
  let mut lio = Lio::new(64).unwrap();
  let dir = std::env::temp_dir().join(format!(
    "lio_test_fs_read_dir_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();
  std::fs::create_dir(dir.join("nested")).unwrap();
  let expected_count = 48usize;
  for idx in 0..expected_count {
    std::fs::write(dir.join(format!("alpha_{idx}.txt")), b"a").unwrap();
  }

  let mut open = fs::read_dir(&dir).with_lio(&lio).send();
  let mut read_dir: ReadDir =
    poll_recv(&mut lio, &mut open).expect("open read_dir");

  let mut names = BTreeSet::new();
  for entry in &mut read_dir {
    let entry = entry.expect("read_dir entry");
    names.insert(entry.file_name().as_bytes().to_vec());
    assert_eq!(entry.path().parent(), Some(dir.as_path()));
  }

  assert_eq!(names.len(), expected_count + 1);
  assert!(names.contains(b"alpha_0.txt".as_slice()));
  assert!(
    names.contains(format!("alpha_{}.txt", expected_count - 1).as_bytes())
  );
  assert!(names.contains(b"nested".as_slice()));

  for idx in 0..expected_count {
    std::fs::remove_file(dir.join(format!("alpha_{idx}.txt"))).ok();
  }
  std::fs::remove_dir(dir.join("nested")).ok();
  std::fs::remove_dir(&dir).ok();
}

#[test]
fn fs_read_dir_returns_none_for_empty_directory() {
  let mut lio = Lio::new(64).unwrap();
  let dir = std::env::temp_dir().join(format!(
    "lio_test_fs_read_dir_empty_{}_{}",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::SystemTime::UNIX_EPOCH)
      .unwrap()
      .as_nanos()
  ));
  std::fs::create_dir(&dir).unwrap();

  let mut open = fs::read_dir(&dir).with_lio(&lio).send();
  let mut read_dir: ReadDir =
    poll_recv(&mut lio, &mut open).expect("open read_dir");

  assert!(read_dir.next().is_none());

  std::fs::remove_dir(&dir).ok();
}
