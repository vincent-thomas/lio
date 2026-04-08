use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct StringsCommand {
  pub min_len: usize,
  pub path: Option<String>,
}

impl Command for StringsCommand {
  fn name() -> &'static str {
    "strings"
  }

  fn summary() -> &'static str {
    "Print printable strings from binary data."
  }

  fn usage() -> &'static str {
    "strings [-n N] [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut min_len = 4usize;
    let mut index = 0;
    if args.len() >= 2 && args[0] == "-n" {
      min_len = parse_usize_arg(&args[1], "strings")?;
      index = 2;
    }
    if args.len() > index + 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "strings: invalid arguments",
      ));
    }
    Ok(Self { min_len, path: args.get(index).cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let data = io_util::read_to_bytes(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    let mut current = Vec::new();

    for byte in data {
      if byte.is_ascii_graphic() || byte == b' ' {
        current.push(byte);
      } else if current.len() >= self.min_len {
        let mut out = std::mem::take(&mut current);
        out.push(b'\n');
        io_util::write_all(ctx.lio(), &stdout, out)?;
      } else {
        current.clear();
      }
    }
    if current.len() >= self.min_len {
      current.push(b'\n');
      io_util::write_all(ctx.lio(), &stdout, current)?;
    }
    Ok(())
  }
}

fn parse_usize_arg(value: &str, applet: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{applet}: invalid count '{value}'"),
    )
  })
}
