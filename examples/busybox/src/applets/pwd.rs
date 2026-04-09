use std::io;

use crate::{
  app::AppContext,
  command::Command,
  util::{cwd, io as io_util},
};

#[derive(Debug, Clone, Copy, Default)]
pub struct PwdCommand;

impl Command for PwdCommand {
  fn name() -> &'static str {
    "pwd"
  }

  fn summary() -> &'static str {
    "Print the current working directory."
  }

  fn usage() -> &'static str {
    "pwd"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if !args.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "pwd: expected no arguments",
      ));
    }
    Ok(Self)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mut output = cwd::current_working_directory_bytes(ctx)?;
    output.push(b'\n');
    io_util::write_all(ctx.lio(), &ctx.stdout(), output)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::ffi::OsStrExt;

  #[test]
  fn parse_pwd_rejects_arguments() {
    let err = PwdCommand::parse(&["extra".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn pwd_reads_current_working_directory() {
    let ctx = AppContext::new().unwrap();
    let actual = cwd::current_working_directory_bytes(&ctx).unwrap();
    let expected = std::env::current_dir().unwrap();
    let expected = expected.as_os_str().as_bytes().to_vec();
    assert_eq!(actual, expected);
  }
}
