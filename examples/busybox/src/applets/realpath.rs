use std::{
  collections::VecDeque,
  ffi::{CStr, CString},
  io,
  path::{Component, Path, PathBuf},
};

use crate::{app::AppContext, command::Command, util::io as io_util};

use super::readlink::read_link_target;

#[derive(Debug, Clone, Default)]
pub struct RealpathCommand {
  pub paths: Vec<String>,
}

impl Command for RealpathCommand {
  fn name() -> &'static str {
    "realpath"
  }

  fn summary() -> &'static str {
    "Print the resolved absolute path."
  }

  fn usage() -> &'static str {
    "realpath <path...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    if args.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "realpath: missing operand",
      ));
    }
    Ok(Self { paths: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    for path in &self.paths {
      let resolved = resolve_realpath(ctx, Path::new(path))?;
      let mut output = resolved.into_os_string().into_encoded_bytes();
      output.push(b'\n');
      io_util::write_all(ctx.lio(), &ctx.stdout(), output)?;
    }
    Ok(())
  }
}

fn resolve_realpath(ctx: &AppContext, path: &Path) -> io::Result<PathBuf> {
  let mut current =
    if path.is_absolute() { PathBuf::from("/") } else { getcwd()? };
  let mut queue = owned_components(path);
  let mut traversals = 0usize;

  while let Some(component) = queue.pop_front() {
    match component.as_str() {
      "/" => current = PathBuf::from("/"),
      "." => {}
      ".." => {
        current.pop();
      }
      _ => {
        current.push(&component);
        let kind = lstat_kind(&current)?;
        if kind == FileKind::Symlink {
          traversals += 1;
          if traversals > 40 {
            return Err(io::Error::new(
              io::ErrorKind::InvalidData,
              "realpath: too many levels of symbolic links",
            ));
          }
          let target = PathBuf::from(read_link_target(ctx, &current)?);
          let remaining = std::mem::take(&mut queue);
          current.pop();
          if target.is_absolute() {
            current = PathBuf::from("/");
          }
          for rest in remaining.into_iter().rev() {
            queue.push_front(rest);
          }
          for target_component in owned_components(&target).into_iter().rev() {
            queue.push_front(target_component);
          }
        }
      }
    }
  }

  Ok(current)
}

fn owned_components(path: &Path) -> VecDeque<String> {
  let mut components = VecDeque::new();
  for component in path.components() {
    match component {
      Component::RootDir => components.push_back("/".into()),
      Component::CurDir => components.push_back(".".into()),
      Component::ParentDir => components.push_back("..".into()),
      Component::Normal(part) => {
        components.push_back(part.to_string_lossy().into_owned());
      }
      Component::Prefix(_) => components.push_back(String::new()),
    }
  }
  components
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
  File,
  Directory,
  Symlink,
}

fn lstat_kind(path: &Path) -> io::Result<FileKind> {
  let cpath = CString::new(path.as_os_str().to_string_lossy().as_bytes())
    .map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "realpath: invalid path")
    })?;
  let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
  let result = unsafe { libc::lstat(cpath.as_ptr(), st.as_mut_ptr()) };
  if result != 0 {
    return Err(io::Error::last_os_error());
  }
  let st = unsafe { st.assume_init() };
  let mode = st.st_mode & libc::S_IFMT;
  if mode == libc::S_IFLNK {
    Ok(FileKind::Symlink)
  } else if mode == libc::S_IFDIR {
    Ok(FileKind::Directory)
  } else {
    Ok(FileKind::File)
  }
}

fn getcwd() -> io::Result<PathBuf> {
  let mut buf = vec![0u8; 4096];
  let ptr = unsafe { libc::getcwd(buf.as_mut_ptr().cast(), buf.len()) };
  if ptr.is_null() {
    return Err(io::Error::last_os_error());
  }
  let cwd = unsafe { CStr::from_ptr(ptr) };
  Ok(PathBuf::from(cwd.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, os::unix::fs::symlink};

  #[test]
  fn realpath_resolves_nested_symlinks() {
    let ctx = AppContext::new().unwrap();
    let root = unique_temp_path("realpath-root");
    let dir = root.join("dir");
    let file = dir.join("file.txt");
    let link = root.join("link");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::write(&file, b"hello").unwrap();
    symlink("dir/file.txt", &link).unwrap();

    let resolved = resolve_realpath(&ctx, &link).unwrap();
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
