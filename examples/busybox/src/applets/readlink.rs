use std::{ffi::CString, io, path::Path};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct ReadlinkCommand {
  pub no_newline: bool,
  pub path: String,
}

impl Command for ReadlinkCommand {
  fn name() -> &'static str {
    "readlink"
  }

  fn summary() -> &'static str {
    "Print the target of a symbolic link."
  }

  fn usage() -> &'static str {
    "readlink [-n] <path>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut no_newline = false;
    let mut index = 0;

    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-n" => {
          no_newline = true;
          index += 1;
        }
        _ if arg.starts_with('-') => {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("readlink: unrecognized option '{arg}'"),
          ));
        }
        _ => break,
      }
    }

    let Some(path) = args.get(index) else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "readlink: missing operand",
      ));
    };
    if index + 1 != args.len() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "readlink: extra operand",
      ));
    }

    Ok(Self { no_newline, path: path.clone() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let target = read_link_target(ctx, Path::new(&self.path))?;
    let mut output = target.into_bytes();
    if !self.no_newline {
      output.push(b'\n');
    }
    io_util::write_all(ctx.lio(), &ctx.stdout(), output)
  }
}

pub(crate) fn read_link_target(
  ctx: &AppContext,
  path: &Path,
) -> io::Result<String> {
  let cpath = CString::new(path.as_os_str().to_string_lossy().as_bytes())
    .map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "readlink: invalid path")
    })?;
  let mut receiver = api::readlinkat(&ctx.cwd(), cpath, vec![0; 4096])
    .with_lio(ctx.lio())
    .send();
  let (result, buf) = io_util::run_recv(ctx.lio(), &mut receiver);
  let n = result? as usize;
  String::from_utf8(buf[..n].to_vec())
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, os::unix::fs::symlink, path::PathBuf};

  #[test]
  fn parse_readlink_supports_n_flag() {
    let parsed = ReadlinkCommand::parse(&["-n".into(), "link".into()]).unwrap();
    assert!(parsed.no_newline);
    assert_eq!(parsed.path, "link");
  }

  #[test]
  fn readlink_reads_symlink_target() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("readlink-target");
    let link = unique_temp_path("readlink-link");
    fs::write(&target, b"hello").unwrap();
    symlink(&target, &link).unwrap();

    let rendered = read_link_target(&ctx, &link).unwrap();
    assert_eq!(rendered, target.display().to_string());

    fs::remove_file(link).unwrap();
    fs::remove_file(target).unwrap();
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
