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

const OUTPUT_FLUSH_THRESHOLD: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
struct CommOptions {
  show_left: bool,
  show_right: bool,
  show_common: bool,
}

impl Default for CommOptions {
  fn default() -> Self {
    Self { show_left: true, show_right: true, show_common: true }
  }
}

#[derive(Debug, Clone, Default)]
pub struct CommCommand {
  options: Option<CommOptions>,
  pub left_path: String,
  pub right_path: String,
}

impl Command for CommCommand {
  fn name() -> &'static str {
    "comm"
  }

  fn summary() -> &'static str {
    "Compare two sorted files line by line."
  }

  fn usage() -> &'static str {
    "comm [-1] [-2] [-3] <file1> <file2>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "hide_left",
        short: &['1'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "hide_right",
        short: &['2'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "hide_common",
        short: &['3'],
        long: &[],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("comm", SPECS).parse(args)?;
    if parsed.positional().len() != 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "comm: expected two input files",
      ));
    }
    Ok(Self {
      options: Some(CommOptions {
        show_left: !parsed.get_flag_exists("hide_left"),
        show_right: !parsed.get_flag_exists("hide_right"),
        show_common: !parsed.get_flag_exists("hide_common"),
      }),
      left_path: parsed.positional()[0].clone(),
      right_path: parsed.positional()[1].clone(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let options = self.options.expect("comm options should be parsed");
    let left_fd = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        std::ffi::CString::new(self.left_path.as_str())?,
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )?;
    let right_fd = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        std::ffi::CString::new(self.right_path.as_str())?,
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )?;
    let mut left = LineReader::new(left_fd);
    let mut right = LineReader::new(right_fd);
    let mut left_line = left.next_line(ctx)?;
    let mut right_line = right.next_line(ctx)?;

    let stdout = ctx.stdout();
    let mut out = Vec::new();
    while left_line.is_some() || right_line.is_some() {
      let line = match (left_line.as_ref(), right_line.as_ref()) {
        (Some(l), Some(r)) if l == r => {
          let content = l.clone();
          left_line = left.next_line(ctx)?;
          right_line = right.next_line(ctx)?;
          if options.show_common {
            format!("{}{}\n", comm_prefix(&options, 3), content)
          } else {
            String::new()
          }
        }
        (Some(l), Some(r)) if l < r => {
          let content = l.clone();
          left_line = left.next_line(ctx)?;
          if options.show_left {
            format!("{}{}\n", comm_prefix(&options, 1), content)
          } else {
            String::new()
          }
        }
        (Some(_), Some(r)) => {
          let content = r.clone();
          right_line = right.next_line(ctx)?;
          if options.show_right {
            format!("{}{}\n", comm_prefix(&options, 2), content)
          } else {
            String::new()
          }
        }
        (Some(l), None) => {
          let content = l.clone();
          left_line = left.next_line(ctx)?;
          if options.show_left {
            format!("{}{}\n", comm_prefix(&options, 1), content)
          } else {
            String::new()
          }
        }
        (None, Some(r)) => {
          let content = r.clone();
          right_line = right.next_line(ctx)?;
          if options.show_right {
            format!("{}{}\n", comm_prefix(&options, 2), content)
          } else {
            String::new()
          }
        }
        (None, None) => break,
      };
      if !line.is_empty() {
        out.extend_from_slice(line.as_bytes());
        if out.len() >= OUTPUT_FLUSH_THRESHOLD {
          io_util::write_all(ctx.lio(), &stdout, std::mem::take(&mut out))?;
        }
      }
    }
    if !out.is_empty() {
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }
    Ok(())
  }
}

fn comm_prefix(options: &CommOptions, column: u8) -> &'static str {
  match column {
    1 => "",
    2 => {
      if options.show_left {
        "\t"
      } else {
        ""
      }
    }
    3 => match (options.show_left, options.show_right) {
      (true, true) => "\t\t",
      (true, false) | (false, true) => "\t",
      (false, false) => "",
    },
    _ => "",
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
        let line = decode_line(&self.pending[..pos])?;
        self.pending.drain(..=pos);
        return Ok(Some(line));
      }

      if self.eof {
        if self.pending.is_empty() {
          return Ok(None);
        }
        let line = decode_line(&self.pending)?;
        self.pending.clear();
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

fn decode_line(bytes: &[u8]) -> io::Result<String> {
  String::from_utf8(bytes.to_vec())
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn comm_prefix_matches_enabled_columns() {
    let opts = CommOptions::default();
    assert_eq!(comm_prefix(&opts, 1), "");
    assert_eq!(comm_prefix(&opts, 2), "\t");
    assert_eq!(comm_prefix(&opts, 3), "\t\t");
  }
}
