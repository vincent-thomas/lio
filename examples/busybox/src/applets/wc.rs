use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy)]
struct WcMode {
  lines: bool,
  words: bool,
  bytes: bool,
}

impl WcMode {
  fn all() -> Self {
    Self { lines: true, words: true, bytes: true }
  }
}

#[derive(Debug, Clone, Default)]
pub struct WcCommand {
  mode: Option<WcMode>,
  pub files: Vec<String>,
}

impl Command for WcCommand {
  fn name() -> &'static str {
    "wc"
  }

  fn summary() -> &'static str {
    "Print newline, word, and byte counts."
  }

  fn usage() -> &'static str {
    "wc [-lwc] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let (mode, files) = parse_wc_args(args)?;
    Ok(Self { mode: Some(mode), files: files.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let wc_mode = self.mode.expect("wc mode should be parsed");
    if self.files.is_empty() {
      let counts = wc_fd(ctx, &ctx.stdin())?;
      return print_wc_counts(ctx, counts, wc_mode, None);
    }

    let mut total = (0usize, 0usize, 0usize);
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      let rx = api::openat(&ctx.cwd(), cpath, libc::O_RDONLY)
        .with_lio(ctx.lio())
        .send();
      let file = io_util::run(ctx.lio(), rx)?;
      let counts = wc_fd(ctx, &file)?;
      total.0 += counts.0;
      total.1 += counts.1;
      total.2 += counts.2;
      print_wc_counts(ctx, counts, wc_mode, Some(path))?;
    }

    if self.files.len() > 1 {
      print_wc_counts(ctx, total, wc_mode, Some("total"))?;
    }

    Ok(())
  }
}

fn parse_wc_args(args: &[String]) -> io::Result<(WcMode, &[String])> {
  let mut mode = WcMode { lines: false, words: false, bytes: false };
  let mut index = 0;

  while let Some(arg) = args.get(index) {
    if !arg.starts_with('-') || arg == "-" {
      break;
    }
    for ch in arg.chars().skip(1) {
      match ch {
        'l' => mode.lines = true,
        'w' => mode.words = true,
        'c' => mode.bytes = true,
        _ => {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("wc: unsupported flag -{ch}"),
          ));
        }
      }
    }
    index += 1;
  }

  if !mode.lines && !mode.words && !mode.bytes {
    mode = WcMode::all();
  }

  Ok((mode, &args[index..]))
}

fn wc_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
) -> io::Result<(usize, usize, usize)> {
  let mut buf = vec![0u8; 8192];
  let mut lines = 0usize;
  let mut words = 0usize;
  let mut bytes = 0usize;
  let mut in_word = false;

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    bytes += n;
    for &byte in &buf[..n] {
      if byte == b'\n' {
        lines += 1;
      }
      if byte.is_ascii_whitespace() {
        in_word = false;
      } else if !in_word {
        words += 1;
        in_word = true;
      }
    }
  }

  Ok((lines, words, bytes))
}

fn print_wc_counts(
  ctx: &AppContext,
  counts: (usize, usize, usize),
  mode: WcMode,
  path: Option<&str>,
) -> io::Result<()> {
  let mut output = String::new();
  if mode.lines {
    output.push_str(&format!("{:>8}", counts.0));
  }
  if mode.words {
    output.push_str(&format!("{:>8}", counts.1));
  }
  if mode.bytes {
    output.push_str(&format!("{:>8}", counts.2));
  }
  if let Some(path) = path {
    output.push(' ');
    output.push_str(path);
  }
  output.push('\n');
  io_util::write_all(ctx.lio(), &ctx.stdout(), output.into_bytes())
}
