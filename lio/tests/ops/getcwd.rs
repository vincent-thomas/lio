#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::{Lio, api};
use std::{os::unix::ffi::OsStrExt, sync::mpsc};

use common::poll_until_recv;

#[test]
fn test_getcwd_basic() {
  let mut lio = Lio::new(64).unwrap();
  let expected = std::env::current_dir().unwrap();
  let expected = expected.as_os_str().as_bytes().to_vec();

  let (sender, receiver) = mpsc::channel();
  api::getcwd(vec![0; 512]).with_lio(&mut lio).send_with(sender);

  let (result, buf) = poll_until_recv(&mut lio, &receiver);
  let bytes = result.unwrap() as usize;
  assert_eq!(&buf[..bytes], expected.as_slice());
}

#[test]
fn test_getcwd_small_buffer_fails() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();
  api::getcwd(vec![0; 1]).with_lio(&mut lio).send_with(sender);

  let (result, _buf) = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "getcwd should fail when the buffer is too small");
}
