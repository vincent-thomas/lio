use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy)]
struct CommOptions {
  show_left: bool,
  show_right: bool,
  show_common: bool,
}

impl Default for CommOptions {
  fn default() -> Self {
    Self { show_left: true, show_right: true, show_common: true }
  }
}

#[derive(Debug, Clone, Default)]
pub struct CommCommand {
  options: Option<CommOptions>,
  pub left_path: String,
  pub right_path: String,
}

impl Command for CommCommand {
  fn name() -> &'static str {
    "comm"
  }

  fn summary() -> &'static str {
    "Compare two sorted files line by line."
  }

  fn usage() -> &'static str {
    "comm [-1] [-2] [-3] <file1> <file2>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut options = CommOptions::default();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-1" => {
          options.show_left = false;
          index += 1;
        }
        "-2" => {
          options.show_right = false;
          index += 1;
        }
        "-3" => {
          options.show_common = false;
          index += 1;
        }
        _ => break,
      }
    }
    if args.len() != index + 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "comm: expected two input files",
      ));
    }
    Ok(Self {
      options: Some(options),
      left_path: args[index].clone(),
      right_path: args[index + 1].clone(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let options = self.options.expect("comm options should be parsed");
    let left: Vec<String> =
      io_util::read_to_string(ctx.lio(), Some(&self.left_path))?
        .lines()
        .map(str::to_string)
        .collect();
    let right: Vec<String> =
      io_util::read_to_string(ctx.lio(), Some(&self.right_path))?
        .lines()
        .map(str::to_string)
        .collect();

    let stdout = ctx.stdout();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < left.len() || j < right.len() {
      let line = match (left.get(i), right.get(j)) {
        (Some(l), Some(r)) if l == r => {
          i += 1;
          j += 1;
          if options.show_common {
            format!("{}{}{}\n", comm_prefix(&options, 3), "", l)
          } else {
            String::new()
          }
        }
        (Some(l), Some(r)) if l < r => {
          i += 1;
          if options.show_left {
            format!("{}{}\n", comm_prefix(&options, 1), l)
          } else {
            String::new()
          }
        }
        (Some(_), Some(r)) => {
          j += 1;
          if options.show_right {
            format!("{}{}\n", comm_prefix(&options, 2), r)
          } else {
            String::new()
          }
        }
        (Some(l), None) => {
          i += 1;
          if options.show_left {
            format!("{}{}\n", comm_prefix(&options, 1), l)
          } else {
            String::new()
          }
        }
        (None, Some(r)) => {
          j += 1;
          if options.show_right {
            format!("{}{}\n", comm_prefix(&options, 2), r)
          } else {
            String::new()
          }
        }
        (None, None) => break,
      };
      if !line.is_empty() {
        io_util::write_all(ctx.lio(), &stdout, line.into_bytes())?;
      }
    }
    Ok(())
  }
}

fn comm_prefix(options: &CommOptions, column: u8) -> &'static str {
  match column {
    1 => "",
    2 => {
      if options.show_left {
        "\t"
      } else {
        ""
      }
    }
    3 => match (options.show_left, options.show_right) {
      (true, true) => "\t\t",
      (true, false) | (false, true) => "\t",
      (false, false) => "",
    },
    _ => "",
  }
}
