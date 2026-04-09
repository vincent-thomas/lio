use std::io;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

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
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "reverse",
        short: &['r'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "numeric",
        short: &['n'],
        long: &[],
        takes_value: false,
      },
      FlagSpec { name: "unique", short: &['u'], long: &[], takes_value: false },
    ];
    let parsed = FlagParser::new("sort", SPECS).parse(args)?;
    if parsed.positional().len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "sort: too many file operands",
      ));
    }
    Ok(Self {
      options: Some(SortOptions {
        reverse: parsed.get_flag_exists("reverse"),
        numeric: parsed.get_flag_exists("numeric"),
        unique: parsed.get_flag_exists("unique"),
      }),
      path: parsed.positional().first().cloned(),
    })
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

    let total_len: usize = lines.iter().map(|line| line.len() + 1).sum();
    let mut out = Vec::with_capacity(total_len);
    for line in lines {
      out.extend_from_slice(line.as_bytes());
      out.push(b'\n');
    }
    io_util::write_all(ctx.lio(), &ctx.stdout(), out)
  }
}
