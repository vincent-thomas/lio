use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct RmdirCommand {
  pub paths: Vec<String>,
}

impl Command for RmdirCommand {
  fn name() -> &'static str {
    "rmdir"
  }

  fn summary() -> &'static str {
    "Remove empty directories."
  }

  fn usage() -> &'static str {
    "rmdir <dir...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rmdir: missing operand",
      ));
    }
    Ok(Self { paths: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    for path in &self.paths {
      let cpath = CString::new(path.as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "rmdir: invalid path")
      })?;
      let mut receiver = api::unlinkat(&ctx.cwd(), cpath, libc::AT_REMOVEDIR)
        .with_lio(ctx.lio())
        .send();
      io_util::run_recv(ctx.lio(), &mut receiver)?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, path::PathBuf};

  #[test]
  fn parse_rmdir_requires_operand() {
    let err = RmdirCommand::parse(&[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn rmdir_removes_empty_directory() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("rmdir-empty");
    fs::create_dir(&path).unwrap();

    RmdirCommand { paths: vec![path.display().to_string()] }
      .execute(&ctx)
      .unwrap();

    assert!(!path.exists());
  }

  #[test]
  fn rmdir_rejects_non_empty_directory() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("rmdir-non-empty");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("file.txt"), b"hello").unwrap();

    let err = RmdirCommand { paths: vec![path.display().to_string()] }
      .execute(&ctx)
      .unwrap_err();
    assert!(path.exists());
    fs::remove_file(path.join("file.txt")).unwrap();
    fs::remove_dir(&path).unwrap();
    assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty);
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
