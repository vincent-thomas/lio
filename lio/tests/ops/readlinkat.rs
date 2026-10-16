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

use common::{path_to_cstring, poll_until_recv, symlink_file, temp_path};

#[test]
fn test_readlinkat_basic() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = Resource::cwd();
  let target = temp_path("readlinkat_target");
  let link = temp_path("readlinkat_link");

  fs::remove_file(&target).ok();
  fs::remove_file(&link).ok();
  fs::write(&target, b"").expect("failed to create target file");
  symlink_file(&target, &link);

  let (sender, receiver) = mpsc::channel();
  api::readlinkat(&cwd, path_to_cstring(&link), vec![0; 512])
    .with_lio(&mut lio)
    .send_with(sender);

  let (result, buf) = poll_until_recv(&mut lio, &receiver);
  let bytes = result.unwrap() as usize;
  assert_eq!(&buf[..bytes], target.as_os_str().as_encoded_bytes());

  fs::remove_file(&link).ok();
  fs::remove_file(&target).ok();
}

#[test]
fn test_readlinkat_missing_path_fails() {
  let mut lio = Lio::new(64).unwrap();
  let cwd = Resource::cwd();
  let link = temp_path("readlinkat_missing");
  fs::remove_file(&link).ok();

  let (sender, receiver) = mpsc::channel();
  api::readlinkat(&cwd, path_to_cstring(&link), vec![0; 64])
    .with_lio(&mut lio)
    .send_with(sender);

  let (result, _buf) = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "readlinkat should fail for missing path");
}
