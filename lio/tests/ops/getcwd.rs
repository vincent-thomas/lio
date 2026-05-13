#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::{
  Lio, api,
  backend::ds::{DSBackend, DSConfig},
};

fn new_ds_lio(config: DSConfig) -> Lio {
  Lio::new_with_backend(DSBackend::with_config(config), 64).unwrap()
}

fn expected_cwd(config: DSConfig) -> Vec<u8> {
  let describe = config.describe();
  let cwd = describe
    .split(" cwd=")
    .nth(1)
    .and_then(|rest| rest.split(" readdir_root=").next())
    .expect("DSConfig::describe() should include cwd");
  cwd.as_bytes().to_vec()
}

#[test]
fn test_getcwd_basic() {
  let config = DSConfig { fault_every: 0, ..DSConfig::default() };
  let expected = expected_cwd(config);
  let mut lio = new_ds_lio(config);

  let mut receiver = api::getcwd(vec![0; 512]).with_lio(&mut lio).send();
  let (result, buf) = common::poll_recv(&mut lio, &mut receiver);
  let bytes = result.unwrap() as usize;
  assert_eq!(&buf[..bytes], expected.as_slice());
}

#[test]
fn test_getcwd_small_buffer_fails() {
  let mut lio = new_ds_lio(DSConfig { fault_every: 0, ..DSConfig::default() });

  let mut receiver = api::getcwd(vec![0; 1]).with_lio(&mut lio).send();
  let (result, _buf) = common::poll_recv(&mut lio, &mut receiver);
  assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ERANGE));
}
