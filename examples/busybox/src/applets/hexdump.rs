use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct HexdumpCommand {
  pub canonical: bool,
  pub path: Option<String>,
}

impl Command for HexdumpCommand {
  fn name() -> &'static str {
    "hexdump"
  }
  fn summary() -> &'static str {
    "Display file contents in hexadecimal."
  }
  fn usage() -> &'static str {
    "hexdump [-C] [file]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    let mut canonical = false;
    let mut index = 0;
    if args.first().map(String::as_str) == Some("-C") {
      canonical = true;
      index = 1;
    }
    if args.len() > index + 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "hexdump: too many file operands",
      ));
    }
    Ok(Self { canonical, path: args.get(index).cloned() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let data = io_util::read_to_bytes(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    for (offset, chunk) in data.chunks(16).enumerate() {
      let mut line = if self.canonical {
        format!("{:08x}  ", offset * 16)
      } else {
        format!("{:07x} ", offset * 16)
      };
      for i in 0..16 {
        if let Some(byte) = chunk.get(i) {
          line.push_str(&format!("{byte:02x} "));
        } else {
          line.push_str("   ");
        }
        if self.canonical && i == 7 {
          line.push(' ');
        }
      }
      if self.canonical {
        line.push_str(" |");
        for byte in chunk {
          let ch = if byte.is_ascii_graphic() || *byte == b' ' {
            *byte as char
          } else {
            '.'
          };
          line.push(ch);
        }
        line.push('|');
      }
      line.push('\n');
      io_util::write_all(ctx.lio(), &stdout, line.into_bytes())?;
    }
    io_util::write_all(
      ctx.lio(),
      &stdout,
      if self.canonical {
        format!("{:08x}\n", data.len()).into_bytes()
      } else {
        format!("{:07x}\n", data.len()).into_bytes()
      },
    )
  }
}
