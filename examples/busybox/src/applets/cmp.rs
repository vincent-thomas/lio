use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct CmpCommand {
  pub silent: bool,
  pub left: String,
  pub right: String,
}

impl Command for CmpCommand {
  fn name() -> &'static str {
    "cmp"
  }

  fn summary() -> &'static str {
    "Compare two files byte by byte."
  }

  fn usage() -> &'static str {
    "cmp [-s] <file1> <file2>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut silent = false;
    let mut index = 0;
    if args.first().is_some_and(|arg| arg == "-s") {
      silent = true;
      index = 1;
    }
    if args.len() - index != 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cmp: expected exactly two files",
      ));
    }
    Ok(Self {
      silent,
      left: args[index].clone(),
      right: args[index + 1].clone(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let left = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(self.left.as_str())?,
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )?;
    let right = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(self.right.as_str())?,
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )?;

    if let Some(message) =
      compare_streams(ctx, &left, &right, &self.left, &self.right)?
    {
      if !self.silent {
        io_util::write_all(ctx.lio(), &ctx.stdout(), message.into_bytes())?;
      }
    }

    Ok(())
  }
}

fn compare_streams(
  ctx: &AppContext,
  left: &lio::api::resource::Resource,
  right: &lio::api::resource::Resource,
  left_name: &str,
  right_name: &str,
) -> io::Result<Option<String>> {
  let mut left_buf = vec![0u8; 8192];
  let mut right_buf = vec![0u8; 8192];
  let mut byte_index = 0usize;
  let mut line = 1usize;

  loop {
    let left_rx = api::read(left, left_buf).with_lio(ctx.lio()).send();
    let right_rx = api::read(right, right_buf).with_lio(ctx.lio()).send();
    let mut results =
      io_util::run_all(ctx.lio(), vec![left_rx, right_rx]).into_iter();
    let (left_result, returned_left) =
      results.next().expect("left read result missing");
    let (right_result, returned_right) =
      results.next().expect("right read result missing");
    left_buf = returned_left;
    right_buf = returned_right;

    let left_n = left_result? as usize;
    let right_n = right_result? as usize;

    if left_n == 0 && right_n == 0 {
      return Ok(None);
    }

    let common = left_n.min(right_n);
    for i in 0..common {
      let l = left_buf[i];
      let r = right_buf[i];
      if l != r {
        return Ok(Some(format!(
          "{} {} differ: byte {}, line {}\n",
          left_name,
          right_name,
          byte_index + i + 1,
          line
        )));
      }
      if l == b'\n' {
        line += 1;
      }
    }

    byte_index += common;

    if left_n != right_n {
      let eof_name = if left_n < right_n { left_name } else { right_name };
      return Ok(Some(format!("cmp: EOF on {eof_name}\n")));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_cmp_supports_s_flag() {
    let parsed =
      CmpCommand::parse(&["-s".into(), "a".into(), "b".into()]).unwrap();
    assert!(parsed.silent);
    assert_eq!(parsed.left, "a");
    assert_eq!(parsed.right, "b");
  }

  #[test]
  fn compare_streams_reports_first_difference() {
    let ctx = AppContext::new().unwrap();
    let left_path = temp_path("cmp-left");
    let right_path = temp_path("cmp-right");
    std::fs::write(&left_path, b"a\nb\nc\n").unwrap();
    std::fs::write(&right_path, b"a\nx\nc\n").unwrap();

    let left = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(left_path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();
    let right = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(right_path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();

    let message = compare_streams(
      &ctx,
      &left,
      &right,
      left_path.to_str().unwrap(),
      right_path.to_str().unwrap(),
    )
    .unwrap()
    .unwrap();
    assert!(message.contains("differ: byte 3, line 2"));

    std::fs::remove_file(left_path).unwrap();
    std::fs::remove_file(right_path).unwrap();
  }

  fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
      "busybox-{}-{}-{}",
      name,
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ))
  }
}
