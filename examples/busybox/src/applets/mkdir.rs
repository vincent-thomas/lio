use std::{
  ffi::CString,
  fs, io,
  os::unix::fs::PermissionsExt,
  path::{Component, Path, PathBuf},
};

use lio::api;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    fs as fs_util, io as io_util,
  },
};

#[derive(Debug, Clone)]
pub struct MkdirCommand {
  pub parents: bool,
  pub mode: u32,
  pub verbose: bool,
  pub paths: Vec<String>,
}

impl Default for MkdirCommand {
  fn default() -> Self {
    Self { parents: false, mode: 0o777, verbose: false, paths: Vec::new() }
  }
}

impl Command for MkdirCommand {
  fn name() -> &'static str {
    "mkdir"
  }

  fn summary() -> &'static str {
    "Create directories."
  }

  fn usage() -> &'static str {
    "mkdir [-p] [-m mode] [-v] <dir...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "parents",
        short: &['p'],
        long: &[],
        takes_value: false,
      },
      FlagSpec { name: "mode", short: &['m'], long: &[], takes_value: true },
      FlagSpec {
        name: "verbose",
        short: &['v'],
        long: &[],
        takes_value: false,
      },
    ];

    let parsed =
      FlagParser::new("mkdir", SPECS).parse(args).map_err(|err| {
        if err.kind() == io::ErrorKind::InvalidInput
          && err.to_string().contains("missing value for '-m'")
        {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "mkdir: option requires an argument -- 'm'",
          )
        } else {
          err
        }
      })?;

    if parsed.positional().is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "mkdir: missing operand",
      ));
    }

    Ok(Self {
      parents: parsed.get_flag_exists("parents"),
      mode: parsed
        .get_flag_value("mode")
        .map(parse_mode)
        .transpose()?
        .unwrap_or(0o777),
      verbose: parsed.get_flag_exists("verbose"),
      paths: parsed.positional().to_vec(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    for path in &self.paths {
      create_directory(ctx, Path::new(path), self)?;
    }
    Ok(())
  }
}

fn parse_mode(value: &str) -> io::Result<u32> {
  u32::from_str_radix(value, 8).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("mkdir: invalid mode '{value}'"),
    )
  })
}

fn create_directory(
  ctx: &AppContext,
  path: &Path,
  options: &MkdirCommand,
) -> io::Result<()> {
  if options.parents {
    create_directory_parents(ctx, path, options)?;
  } else {
    mkdir_path(ctx, path, options.mode)?;
    fs::set_permissions(path, fs::Permissions::from_mode(options.mode))?;
    write_verbose(ctx, path, options.verbose)?;
  }
  Ok(())
}

fn create_directory_parents(
  ctx: &AppContext,
  path: &Path,
  options: &MkdirCommand,
) -> io::Result<()> {
  let mut current = PathBuf::new();

  for component in path.components() {
    match component {
      Component::RootDir => current.push(component.as_os_str()),
      Component::CurDir => current.push(component.as_os_str()),
      Component::ParentDir => current.push(component.as_os_str()),
      Component::Normal(part) => {
        current.push(part);
        let existed = fs_util::stat_path(ctx, &current, true)?.is_some();
        match mkdir_path(ctx, &current, options.mode) {
          Ok(()) => {
            fs::set_permissions(
              &current,
              fs::Permissions::from_mode(options.mode),
            )?;
            if !existed {
              write_verbose(ctx, &current, options.verbose)?;
            }
          }
          Err(err)
            if err.kind() == io::ErrorKind::AlreadyExists
              && fs_util::stat_path(ctx, &current, true)?
                .is_some_and(|stat| stat.is_dir()) => {}
          Err(err) => return Err(err),
        }
      }
      Component::Prefix(_) => current.push(component.as_os_str()),
    }
  }

  Ok(())
}

fn mkdir_path(ctx: &AppContext, path: &Path, mode: u32) -> io::Result<()> {
  let cpath = path_to_cstring(path)?;
  let mut receiver =
    api::mkdirat(&ctx.cwd(), cpath, mode).with_lio(ctx.lio()).send();
  io_util::run_recv(ctx.lio(), &mut receiver)
}

fn write_verbose(
  ctx: &AppContext,
  path: &Path,
  enabled: bool,
) -> io::Result<()> {
  if !enabled {
    return Ok(());
  }
  let line = format!("mkdir: created directory '{}'\n", path.display());
  io_util::write_all(ctx.lio(), &ctx.stdout(), line.into_bytes())
}

fn path_to_cstring(path: &Path) -> io::Result<CString> {
  CString::new(path.as_os_str().to_string_lossy().as_bytes()).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "mkdir: invalid path")
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_mkdir_supports_flags() {
    let parsed = MkdirCommand::parse(&[
      "-p".into(),
      "-m".into(),
      "755".into(),
      "-v".into(),
      "a".into(),
      "b".into(),
    ])
    .unwrap();

    assert!(parsed.parents);
    assert!(parsed.verbose);
    assert_eq!(parsed.mode, 0o755);
    assert_eq!(parsed.paths, vec!["a", "b"]);
  }

  #[test]
  fn parse_mkdir_supports_double_dash() {
    let parsed = MkdirCommand::parse(&["--".into(), "-dir".into()]).unwrap();
    assert_eq!(parsed.paths, vec!["-dir"]);
  }

  #[test]
  fn mkdir_creates_directory() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("mkdir-basic");

    MkdirCommand {
      paths: vec![path.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(path.is_dir());
    fs::remove_dir(path).unwrap();
  }

  #[test]
  fn mkdir_p_creates_nested_directories() {
    let ctx = AppContext::new().unwrap();
    let root = unique_temp_path("mkdir-parents");
    let nested = root.join("a").join("b");

    MkdirCommand {
      parents: true,
      paths: vec![nested.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(nested.is_dir());
    fs::remove_dir(nested).unwrap();
    fs::remove_dir(root.join("a")).unwrap();
    fs::remove_dir(root).unwrap();
  }

  #[test]
  fn mkdir_mode_applies_permissions() {
    let ctx = AppContext::new().unwrap();
    let path = unique_temp_path("mkdir-mode");

    MkdirCommand {
      mode: 0o750,
      paths: vec![path.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o750);
    fs::remove_dir(path).unwrap();
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
