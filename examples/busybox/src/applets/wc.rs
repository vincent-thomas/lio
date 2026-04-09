use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy)]
struct WcMode {
  lines: bool,
  words: bool,
  bytes: bool,
  longest_line: bool,
}

impl WcMode {
  fn all() -> Self {
    Self { lines: true, words: true, bytes: true, longest_line: false }
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
    "wc [-lwcL] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let (mode, files) = parse_wc_args(args)?;
    Ok(Self { mode: Some(mode), files: files.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let wc_mode = self.mode.expect("wc mode should be parsed");
    let mut output = String::new();
    if self.files.is_empty() {
      let counts = wc_fd(ctx, &ctx.stdin())?;
      render_wc_counts(&mut output, counts, wc_mode, None);
      return io_util::write_all(ctx.lio(), &ctx.stdout(), output.into_bytes());
    }

    let mut total = (0usize, 0usize, 0usize, 0usize);
    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    for (path, file) in
      self.files.iter().zip(io_util::run_all(ctx.lio(), open_receivers))
    {
      let file = file?;
      let counts = wc_fd(ctx, &file)?;
      total.0 += counts.0;
      total.1 += counts.1;
      total.2 += counts.2;
      total.3 = total.3.max(counts.3);
      render_wc_counts(&mut output, counts, wc_mode, Some(path));
    }

    if self.files.len() > 1 {
      render_wc_counts(&mut output, total, wc_mode, Some("total"));
    }

    io_util::write_all(ctx.lio(), &ctx.stdout(), output.into_bytes())
  }
}

fn parse_wc_args(args: &[String]) -> io::Result<(WcMode, &[String])> {
  let mut mode =
    WcMode { lines: false, words: false, bytes: false, longest_line: false };
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
        'L' => mode.longest_line = true,
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

  if !mode.lines && !mode.words && !mode.bytes && !mode.longest_line {
    mode = WcMode::all();
  }

  Ok((mode, &args[index..]))
}

fn wc_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
) -> io::Result<(usize, usize, usize, usize)> {
  let mut buf = vec![0u8; 8192];
  let mut lines = 0usize;
  let mut words = 0usize;
  let mut bytes = 0usize;
  let mut longest_line = 0usize;
  let mut current_line_len = 0usize;
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
        longest_line = longest_line.max(current_line_len);
        current_line_len = 0;
      } else {
        current_line_len += 1;
      }
      if byte.is_ascii_whitespace() {
        in_word = false;
      } else if !in_word {
        words += 1;
        in_word = true;
      }
    }
  }

  longest_line = longest_line.max(current_line_len);

  Ok((lines, words, bytes, longest_line))
}

fn render_wc_counts(
  output: &mut String,
  counts: (usize, usize, usize, usize),
  mode: WcMode,
  path: Option<&str>,
) {
  if mode.lines {
    output.push_str(&format!("{:>8}", counts.0));
  }
  if mode.words {
    output.push_str(&format!("{:>8}", counts.1));
  }
  if mode.bytes {
    output.push_str(&format!("{:>8}", counts.2));
  }
  if mode.longest_line {
    output.push_str(&format!("{:>8}", counts.3));
  }
  if let Some(path) = path {
    output.push(' ');
    output.push_str(path);
  }
  output.push('\n');
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_wc_supports_longest_line_flag() {
    let args = ["-L".into(), "file".into()];
    let (mode, files) = parse_wc_args(&args).unwrap();
    assert!(mode.longest_line);
    assert_eq!(files, ["file"]);
  }

  #[test]
  fn wc_counts_longest_line_without_newline() {
    let ctx = AppContext::new().unwrap();
    let path = std::env::temp_dir().join(format!(
      "busybox-wc-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    std::fs::write(&path, b"a\nabcd\nabc").unwrap();
    let file = io_util::run(
      ctx.lio(),
      api::openat(
        &lio::api::resource::Resource::cwd(),
        std::ffi::CString::new(path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();
    let counts = wc_fd(&ctx, &file).unwrap();
    assert_eq!(counts, (2, 3, 10, 4));
    std::fs::remove_file(path).unwrap();
  }
}
