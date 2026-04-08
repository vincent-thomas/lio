use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct CmpCommand {
  pub left: String,
  pub right: String,
}

impl Command for CmpCommand {
  fn name() -> &'static str {
    "cmp"
  }

  fn summary() -> &'static str {
    "Compare two files byte by byte."
  }

  fn usage() -> &'static str {
    "cmp <file1> <file2>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() != 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cmp: expected exactly two files",
      ));
    }
    Ok(Self { left: args[0].clone(), right: args[1].clone() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let left = io_util::read_to_bytes(ctx.lio(), Some(&self.left))?;
    let right = io_util::read_to_bytes(ctx.lio(), Some(&self.right))?;

    let max_len = left.len().max(right.len());
    let mut line = 1usize;
    for i in 0..max_len {
      let l = left.get(i);
      let r = right.get(i);
      if l != r {
        let message = match (l, r) {
          (Some(_), Some(_)) => format!(
            "{} {} differ: byte {}, line {}\n",
            self.left,
            self.right,
            i + 1,
            line
          ),
          (None, _) => format!("cmp: EOF on {}\n", self.left),
          (_, None) => format!("cmp: EOF on {}\n", self.right),
        };
        io_util::write_all(ctx.lio(), &ctx.stdout(), message.into_bytes())?;
        return Ok(());
      }
      if l == Some(&b'\n') {
        line += 1;
      }
    }

    Ok(())
  }
}
