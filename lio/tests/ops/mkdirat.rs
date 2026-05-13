#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::{
  Lio,
  api::{self, resource::Resource},
  backend::ds::{DSBackend, DSConfig},
};
use std::ffi::CString;

fn new_ds_lio() -> Lio {
  Lio::new_with_backend(
    DSBackend::with_config(DSConfig { fault_every: 0, ..DSConfig::default() }),
    64,
  )
  .unwrap()
}

fn stat_path(
  lio: &mut Lio,
  path: CString,
) -> Result<lio::api::FileStat, std::io::Error> {
  let mut receiver =
    api::statat(&Resource::cwd(), path, true).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver)
}

fn mkdir(lio: &mut Lio, path: CString) -> Result<(), std::io::Error> {
  let mut receiver =
    api::mkdirat(&Resource::cwd(), path, 0o755).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver)
}

fn unlink_dir(lio: &mut Lio, path: CString) {
  let mut receiver = api::unlinkat(&Resource::cwd(), path, libc::AT_REMOVEDIR)
    .with_lio(lio)
    .send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

#[test]
fn test_mkdirat_basic() {
  let mut lio = new_ds_lio();
  let path =
    CString::new(format!("/tmp/lio_test_mkdirat_{}", std::process::id()))
      .unwrap();

  mkdir(&mut lio, path.clone()).expect("mkdirat should succeed");

  let stat = stat_path(&mut lio, path.clone()).expect("directory should exist");
  assert!(stat.is_dir());

  unlink_dir(&mut lio, path);
}

#[test]
fn test_mkdirat_existing_path_fails() {
  let mut lio = new_ds_lio();
  let path = CString::new(format!(
    "/tmp/lio_test_mkdirat_existing_{}",
    std::process::id()
  ))
  .unwrap();

  mkdir(&mut lio, path.clone()).unwrap();

  let result = mkdir(&mut lio, path.clone());
  assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EEXIST));

  unlink_dir(&mut lio, path);
}
