use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct FoldCommand {
  pub width: usize,
  pub path: Option<String>,
}

impl Command for FoldCommand {
  fn name() -> &'static str {
    "fold"
  }

  fn summary() -> &'static str {
    "Wrap input lines to fit a width."
  }

  fn usage() -> &'static str {
    "fold [-w N] [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut width = 80usize;
    let mut index = 0;
    if args.len() >= 2 && args[0] == "-w" {
      width = parse_usize_arg(&args[1], "fold")?;
      index = 2;
    }
    if width == 0 || args.len() > index + 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "fold: invalid arguments",
      ));
    }
    Ok(Self { width, path: args.get(index).cloned() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    let stdout = ctx.stdout();
    for raw_line in input.split_inclusive('\n') {
      let has_newline = raw_line.ends_with('\n');
      let line = raw_line.trim_end_matches('\n');
      let mut current = String::new();
      let mut count = 0usize;
      for ch in line.chars() {
        current.push(ch);
        count += 1;
        if count >= self.width {
          current.push('\n');
          io_util::write_all(
            ctx.lio(),
            &stdout,
            std::mem::take(&mut current).into_bytes(),
          )?;
          count = 0;
        }
      }
      if !current.is_empty() || has_newline {
        if has_newline {
          current.push('\n');
        }
        if !current.is_empty() {
          io_util::write_all(ctx.lio(), &stdout, current.into_bytes())?;
        }
      }
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
