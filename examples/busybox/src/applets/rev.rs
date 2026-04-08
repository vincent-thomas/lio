use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct RevCommand {
  pub path: Option<String>,
}

impl Command for RevCommand {
  fn name() -> &'static str {
    "rev"
  }

  fn summary() -> &'static str {
    "Reverse lines characterwise."
  }

  fn usage() -> &'static str {
    "rev [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() > 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rev: too many file operands",
      ));
    }
    Ok(Self { path: args.first().cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    for line in input.split_inclusive('\n') {
      let has_newline = line.ends_with('\n');
      let mut chars: Vec<char> = line.trim_end_matches('\n').chars().collect();
      chars.reverse();
      let mut out: String = chars.into_iter().collect();
      if has_newline {
        out.push('\n');
      }
      io_util::write_all(ctx.lio(), &stdout, out.into_bytes())?;
    }

    if !input.is_empty() && !input.ends_with('\n') && !input.contains('\n') {
      let mut chars: Vec<char> = input.chars().collect();
      chars.reverse();
      io_util::write_all(
        ctx.lio(),
        &stdout,
        chars.into_iter().collect::<String>().into_bytes(),
      )?;
    }

    Ok(())
  }
}
