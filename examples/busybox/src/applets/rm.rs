use std::{ffi::CString, io, path::Path};

use lio::api;

use crate::{
  app::AppContext,
  applets::support::read_yes_from_tty,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    fs as fs_util, io as io_util,
  },
};

#[derive(Debug, Clone, Default)]
pub struct RmCommand {
  pub dir: bool,
  pub force: bool,
  pub interactive: bool,
  pub recursive: bool,
  pub verbose: bool,
  pub paths: Vec<String>,
}

impl Command for RmCommand {
  fn name() -> &'static str {
    "rm"
  }

  fn summary() -> &'static str {
    "Remove files or directories."
  }

  fn usage() -> &'static str {
    "rm [-d] [-f] [-i] [-r|-R] [-v] <path...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "dir",
        short: &['d'],
        long: &["dir"],
        takes_value: false,
      },
      FlagSpec {
        name: "force",
        short: &['f'],
        long: &["force"],
        takes_value: false,
      },
      FlagSpec {
        name: "interactive",
        short: &['i'],
        long: &["interactive"],
        takes_value: false,
      },
      FlagSpec {
        name: "recursive",
        short: &['r', 'R'],
        long: &["recursive"],
        takes_value: false,
      },
      FlagSpec {
        name: "verbose",
        short: &['v'],
        long: &["verbose"],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("rm", SPECS).parse(args)?;

    if parsed.positional().is_empty() {
      let dir = parsed.get_flag_exists("dir");
      let force = parsed.get_flag_exists("force");
      let interactive = parsed.get_flag_exists("interactive");
      let recursive = parsed.get_flag_exists("recursive");
      let verbose = parsed.get_flag_exists("verbose");
      if force {
        return Ok(Self {
          dir,
          force,
          interactive,
          recursive,
          verbose,
          paths: Vec::new(),
        });
      }
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rm: missing operand",
      ));
    }

    Ok(Self {
      dir: parsed.get_flag_exists("dir"),
      force: parsed.get_flag_exists("force"),
      interactive: parsed.get_flag_exists("interactive"),
      recursive: parsed.get_flag_exists("recursive"),
      verbose: parsed.get_flag_exists("verbose"),
      paths: parsed.positional().to_vec(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    for path in &self.paths {
      if let Err(err) = remove_path(ctx, Path::new(path), self) {
        if self.force && err.kind() == io::ErrorKind::NotFound {
          continue;
        }
        return Err(err);
      }
    }
    Ok(())
  }
}

fn remove_path(
  ctx: &AppContext,
  path: &Path,
  options: &RmCommand,
) -> io::Result<()> {
  reject_protected_path(path)?;
  let metadata = match fs_util::stat_path(ctx, path, false)? {
    Some(metadata) => metadata,
    None if options.force => {
      return Ok(());
    }
    None => return Err(io::Error::from_raw_os_error(libc::ENOENT)),
  };

  let is_dir = metadata.is_dir();
  if options.interactive && !confirm_removal(ctx, path, is_dir)? {
    return Ok(());
  }

  if is_dir {
    if options.recursive {
      for entry in fs_util::read_dir_path(ctx, path)? {
        remove_path(ctx, &path.join(entry.name), options)?;
      }

      unlink_path(ctx, path, libc::AT_REMOVEDIR)?;
    } else if options.dir {
      unlink_path(ctx, path, libc::AT_REMOVEDIR)?;
    } else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("rm: cannot remove '{}': is a directory", path.display()),
      ));
    }
  } else {
    unlink_path(ctx, path, 0)?;
  }

  if options.verbose {
    let rendered = if is_dir {
      format!("removed directory '{}'\n", path.display())
    } else {
      format!("removed '{}'\n", path.display())
    };
    io_util::write_all(ctx.lio(), &ctx.stdout(), rendered.into_bytes())?;
  }

  Ok(())
}

fn reject_protected_path(path: &Path) -> io::Result<()> {
  if matches!(
    path.file_name().and_then(|value| value.to_str()),
    Some("." | "..")
  ) || path == Path::new(".")
    || path == Path::new("..")
    || path.parent().is_none()
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("rm: refusing to remove '{}'", path.display()),
    ));
  }
  Ok(())
}

fn unlink_path(ctx: &AppContext, path: &Path, flags: i32) -> io::Result<()> {
  let cpath = path_to_cstring(path)?;
  let mut result =
    api::unlinkat(&ctx.cwd(), cpath, flags).with_lio(ctx.lio()).send();
  let result = io_util::run_recv(ctx.lio(), &mut result);
  result
}

fn path_to_cstring(path: &Path) -> io::Result<CString> {
  CString::new(path.as_os_str().to_string_lossy().as_bytes()).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "rm: invalid path")
  })
}

fn confirm_removal(
  ctx: &AppContext,
  path: &Path,
  is_dir: bool,
) -> io::Result<bool> {
  let prompt = if is_dir {
    format!("rm: remove directory '{}'? ", path.display())
  } else {
    format!("rm: remove '{}'? ", path.display())
  };
  io_util::write_all(ctx.lio(), &ctx.stderr(), prompt.into_bytes())?;
  Ok(read_yes_from_tty(ctx.lio())?.is_some_and(|ch| ch == 'y' || ch == 'Y'))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use std::path::PathBuf;

  #[test]
  fn parse_rm_command_supports_force_and_recursive() {
    let parsed = RmCommand::parse(&[
      "-d".into(),
      "-f".into(),
      "-i".into(),
      "-r".into(),
      "-v".into(),
      "a".into(),
      "b".into(),
    ])
    .unwrap();
    assert!(parsed.dir);
    assert!(parsed.force);
    assert!(parsed.interactive);
    assert!(parsed.recursive);
    assert!(parsed.verbose);
    assert_eq!(parsed.paths, vec!["a", "b"]);
  }

  #[test]
  fn parse_rm_force_without_operands_is_ok() {
    let parsed = RmCommand::parse(&["-f".into()]).unwrap();
    assert!(parsed.force);
    assert!(parsed.paths.is_empty());
  }

  #[test]
  fn parse_rm_command_supports_double_dash() {
    let parsed =
      RmCommand::parse(&["-f".into(), "--".into(), "-file".into()]).unwrap();
    assert!(parsed.force);
    assert_eq!(parsed.paths, vec!["-file"]);
  }

  #[test]
  fn rm_rejects_directory_without_recursive_flag() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("rm-dir");
    fs::create_dir(&path).unwrap();

    let err = remove_path(&ctx, &path, &RmCommand::default())
      .expect_err("rm should fail");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    fs::remove_dir(&path).unwrap();
  }

  #[test]
  fn rm_d_removes_empty_directory_without_recursive_flag() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("rm-d-dir");
    fs::create_dir(&path).unwrap();

    remove_path(
      &ctx,
      &path,
      &RmCommand {
        dir: true,
        paths: vec![path.display().to_string()],
        ..RmCommand::default()
      },
    )
    .unwrap();

    assert!(!path.exists());
  }

  #[test]
  fn rm_recursive_removes_directory_tree() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("rm-tree");
    let nested = path.join("nested");
    fs::create_dir(&path).unwrap();
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("file.txt"), b"hello").unwrap();

    remove_path(
      &ctx,
      &path,
      &RmCommand {
        recursive: true,
        paths: vec![path.display().to_string()],
        ..RmCommand::default()
      },
    )
    .unwrap();
    assert!(!path.exists());
  }

  #[test]
  fn rm_rejects_dot_and_dotdot() {
    let ctx = AppContext::new().unwrap();
    let err =
      remove_path(&ctx, Path::new("."), &RmCommand::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err =
      remove_path(&ctx, Path::new(".."), &RmCommand::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
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
