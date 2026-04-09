use std::{
  fs, io,
  os::unix::fs::{MetadataExt, PermissionsExt},
  path::{Path, PathBuf},
};

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    fs as fs_util, io as io_util,
  },
};

#[derive(Debug, Clone, Default)]
pub struct CpCommand {
  pub recursive: bool,
  pub force: bool,
  pub no_clobber: bool,
  pub update: bool,
  pub verbose: bool,
  pub preserve: bool,
  pub dereference: DereferenceMode,
  pub sources: Vec<String>,
  pub dest: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DereferenceMode {
  #[default]
  Never,
  CommandLine,
  Always,
}

impl Command for CpCommand {
  fn name() -> &'static str {
    "cp"
  }

  fn summary() -> &'static str {
    "Copy files and directories."
  }

  fn usage() -> &'static str {
    "cp [-afp] [-H|-L|-P] [-r|-R] [-v] <source>... <dest>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec { name: "force", short: &['f'], long: &[], takes_value: false },
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
        name: "archive",
        short: &['a'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "preserve",
        short: &['p'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "deref_always",
        short: &['L'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "deref_never",
        short: &['P'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "deref_cmdline",
        short: &['H'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "recursive",
        short: &['r', 'R'],
        long: &[],
        takes_value: false,
      },
      FlagSpec {
        name: "verbose",
        short: &['v'],
        long: &[],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("cp", SPECS).parse(args)?;

    if parsed.positional().len() < 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cp: missing file operand",
      ));
    }

    let mut command = Self::default();
    command.force = parsed.get_flag_exists("force");
    command.no_clobber = parsed.get_flag_exists("no-clobber");
    command.update = parsed.get_flag_exists("update");
    if parsed.get_flag_exists("archive") {
      command.recursive = true;
      command.preserve = true;
      command.dereference = DereferenceMode::Never;
    }
    if parsed.get_flag_exists("preserve") {
      command.preserve = true;
    }
    if parsed.get_flag_exists("deref_always") {
      command.dereference = DereferenceMode::Always;
    } else if parsed.get_flag_exists("deref_never") {
      command.dereference = DereferenceMode::Never;
    } else if parsed.get_flag_exists("deref_cmdline") {
      command.dereference = DereferenceMode::CommandLine;
    }
    if parsed.get_flag_exists("recursive") {
      command.recursive = true;
    }
    command.verbose = parsed.get_flag_exists("verbose");

    let (dest, sources) =
      parsed.positional().split_last().expect("validated arg length");
    command.sources = sources.to_vec();
    command.dest = dest.clone();
    Ok(command)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    execute_cp(ctx, self)
  }
}

fn execute_cp(ctx: &AppContext, command: &CpCommand) -> io::Result<()> {
  let dest_path = Path::new(&command.dest);
  let dest_is_dir =
    fs_util::stat_path(ctx, dest_path, true)?.is_some_and(|stat| stat.is_dir());

  if command.sources.len() > 1 && !dest_is_dir {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("cp: target '{}' is not a directory", dest_path.display()),
    ));
  }

  for source in &command.sources {
    let source_path = Path::new(source);
    let target = resolve_destination(source_path, dest_path, dest_is_dir)?;
    copy_path(ctx, source_path, &target, command, true)?;
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
        format!("cp: cannot determine target name for '{}'", source.display()),
      ));
    };
    return Ok(dest.join(name));
  }
  Ok(dest.to_path_buf())
}

fn copy_path(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &CpCommand,
  command_line_source: bool,
) -> io::Result<()> {
  let follow_symlink =
    should_follow_symlink(command.dereference, command_line_source);
  let metadata = fs_util::stat_path(ctx, source, follow_symlink)?
    .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;

  if metadata.is_dir() {
    if !command.recursive {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "cp: -R not specified; omitting directory '{}'",
          source.display()
        ),
      ));
    }
    if dest.starts_with(source) {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
          "cp: cannot copy '{}' into subdirectory '{}'",
          source.display(),
          dest.display()
        ),
      ));
    }
    copy_dir_recursive(ctx, source, dest, command, metadata.permissions)?;
    return Ok(());
  }

  if metadata.is_symlink() && !follow_symlink {
    copy_symlink(ctx, source, dest, command)?;
    return Ok(());
  }

  let source_metadata = fs::metadata(source)?;
  copy_regular_file(
    ctx,
    source,
    dest,
    command,
    metadata.permissions,
    source_metadata.mtime(),
    source_metadata.atime(),
  )
}

fn should_follow_symlink(
  mode: DereferenceMode,
  command_line_source: bool,
) -> bool {
  match mode {
    DereferenceMode::Never => false,
    DereferenceMode::CommandLine => command_line_source,
    DereferenceMode::Always => true,
  }
}

fn ensure_parent(dest: &Path) -> io::Result<()> {
  if let Some(parent) = dest.parent() {
    fs::create_dir_all(parent)?;
  }
  Ok(())
}

fn maybe_remove_existing(dest: &Path, command: &CpCommand) -> io::Result<()> {
  if !command.force || !dest.exists() {
    return Ok(());
  }

  let metadata = fs::symlink_metadata(dest)?;
  if metadata.is_dir() {
    fs::remove_dir_all(dest)?;
  } else {
    fs::remove_file(dest)?;
  }
  Ok(())
}

fn should_skip_copy(
  source: &Path,
  dest: &Path,
  command: &CpCommand,
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

  Ok(false)
}

fn copy_regular_file(
  _ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &CpCommand,
  mode: u32,
  mtime: i64,
  atime: i64,
) -> io::Result<()> {
  ensure_parent(dest)?;
  if should_skip_copy(source, dest, command)? {
    return Ok(());
  }
  maybe_remove_existing(dest, command)?;
  fs::copy(source, dest)?;
  if command.preserve {
    fs::set_permissions(dest, fs::Permissions::from_mode(mode & 0o7777))?;
    preserve_file_times(dest, atime, mtime, false)?;
  }
  write_verbose(_ctx, command.verbose, source, dest)
}

fn copy_symlink(
  _ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &CpCommand,
) -> io::Result<()> {
  ensure_parent(dest)?;
  if should_skip_copy(source, dest, command)? {
    return Ok(());
  }
  maybe_remove_existing(dest, command)?;
  let target = fs::read_link(source)?;
  std::os::unix::fs::symlink(target, dest)?;
  if command.preserve {
    let metadata = fs::symlink_metadata(source)?;
    preserve_file_times(dest, metadata.atime(), metadata.mtime(), true)?;
  }
  write_verbose(_ctx, command.verbose, source, dest)
}

fn copy_dir_recursive(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  command: &CpCommand,
  mode: u32,
) -> io::Result<()> {
  maybe_remove_existing(dest, command)?;
  if !dest.exists() {
    fs::create_dir_all(dest)?;
  }

  for entry in fs_util::read_dir_path(ctx, source)? {
    let child_source = source.join(&entry.name);
    let child_dest = dest.join(&entry.name);
    copy_path(ctx, &child_source, &child_dest, command, false)?;
  }

  if command.preserve {
    fs::set_permissions(dest, fs::Permissions::from_mode(mode & 0o7777))?;
    let metadata = fs::metadata(source)?;
    preserve_file_times(dest, metadata.atime(), metadata.mtime(), false)?;
  }

  write_verbose(ctx, command.verbose, source, dest)
}

fn preserve_file_times(
  path: &Path,
  atime_secs: i64,
  mtime_secs: i64,
  no_follow: bool,
) -> io::Result<()> {
  let c_path =
    std::ffi::CString::new(path.as_os_str().to_string_lossy().as_bytes())
      .map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "cp: invalid path")
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
  let line = format!("'{}' -> '{}'\n", source.display(), dest.display());
  io_util::write_all(ctx.lio(), &ctx.stdout(), line.into_bytes())
}

#[cfg(test)]
mod tests {
  use super::*;

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

  #[test]
  fn parse_cp_supports_flags_and_multiple_sources() {
    let parsed = CpCommand::parse(&[
      "-f".into(),
      "-p".into(),
      "-H".into(),
      "-R".into(),
      "-v".into(),
      "a".into(),
      "b".into(),
      "dir".into(),
    ])
    .unwrap();
    assert!(parsed.force);
    assert!(!parsed.no_clobber);
    assert!(!parsed.update);
    assert!(parsed.preserve);
    assert!(parsed.recursive);
    assert!(parsed.verbose);
    assert_eq!(parsed.dereference, DereferenceMode::CommandLine);
    assert_eq!(parsed.sources, vec!["a", "b"]);
    assert_eq!(parsed.dest, "dir");
  }

  #[test]
  fn cp_copies_regular_file() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("cp-source");
    let dest = unique_temp_path("cp-dest");
    fs::write(&source, b"hello").unwrap();

    execute_cp(
      &ctx,
      &CpCommand {
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    assert_eq!(fs::read(&dest).unwrap(), b"hello");
    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn cp_requires_directory_for_multiple_sources() {
    let ctx = AppContext::new().unwrap();
    let source_a = unique_temp_path("cp-multi-a");
    let source_b = unique_temp_path("cp-multi-b");
    let dest = unique_temp_path("cp-multi-dest");
    fs::write(&source_a, b"a").unwrap();
    fs::write(&source_b, b"b").unwrap();

    let err = execute_cp(
      &ctx,
      &CpCommand {
        sources: vec![
          source_a.display().to_string(),
          source_b.display().to_string(),
        ],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    fs::remove_file(source_a).unwrap();
    fs::remove_file(source_b).unwrap();
  }

  #[test]
  fn cp_recursive_copies_directory_tree() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("cp-tree-source");
    let dest = unique_temp_path("cp-tree-dest");
    fs::create_dir(&source).unwrap();
    fs::create_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested").join("file.txt"), b"hello").unwrap();

    execute_cp(
      &ctx,
      &CpCommand {
        recursive: true,
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    assert_eq!(
      fs::read(dest.join("nested").join("file.txt")).unwrap(),
      b"hello"
    );
    fs::remove_file(source.join("nested").join("file.txt")).unwrap();
    fs::remove_dir(source.join("nested")).unwrap();
    fs::remove_dir(source).unwrap();
    fs::remove_file(dest.join("nested").join("file.txt")).unwrap();
    fs::remove_dir(dest.join("nested")).unwrap();
    fs::remove_dir(dest).unwrap();
  }

  #[test]
  fn cp_rejects_directory_without_recursive_flag() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("cp-dir-source");
    let dest = unique_temp_path("cp-dir-dest");
    fs::create_dir(&source).unwrap();

    let err = execute_cp(
      &ctx,
      &CpCommand {
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap_err();

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    fs::remove_dir(source).unwrap();
  }

  #[test]
  fn cp_preserves_permissions_with_p() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("cp-preserve-source");
    let dest = unique_temp_path("cp-preserve-dest");
    fs::write(&source, b"hello").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o751)).unwrap();

    execute_cp(
      &ctx,
      &CpCommand {
        preserve: true,
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o751);

    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn cp_h_follows_command_line_symlink() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("cp-follow-target");
    let source = unique_temp_path("cp-follow-link");
    let dest = unique_temp_path("cp-follow-dest");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, &source).unwrap();

    execute_cp(
      &ctx,
      &CpCommand {
        dereference: DereferenceMode::CommandLine,
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_file());
    assert_eq!(fs::read(&dest).unwrap(), b"hello");

    fs::remove_file(target).unwrap();
    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn cp_p_keeps_symlink_when_not_following() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("cp-link-target");
    let source = unique_temp_path("cp-link-source");
    let dest = unique_temp_path("cp-link-dest");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, &source).unwrap();

    execute_cp(
      &ctx,
      &CpCommand {
        dereference: DereferenceMode::Never,
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    assert!(fs::symlink_metadata(&dest).unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&dest).unwrap(), target);

    fs::remove_file(target).unwrap();
    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn parse_cp_supports_no_clobber_and_update() {
    let parsed = CpCommand::parse(&[
      "-n".into(),
      "--update".into(),
      "a".into(),
      "b".into(),
    ])
    .unwrap();
    assert!(parsed.no_clobber);
    assert!(parsed.update);
    assert_eq!(parsed.sources, vec!["a"]);
    assert_eq!(parsed.dest, "b");
  }

  #[test]
  fn cp_n_keeps_existing_destination() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("cp-no-clobber-source");
    let dest = unique_temp_path("cp-no-clobber-dest");
    fs::write(&source, b"source").unwrap();
    fs::write(&dest, b"dest").unwrap();

    execute_cp(
      &ctx,
      &CpCommand {
        no_clobber: true,
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    assert_eq!(fs::read(&dest).unwrap(), b"dest");
    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }

  #[test]
  fn cp_u_skips_when_destination_is_newer() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("cp-update-source");
    let dest = unique_temp_path("cp-update-dest");
    fs::write(&source, b"old-source").unwrap();
    fs::write(&dest, b"new-dest").unwrap();
    let newer = [libc::timespec { tv_sec: 2_000_000_000, tv_nsec: 0 }; 2];
    let older = [libc::timespec { tv_sec: 1_000_000_000, tv_nsec: 0 }; 2];
    let source_c =
      std::ffi::CString::new(source.as_os_str().to_string_lossy().as_bytes())
        .unwrap();
    let dest_c =
      std::ffi::CString::new(dest.as_os_str().to_string_lossy().as_bytes())
        .unwrap();
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

    execute_cp(
      &ctx,
      &CpCommand {
        update: true,
        sources: vec![source.display().to_string()],
        dest: dest.display().to_string(),
        ..CpCommand::default()
      },
    )
    .unwrap();

    assert_eq!(fs::read(&dest).unwrap(), b"new-dest");
    fs::remove_file(source).unwrap();
    fs::remove_file(dest).unwrap();
  }
}
