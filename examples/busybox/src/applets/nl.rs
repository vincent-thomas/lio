use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct NlCommand {
  pub path: Option<String>,
}

impl Command for NlCommand {
  fn name() -> &'static str {
    "nl"
  }

  fn summary() -> &'static str {
    "Number lines of files."
  }

  fn usage() -> &'static str {
    "nl [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "nl: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    for (i, line) in input.lines().enumerate() {
      let out = format!("{:>6}\t{}\n", i + 1, line).into_bytes();
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }
    Ok(())
  }
}
