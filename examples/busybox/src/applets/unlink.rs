use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct UnlinkCommand {
  pub path: String,
}

impl Command for UnlinkCommand {
  fn name() -> &'static str {
    "unlink"
  }

  fn summary() -> &'static str {
    "Remove a single file."
  }

  fn usage() -> &'static str {
    "unlink <file>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() != 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "unlink: expected exactly one operand",
      ));
    }
    Ok(Self { path: args[0].clone() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let cpath = CString::new(self.path.as_bytes()).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "unlink: invalid path")
    })?;
    let mut receiver =
      api::unlinkat(&ctx.cwd(), cpath, 0).with_lio(ctx.lio()).send();
    io_util::run_recv(ctx.lio(), &mut receiver)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, path::PathBuf};

  #[test]
  fn parse_unlink_requires_exactly_one_operand() {
    let err = UnlinkCommand::parse(&[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = UnlinkCommand::parse(&["a".into(), "b".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn unlink_removes_file() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("unlink-file");
    fs::write(&path, b"hello").unwrap();

    UnlinkCommand { path: path.display().to_string() }.execute(&ctx).unwrap();

    assert!(!path.exists());
  }

  #[test]
  fn unlink_rejects_directory() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("unlink-dir");
    fs::create_dir(&path).unwrap();

    let err = UnlinkCommand { path: path.display().to_string() }
      .execute(&ctx)
      .unwrap_err();
    assert!(path.exists());
    fs::remove_dir(&path).unwrap();
    assert!(matches!(
      err.kind(),
      io::ErrorKind::IsADirectory | io::ErrorKind::PermissionDenied
    ));
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
