use std::io;

use crate::{
  app::AppContext,
  applets::support::{expand_tr_set, interpret_backslash_escapes},
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct TrCommand {
  pub delete: bool,
  pub squeeze: bool,
  pub set1: Vec<u8>,
  pub set2: Vec<u8>,
}

impl Command for TrCommand {
  fn name() -> &'static str {
    "tr"
  }
  fn summary() -> &'static str {
    "Translate or delete characters."
  }
  fn usage() -> &'static str {
    "tr [-d] [-s] <set1> [set2]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    if args.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "tr: missing set operand",
      ));
    }
    let mut delete = false;
    let mut squeeze = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-d" => {
          delete = true;
          index += 1;
        }
        "-s" => {
          squeeze = true;
          index += 1;
        }
        _ => break,
      }
    }
    let Some(set1_raw) = args.get(index) else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "tr: missing set1",
      ));
    };
    let set1 = expand_tr_set(&interpret_backslash_escapes(set1_raw));
    let set2 = if delete {
      Vec::new()
    } else {
      let Some(raw) = args.get(index + 1) else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "tr: missing set2",
        ));
      };
      expand_tr_set(&interpret_backslash_escapes(raw))
    };
    Ok(Self { delete, squeeze, set1, set2 })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = io_util::read_to_bytes(ctx.lio(), None)?;
    let mut output = Vec::with_capacity(input.len());
    let squeeze_set = if self.squeeze {
      if self.delete { self.set1.clone() } else { self.set2.clone() }
    } else {
      Vec::new()
    };
    for byte in input {
      if let Some(position) =
        self.set1.iter().position(|candidate| *candidate == byte)
      {
        if self.delete {
          continue;
        }
        let transformed = self
          .set2
          .get(position)
          .copied()
          .or_else(|| self.set2.last().copied())
          .unwrap_or(byte);
        if self.squeeze
          && squeeze_set.contains(&transformed)
          && output.last().copied() == Some(transformed)
        {
          continue;
        }
        output.push(transformed);
      } else {
        if self.squeeze
          && squeeze_set.contains(&byte)
          && output.last().copied() == Some(byte)
        {
          continue;
        }
        output.push(byte);
      }
    }
    io_util::write_all(ctx.lio(), &ctx.stdout(), output)
  }
}
