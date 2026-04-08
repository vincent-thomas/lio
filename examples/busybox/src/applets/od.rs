use std::io;

use crate::{
  app::AppContext, applets::support::od_char_repr, command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Copy)]
enum OdMode {
  Octal,
  Hex16,
  Chars,
}

#[derive(Debug, Clone, Default)]
pub struct OdCommand {
  mode: Option<OdMode>,
  pub path: Option<String>,
}

impl Command for OdCommand {
  fn name() -> &'static str {
    "od"
  }
  fn summary() -> &'static str {
    "Dump files in octal or other formats."
  }
  fn usage() -> &'static str {
    "od [-x|-c] [file]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    let mut mode = OdMode::Octal;
    let mut index = 0;
    if let Some(flag) = args.first().map(String::as_str) {
      match flag {
        "-x" => {
          mode = OdMode::Hex16;
          index = 1;
        }
        "-c" => {
          mode = OdMode::Chars;
          index = 1;
        }
        _ => {}
      }
    }
    if args.len() > index + 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "od: too many file operands",
      ));
    }
    Ok(Self { mode: Some(mode), path: args.get(index).cloned() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mode = self.mode.expect("od mode should be parsed");
    let data = io_util::read_to_bytes(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    for (offset, chunk) in data.chunks(16).enumerate() {
      let mut line = format!("{:07o} ", offset * 16);
      match mode {
        OdMode::Octal => {
          for byte in chunk {
            line.push_str(&format!(" {:03o}", byte));
          }
        }
        OdMode::Hex16 => {
          for word in chunk.chunks(2) {
            let value = if word.len() == 2 {
              u16::from_le_bytes([word[0], word[1]])
            } else {
              word[0] as u16
            };
            line.push_str(&format!(" {:04x}", value));
          }
        }
        OdMode::Chars => {
          for byte in chunk {
            line.push(' ');
            line.push_str(&od_char_repr(*byte));
          }
        }
      }
      line.push('\n');
      io_util::write_all(ctx.lio(), &stdout, line.into_bytes())?;
    }
    io_util::write_all(
      ctx.lio(),
      &stdout,
      format!("{:07o}\n", data.len()).into_bytes(),
    )
  }
}
