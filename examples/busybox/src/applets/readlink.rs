use std::{ffi::CString, io, path::Path};

use lio::api;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

use super::realpath::{CanonicalizeMode, write_resolved_path};

#[derive(Debug, Clone, Default)]
pub struct ReadlinkCommand {
  pub canonicalize: Option<CanonicalizeMode>,
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
    "readlink [-f|-e|-m] [-n] <path>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "canonicalize_existing",
        short: &['f', 'e'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "canonicalize_missing",
        short: &['m'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "no_newline",
        short: &['n'],
        long: &[],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("readlink", SPECS).parse(args)?;

    let Some(path) = parsed.positional().first() else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "readlink: missing operand",
      ));
    };
    if parsed.positional().len() != 1 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "readlink: extra operand",
      ));
    }

    let canonicalize = if parsed.get_flag_exists("canonicalize_missing") {
      Some(CanonicalizeMode::MissingOk)
    } else if parsed.get_flag_exists("canonicalize_existing") {
      Some(CanonicalizeMode::Existing)
    } else {
      None
    };

    Ok(Self {
      canonicalize,
      no_newline: parsed.get_flag_exists("no_newline"),
      path: path.clone(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    if let Some(mode) = self.canonicalize {
      return write_resolved_path(
        ctx,
        Path::new(&self.path),
        mode,
        !self.no_newline,
      );
    }

    let mut output = read_link_target(ctx, Path::new(&self.path))?.into_bytes();
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
  use crate::applets::realpath::resolve_realpath;
  use std::{fs, os::unix::fs::symlink, path::PathBuf};

  #[test]
  fn parse_readlink_supports_n_flag() {
    let parsed = ReadlinkCommand::parse(&["-n".into(), "link".into()]).unwrap();
    assert!(parsed.no_newline);
    assert_eq!(parsed.canonicalize, None);
    assert_eq!(parsed.path, "link");
  }

  #[test]
  fn parse_readlink_supports_f_flag() {
    let parsed =
      ReadlinkCommand::parse(&["-f".into(), "-n".into(), "link".into()])
        .unwrap();
    assert_eq!(parsed.canonicalize, Some(CanonicalizeMode::Existing));
    assert!(parsed.no_newline);
    assert_eq!(parsed.path, "link");
  }

  #[test]
  fn parse_readlink_supports_e_and_m_flags() {
    let existing =
      ReadlinkCommand::parse(&["-e".into(), "link".into()]).unwrap();
    assert_eq!(existing.canonicalize, Some(CanonicalizeMode::Existing));

    let missing =
      ReadlinkCommand::parse(&["-m".into(), "link".into()]).unwrap();
    assert_eq!(missing.canonicalize, Some(CanonicalizeMode::MissingOk));
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

  #[test]
  fn readlink_f_resolves_absolute_target_path() {
    let ctx = AppContext::new().unwrap();
    let root = unique_temp_path("readlink-f-root");
    let dir = root.join("dir");
    let file = dir.join("file.txt");
    let link = root.join("link");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::write(&file, b"hello").unwrap();
    symlink("dir/file.txt", &link).unwrap();

    let command = ReadlinkCommand {
      canonicalize: Some(CanonicalizeMode::Existing),
      no_newline: true,
      path: link.display().to_string(),
    };
    let resolved = if let Some(mode) = command.canonicalize {
      resolve_realpath(&ctx, Path::new(&command.path), mode).unwrap()
    } else {
      Path::new(&read_link_target(&ctx, Path::new(&command.path)).unwrap())
        .to_path_buf()
    };
    assert_eq!(resolved, fs::canonicalize(&file).unwrap());

    fs::remove_file(link).unwrap();
    fs::remove_file(file).unwrap();
    fs::remove_dir(dir).unwrap();
    fs::remove_dir(root).unwrap();
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
