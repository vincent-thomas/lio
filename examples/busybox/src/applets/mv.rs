use std::{
  ffi::CString,
  fs, io,
  os::unix::fs::{MetadataExt, PermissionsExt},
  path::{Path, PathBuf},
};

use lio::{api, api::ops::LinkKind};

use crate::{
  app::AppContext,
  applets::readlink::read_link_target,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    fs as fs_util, io as io_util,
  },
};

#[derive(Debug, Clone, Default)]
pub struct MvCommand {
  pub force: bool,
  pub no_clobber: bool,
  pub update: bool,
  pub verbose: bool,
  pub sources: Vec<String>,
  pub dest: String,
}

impl Command for MvCommand {
  fn name() -> &'static str {
    "mv"
  }

  fn summary() -> &'static str {
    "Move or rename files."
  }

  fn usage() -> &'static str {
    "mv [-fnuv] <source>... <dest>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "force",
        short: &['f'],
        long: &["force"],
        takes_value: false,
      },
      FlagSpec {
        name: "no-clobber",
        short: &['n'],
        long: &["no-clobber"],
        takes_value: false,
      },
      FlagSpec {
        name: "update",
        short: &['u'],
        long: &["update"],
        takes_value: false,
      },
      FlagSpec {
        name: "verbose",
        short: &['v'],
        long: &["verbose"],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("mv", SPECS).parse(args)?;

    if parsed.positional().len() < 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "mv: missing file operand",
      ));
    }

    let (dest, sources) =
      parsed.positional().split_last().expect("validated arg length");
    Ok(Self {
      force: parsed.get_flag_exists("force"),
      no_clobber: parsed.get_flag_exists("no-clobber"),
      update: parsed.get_flag_exists("update"),
      verbose: parsed.get_flag_exists("verbose"),
      sources: sources.to_vec(),
      dest: dest.clone(),
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let dest_path = Path::new(&self.dest);
    let dest_is_dir = fs_util::stat_path(ctx, dest_path, true)?
      .is_some_and(|stat| stat.is_dir());

    if self.sources.len() > 1 && !dest_is_dir {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("mv: target '{}' is not a directory", dest_path.display()),
      ));
    }

    for source in &self.sources {
      let source_path = Path::new(source);
      let target = resolve_destination(source_path, dest_path, dest_is_dir)?;
      validate_move(source_path, &target)?;
      move_path(ctx, source_path, &target, self)?;
    }

    Ok(())
  }
}

fn validate_move(source: &Path, dest: &Path) -> io::Result<()> {
  if source == dest {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "mv: '{}' and '{}' are the same file",
        source.display(),
        dest.display()
      ),
    ));
  }

  if source.is_dir() && dest.starts_with(source) {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "mv: cannot move '{}' to a subdirectory of itself, '{}'",
        source.display(),
        dest.display()
      ),
    ));
  }

  Ok(())
}

fn resolve_destination(
  source: &Path,
  dest: &Path,
  dest_is_dir: bool,
) -> io::Result<PathBuf> {
  if dest_is_dir {
    let Some(name) = source.file_name() else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("mv: cannot determine target name for '{}'", source.display()),
      ));
    };
    return Ok(dest.join(name));
  }

  Ok(dest.to_path_buf())
}

fn move_path(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &MvCommand,
) -> io::Result<()> {
  if should_skip_move(source, dest, command)? {
    return Ok(());
  }

  match rename_path(ctx, source, dest) {
    Ok(()) => write_verbose(ctx, command.verbose, source, dest),
    Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
      copy_and_remove_source(ctx, source, dest, command)
    }
    Err(err) => Err(err),
  }
}

fn should_skip_move(
  source: &Path,
  dest: &Path,
  command: &MvCommand,
) -> io::Result<bool> {
  if !dest.exists() {
    return Ok(false);
  }

  if command.no_clobber {
    return Ok(true);
  }

  if command.update {
    let source_mtime = fs::metadata(source)?.mtime();
    let dest_mtime = fs::metadata(dest)?.mtime();
    if dest_mtime >= source_mtime {
      return Ok(true);
    }
  }

  let _ = command.force;
  Ok(false)
}

fn rename_path(ctx: &AppContext, source: &Path, dest: &Path) -> io::Result<()> {
  let old_path = path_to_cstring(source, "mv")?;
  let new_path = path_to_cstring(dest, "mv")?;
  let mut receiver = api::renameat(&ctx.cwd(), old_path, &ctx.cwd(), new_path)
    .with_lio(ctx.lio())
    .send();
  io_util::run_recv(ctx.lio(), &mut receiver)
}

fn copy_and_remove_source(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &MvCommand,
) -> io::Result<()> {
  let metadata = fs_util::stat_path(ctx, source, false)?
    .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;

  if metadata.is_dir() {
    remove_existing_destination(dest, true)?;
    copy_dir_recursive(ctx, source, dest, metadata.permissions, command)?;
    remove_source_dir_tree(ctx, source)?;
    write_verbose(ctx, command.verbose, source, dest)?;
    return Ok(());
  }

  if metadata.is_symlink() {
    remove_existing_destination(dest, false)?;
    copy_symlink_and_remove_source(ctx, source, dest)?;
    write_verbose(ctx, command.verbose, source, dest)?;
    return Ok(());
  }

  remove_existing_destination(dest, false)?;
  let source_metadata = fs::metadata(source)?;
  copy_regular_file(
    ctx,
    source,
    dest,
    metadata.permissions,
    source_metadata.atime(),
    source_metadata.mtime(),
  )?;
  unlink_path(ctx, source)?;
  write_verbose(ctx, command.verbose, source, dest)?;
  Ok(())
}

fn copy_symlink_and_remove_source(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
) -> io::Result<()> {
  let target = read_link_target(ctx, source)?;
  let source_cpath = CString::new(target.as_str()).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "mv: invalid symlink target")
  })?;
  let dest_cpath = path_to_cstring(dest, "mv")?;
  let mut receiver = api::linkat(
    &ctx.cwd(),
    source_cpath,
    &ctx.cwd(),
    dest_cpath,
    LinkKind::Soft,
  )
  .with_lio(ctx.lio())
  .send();
  io_util::run_recv(ctx.lio(), &mut receiver)?;
  let metadata = fs::symlink_metadata(source)?;
  preserve_file_times(dest, metadata.atime(), metadata.mtime(), true)?;
  unlink_path(ctx, source)
}

fn copy_regular_file(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  mode: u32,
  atime: i64,
  mtime: i64,
) -> io::Result<()> {
  let _ = ctx;
  fs::copy(source, dest)?;
  fs::set_permissions(dest, fs::Permissions::from_mode(mode & 0o7777))?;
  preserve_file_times(dest, atime, mtime, false)?;
  Ok(())
}

fn copy_dir_recursive(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  permissions: u32,
  command: &MvCommand,
) -> io::Result<()> {
  mkdir_path(ctx, dest, permissions)?;

  for entry in fs_util::read_dir_path(ctx, source)? {
    let child_source = source.join(&entry.name);
    let child_dest = dest.join(entry.name);
    copy_and_remove_source(ctx, &child_source, &child_dest, command)?;
  }

  fs::set_permissions(dest, fs::Permissions::from_mode(permissions))?;
  let metadata = fs::metadata(source)?;
  preserve_file_times(dest, metadata.atime(), metadata.mtime(), false)?;
  Ok(())
}

fn remove_source_dir_tree(ctx: &AppContext, path: &Path) -> io::Result<()> {
  let cpath = path_to_cstring(path, "mv")?;
  let mut receiver = api::unlinkat(&ctx.cwd(), cpath, libc::AT_REMOVEDIR)
    .with_lio(ctx.lio())
    .send();
  io_util::run_recv(ctx.lio(), &mut receiver)
}

fn mkdir_path(ctx: &AppContext, path: &Path, mode: u32) -> io::Result<()> {
  let cpath = path_to_cstring(path, "mv")?;
  let mut receiver =
    api::mkdirat(&ctx.cwd(), cpath, mode).with_lio(ctx.lio()).send();
  io_util::run_recv(ctx.lio(), &mut receiver)
}

fn unlink_path(ctx: &AppContext, path: &Path) -> io::Result<()> {
  let cpath = path_to_cstring(path, "mv")?;
  let mut receiver =
    api::unlinkat(&ctx.cwd(), cpath, 0).with_lio(ctx.lio()).send();
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

fn remove_existing_destination(
  dest: &Path,
  source_is_dir: bool,
) -> io::Result<()> {
  let Ok(metadata) = fs::symlink_metadata(dest) else {
    return Ok(());
  };

  if metadata.is_dir() {
    if !source_is_dir {
      return Err(io::Error::new(
        io::ErrorKind::IsADirectory,
        format!("mv: cannot overwrite directory '{}'", dest.display()),
      ));
    }
    fs::remove_dir(dest)
  } else {
    fs::remove_file(dest)
  }
}

fn preserve_file_times(
  path: &Path,
  atime_secs: i64,
  mtime_secs: i64,
  no_follow: bool,
) -> io::Result<()> {
  let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
    .map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "mv: invalid path")
    })?;
  let times = [
    libc::timespec { tv_sec: atime_secs, tv_nsec: 0 },
    libc::timespec { tv_sec: mtime_secs, tv_nsec: 0 },
  ];
  let flags = if no_follow { libc::AT_SYMLINK_NOFOLLOW } else { 0 };
  let rc = unsafe {
    libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), flags)
  };
  if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn write_verbose(
  ctx: &AppContext,
  enabled: bool,
  source: &Path,
  dest: &Path,
) -> io::Result<()> {
  if !enabled {
    return Ok(());
  }
  let line =
    format!("renamed '{}' -> '{}'\n", source.display(), dest.display());
  io_util::write_all(ctx.lio(), &ctx.stdout(), line.into_bytes())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_mv_requires_source_and_destination() {
    let err = MvCommand::parse(&[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = MvCommand::parse(&["only".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_mv_collects_sources_and_destination() {
    let parsed =
      MvCommand::parse(&["a".into(), "b".into(), "dir".into()]).unwrap();
    assert_eq!(parsed.sources, vec!["a", "b"]);
    assert_eq!(parsed.dest, "dir");
  }

  #[test]
  fn parse_mv_supports_overwrite_policy_flags() {
    let parsed =
      MvCommand::parse(&["-nuv".into(), "a".into(), "dest".into()]).unwrap();
    assert!(parsed.no_clobber);
    assert!(parsed.update);
    assert!(parsed.verbose);
    assert_eq!(parsed.sources, vec!["a"]);
    assert_eq!(parsed.dest, "dest");
  }

  #[test]
  fn mv_renames_single_path() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-source");
    let dest = unique_temp_path("mv-dest");
    fs::write(&source, b"hello").unwrap();

    MvCommand {
      sources: vec![source.display().to_string()],
      dest: dest.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(!source.exists());
    assert_eq!(fs::read(&dest).unwrap(), b"hello");

    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn mv_moves_single_path_into_existing_directory() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-into-dir-source");
    let dest_dir = unique_temp_path("mv-into-dir");
    fs::write(&source, b"hello").unwrap();
    fs::create_dir(&dest_dir).unwrap();
    let moved = dest_dir.join(source.file_name().unwrap());

    MvCommand {
      sources: vec![source.display().to_string()],
      dest: dest_dir.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap();

    assert!(!source.exists());
    assert_eq!(fs::read(&moved).unwrap(), b"hello");

    fs::remove_file(moved).unwrap();
    fs::remove_dir(dest_dir).unwrap();
  }

  #[test]
  fn mv_requires_directory_for_multiple_sources() {
    let ctx = AppContext::new().unwrap();
    let source_a = unique_temp_path("mv-multi-a");
    let source_b = unique_temp_path("mv-multi-b");
    let dest = unique_temp_path("mv-multi-dest");
    fs::write(&source_a, b"a").unwrap();
    fs::write(&source_b, b"b").unwrap();

    let err = MvCommand {
      sources: vec![
        source_a.display().to_string(),
        source_b.display().to_string(),
      ],
      dest: dest.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(source_a.exists());
    assert!(source_b.exists());

    fs::remove_file(source_a).unwrap();
    fs::remove_file(source_b).unwrap();
  }

  #[test]
  fn mv_moves_multiple_sources_into_existing_directory() {
    let ctx = AppContext::new().unwrap();
    let source_a = unique_temp_path("mv-many-a");
    let source_b = unique_temp_path("mv-many-b");
    let dest_dir = unique_temp_path("mv-many-dir");
    fs::write(&source_a, b"a").unwrap();
    fs::write(&source_b, b"b").unwrap();
    fs::create_dir(&dest_dir).unwrap();

    let moved_a = dest_dir.join(source_a.file_name().unwrap());
    let moved_b = dest_dir.join(source_b.file_name().unwrap());

    MvCommand {
      sources: vec![
        source_a.display().to_string(),
        source_b.display().to_string(),
      ],
      dest: dest_dir.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap();

    assert_eq!(fs::read(&moved_a).unwrap(), b"a");
    assert_eq!(fs::read(&moved_b).unwrap(), b"b");

    fs::remove_file(moved_a).unwrap();
    fs::remove_file(moved_b).unwrap();
    fs::remove_dir(dest_dir).unwrap();
  }

  #[test]
  fn mv_rejects_same_source_and_destination() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-same");
    fs::write(&source, b"hello").unwrap();

    let err = MvCommand {
      sources: vec![source.display().to_string()],
      dest: source.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    fs::remove_file(source).unwrap();
  }

  #[test]
  fn mv_rejects_moving_directory_into_itself() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-self-dir");
    fs::create_dir(&source).unwrap();
    let dest = source.join("nested");

    let err = MvCommand {
      sources: vec![source.display().to_string()],
      dest: dest.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    fs::remove_dir(source).unwrap();
  }

  #[test]
  fn fallback_copy_removes_source_for_regular_file() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-fallback-source");
    let dest = unique_temp_path("mv-fallback-dest");
    fs::write(&source, b"hello").unwrap();

    copy_and_remove_source(&ctx, &source, &dest, &MvCommand::default())
      .unwrap();

    assert!(!source.exists());
    assert_eq!(fs::read(&dest).unwrap(), b"hello");

    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn fallback_copy_rejects_directory() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-fallback-dir-source");
    let dest = unique_temp_path("mv-fallback-dir-dest");
    fs::create_dir(&source).unwrap();

    copy_and_remove_source(&ctx, &source, &dest, &MvCommand::default())
      .unwrap();

    assert!(!source.exists());
    assert!(dest.exists());

    fs::remove_dir(dest).unwrap();
  }

  #[test]
  fn fallback_copy_moves_directory_tree() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-fallback-tree-source");
    let nested = source.join("nested");
    let dest = unique_temp_path("mv-fallback-tree-dest");
    fs::create_dir(&source).unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o750)).unwrap();
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("file.txt"), b"hello").unwrap();

    copy_and_remove_source(&ctx, &source, &dest, &MvCommand::default())
      .unwrap();

    assert!(!source.exists());
    assert_eq!(
      fs::read(dest.join("nested").join("file.txt")).unwrap(),
      b"hello"
    );
    let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o750);

    fs::remove_file(dest.join("nested").join("file.txt")).unwrap();
    fs::remove_dir(dest.join("nested")).unwrap();
    fs::remove_dir(dest).unwrap();
  }

  #[test]
  fn fallback_copy_moves_symlink() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("mv-fallback-symlink-target");
    let source = unique_temp_path("mv-fallback-symlink-source");
    let dest = unique_temp_path("mv-fallback-symlink-dest");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, &source).unwrap();

    copy_symlink_and_remove_source(&ctx, &source, &dest).unwrap();

    assert!(!source.exists());
    assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&dest).unwrap(), target);

    fs::remove_file(dest).unwrap();
    fs::remove_file(target).unwrap();
  }

  #[test]
  fn fallback_copy_replaces_existing_symlink_destination() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-replace-source");
    let stale_target = unique_temp_path("mv-replace-stale");
    let dest = unique_temp_path("mv-replace-dest");
    fs::write(&source, b"hello").unwrap();
    fs::write(&stale_target, b"stale").unwrap();
    std::os::unix::fs::symlink(&stale_target, &dest).unwrap();

    copy_and_remove_source(&ctx, &source, &dest, &MvCommand::default())
      .unwrap();

    assert!(!source.exists());
    assert_eq!(fs::read(&dest).unwrap(), b"hello");
    assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_file());

    fs::remove_file(dest).unwrap();
    fs::remove_file(stale_target).unwrap();
  }

  #[test]
  fn fallback_copy_preserves_permissions_for_regular_files() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-preserve-source");
    let dest = unique_temp_path("mv-preserve-dest");
    fs::write(&source, b"hello").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();

    copy_and_remove_source(&ctx, &source, &dest, &MvCommand::default())
      .unwrap();

    let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o751);

    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn mv_n_keeps_existing_destination_and_source() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-no-clobber-source");
    let dest = unique_temp_path("mv-no-clobber-dest");
    fs::write(&source, b"source").unwrap();
    fs::write(&dest, b"dest").unwrap();

    MvCommand {
      no_clobber: true,
      sources: vec![source.display().to_string()],
      dest: dest.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap();

    assert_eq!(fs::read(&source).unwrap(), b"source");
    assert_eq!(fs::read(&dest).unwrap(), b"dest");

    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn mv_u_skips_when_destination_is_newer() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-update-source");
    let dest = unique_temp_path("mv-update-dest");
    fs::write(&source, b"old-source").unwrap();
    fs::write(&dest, b"new-dest").unwrap();
    let newer = [libc::timespec { tv_sec: 2_000_000_000, tv_nsec: 0 }; 2];
    let older = [libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 }; 2];
    let source_c =
      CString::new(source.as_os_str().to_string_lossy().as_bytes()).unwrap();
    let dest_c =
      CString::new(dest.as_os_str().to_string_lossy().as_bytes()).unwrap();
    assert_eq!(
      unsafe {
        libc::utimensat(libc::AT_FDCWD, source_c.as_ptr(), older.as_ptr(), 0)
      },
      0
    );
    assert_eq!(
      unsafe {
        libc::utimensat(libc::AT_FDCWD, dest_c.as_ptr(), newer.as_ptr(), 0)
      },
      0
    );

    MvCommand {
      update: true,
      sources: vec![source.display().to_string()],
      dest: dest.display().to_string(),
      ..MvCommand::default()
    }
    .execute(&ctx)
    .unwrap();

    assert_eq!(fs::read(&source).unwrap(), b"old-source");
    assert_eq!(fs::read(&dest).unwrap(), b"new-dest");

    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
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
