use std::io;

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
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    for raw_line in input.lines() {
      if self.suppress_without_delim && !raw_line.contains(self.delimiter) {
        continue;
      }
      let selected =
        raw_line.split(self.delimiter).nth(self.field - 1).unwrap_or("");
      let mut out = selected.as_bytes().to_vec();
      out.push(b'\n');
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }

    Ok(())
  }
}
