use std::{ffi::OsString, io, os::unix::ffi::OsStringExt, path::PathBuf};

use lio::api;

use crate::{app::AppContext, util::io as io_util};

const INITIAL_GETCWD_CAPACITY: usize = 256;

pub fn current_working_directory_bytes(
  ctx: &AppContext,
) -> io::Result<Vec<u8>> {
  let mut capacity = INITIAL_GETCWD_CAPACITY;
  loop {
    let rx = api::getcwd(vec![0; capacity]).with_lio(ctx.lio()).send();
    let (result, buf) = io_util::run(ctx.lio(), rx);
    match result {
      Ok(_) => return Ok(buf),
      Err(err) if err.raw_os_error() == Some(libc::ERANGE) => {
        capacity = capacity.saturating_mul(2);
      }
      Err(err) => return Err(err),
    }
  }
}

pub fn current_working_directory(ctx: &AppContext) -> io::Result<PathBuf> {
  Ok(PathBuf::from(OsString::from_vec(current_working_directory_bytes(ctx)?)))
}
