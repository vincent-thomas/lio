//! A path with an interior NUL byte cannot be passed to the OS. Like
//! `std::fs`, `lio::fs` reports that as `InvalidInput` instead of panicking.
#![cfg(feature = "high")]

#[path = "common.rs"]
mod common;

use common::poll_recv;
use lio::{Lio, fs};
use std::io::ErrorKind;

const BAD: &str = "lio\0nul";

fn assert_invalid_input<T>(result: std::io::Result<T>) {
  match result {
    Err(err) => assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}"),
    Ok(_) => panic!("a path with a NUL byte must not succeed"),
  }
}

#[test]
fn read_dir_rejects_a_nul_path() {
  let mut lio = Lio::new(8).unwrap();
  let mut recv = fs::read_dir(BAD).with_lio(&lio).send();
  assert_invalid_input(poll_recv(&mut lio, &mut recv));
}

#[test]
fn metadata_rejects_a_nul_path() {
  let mut lio = Lio::new(8).unwrap();
  let mut recv = fs::metadata(BAD).with_lio(&lio).send();
  assert_invalid_input(poll_recv(&mut lio, &mut recv));
  let mut recv = fs::symlink_metadata(BAD).with_lio(&lio).send();
  assert_invalid_input(poll_recv(&mut lio, &mut recv));
}

#[test]
fn read_link_rejects_a_nul_path() {
  let mut lio = Lio::new(8).unwrap();
  let mut recv = fs::read_link(BAD).with_lio(&lio).send();
  assert_invalid_input(poll_recv(&mut lio, &mut recv));
}

#[test]
fn read_to_string_rejects_a_nul_path() {
  let mut lio = Lio::new(8).unwrap();
  let mut recv = fs::read_to_string(BAD).with_lio(&lio).send();
  assert_invalid_input(poll_recv(&mut lio, &mut recv));
}
