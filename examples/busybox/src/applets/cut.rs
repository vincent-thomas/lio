use std::io;

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct CutCommand {
  pub suppress_without_delim: bool,
  pub delimiter: char,
  pub field: usize,
  pub path: Option<String>,
}

impl Command for CutCommand {
  fn name() -> &'static str {
    "cut"
  }

  fn summary() -> &'static str {
    "Remove sections from each line."
  }

  fn usage() -> &'static str {
    "cut [-s] -d <delim> -f <field> [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut suppress_without_delim = false;
    let mut index = 0;
    if args.first().map(String::as_str) == Some("-s") {
      suppress_without_delim = true;
      index += 1;
    }

    if args.len() < index + 4 || args[index] != "-d" || args[index + 2] != "-f"
    {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cut: invalid arguments",
      ));
    }

    let delimiter = args[index + 1].chars().next().unwrap_or('\t');
    let field = args[index + 3].parse::<usize>().map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "cut: invalid field")
    })?;
    if field == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cut: field is 1-based",
      ));
    }

    Ok(Self {
      suppress_without_delim,
      delimiter,
      field,
      path: args.get(index + 4).cloned(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = open_input(ctx, self.path.as_deref())?;
    let stdout = ctx.stdout();
    stream_lines(
      ctx,
      &input,
      |line| {
        cut_line(line, self.delimiter, self.field, self.suppress_without_delim)
      },
      |bytes| {
        if bytes.is_empty() {
          return Ok(());
        }
        io_util::write_all(ctx.lio(), &stdout, bytes)
      },
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

fn cut_line(
  line: &[u8],
  delimiter: char,
  field: usize,
  suppress_without_delim: bool,
) -> Vec<u8> {
  let has_newline = line.last() == Some(&b'\n');
  let text = String::from_utf8_lossy(if has_newline {
    &line[..line.len() - 1]
  } else {
    line
  });
  if suppress_without_delim && !text.contains(delimiter) {
    return Vec::new();
  }
  let selected = text.split(delimiter).nth(field - 1).unwrap_or("");
  let mut out = selected.as_bytes().to_vec();
  out.push(b'\n');
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cut_line_selects_requested_field() {
    assert_eq!(cut_line(b"a:b:c\n", ':', 2, false), b"b\n");
    assert!(cut_line(b"abc\n", ':', 1, true).is_empty());
  }
}
