use std::io;

use lio::api;

use crate::{
  app::AppContext, applets::support::od_char_repr, command::Command,
  util::io as io_util,
};

const OUTPUT_FLUSH_THRESHOLD: usize = 64 * 1024;

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
    let input = open_input(ctx, self.path.as_deref())?;
    let stdout = ctx.stdout();
    let mut buf = vec![0u8; 8192];
    let mut pending = Vec::new();
    let mut out = String::new();
    let mut offset = 0usize;

    loop {
      let rx = api::read(&input, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        break;
      }

      pending.extend_from_slice(&buf[..n]);
      while pending.len() >= 16 {
        out.push_str(&render_od_line(&pending[..16], offset, mode));
        pending.drain(..16);
        offset += 16;
        if out.len() >= OUTPUT_FLUSH_THRESHOLD {
          io_util::write_all(
            ctx.lio(),
            &stdout,
            std::mem::take(&mut out).into_bytes(),
          )?;
        }
      }
    }

    if !pending.is_empty() {
      out.push_str(&render_od_line(&pending, offset, mode));
      offset += pending.len();
    }

    out.push_str(&format!("{:07o}\n", offset));

    io_util::write_all(ctx.lio(), &stdout, out.into_bytes())
  }
}

fn open_input(
  ctx: &AppContext,
  path: Option<&str>,
) -> io::Result<lio::api::resource::Resource> {
  match path {
    Some(path) => io_util::run(
      ctx.lio(),
      api::openat(&ctx.cwd(), std::ffi::CString::new(path)?, libc::O_RDONLY, 0)
        .with_lio(ctx.lio())
        .send(),
    ),
    None => Ok(ctx.stdin()),
  }
}

fn render_od_line(chunk: &[u8], offset: usize, mode: OdMode) -> String {
  let mut line = format!("{:07o} ", offset);
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
  line
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn render_od_line_formats_chars_mode() {
    assert_eq!(render_od_line(b"a\n", 0, OdMode::Chars), "0000000  a \\n\n");
  }
}
