use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct PasteCommand {
  pub serial: bool,
  pub delimiter: String,
  pub files: Vec<String>,
}

impl Command for PasteCommand {
  fn name() -> &'static str {
    "paste"
  }

  fn summary() -> &'static str {
    "Merge lines of files."
  }

  fn usage() -> &'static str {
    "paste [-s] [-d <delims>] <file...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut serial = false;
    let mut delimiter = "\t".to_string();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-s" => {
          serial = true;
          index += 1;
        }
        "-d" => {
          let Some(value) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "paste: missing delimiter",
            ));
          };
          delimiter = value.clone();
          index += 2;
        }
        _ => break,
      }
    }
    let files = args[index..].to_vec();
    if files.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "paste: missing file operand",
      ));
    }
    Ok(Self { serial, delimiter, files })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let stdout = ctx.stdout();
    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    let mut columns = Vec::with_capacity(self.files.len());
    for file in io_util::run_all(ctx.lio(), open_receivers) {
      columns.push(io_util::read_to_string_fd(ctx.lio(), &file?)?);
    }

    if self.serial {
      for column in columns {
        let mut line = String::new();
        for (idx, part) in column.lines().enumerate() {
          if idx > 0 {
            line.push(delim_at(&self.delimiter, idx - 1));
          }
          line.push_str(part);
        }
        line.push('\n');
        io_util::write_all(ctx.lio(), &stdout, line.into_bytes())?;
      }
    } else {
      let line_sets: Vec<Vec<&str>> =
        columns.iter().map(|s| s.lines().collect()).collect();
      let max_lines = line_sets.iter().map(Vec::len).max().unwrap_or(0);

      for row in 0..max_lines {
        let mut line = String::new();
        for (col, lines) in line_sets.iter().enumerate() {
          if col > 0 {
            line.push(delim_at(&self.delimiter, col - 1));
          }
          if let Some(part) = lines.get(row) {
            line.push_str(part);
          }
        }
        line.push('\n');
        io_util::write_all(ctx.lio(), &stdout, line.into_bytes())?;
      }
    }

    Ok(())
  }
}

fn delim_at(delims: &str, index: usize) -> char {
  delims.chars().nth(index % delims.chars().count().max(1)).unwrap_or('\t')
}
