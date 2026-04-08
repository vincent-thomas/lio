use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy, Default)]
pub struct ClearCommand;

impl Command for ClearCommand {
  fn name() -> &'static str {
    "clear"
  }

  fn summary() -> &'static str {
    "Clear the terminal screen."
  }

  fn usage() -> &'static str {
    "clear"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if !args.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "clear: expected no arguments",
      ));
    }
    Ok(Self)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    io_util::write_all(ctx.lio(), &ctx.stdout(), b"\x1b[2J\x1b[H".to_vec())
  }
}
