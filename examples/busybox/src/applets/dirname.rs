use std::{io, path::Path};

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct DirnameCommand {
  pub path: String,
}

impl Command for DirnameCommand {
  fn name() -> &'static str {
    "dirname"
  }

  fn summary() -> &'static str {
    "Strip the last path component."
  }

  fn usage() -> &'static str {
    "dirname <path>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() != 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "dirname: expected exactly one path",
      ));
    }
    Ok(Self { path: args[0].clone() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let output = Path::new(&self.path)
      .parent()
      .and_then(|parent| parent.to_str())
      .filter(|parent| !parent.is_empty())
      .unwrap_or(".");

    let mut out = output.as_bytes().to_vec();
    out.push(b'\n');
    io_util::write_all(ctx.lio(), &ctx.stdout(), out)
  }
}
