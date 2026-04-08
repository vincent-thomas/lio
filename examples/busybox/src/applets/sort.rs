use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy, Default)]
struct SortOptions {
  reverse: bool,
  numeric: bool,
  unique: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SortCommand {
  options: Option<SortOptions>,
  pub path: Option<String>,
}

impl Command for SortCommand {
  fn name() -> &'static str {
    "sort"
  }

  fn summary() -> &'static str {
    "Sort lines of text files."
  }

  fn usage() -> &'static str {
    "sort [-r] [-n] [-u] [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut options = SortOptions::default();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-r" => {
          options.reverse = true;
          index += 1;
        }
        "-n" => {
          options.numeric = true;
          index += 1;
        }
        "-u" => {
          options.unique = true;
          index += 1;
        }
        _ => break,
      }
    }
    if args.len() > index + 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "sort: too many file operands",
      ));
    }
    Ok(Self { options: Some(options), path: args.get(index).cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let options = self.options.expect("sort options should be parsed");
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    let mut lines: Vec<String> = input.lines().map(str::to_string).collect();
    if options.numeric {
      lines.sort_by(|a, b| {
        let left = a.trim().parse::<f64>().unwrap_or(f64::NAN);
        let right = b.trim().parse::<f64>().unwrap_or(f64::NAN);
        left.partial_cmp(&right).unwrap_or_else(|| a.cmp(b))
      });
    } else {
      lines.sort();
    }
    if options.unique {
      lines.dedup();
    }
    if options.reverse {
      lines.reverse();
    }

    let stdout = ctx.stdout();
    for line in lines {
      let mut out = line.into_bytes();
      out.push(b'\n');
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }
    Ok(())
  }
}
