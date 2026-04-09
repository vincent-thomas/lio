use std::io;

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct NlCommand {
  pub path: Option<String>,
}

impl Command for NlCommand {
  fn name() -> &'static str {
    "nl"
  }

  fn summary() -> &'static str {
    "Number lines of files."
  }

  fn usage() -> &'static str {
    "nl [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "nl: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = open_input(ctx, self.path.as_deref())?;
    let stdout = ctx.stdout();
    let mut line_no = 1usize;
    stream_lines(ctx, &input, |line| {
      let bytes = number_line(line_no, line);
      line_no += 1;
      io_util::write_all(ctx.lio(), &stdout, bytes)
    })?;
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

fn stream_lines<W>(
  ctx: &AppContext,
  input: &lio::api::resource::Resource,
  mut write: W,
) -> io::Result<()>
where
  W: FnMut(&[u8]) -> io::Result<()>,
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
        write(&pending)?;
      }
      break;
    }

    pending.extend_from_slice(&buf[..n]);
    while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
      let line = pending[..=pos].to_vec();
      pending.drain(..=pos);
      write(&line)?;
    }
  }

  Ok(())
}

fn number_line(line_no: usize, line: &[u8]) -> Vec<u8> {
  let has_newline = line.last() == Some(&b'\n');
  let content = if has_newline { &line[..line.len() - 1] } else { line };
  let mut out = format!("{:>6}\t", line_no).into_bytes();
  out.extend_from_slice(content);
  out.push(b'\n');
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn number_line_formats_expected_output() {
    assert_eq!(number_line(2, b"hello\n"), b"     2\thello\n");
    assert_eq!(number_line(3, b"hello"), b"     3\thello\n");
  }
}
