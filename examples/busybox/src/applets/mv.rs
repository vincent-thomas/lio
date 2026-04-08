use std::{
  ffi::CString,
  fs, io,
  os::unix::fs::PermissionsExt,
  path::{Path, PathBuf},
};

use lio::{api, api::ops::LinkKind};

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct MvCommand {
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
    "mv <source>... <dest>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() < 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "mv: missing file operand",
      ));
    }

    let (dest, sources) = args.split_last().expect("validated arg length");
    Ok(Self { sources: sources.to_vec(), dest: dest.clone() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let dest_path = Path::new(&self.dest);
    let dest_is_dir = fs::metadata(dest_path)
      .map(|metadata| metadata.is_dir())
      .unwrap_or(false);

    if self.sources.len() > 1 && !dest_is_dir {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("mv: target '{}' is not a directory", dest_path.display()),
      ));
    }

    for source in &self.sources {
      let source_path = Path::new(source);
      let target = resolve_destination(source_path, dest_path, dest_is_dir)?;
      move_path(ctx, source_path, &target)?;
    }

    Ok(())
  }
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

fn move_path(ctx: &AppContext, source: &Path, dest: &Path) -> io::Result<()> {
  match rename_path(ctx, source, dest) {
    Ok(()) => Ok(()),
    Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
      copy_and_remove_source(ctx, source, dest)
    }
    Err(err) => Err(err),
  }
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
) -> io::Result<()> {
  let metadata = fs::symlink_metadata(source)?;
  let file_type = metadata.file_type();

  if file_type.is_dir() {
    copy_dir_recursive(ctx, source, dest, metadata.permissions())?;
    remove_source_dir_tree(ctx, source)?;
    return Ok(());
  }

  if file_type.is_symlink() {
    copy_symlink_and_remove_source(ctx, source, dest)?;
    return Ok(());
  }

  fs::copy(source, dest)?;
  fs::set_permissions(dest, metadata.permissions())?;
  unlink_path(ctx, source)?;
  Ok(())
}

fn copy_symlink_and_remove_source(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
) -> io::Result<()> {
  let target = fs::read_link(source)?;
  let source_cpath = path_to_cstring(&target, "mv")?;
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
  unlink_path(ctx, source)
}

fn copy_dir_recursive(
  ctx: &AppContext,
  source: &Path,
  dest: &Path,
  permissions: fs::Permissions,
) -> io::Result<()> {
  mkdir_path(ctx, dest, permissions.mode())?;

  for entry in fs::read_dir(source)? {
    let entry = entry?;
    let child_source = entry.path();
    let child_dest = dest.join(entry.file_name());
    copy_and_remove_source(ctx, &child_source, &child_dest)?;
  }

  fs::set_permissions(dest, permissions)?;
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
  fn mv_renames_single_path() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-source");
    let dest = unique_temp_path("mv-dest");
    fs::write(&source, b"hello").unwrap();

    MvCommand {
      sources: vec![source.display().to_string()],
      dest: dest.display().to_string(),
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
  fn fallback_copy_removes_source_for_regular_file() {
    let ctx = AppContext::new().unwrap();
    let source = unique_temp_path("mv-fallback-source");
    let dest = unique_temp_path("mv-fallback-dest");
    fs::write(&source, b"hello").unwrap();

    copy_and_remove_source(&ctx, &source, &dest).unwrap();

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

    copy_and_remove_source(&ctx, &source, &dest).unwrap();

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

    copy_and_remove_source(&ctx, &source, &dest).unwrap();

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
