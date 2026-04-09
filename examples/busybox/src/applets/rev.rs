use std::io;

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct RevCommand {
  pub path: Option<String>,
}

impl Command for RevCommand {
  fn name() -> &'static str {
    "rev"
  }

  fn summary() -> &'static str {
    "Reverse lines characterwise."
  }

  fn usage() -> &'static str {
    "rev [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rev: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = open_input(ctx, self.path.as_deref())?;
    let stdout = ctx.stdout();
    stream_lines(
      ctx,
      &input,
      |line| reverse_line(line),
      |bytes| io_util::write_all(ctx.lio(), &stdout, bytes),
    )?;

    Ok(())
  }
}

fn open_input(
  ctx: &AppContext,
  path: Option<&str>,
) -> io::Result<lio::api::resource::Resource> {
  match path {
    Some(path) => io_util::run(
      ctx.lio(),
      api::openat(&ctx.cwd(), std::ffi::CString::new(path)?, libc::O_RDONLY, 0)
        .with_lio(ctx.lio())
        .send(),
    ),
    None => Ok(ctx.stdin()),
  }
}

fn stream_lines<F, W>(
  ctx: &AppContext,
  input: &lio::api::resource::Resource,
  transform: F,
  mut write: W,
) -> io::Result<()>
where
  F: Fn(&[u8]) -> Vec<u8>,
  W: FnMut(Vec<u8>) -> io::Result<()>,
{
  let mut buf = vec![0u8; 8192];
  let mut pending = Vec::new();

  loop {
    let rx = api::read(input, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;
    let n = result? as usize;
    if n == 0 {
      if !pending.is_empty() {
        write(transform(&pending))?;
      }
      break;
    }

    pending.extend_from_slice(&buf[..n]);
    while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
      let line = pending[..=pos].to_vec();
      pending.drain(..=pos);
      write(transform(&line))?;
    }
  }

  Ok(())
}

fn reverse_line(line: &[u8]) -> Vec<u8> {
  let has_newline = line.last() == Some(&b'\n');
  let content = if has_newline { &line[..line.len() - 1] } else { line };
  let mut chars: Vec<char> = String::from_utf8_lossy(content).chars().collect();
  chars.reverse();
  let mut out = chars.into_iter().collect::<String>().into_bytes();
  if has_newline {
    out.push(b'\n');
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reverse_line_preserves_newline() {
    assert_eq!(reverse_line(b"abc\n"), b"cba\n");
    assert_eq!(reverse_line(b"abc"), b"cba");
  }
}
