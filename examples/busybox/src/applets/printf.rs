use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct PrintfCommand {
  pub format: String,
  pub args: Vec<String>,
}

impl Command for PrintfCommand {
  fn name() -> &'static str {
    "printf"
  }

  fn summary() -> &'static str {
    "Format and print arguments."
  }

  fn usage() -> &'static str {
    "printf <format> [arg...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let Some(format) = args.first() else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "printf: missing format operand",
      ));
    };
    Ok(Self { format: format.clone(), args: args[1..].to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let rendered = render_printf_format(&self.format, &self.args);
    io_util::write_all(ctx.lio(), &ctx.stdout(), rendered.into_bytes())
  }
}

fn interpret_backslash_escapes(input: &str) -> String {
  let mut out = String::new();
  let mut chars = input.chars();
  while let Some(ch) = chars.next() {
    if ch != '\\' {
      out.push(ch);
      continue;
    }
    match chars.next() {
      Some('a') => out.push('\x07'),
      Some('b') => out.push('\x08'),
      Some('e') | Some('E') => out.push('\x1b'),
      Some('f') => out.push('\x0c'),
      Some('n') => out.push('\n'),
      Some('r') => out.push('\r'),
      Some('t') => out.push('\t'),
      Some('v') => out.push('\x0b'),
      Some('\\') => out.push('\\'),
      Some('0') => out.push('\0'),
      Some(other) => {
        out.push('\\');
        out.push(other);
      }
      None => out.push('\\'),
    }
  }
  out
}

fn render_printf_format(format: &str, args: &[String]) -> String {
  let mut out = String::new();
  let escaped = interpret_backslash_escapes(format);
  let mut chars = escaped.chars();
  let mut arg_index = 0usize;

  while let Some(ch) = chars.next() {
    if ch != '%' {
      out.push(ch);
      continue;
    }

    match chars.next() {
      Some('%') => out.push('%'),
      Some('s') => {
        if let Some(arg) = args.get(arg_index) {
          out.push_str(arg);
        }
        arg_index += 1;
      }
      Some('d') | Some('i') => {
        let value = args
          .get(arg_index)
          .and_then(|arg| arg.parse::<i64>().ok())
          .unwrap_or(0);
        out.push_str(&value.to_string());
        arg_index += 1;
      }
      Some('u') => {
        let value = args
          .get(arg_index)
          .and_then(|arg| arg.parse::<u64>().ok())
          .unwrap_or(0);
        out.push_str(&value.to_string());
        arg_index += 1;
      }
      Some('x') => {
        let value = args
          .get(arg_index)
          .and_then(|arg| arg.parse::<u64>().ok())
          .unwrap_or(0);
        out.push_str(&format!("{value:x}"));
        arg_index += 1;
      }
      Some('X') => {
        let value = args
          .get(arg_index)
          .and_then(|arg| arg.parse::<u64>().ok())
          .unwrap_or(0);
        out.push_str(&format!("{value:X}"));
        arg_index += 1;
      }
      Some('o') => {
        let value = args
          .get(arg_index)
          .and_then(|arg| arg.parse::<u64>().ok())
          .unwrap_or(0);
        out.push_str(&format!("{value:o}"));
        arg_index += 1;
      }
      Some(other) => {
        out.push('%');
        out.push(other);
      }
      None => out.push('%'),
    }
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn printf_renders_basic_substitutions() {
    let rendered = render_printf_format(
      "a=%s b=%d c=%x",
      &["hi".into(), "7".into(), "15".into()],
    );
    assert_eq!(rendered, "a=hi b=7 c=f");
  }
}
