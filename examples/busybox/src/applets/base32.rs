use std::io;

use lio::api;

use crate::{
  app::AppContext, applets::support::stream_base32_output, command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct Base32Command {
  pub path: Option<String>,
}

impl Command for Base32Command {
  fn name() -> &'static str {
    "base32"
  }
  fn summary() -> &'static str {
    "Base32 encode data."
  }
  fn usage() -> &'static str {
    "base32 [file]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "base32: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = match self.path.as_deref() {
      Some(path) => io_util::run(
        ctx.lio(),
        api::openat(
          &ctx.cwd(),
          std::ffi::CString::new(path)?,
          libc::O_RDONLY,
          0,
        )
        .with_lio(ctx.lio())
        .send(),
      )?,
      None => ctx.stdin(),
    };
    stream_base32_output(ctx.lio(), &input, &ctx.stdout(), 76)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::applets::support::{encode_base32, stream_base32_output};
  use std::{ffi::CString, os::fd::FromRawFd, path::PathBuf};

  #[test]
  fn parse_base32_accepts_optional_file() {
    let parsed = Base32Command::parse(&["file".into()]).unwrap();
    assert_eq!(parsed.path.as_deref(), Some("file"));
  }

  #[test]
  fn streaming_base32_matches_previous_output() {
    let ctx = AppContext::new().unwrap();
    let data = b"hello world, hello world, hello world, hello world";
    let input_path = temp_path("base32");
    std::fs::write(&input_path, data).unwrap();

    let input = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(input_path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();

    let mut pipe_fds = [0; 2];
    unsafe { assert_eq!(libc::pipe(pipe_fds.as_mut_ptr()), 0) };
    let output =
      unsafe { lio::api::resource::Resource::from_raw_fd(pipe_fds[1]) };

    stream_base32_output(ctx.lio(), &input, &output, 76).unwrap();
    drop(output);

    let reader =
      unsafe { lio::api::resource::Resource::from_raw_fd(pipe_fds[0]) };
    let streamed = io_util::read_to_string_fd(ctx.lio(), &reader).unwrap();
    let expected = wrap_encoded(&encode_base32(data), 76);
    assert_eq!(streamed, expected);

    std::fs::remove_file(input_path).unwrap();
  }

  fn temp_path(name: &str) -> PathBuf {
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

  fn wrap_encoded(encoded: &str, width: usize) -> String {
    if encoded.is_empty() {
      return "\n".to_string();
    }

    let mut out = String::new();
    for chunk in encoded.as_bytes().chunks(width) {
      out.push_str(std::str::from_utf8(chunk).unwrap());
      out.push('\n');
    }
    out
  }
}
