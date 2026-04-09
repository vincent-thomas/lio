use std::{
  io,
  time::{Duration, Instant},
};

use crate::{
  app::AppContext, command::Command, exit_with_status,
  util::process as process_util,
};

#[derive(Debug, Clone, Default)]
pub struct TimeoutCommand {
  pub seconds: f64,
  pub command: String,
  pub args: Vec<String>,
}

impl Command for TimeoutCommand {
  fn name() -> &'static str {
    "timeout"
  }

  fn summary() -> &'static str {
    "Run a command with a time limit."
  }

  fn usage() -> &'static str {
    "timeout <seconds> <command> [args...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() < 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "timeout: expected <seconds> <command> [args...]",
      ));
    }
    let seconds = args[0].parse::<f64>().map_err(|_| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("timeout: invalid seconds '{}'", args[0]),
      )
    })?;
    if !seconds.is_finite() || seconds < 0.0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "timeout: seconds must be a non-negative number",
      ));
    }
    Ok(Self { seconds, command: args[1].clone(), args: args[2..].to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let pid = process_util::spawn_command(ctx, &self.command, &self.args)?;
    let deadline = Instant::now() + Duration::from_secs_f64(self.seconds);

    loop {
      if let Some(status) = process_util::try_wait_for_child(pid)? {
        return propagate_child_status(status);
      }
      if Instant::now() >= deadline {
        process_util::signal_child(pid, libc::SIGKILL)?;
        let _ = process_util::wait_for_child(pid)?;
        return Err(exit_with_status(124));
      }
      let remaining = deadline.saturating_duration_since(Instant::now());
      let sleep_for = remaining.min(Duration::from_millis(50));
      let rx = lio::api::sleep(sleep_for).with_lio(ctx.lio()).send();
      crate::util::io::run(ctx.lio(), rx)?;
    }
  }
}

fn propagate_child_status(status: process_util::ChildStatus) -> io::Result<()> {
  match status {
    process_util::ChildStatus::Exited(0) => Ok(()),
    process_util::ChildStatus::Exited(code) => {
      Err(exit_with_status(code.clamp(0, 255) as u8))
    }
    process_util::ChildStatus::Signaled(signal) => Err(io::Error::other(
      format!("timeout: child terminated by signal {signal}"),
    )),
    process_util::ChildStatus::Other(raw) => Err(io::Error::other(format!(
      "timeout: child did not exit normally ({raw})"
    ))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_timeout_splits_command_and_args() {
    let parsed =
      TimeoutCommand::parse(&["1.5".into(), "echo".into(), "a".into()])
        .unwrap();
    assert_eq!(parsed.seconds, 1.5);
    assert_eq!(parsed.command, "echo");
    assert_eq!(parsed.args, vec!["a"]);
  }

  #[test]
  fn parse_timeout_rejects_invalid_seconds() {
    assert!(TimeoutCommand::parse(&["nope".into(), "echo".into()]).is_err());
    assert!(TimeoutCommand::parse(&["-1".into(), "echo".into()]).is_err());
  }
}
