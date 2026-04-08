use std::{
  ffi::CString,
  fs, io,
  path::{Path, PathBuf},
};

use lio::{api, api::ops::LinkKind};

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct LnCommand {
  pub symbolic: bool,
  pub force: bool,
  pub no_dereference: bool,
  pub backup: bool,
  pub backup_suffix: String,
  pub no_target_directory: bool,
  pub target_directory: Option<String>,
  pub verbose: bool,
  pub paths: Vec<String>,
}

impl Command for LnCommand {
  fn name() -> &'static str {
    "ln"
  }

  fn summary() -> &'static str {
    "Create hard or symbolic links."
  }

  fn usage() -> &'static str {
    "ln [-s] [-f] [-n] [-b] [-S suffix] [-T] [-t dir] [-v] <target> [linkname]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut command = Self { backup_suffix: "~".into(), ..Self::default() };
    let mut index = 0;

    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-s" => {
          command.symbolic = true;
          index += 1;
        }
        "-f" => {
          command.force = true;
          index += 1;
        }
        "-n" => {
          command.no_dereference = true;
          index += 1;
        }
        "-b" => {
          command.backup = true;
          index += 1;
        }
        "-T" => {
          command.no_target_directory = true;
          index += 1;
        }
        "-v" => {
          command.verbose = true;
          index += 1;
        }
        "-S" => {
          let Some(suffix) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "ln: option requires an argument -- 'S'",
            ));
          };
          command.backup_suffix = suffix.clone();
          index += 2;
        }
        "-t" => {
          let Some(dir) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "ln: option requires an argument -- 't'",
            ));
          };
          command.target_directory = Some(dir.clone());
          index += 2;
        }
        _ if arg.starts_with('-') => {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ln: unrecognized option '{arg}'"),
          ));
        }
        _ => break,
      }
    }

    command.paths = args[index..].to_vec();
    if command.paths.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "ln: missing file operand",
      ));
    }

    validate_operands(&command)?;
    Ok(command)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let plans = build_link_plan(self)?;
    for (source, dest) in plans {
      create_link(ctx, &source, &dest, self)?;
    }
    Ok(())
  }
}

fn validate_operands(command: &LnCommand) -> io::Result<()> {
  if command.target_directory.is_some() && command.no_target_directory {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "ln: cannot combine -t and -T",
    ));
  }

  if command.target_directory.is_some() {
    if command.paths.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "ln: missing file operand",
      ));
    }
    return Ok(());
  }

  if command.paths.len() > 2 && command.no_target_directory {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "ln: extra operand with -T",
    ));
  }

  if command.paths.len() > 1 {
    let dest =
      Path::new(command.paths.last().expect("validated operand count"));
    if command.no_target_directory
      || !destination_is_directory(dest, command.no_dereference)
    {
      if command.paths.len() != 2 {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("ln: target '{}' is not a directory", dest.display()),
        ));
      }
    }
  }

  Ok(())
}

fn build_link_plan(command: &LnCommand) -> io::Result<Vec<(PathBuf, PathBuf)>> {
  if let Some(dir) = &command.target_directory {
    let dir_path = PathBuf::from(dir);
    return command
      .paths
      .iter()
      .map(|source| {
        let source_path = PathBuf::from(source);
        let Some(name) =
          source_path.file_name().map(|name| name.to_os_string())
        else {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ln: invalid source '{}'", source_path.display()),
          ));
        };
        Ok((source_path, dir_path.join(name)))
      })
      .collect();
  }

  if command.paths.len() == 1 {
    let source = PathBuf::from(&command.paths[0]);
    let Some(name) = source.file_name().map(|name| name.to_os_string()) else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("ln: invalid source '{}'", source.display()),
      ));
    };
    return Ok(vec![(source, PathBuf::from(name))]);
  }

  if command.paths.len() == 2 {
    let source = PathBuf::from(&command.paths[0]);
    let dest = PathBuf::from(&command.paths[1]);
    if !command.no_target_directory
      && destination_is_directory(&dest, command.no_dereference)
    {
      let Some(name) = source.file_name().map(|name| name.to_os_string())
      else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("ln: invalid source '{}'", source.display()),
        ));
      };
      return Ok(vec![(source, dest.join(name))]);
    }
    return Ok(vec![(source, dest)]);
  }

  let dest_dir =
    PathBuf::from(command.paths.last().expect("validated operand count"));
  command.paths[..command.paths.len() - 1]
    .iter()
    .map(|source| {
      let source_path = PathBuf::from(source);
      let Some(name) = source_path.file_name().map(|name| name.to_os_string())
      else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("ln: invalid source '{}'", source_path.display()),
        ));
      };
      Ok((source_path, dest_dir.join(name)))
    })
    .collect()
}

fn destination_is_directory(path: &Path, no_dereference: bool) -> bool {
  let metadata = if no_dereference {
    fs::symlink_metadata(path)
  } else {
    fs::metadata(path)
  };
  metadata.map(|item| item.is_dir()).unwrap_or(false)
}

fn create_link(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &LnCommand,
) -> io::Result<()> {
  prepare_destination(ctx, dest, command)?;

  let source_cpath = path_to_cstring(source, "ln")?;
  let dest_cpath = path_to_cstring(dest, "ln")?;
  let mut receiver = api::linkat(
    &ctx.cwd(),
    source_cpath,
    &ctx.cwd(),
    dest_cpath,
    if command.symbolic { LinkKind::Soft } else { LinkKind::Hard },
  )
  .with_lio(ctx.lio())
  .send();
  io_util::run_recv(ctx.lio(), &mut receiver)?;

  if command.verbose {
    let line = format!("'{}' => '{}'\n", dest.display(), source.display());
    io_util::write_all(ctx.lio(), &ctx.stdout(), line.into_bytes())?;
  }

  Ok(())
}

fn prepare_destination(
  ctx: &AppContext,
  dest: &Path,
  command: &LnCommand,
) -> io::Result<()> {
  let exists = fs::symlink_metadata(dest).is_ok();
  if !exists {
    return Ok(());
  }

  if command.backup {
    let backup =
      PathBuf::from(format!("{}{}", dest.display(), command.backup_suffix));
    if fs::symlink_metadata(&backup).is_ok() {
      remove_path(ctx, &backup)?;
    }
    fs::rename(dest, &backup)?;
    return Ok(());
  }

  if command.force {
    return remove_path(ctx, dest);
  }

  Err(io::Error::new(
    io::ErrorKind::AlreadyExists,
    format!("ln: failed to create '{}': File exists", dest.display()),
  ))
}

fn remove_path(ctx: &AppContext, path: &Path) -> io::Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  let flags =
    if metadata.file_type().is_dir() { libc::AT_REMOVEDIR } else { 0 };
  let cpath = path_to_cstring(path, "ln")?;
  let mut receiver =
    api::unlinkat(&ctx.cwd(), cpath, flags).with_lio(ctx.lio()).send();
  io_util::run_recv(ctx.lio(), &mut receiver)
}

fn path_to_cstring(path: &Path, command: &str) -> io::Result<CString> {
  CString::new(path.as_os_str().to_string_lossy().as_bytes()).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{command}: invalid path"),
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::unix::fs::MetadataExt;

  #[test]
  fn parse_ln_supports_flags() {
    let parsed = LnCommand::parse(&[
      "-s".into(),
      "-f".into(),
      "-n".into(),
      "-b".into(),
      "-S".into(),
      ".bak".into(),
      "-T".into(),
      "-v".into(),
      "src".into(),
      "dst".into(),
    ])
    .unwrap();

    assert!(parsed.symbolic);
    assert!(parsed.force);
    assert!(parsed.no_dereference);
    assert!(parsed.backup);
    assert!(parsed.no_target_directory);
    assert!(parsed.verbose);
    assert_eq!(parsed.backup_suffix, ".bak");
  }

  #[test]
  fn ln_creates_hard_link() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("ln-hard-source");
    let dest = unique_temp_path("ln-hard-dest");
    fs::write(&source, b"hello").unwrap();

    LnCommand {
      paths: vec![source.display().to_string(), dest.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    let src_meta = fs::metadata(&source).unwrap();
    let dst_meta = fs::metadata(&dest).unwrap();
    assert_eq!(src_meta.ino(), dst_meta.ino());

    fs::remove_file(dest).unwrap();
    fs::remove_file(source).unwrap();
  }

  #[test]
  fn ln_s_creates_symbolic_link() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("ln-soft-source");
    let dest = unique_temp_path("ln-soft-dest");
    fs::write(&source, b"hello").unwrap();

    LnCommand {
      symbolic: true,
      paths: vec![source.display().to_string(), dest.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&dest).unwrap(), source);

    fs::remove_file(dest).unwrap();
    fs::remove_file(source).unwrap();
  }

  #[test]
  fn ln_force_replaces_existing_destination() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("ln-force-source");
    let dest = unique_temp_path("ln-force-dest");
    fs::write(&source, b"hello").unwrap();
    fs::write(&dest, b"old").unwrap();

    LnCommand {
      force: true,
      paths: vec![source.display().to_string(), dest.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    let src_meta = fs::metadata(&source).unwrap();
    let dst_meta = fs::metadata(&dest).unwrap();
    assert_eq!(src_meta.ino(), dst_meta.ino());

    fs::remove_file(dest).unwrap();
    fs::remove_file(source).unwrap();
  }

  #[test]
  fn ln_backup_preserves_existing_destination() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("ln-backup-source");
    let dest = unique_temp_path("ln-backup-dest");
    let backup = PathBuf::from(format!("{}{}", dest.display(), ".bak"));
    fs::write(&source, b"hello").unwrap();
    fs::write(&dest, b"old").unwrap();

    LnCommand {
      backup: true,
      backup_suffix: ".bak".into(),
      force: true,
      paths: vec![source.display().to_string(), dest.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    assert_eq!(fs::read(&backup).unwrap(), b"old");
    fs::remove_file(dest).unwrap();
    fs::remove_file(source).unwrap();
    fs::remove_file(backup).unwrap();
  }

  #[test]
  fn ln_t_places_links_in_target_directory() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("ln-t-source");
    let dir = unique_temp_path("ln-t-dir");
    fs::write(&source, b"hello").unwrap();
    fs::create_dir(&dir).unwrap();
    let linked = dir.join(source.file_name().unwrap());

    LnCommand {
      target_directory: Some(dir.display().to_string()),
      paths: vec![source.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(linked.exists());
    fs::remove_file(linked).unwrap();
    fs::remove_file(source).unwrap();
    fs::remove_dir(dir).unwrap();
  }

  #[test]
  fn ln_n_treats_symlink_to_directory_as_file_destination() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("ln-n-source");
    let real_dir = unique_temp_path("ln-n-real-dir");
    let dest = unique_temp_path("ln-n-dest");
    fs::write(&source, b"hello").unwrap();
    fs::create_dir(&real_dir).unwrap();
    std::os::unix::fs::symlink(&real_dir, &dest).unwrap();

    LnCommand {
      symbolic: true,
      force: true,
      no_dereference: true,
      paths: vec![source.display().to_string(), dest.display().to_string()],
      ..Default::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());

    fs::remove_file(dest).unwrap();
    fs::remove_file(source).unwrap();
    fs::remove_dir(real_dir).unwrap();
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
