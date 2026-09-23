use std::{ffi::CString, io, os::unix::ffi::OsStrExt, path::Path};

use lio::{Lio, api, api::resource::Resource};

use super::SearchBinaryMode;
use crate::util::{fs as fs_util, io as io_util};

pub(super) fn path_is_explicit_file(
  ctx: &crate::app::AppContext,
  cwd: &Path,
  value: &str,
) -> io::Result<bool> {
  let path = cwd.join(value);
  let path = CString::new(path.as_os_str().as_bytes())
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;
  let mut rx = api::statat(&ctx.cwd(), path, true).with_lio(ctx.lio()).send();
  match io_util::run_recv(ctx.lio(), &mut rx) {
    Ok(stat) => Ok(stat.is_file()),
    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
    Err(err) => Err(err),
  }
}

pub(super) fn split_records_with_numbers(
  bytes: &[u8],
  delimiter: u8,
) -> Vec<(usize, usize, &[u8])> {
  let mut lines = Vec::new();
  let mut line_start = 0usize;
  let mut line_number = 1usize;

  for (index, &byte) in bytes.iter().enumerate() {
    if byte != delimiter {
      continue;
    }
    lines.push((line_number, line_start, &bytes[line_start..index]));
    line_start = index + 1;
    line_number += 1;
  }

  if line_start < bytes.len() {
    lines.push((line_number, line_start, &bytes[line_start..]));
  }

  lines
}

pub(super) fn contains_regex_meta(pattern: &str) -> bool {
  let mut escaped = false;
  for ch in pattern.chars() {
    if escaped {
      escaped = false;
      continue;
    }
    if ch == '\\' {
      escaped = true;
      continue;
    }
    if matches!(
      ch,
      '.'
        | '+'
        | '*'
        | '?'
        | '('
        | ')'
        | '['
        | ']'
        | '{'
        | '}'
        | '|'
        | '^'
        | '$'
    ) {
      return true;
    }
  }
  false
}

pub(super) fn read_searchable_file(
  lio: &Lio,
  path: &Path,
  mode: SearchBinaryMode,
) -> io::Result<Option<Vec<u8>>> {
  let cpath = fs_util::path_to_cstring(path)?;
  let fd = match io_util::run(
    lio,
    api::openat(&Resource::cwd(), cpath, libc::O_RDONLY, 0)
      .with_lio(lio)
      .send(),
  ) {
    Ok(fd) => fd,
    Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
      return Ok(None);
    }
    Err(err) => return Err(err),
  };
  let bytes = match io_util::read_to_bytes_fd(lio, &fd) {
    Ok(bytes) => bytes,
    Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
      return Ok(None);
    }
    Err(err) => return Err(err),
  };
  if bytes.contains(&0) && matches!(mode, SearchBinaryMode::Skip) {
    return Ok(None);
  }
  Ok(Some(bytes))
}
