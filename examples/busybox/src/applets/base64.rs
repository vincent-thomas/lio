use std::io;

use crate::{
  app::AppContext,
  applets::support::{encode_base64, write_wrapped_output},
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct Base64Command {
  pub path: Option<String>,
}

impl Command for Base64Command {
  fn name() -> &'static str {
    "base64"
  }
  fn summary() -> &'static str {
    "Base64 encode data."
  }
  fn usage() -> &'static str {
    "base64 [file]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "base64: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let data = io_util::read_to_bytes(ctx.lio(), self.path.as_deref())?;
    write_wrapped_output(ctx.lio(), &encode_base64(&data), 76)
  }
}
