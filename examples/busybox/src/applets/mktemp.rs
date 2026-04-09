use std::{
  ffi::CString,
  io,
  path::{Path, PathBuf},
  time::{SystemTime, UNIX_EPOCH},
};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone)]
pub struct MktempCommand {
  pub directory: bool,
  pub dry_run: bool,
  pub quiet: bool,
  pub suffix: String,
  pub temp_dir: Option<String>,
  pub template: String,
}

impl Default for MktempCommand {
  fn default() -> Self {
    Self {
      directory: false,
      dry_run: false,
      quiet: false,
      suffix: String::new(),
      temp_dir: None,
      template: "tmp.XXXXXXXXXX".into(),
    }
  }
}

impl Command for MktempCommand {
  fn name() -> &'static str {
    "mktemp"
  }

  fn summary() -> &'static str {
    "Create a unique temporary file or directory."
  }

  fn usage() -> &'static str {
    "mktemp [-d] [-q] [-u] [-p dir] [--suffix suffix] [template]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut command = Self::default();
    let mut index = 0;

    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-d" => {
          command.directory = true;
          index += 1;
        }
        "-q" => {
          command.quiet = true;
          index += 1;
        }
        "-u" => {
          command.dry_run = true;
          index += 1;
        }
        "-t" => {
          command.temp_dir = Some(std::env::temp_dir().display().to_string());
          index += 1;
        }
        "-p" => {
          let Some(dir) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "mktemp: option requires an argument -- 'p'",
            ));
          };
          command.temp_dir = Some(dir.clone());
          index += 2;
        }
        "--suffix" => {
          let Some(suffix) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "mktemp: option requires an argument -- 'suffix'",
            ));
          };
          command.suffix = suffix.clone();
          index += 2;
        }
        _ if arg.starts_with('-') => {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mktemp: unrecognized option '{arg}'"),
          ));
        }
        _ => break,
      }
    }

    if let Some(template) = args.get(index) {
      command.template = template.clone();
      index += 1;
    }

    if index != args.len() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "mktemp: too many operands",
      ));
    }

    if !command.template.contains("XXX") {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "mktemp: template must contain at least 3 consecutive 'X' characters",
      ));
    }

    Ok(command)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let path = create_temp_path(ctx, self)?;
    let line = format!("{}\n", path.display());
    io_util::write_all(ctx.lio(), &ctx.stdout(), line.into_bytes())
  }
}

fn create_temp_path(
  ctx: &AppContext,
  command: &MktempCommand,
) -> io::Result<PathBuf> {
  let base_dir = match &command.temp_dir {
    Some(dir) => PathBuf::from(dir),
    None => std::env::temp_dir(),
  };

  for attempt in 0..1024u32 {
    let candidate =
      render_candidate(&base_dir, &command.template, &command.suffix, attempt);
    if command.dry_run {
      if !candidate.exists() {
        return Ok(candidate);
      }
      continue;
    }

    match create_candidate(ctx, &candidate, command.directory) {
      Ok(()) => return Ok(candidate),
      Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
      Err(err) if command.quiet => {
        return Err(io::Error::new(
          err.kind(),
          "mktemp: failed to create template",
        ));
      }
      Err(err) => return Err(err),
    }
  }

  Err(io::Error::new(
    io::ErrorKind::AlreadyExists,
    "mktemp: failed to create unique temporary path",
  ))
}

fn render_candidate(
  base_dir: &Path,
  template: &str,
  suffix: &str,
  attempt: u32,
) -> PathBuf {
  let stamp = format!(
    "{:x}{:x}{:x}",
    std::process::id(),
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
    attempt
  );
  let rendered = replace_x_run(template, &stamp);
  base_dir.join(format!("{rendered}{suffix}"))
}

fn replace_x_run(template: &str, replacement: &str) -> String {
  let bytes = template.as_bytes();
  let mut start = None;
  let mut len = 0usize;

  for (index, byte) in bytes.iter().enumerate() {
    if *byte == b'X' {
      if start.is_none() {
        start = Some(index);
      }
      len += 1;
    } else if len >= 3 {
      break;
    } else {
      start = None;
      len = 0;
    }
  }

  let start = start.expect("template validated to contain X run");
  let len = len.max(3);
  let token =
    if replacement.len() >= len { &replacement[..len] } else { replacement };

  format!("{}{}{}", &template[..start], token, &template[start + len..])
}

fn create_candidate(
  ctx: &AppContext,
  path: &Path,
  directory: bool,
) -> io::Result<()> {
  let cpath = CString::new(path.as_os_str().to_string_lossy().as_bytes())
    .map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "mktemp: invalid path")
    })?;

  if directory {
    let mut receiver =
      api::mkdirat(&ctx.cwd(), cpath, 0o700).with_lio(ctx.lio()).send();
    io_util::run_recv(ctx.lio(), &mut receiver)
  } else {
    let mut receiver = api::openat(
      &ctx.cwd(),
      cpath,
      libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
      0o600,
    )
    .with_lio(ctx.lio())
    .send();
    let _ = io_util::run_recv(ctx.lio(), &mut receiver)?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  #[test]
  fn parse_mktemp_supports_flags() {
    let parsed = MktempCommand::parse(&[
      "-d".into(),
      "-q".into(),
      "-u".into(),
      "-p".into(),
      "/tmp".into(),
      "--suffix".into(),
      ".txt".into(),
      "name.XXXX".into(),
    ])
    .unwrap();

    assert!(parsed.directory);
    assert!(parsed.quiet);
    assert!(parsed.dry_run);
    assert_eq!(parsed.temp_dir.as_deref(), Some("/tmp"));
    assert_eq!(parsed.suffix, ".txt");
    assert_eq!(parsed.template, "name.XXXX");
  }

  #[test]
  fn mktemp_creates_file() {
    let ctx = AppContext::new().unwrap();
    let dir = unique_temp_dir("mktemp-file-root");
    fs::create_dir(&dir).unwrap();

    let path = create_temp_path(
      &ctx,
      &MktempCommand {
        temp_dir: Some(dir.display().to_string()),
        template: "file.XXXXXX".into(),
        ..Default::default()
      },
    )
    .unwrap();

    assert!(path.is_file());
    fs::remove_file(&path).unwrap();
    fs::remove_dir(dir).unwrap();
  }

  #[test]
  fn mktemp_creates_directory() {
    let ctx = AppContext::new().unwrap();
    let dir = unique_temp_dir("mktemp-dir-root");
    fs::create_dir(&dir).unwrap();

    let path = create_temp_path(
      &ctx,
      &MktempCommand {
        directory: true,
        temp_dir: Some(dir.display().to_string()),
        template: "dir.XXXXXX".into(),
        ..Default::default()
      },
    )
    .unwrap();

    assert!(path.is_dir());
    fs::remove_dir(&path).unwrap();
    fs::remove_dir(dir).unwrap();
  }

  #[test]
  fn mktemp_dry_run_does_not_create_path() {
    let ctx = AppContext::new().unwrap();
    let dir = unique_temp_dir("mktemp-dry-run-root");
    fs::create_dir(&dir).unwrap();

    let path = create_temp_path(
      &ctx,
      &MktempCommand {
        dry_run: true,
        temp_dir: Some(dir.display().to_string()),
        template: "dry.XXXXXX".into(),
        ..Default::default()
      },
    )
    .unwrap();

    assert!(!path.exists());
    fs::remove_dir(dir).unwrap();
  }

  fn unique_temp_dir(name: &str) -> PathBuf {
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
