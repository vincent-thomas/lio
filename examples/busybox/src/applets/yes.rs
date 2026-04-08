use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct YesCommand {
  pub args: Vec<String>,
}

impl Command for YesCommand {
  fn name() -> &'static str {
    "yes"
  }

  fn summary() -> &'static str {
    "Output a string repeatedly until killed."
  }

  fn usage() -> &'static str {
    "yes [string...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { args: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let stdout = ctx.stdout();
    let mut buf = if self.args.is_empty() {
      b"y\n".to_vec()
    } else {
      format!("{}\n", self.args.join(" ")).into_bytes()
    };

    loop {
      let (result, returned_buf) = io_util::write_once(ctx.lio(), &stdout, buf);
      result?;
      buf = returned_buf;
    }
  }
}
