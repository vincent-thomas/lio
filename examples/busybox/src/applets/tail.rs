use std::{ffi::CString, io, time::Duration};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy)]
enum TailMode {
  Lines(usize),
  Bytes(usize),
}

#[derive(Debug, Clone, Default)]
pub struct TailCommand {
  mode: Option<TailMode>,
  pub follow: bool,
  pub files: Vec<String>,
}

impl Command for TailCommand {
  fn name() -> &'static str {
    "tail"
  }

  fn summary() -> &'static str {
    "Print the last part of files."
  }

  fn usage() -> &'static str {
    "tail [-f] [-n N|-c N] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut follow = false;
    let mut filtered = Vec::new();
    for arg in args {
      if arg == "-f" {
        follow = true;
      } else {
        filtered.push(arg.clone());
      }
    }
    let (mode, files) =
      parse_count_mode(&filtered, TailMode::Lines(10), "tail")?;
    Ok(Self { mode: Some(mode), follow, files: files.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mode = self.mode.expect("tail mode should be parsed");
    if self.files.is_empty() {
      tail_fd(ctx, &ctx.stdin(), mode, self.follow)
    } else {
      for (i, path) in self.files.iter().enumerate() {
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
        let cpath = CString::new(path.as_str())?;
        let rx = api::openat(&ctx.cwd(), cpath, libc::O_RDONLY)
          .with_lio(ctx.lio())
          .send();
        let file = io_util::run(ctx.lio(), rx)?;
        tail_fd(ctx, &file, mode, self.follow)?;
      }
      Ok(())
    }
  }
}

fn tail_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  mode: TailMode,
  follow: bool,
) -> io::Result<()> {
  let initial = io_util::read_to_bytes_fd(ctx.lio(), fd)?;
  let stdout = ctx.stdout();
  let output = match mode {
    TailMode::Lines(num_lines) => tail_last_lines(&initial, num_lines),
    TailMode::Bytes(num_bytes) => {
      initial[initial.len().saturating_sub(num_bytes)..].to_vec()
    }
  };
  io_util::write_all(ctx.lio(), &stdout, output)?;

  if follow {
    let mut buf = vec![0u8; 8192];
    loop {
      let rx =
        api::sleep(Duration::from_millis(100)).with_lio(ctx.lio()).send();
      io_util::run(ctx.lio(), rx)?;

      let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;

      let n = result? as usize;
      if n > 0 {
        io_util::write_all(ctx.lio(), &stdout, buf[..n].to_vec())?;
      }
    }
  }

  Ok(())
}

fn parse_count_mode<'a>(
  args: &'a [String],
  default: TailMode,
  applet: &str,
) -> io::Result<(TailMode, &'a [String])> {
  if args.len() >= 2 && args[0] == "-n" {
    let count = parse_usize_arg(&args[1], applet)?;
    return Ok((TailMode::Lines(count), &args[2..]));
  }
  if args.len() >= 2 && args[0] == "-c" {
    let count = parse_usize_arg(&args[1], applet)?;
    return Ok((TailMode::Bytes(count), &args[2..]));
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

fn tail_last_lines(data: &[u8], num_lines: usize) -> Vec<u8> {
  let mut lines = std::collections::VecDeque::with_capacity(num_lines + 1);
  let mut current = Vec::new();

  for &byte in data {
    current.push(byte);
    if byte == b'\n' {
      lines.push_back(std::mem::take(&mut current));
      if lines.len() > num_lines {
        lines.pop_front();
      }
    }
  }

  if !current.is_empty() {
    lines.push_back(current);
    if lines.len() > num_lines {
      lines.pop_front();
    }
  }

  lines.into_iter().flatten().collect()
}
