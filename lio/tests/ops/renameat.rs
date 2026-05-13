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

fn open_file(lio: &mut Lio, path: CString, flags: i32, mode: u32) -> Resource {
  let mut receiver =
    api::openat(&Resource::cwd(), path, flags, mode).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap()
}

fn stat_path(
  lio: &mut Lio,
  path: CString,
) -> Result<lio::api::FileStat, std::io::Error> {
  let mut receiver =
    api::statat(&Resource::cwd(), path, true).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver)
}

fn unlink_file(lio: &mut Lio, path: CString) {
  let mut receiver =
    api::unlinkat(&Resource::cwd(), path, 0).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

#[test]
fn test_renameat_basic() {
  let mut lio = new_ds_lio();
  let old_path = CString::new(format!(
    "/tmp/lio_test_renameat_old_{}.txt",
    std::process::id()
  ))
  .unwrap();
  let new_path = CString::new(format!(
    "/tmp/lio_test_renameat_new_{}.txt",
    std::process::id()
  ))
  .unwrap();

  let _file = open_file(
    &mut lio,
    old_path.clone(),
    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
    0o644,
  );

  let mut receiver = api::renameat(
    &Resource::cwd(),
    old_path.clone(),
    &Resource::cwd(),
    new_path.clone(),
  )
  .with_lio(&mut lio)
  .send();
  common::poll_recv(&mut lio, &mut receiver).expect("renameat should succeed");

  let old_result = stat_path(&mut lio, old_path);
  assert_eq!(old_result.unwrap_err().raw_os_error(), Some(libc::ENOENT));

  let new_stat =
    stat_path(&mut lio, new_path.clone()).expect("new path should exist");
  assert!(new_stat.is_file());

  unlink_file(&mut lio, new_path);
}

#[test]
fn test_renameat_missing_source_fails() {
  let mut lio = new_ds_lio();
  let old_path = CString::new(format!(
    "/tmp/lio_test_renameat_missing_{}.txt",
    std::process::id()
  ))
  .unwrap();
  let new_path = CString::new(format!(
    "/tmp/lio_test_renameat_target_{}.txt",
    std::process::id()
  ))
  .unwrap();

  let mut receiver =
    api::renameat(&Resource::cwd(), old_path, &Resource::cwd(), new_path)
      .with_lio(&mut lio)
      .send();
  let result = common::poll_recv(&mut lio, &mut receiver);
  assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOENT));
}
