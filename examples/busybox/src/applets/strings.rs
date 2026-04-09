use std::io;

use lio::api;

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
    let input = open_input(ctx, self.path.as_deref())?;
    let stdout = ctx.stdout();
    let mut current = Vec::new();
    let mut buf = vec![0u8; 8192];

    loop {
      let rx = api::read(&input, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        break;
      }

      for &byte in &buf[..n] {
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
    }
    if current.len() >= self.min_len {
      current.push(b'\n');
      io_util::write_all(ctx.lio(), &stdout, current)?;
    }
    Ok(())
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

fn parse_usize_arg(value: &str, applet: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{applet}: invalid count '{value}'"),
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn strings_parse_accepts_n_flag() {
    let parsed =
      StringsCommand::parse(&["-n".into(), "5".into(), "file".into()]).unwrap();
    assert_eq!(parsed.min_len, 5);
    assert_eq!(parsed.path.as_deref(), Some("file"));
  }
}
