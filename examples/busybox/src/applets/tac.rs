use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct TacCommand {
  pub path: Option<String>,
}

impl Command for TacCommand {
  fn name() -> &'static str {
    "tac"
  }

  fn summary() -> &'static str {
    "Concatenate and print files in reverse line order."
  }

  fn usage() -> &'static str {
    "tac [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "tac: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    let mut lines: Vec<&str> = input.lines().collect();
    lines.reverse();
    for line in lines {
      let mut out = line.as_bytes().to_vec();
      out.push(b'\n');
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }
    Ok(())
  }
}
