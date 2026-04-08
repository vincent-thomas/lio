use std::{io, path::Path};

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct BasenameCommand {
  pub path: String,
}

impl Command for BasenameCommand {
  fn name() -> &'static str {
    "basename"
  }

  fn summary() -> &'static str {
    "Strip leading directory components."
  }

  fn usage() -> &'static str {
    "basename <path>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() != 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "basename: expected exactly one path",
      ));
    }
    Ok(Self { path: args[0].clone() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let path = self.path.trim_end_matches('/');
    let output = if path.is_empty() {
      "/"
    } else {
      Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path)
    };

    let mut out = output.as_bytes().to_vec();
    out.push(b'\n');
    io_util::write_all(ctx.lio(), &ctx.stdout(), out)
  }
}
