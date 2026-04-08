use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct EchoCommand {
  pub suppress_newline: bool,
  pub interpret_escapes: bool,
  pub args: Vec<String>,
}

impl Command for EchoCommand {
  fn name() -> &'static str {
    "echo"
  }

  fn summary() -> &'static str {
    "Echo arguments to stdout."
  }

  fn usage() -> &'static str {
    "echo [-n] [-e|-E] [arg...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut suppress_newline = false;
    let mut interpret_escapes = false;
    let mut index = 0;

    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-n" => suppress_newline = true,
        "-e" => interpret_escapes = true,
        "-E" => interpret_escapes = false,
        _ => break,
      }
      index += 1;
    }

    Ok(Self {
      suppress_newline,
      interpret_escapes,
      args: args[index..].to_vec(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mut line = self.args.join(" ");
    if self.interpret_escapes {
      line = interpret_echo_escapes(&line);
    }

    let mut line = line.into_bytes();
    if !self.suppress_newline {
      line.push(b'\n');
    }
    io_util::write_all(ctx.lio(), &ctx.stdout(), line)
  }
}

fn interpret_echo_escapes(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut chars = input.chars();

  while let Some(ch) = chars.next() {
    if ch != '\\' {
      out.push(ch);
      continue;
    }

    match chars.next() {
      Some('a') => out.push('\x07'),
      Some('b') => out.push('\x08'),
      Some('c') => break,
      Some('e') | Some('E') => out.push('\x1b'),
      Some('f') => out.push('\x0c'),
      Some('n') => out.push('\n'),
      Some('r') => out.push('\r'),
      Some('t') => out.push('\t'),
      Some('v') => out.push('\x0b'),
      Some('\\') => out.push('\\'),
      Some(other) => {
        out.push('\\');
        out.push(other);
      }
      None => out.push('\\'),
    }
  }

  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn echo_interprets_escapes_when_enabled() {
    let command = EchoCommand {
      interpret_escapes: true,
      args: vec!["a\\nb".into()],
      ..EchoCommand::default()
    };
    assert_eq!(interpret_echo_escapes(&command.args[0]), "a\nb");
  }
}
