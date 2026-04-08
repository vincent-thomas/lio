use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy, Default)]
pub struct SeqCommand {
  pub start: f64,
  pub step: f64,
  pub end: f64,
}

impl Command for SeqCommand {
  fn name() -> &'static str {
    "seq"
  }

  fn summary() -> &'static str {
    "Print a sequence of numbers."
  }

  fn usage() -> &'static str {
    "seq <end> | seq <start> <end> | seq <start> <step> <end>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let (start, step, end) = match args.len() {
      1 => (1.0, 1.0, parse_seq_number(&args[0])?),
      2 => (parse_seq_number(&args[0])?, 1.0, parse_seq_number(&args[1])?),
      3 => (
        parse_seq_number(&args[0])?,
        parse_seq_number(&args[1])?,
        parse_seq_number(&args[2])?,
      ),
      _ => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "seq: expected 1, 2, or 3 numeric arguments",
        ));
      }
    };

    if step == 0.0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "seq: step must not be zero",
      ));
    }

    Ok(Self { start, step, end })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let stdout = ctx.stdout();
    let mut current = self.start;
    let epsilon = self.step.abs() / 1_000_000.0;

    while if self.step.is_sign_positive() {
      current <= self.end + epsilon
    } else {
      current >= self.end - epsilon
    } {
      let mut line = format_seq_number(current).into_bytes();
      line.push(b'\n');
      io_util::write_all(ctx.lio(), &stdout, line)?;
      current += self.step;
    }

    Ok(())
  }
}

fn parse_seq_number(value: &str) -> io::Result<f64> {
  value.parse::<f64>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("seq: invalid number '{value}'"),
    )
  })
}

fn format_seq_number(value: f64) -> String {
  let rounded = value.round();
  if (value - rounded).abs() < 1e-9 {
    format!("{}", rounded as i64)
  } else {
    let mut text = format!("{value}");
    while text.contains('.') && text.ends_with('0') {
      text.pop();
    }
    if text.ends_with('.') {
      text.pop();
    }
    text
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_seq_command_accepts_three_forms() {
    assert_eq!(SeqCommand::parse(&["3".to_string()]).unwrap().end, 3.0);
    assert_eq!(
      SeqCommand::parse(&["2".to_string(), "4".to_string()]).unwrap().start,
      2.0
    );
    assert_eq!(
      SeqCommand::parse(&["1".to_string(), "2".to_string(), "5".to_string()])
        .unwrap()
        .step,
      2.0
    );
  }
}
