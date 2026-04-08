use std::{ffi::CString, io};

use lio::{api, api::resource::Resource};

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy)]
enum HeadMode {
  Lines(usize),
  Bytes(usize),
}

#[derive(Debug, Clone, Default)]
pub struct HeadCommand {
  mode: Option<HeadMode>,
  pub files: Vec<String>,
}

impl Command for HeadCommand {
  fn name() -> &'static str {
    "head"
  }

  fn summary() -> &'static str {
    "Print the first part of files."
  }

  fn usage() -> &'static str {
    "head [-n N|-c N] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let (mode, files) = parse_count_mode(args, HeadMode::Lines(10), "head")?;
    Ok(Self { mode: Some(mode), files: files.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mode = self.mode.expect("head mode should be parsed");
    if self.files.is_empty() {
      return head_fd(ctx, &ctx.stdin(), mode);
    }

    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    for (i, (path, file)) in self
      .files
      .iter()
      .zip(io_util::run_all(ctx.lio(), open_receivers))
      .enumerate()
    {
      if self.files.len() > 1 {
        if i > 0 {
          io_util::write_all(ctx.lio(), &ctx.stdout(), b"\n".to_vec())?;
        }
        io_util::write_all(
          ctx.lio(),
          &ctx.stdout(),
          format!("==> {} <==\n", path).into_bytes(),
        )?;
      }
      head_fd(ctx, &file?, mode)?;
    }

    Ok(())
  }
}

fn head_fd(ctx: &AppContext, fd: &Resource, mode: HeadMode) -> io::Result<()> {
  match mode {
    HeadMode::Lines(num_lines) => head_fd_lines(ctx, fd, num_lines),
    HeadMode::Bytes(num_bytes) => head_fd_bytes(ctx, fd, num_bytes),
  }
}

fn head_fd_lines(
  ctx: &AppContext,
  fd: &Resource,
  num_lines: usize,
) -> io::Result<()> {
  let stdout = ctx.stdout();
  let mut buf = vec![0u8; 8192];
  let mut lines_printed = 0usize;
  let mut pending = Vec::new();

  'outer: loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      if !pending.is_empty() && lines_printed < num_lines {
        io_util::write_all(ctx.lio(), &stdout, std::mem::take(&mut pending))?;
      }
      break;
    }

    pending.extend_from_slice(&buf[..n]);

    while let Some(newline_pos) = pending.iter().position(|&b| b == b'\n') {
      let line_end = newline_pos + 1;
      let line = pending[..line_end].to_vec();
      pending = pending[line_end..].to_vec();
      io_util::write_all(ctx.lio(), &stdout, line)?;
      lines_printed += 1;
      if lines_printed >= num_lines {
        break 'outer;
      }
    }
  }

  Ok(())
}

fn head_fd_bytes(
  ctx: &AppContext,
  fd: &Resource,
  mut remaining: usize,
) -> io::Result<()> {
  let stdout = ctx.stdout();
  let mut buf = vec![0u8; 8192];

  while remaining > 0 {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    let count = n.min(remaining);
    io_util::write_all(ctx.lio(), &stdout, buf[..count].to_vec())?;
    remaining -= count;
  }

  Ok(())
}

fn parse_count_mode<'a>(
  args: &'a [String],
  default: HeadMode,
  applet: &str,
) -> io::Result<(HeadMode, &'a [String])> {
  if args.len() >= 2 && args[0] == "-n" {
    let count = parse_usize_arg(&args[1], applet)?;
    return Ok((HeadMode::Lines(count), &args[2..]));
  }
  if args.len() >= 2 && args[0] == "-c" {
    let count = parse_usize_arg(&args[1], applet)?;
    return Ok((HeadMode::Bytes(count), &args[2..]));
  }
  Ok((default, args))
}

fn parse_usize_arg(value: &str, applet: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{applet}: invalid count '{value}'"),
    )
  })
}
