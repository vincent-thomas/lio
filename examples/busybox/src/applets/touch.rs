use std::{ffi::CString, io};

use lio::api;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

#[derive(Debug, Clone, Default)]
pub struct TouchCommand {
  pub no_create: bool,
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
    "touch [-c] <file...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[FlagSpec {
      name: "no_create",
      short: &['c'],
      long: &[],
      takes_value: false,
    }];
    let parsed = FlagParser::new("touch", SPECS).parse(args)?;
    if parsed.positional().is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "touch: missing file operand",
      ));
    }
    Ok(Self {
      no_create: parsed.get_flag_exists("no_create"),
      files: parsed.positional().to_vec(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let cwd = ctx.cwd();
    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      let flags =
        libc::O_WRONLY | if self.no_create { 0 } else { libc::O_CREAT };
      let mode = if self.no_create { 0 } else { 0o666 };
      open_receivers
        .push(api::openat(&cwd, cpath, flags, mode).with_lio(ctx.lio()).send());
    }

    for result in io_util::run_all(ctx.lio(), open_receivers) {
      match result {
        Ok(_) => {}
        Err(err) if self.no_create && err.kind() == io::ErrorKind::NotFound => {
        }
        Err(err) => return Err(err),
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  #[test]
  fn parse_touch_supports_c_flag() {
    let parsed = TouchCommand::parse(&["-c".into(), "file".into()]).unwrap();
    assert!(parsed.no_create);
    assert_eq!(parsed.files, vec!["file"]);
  }

  #[test]
  fn touch_c_does_not_create_missing_file() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("touch-c");

    TouchCommand { no_create: true, files: vec![path.display().to_string()] }
      .execute(&ctx)
      .unwrap();

    assert!(!path.exists());
  }

  fn unique_temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
      "busybox-{}-{}-{}",
      name,
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ))
  }
}
