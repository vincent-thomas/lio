use std::io;

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

const OUTPUT_FLUSH_THRESHOLD: usize = 64 * 1024;

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
        out.push_str(&render_hexdump_line(
          &pending[..16],
          offset,
          self.canonical,
        ));
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
      out.push_str(&render_hexdump_line(&pending, offset, self.canonical));
      offset += pending.len();
    }

    if self.canonical {
      out.push_str(&format!("{:08x}\n", offset));
    } else {
      out.push_str(&format!("{:07x}\n", offset));
    }

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

fn render_hexdump_line(chunk: &[u8], offset: usize, canonical: bool) -> String {
  let mut line = if canonical {
    format!("{:08x}  ", offset)
  } else {
    format!("{:07x} ", offset)
  };
  for i in 0..16 {
    if let Some(byte) = chunk.get(i) {
      line.push_str(&format!("{byte:02x} "));
    } else {
      line.push_str("   ");
    }
    if canonical && i == 7 {
      line.push(' ');
    }
  }
  if canonical {
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
  line
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn render_hexdump_line_formats_partial_canonical_row() {
    let rendered = render_hexdump_line(b"abc", 0, true);
    assert!(rendered.starts_with("00000000"));
    assert!(rendered.contains("|abc|"));
  }
}
