use std::io;

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct FoldCommand {
  pub width: usize,
  pub path: Option<String>,
}

impl Command for FoldCommand {
  fn name() -> &'static str {
    "fold"
  }

  fn summary() -> &'static str {
    "Wrap input lines to fit a width."
  }

  fn usage() -> &'static str {
    "fold [-w N] [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut width = 80usize;
    let mut index = 0;
    if args.len() >= 2 && args[0] == "-w" {
      width = parse_usize_arg(&args[1], "fold")?;
      index = 2;
    }
    if width == 0 || args.len() > index + 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "fold: invalid arguments",
      ));
    }
    Ok(Self { width, path: args.get(index).cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = open_input(ctx, self.path.as_deref())?;
    let stdout = ctx.stdout();
    stream_lines(
      ctx,
      &input,
      |line| fold_line(line, self.width),
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

fn fold_line(line: &[u8], width: usize) -> Vec<u8> {
  let has_newline = line.last() == Some(&b'\n');
  let content = if has_newline { &line[..line.len() - 1] } else { line };
  let mut out = Vec::with_capacity(line.len() + line.len() / width + 1);
  let mut current = String::new();
  let mut count = 0usize;

  for ch in String::from_utf8_lossy(content).chars() {
    current.push(ch);
    count += 1;
    if count >= width {
      current.push('\n');
      out.extend_from_slice(current.as_bytes());
      current.clear();
      count = 0;
    }
  }
  if !current.is_empty() {
    out.extend_from_slice(current.as_bytes());
  }
  if has_newline && !out.ends_with(b"\n") {
    out.push(b'\n');
  }
  out
}

fn parse_usize_arg(value: &str, applet: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{applet}: invalid count '{value}'"),
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fold_line_wraps_and_preserves_newline() {
    assert_eq!(fold_line(b"abcdef\n", 3), b"abc\ndef\n");
    assert_eq!(fold_line(b"abcdef", 3), b"abc\ndef\n");
  }
}
