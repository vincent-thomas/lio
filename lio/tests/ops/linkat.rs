#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::fs;
use std::sync::mpsc;

use common::{path_to_cstring, poll_until_recv, temp_path};

#[test]
fn test_linkat_hard_link_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = Resource::cwd();
  let source = temp_path("linkat_source");
  let link = temp_path("linkat_link");

  fs::remove_file(&source).ok();
  fs::remove_file(&link).ok();
  fs::write(&source, b"original").expect("failed to create source file");

  let (sender, receiver) = mpsc::channel();
  api::linkat(
    &cwd,
    path_to_cstring(&source),
    &cwd,
    path_to_cstring(&link),
    api::ops::LinkKind::Hard,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("hard link should succeed");

  // Both names refer to the same file: a write through one is visible
  // through the other.
  fs::write(&link, b"changed").unwrap();
  assert_eq!(fs::read(&source).unwrap(), b"changed");

  fs::remove_file(&link).ok();
  fs::remove_file(&source).ok();
}

#[test]
fn test_linkat_soft_link_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = Resource::cwd();
  let target = temp_path("symlinkat_target");
  let link = temp_path("symlinkat_link");

  fs::remove_file(&target).ok();
  fs::remove_file(&link).ok();
  fs::write(&target, b"").expect("failed to create target file");

  let (sender, receiver) = mpsc::channel();
  api::linkat(
    &cwd,
    path_to_cstring(&target),
    &cwd,
    path_to_cstring(&link),
    api::ops::LinkKind::Soft,
  )
  .with_lio(&mut lio)
  .send_with(sender);

  poll_until_recv(&mut lio, &receiver).expect("symbolic link should succeed");

  assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());

  fs::remove_file(&link).ok();
  fs::remove_file(&target).ok();
}
