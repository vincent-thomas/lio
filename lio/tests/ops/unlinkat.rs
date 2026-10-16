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

fn open_file(
  lio: &mut Lio,
  path: CString,
  flags: lio::api::OpenFlags,
  mode: u32,
) -> Resource {
  let mut receiver =
    api::openat(&Resource::cwd(), path, flags, mode).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap()
}

fn mkdir(lio: &mut Lio, path: CString) {
  let mut receiver =
    api::mkdirat(&Resource::cwd(), path, 0o755).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

fn stat_path(
  lio: &mut Lio,
  path: CString,
) -> Result<lio::api::FileStat, std::io::Error> {
  let mut receiver =
    api::statat(&Resource::cwd(), path, true).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver)
}

#[test]
fn test_unlinkat_file() {
  let mut lio = new_ds_lio();
  let path = CString::new(format!(
    "/tmp/lio_test_unlinkat_file_{}.txt",
    std::process::id()
  ))
  .unwrap();

  let _file = open_file(
    &mut lio,
    path.clone(),
    lio::api::OpenFlags::WRITE
      | lio::api::OpenFlags::CREATE
      | lio::api::OpenFlags::TRUNCATE,
    0o644,
  );

  let mut receiver =
    api::unlinkat(&Resource::cwd(), path.clone(), api::UnlinkKind::File)
      .with_lio(&mut lio)
      .send();
  common::poll_recv(&mut lio, &mut receiver).expect("unlinkat should succeed");

  let result = stat_path(&mut lio, path);
  assert_eq!(
    result.unwrap_err().raw_os_error(),
    Some(lio::backend::ds::errno::ENOENT)
  );
}

#[test]
fn test_unlinkat_directory() {
  let mut lio = new_ds_lio();
  let path =
    CString::new(format!("/tmp/lio_test_unlinkat_dir_{}", std::process::id()))
      .unwrap();

  mkdir(&mut lio, path.clone());

  let mut receiver =
    api::unlinkat(&Resource::cwd(), path.clone(), api::UnlinkKind::Directory)
      .with_lio(&mut lio)
      .send();
  common::poll_recv(&mut lio, &mut receiver)
    .expect("unlinkat AT_REMOVEDIR should succeed");

  let result = stat_path(&mut lio, path);
  assert_eq!(
    result.unwrap_err().raw_os_error(),
    Some(lio::backend::ds::errno::ENOENT)
  );
}
