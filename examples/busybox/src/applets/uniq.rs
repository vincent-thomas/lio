use std::io;

use lio::api;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

#[derive(Debug, Clone, Copy, Default)]
struct UniqMode {
  show_count: bool,
  only_duplicates: bool,
  only_unique: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UniqCommand {
  mode: Option<UniqMode>,
  pub path: Option<String>,
}

impl Command for UniqCommand {
  fn name() -> &'static str {
    "uniq"
  }

  fn summary() -> &'static str {
    "Report or filter repeated lines."
  }

  fn usage() -> &'static str {
    "uniq [-c] [-d] [-u] [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec { name: "count", short: &['c'], long: &[], takes_value: false },
      FlagSpec {
        name: "duplicates",
        short: &['d'],
        long: &[],
        takes_value: false,
      },
      FlagSpec { name: "unique", short: &['u'], long: &[], takes_value: false },
    ];
    let parsed = FlagParser::new("uniq", SPECS).parse(args)?;
    if parsed.positional().len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "uniq: too many file operands",
      ));
    }

    Ok(Self {
      mode: Some(UniqMode {
        show_count: parsed.get_flag_exists("count"),
        only_duplicates: parsed.get_flag_exists("duplicates"),
        only_unique: parsed.get_flag_exists("unique"),
      }),
      path: parsed.positional().first().cloned(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mode = self.mode.expect("uniq mode should be parsed");
    let stdout = ctx.stdout();
    let input = open_input(ctx, self.path.as_deref())?;
    let mut reader = LineReader::new(input);
    let mut current = match reader.next_line(ctx)? {
      Some(line) => line,
      None => return Ok(()),
    };
    let mut count = 1usize;

    while let Some(next) = reader.next_line(ctx)? {
      if next == current {
        count += 1;
      } else {
        emit_group(ctx, &stdout, mode, &current, count)?;
        current = next;
        count = 1;
      }
    }
    emit_group(ctx, &stdout, mode, &current, count)?;

    Ok(())
  }
}

fn emit_group(
  ctx: &AppContext,
  stdout: &lio::api::resource::Resource,
  mode: UniqMode,
  line: &str,
  count: usize,
) -> io::Result<()> {
  if (mode.only_duplicates && count == 1) || (mode.only_unique && count != 1) {
    return Ok(());
  }

  let mut out = if mode.show_count {
    format!("{:>7} {}", count, line.trim_end_matches('\n')).into_bytes()
  } else {
    line.as_bytes().to_vec()
  };

  if mode.show_count && !line.ends_with('\n') {
    out.push(b'\n');
  }
  io_util::write_all(ctx.lio(), stdout, out)
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

struct LineReader {
  fd: lio::api::resource::Resource,
  buf: Vec<u8>,
  pending: Vec<u8>,
  eof: bool,
}

impl LineReader {
  fn new(fd: lio::api::resource::Resource) -> Self {
    Self { fd, buf: vec![0u8; 8192], pending: Vec::new(), eof: false }
  }

  fn next_line(&mut self, ctx: &AppContext) -> io::Result<Option<String>> {
    loop {
      if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
        let line = String::from_utf8(self.pending[..=pos].to_vec())
          .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.pending.drain(..=pos);
        return Ok(Some(line));
      }

      if self.eof {
        if self.pending.is_empty() {
          return Ok(None);
        }
        let line = String::from_utf8(std::mem::take(&mut self.pending))
          .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        return Ok(Some(line));
      }

      let rx = api::read(&self.fd, std::mem::take(&mut self.buf))
        .with_lio(ctx.lio())
        .send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      self.buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        self.eof = true;
      } else {
        self.pending.extend_from_slice(&self.buf[..n]);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn emit_group_respects_duplicate_filters() {
    let ctx = AppContext::new().unwrap();
    let stdout = ctx.stdout();
    assert!(
      emit_group(
        &ctx,
        &stdout,
        UniqMode { only_duplicates: true, ..Default::default() },
        "a\n",
        1
      )
      .is_ok()
    );
  }
}
