use std::io;

use crate::{
  app::AppContext,
  applets::support::{encode_base32, write_wrapped_output},
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct Base32Command {
  pub path: Option<String>,
}

impl Command for Base32Command {
  fn name() -> &'static str {
    "base32"
  }
  fn summary() -> &'static str {
    "Base32 encode data."
  }
  fn usage() -> &'static str {
    "base32 [file]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "base32: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let data = io_util::read_to_bytes(ctx.lio(), self.path.as_deref())?;
    write_wrapped_output(ctx.lio(), &encode_base32(&data), 76)
  }
}
