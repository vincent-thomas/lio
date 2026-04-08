use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct TouchCommand {
  pub files: Vec<String>,
}

impl Command for TouchCommand {
  fn name() -> &'static str {
    "touch"
  }

  fn summary() -> &'static str {
    "Create files if they do not exist."
  }

  fn usage() -> &'static str {
    "touch <file...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "touch: missing file operand",
      ));
    }
    Ok(Self { files: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let cwd = ctx.cwd();
    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&cwd, cpath, libc::O_WRONLY | libc::O_CREAT)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    for result in io_util::run_all(ctx.lio(), open_receivers) {
      let _ = result?;
    }
    Ok(())
  }
}
