use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy, Default)]
struct UniqMode {
  show_count: bool,
  only_duplicates: bool,
  only_unique: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UniqCommand {
  mode: Option<UniqMode>,
  pub path: Option<String>,
}

impl Command for UniqCommand {
  fn name() -> &'static str {
    "uniq"
  }

  fn summary() -> &'static str {
    "Report or filter repeated lines."
  }

  fn usage() -> &'static str {
    "uniq [-c] [-d] [-u] [file]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut mode = UniqMode::default();
    let mut path = None;

    for arg in args {
      match arg.as_str() {
        "-c" => mode.show_count = true,
        "-d" => mode.only_duplicates = true,
        "-u" => mode.only_unique = true,
        _ => {
          if path.is_some() {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "uniq: too many file operands",
            ));
          }
          path = Some(arg.clone());
        }
      }
    }

    Ok(Self { mode: Some(mode), path })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mode = self.mode.expect("uniq mode should be parsed");
    let stdout = ctx.stdout();
    let input = io_util::read_to_string(ctx.lio(), self.path.as_deref())?;
    for (line, count) in group_lines(&input) {
      if (mode.only_duplicates && count == 1)
        || (mode.only_unique && count != 1)
      {
        continue;
      }

      let mut out = if mode.show_count {
        format!("{:>7} {}", count, line.trim_end_matches('\n')).into_bytes()
      } else {
        line.as_bytes().to_vec()
      };

      if mode.show_count && !line.ends_with('\n') {
        out.push(b'\n');
      }
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }

    Ok(())
  }
}

fn group_lines(input: &str) -> Vec<(String, usize)> {
  let mut groups = Vec::new();
  let mut iter = input.split_inclusive('\n').peekable();

  while let Some(line) = iter.next() {
    let mut count = 1usize;
    while matches!(iter.peek(), Some(next) if *next == line) {
      iter.next();
      count += 1;
    }
    groups.push((line.to_string(), count));
  }

  if !input.is_empty() && !input.ends_with('\n') {
    let last = input.lines().last().unwrap_or("").to_string();
    match groups.last_mut() {
      Some((line, count)) if line.trim_end_matches('\n') == last => {
        if !line.ends_with('\n') {
          *count += 1;
        }
      }
      _ => groups.push((last, 1)),
    }
  }

  groups
}
