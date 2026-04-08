use std::{io, time::Duration};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy, Default)]
pub struct SleepCommand {
  pub seconds: f64,
}

impl Command for SleepCommand {
  fn name() -> &'static str {
    "sleep"
  }

  fn summary() -> &'static str {
    "Sleep for a duration in seconds."
  }

  fn usage() -> &'static str {
    "sleep <seconds>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() != 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "sleep: expected exactly one argument",
      ));
    }
    let seconds = args[0].parse::<f64>().map_err(|_| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("sleep: invalid seconds '{}'", args[0]),
      )
    })?;
    Ok(Self { seconds })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let rx = api::sleep(Duration::from_secs_f64(self.seconds))
      .with_lio(ctx.lio())
      .send();
    io_util::run(ctx.lio(), rx)?;
    Ok(())
  }
}
