use std::{
  ffi::{CStr, CString},
  fs, io,
  path::{Path, PathBuf},
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
  app::AppContext,
  command::Command,
  util::{
    cwd as cwd_util,
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

#[derive(Debug, Clone, Default)]
pub struct LsCommand {
  pub all: bool,
  pub almost_all: bool,
  pub long: bool,
  pub human_readable: bool,
  pub one: bool,
  pub directory: bool,
  pub classify: bool,
  pub recursive: bool,
  pub color: bool,
  pub full_time: bool,
  pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LsEntry {
  display_name: String,
  full_path: PathBuf,
  file_type: fs::FileType,
  mode: u32,
  attr_marker: char,
  nlink: u64,
  user: String,
  group: String,
  size: u64,
  blocks: u64,
  mtime_secs: i64,
  symlink_target: Option<String>,
}

impl Command for LsCommand {
  fn name() -> &'static str {
    "ls"
  }

  fn summary() -> &'static str {
    "List directory contents."
  }

  fn usage() -> &'static str {
    "ls [-aA1dFGhlRT] [path ...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const FLAGS: &[FlagSpec<'static>] = &[
      FlagSpec {
        name: "all",
        short: &['a'],
        long: &["all"],
        takes_value: false,
      },
      FlagSpec {
        name: "almost_all",
        short: &['A'],
        long: &["almost-all"],
        takes_value: false,
      },
      FlagSpec {
        name: "long",
        short: &['l'],
        long: &["long"],
        takes_value: false,
      },
      FlagSpec {
        name: "human_readable",
        short: &['h'],
        long: &[],
        takes_value: false,
      },
      FlagSpec { name: "one", short: &['1'], long: &[], takes_value: false },
      FlagSpec {
        name: "directory",
        short: &['d'],
        long: &["directory"],
        takes_value: false,
      },
      FlagSpec {
        name: "classify",
        short: &['F'],
        long: &["classify"],
        takes_value: false,
      },
      FlagSpec {
        name: "recursive",
        short: &['R'],
        long: &["recursive"],
        takes_value: false,
      },
      FlagSpec {
        name: "color",
        short: &['G'],
        long: &["color"],
        takes_value: false,
      },
      FlagSpec {
        name: "full_time",
        short: &['T'],
        long: &[],
        takes_value: false,
      },
    ];

    let parsed = FlagParser::new("ls", FLAGS).parse(args)?;
    let paths = if parsed.positional().is_empty() {
      vec![".".into()]
    } else {
      parsed.positional().to_vec()
    };

    Ok(Self {
      all: parsed.get_flag_exists("all"),
      almost_all: parsed.get_flag_exists("almost_all"),
      long: parsed.get_flag_exists("long"),
      human_readable: parsed.get_flag_exists("human_readable"),
      one: parsed.get_flag_exists("one"),
      directory: parsed.get_flag_exists("directory"),
      classify: parsed.get_flag_exists("classify"),
      recursive: parsed.get_flag_exists("recursive"),
      color: parsed.get_flag_exists("color"),
      full_time: parsed.get_flag_exists("full_time"),
      paths,
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let cwd = cwd_util::current_working_directory(ctx)?;
    write_ls(ctx, &cwd, self)
  }
}

fn write_ls(
  ctx: &AppContext,
  cwd: &Path,
  command: &LsCommand,
) -> io::Result<()> {
  let stdout = ctx.stdout();
  let color_enabled = command.color;
  let paths = if command.paths.is_empty() {
    vec![".".to_string()]
  } else {
    command.paths.clone()
  };
  let mut out = String::new();
  let mut file_entries = Vec::new();
  let mut directory_targets = Vec::new();

  for raw_path in &paths {
    let input_path = cwd.join(raw_path);
    let metadata = fs::symlink_metadata(&input_path)?;
    if command.directory || !metadata.is_dir() {
      file_entries.push(entry_for_path(raw_path, &input_path, command)?);
    } else {
      directory_targets.push((raw_path.clone(), input_path));
    }
  }

  if !file_entries.is_empty() {
    render_entries(&mut out, &file_entries, command, false, color_enabled);
    flush_ls_output(ctx, &stdout, &mut out)?;
  }

  let multiple_directories = directory_targets.len() > 1;
  let show_directory_headers =
    command.recursive || multiple_directories || !file_entries.is_empty();
  for (index, (raw_path, input_path)) in directory_targets.iter().enumerate() {
    if !out.is_empty() && (index > 0 || !file_entries.is_empty()) {
      out.push('\n');
    }
    if show_directory_headers {
      out.push_str(raw_path);
      out.push_str(":\n");
    }
    render_directory(
      &mut out,
      cwd,
      raw_path,
      input_path,
      command,
      show_directory_headers,
      color_enabled,
    )?;
    flush_ls_output(ctx, &stdout, &mut out)?;
  }

  Ok(())
}

#[cfg(test)]
fn render_ls(cwd: &Path, command: &LsCommand) -> io::Result<String> {
  let mut out = String::new();
  let color_enabled = command.color;
  let paths = if command.paths.is_empty() {
    vec![".".to_string()]
  } else {
    command.paths.clone()
  };
  let mut file_entries = Vec::new();
  let mut directory_targets = Vec::new();

  for raw_path in &paths {
    let input_path = cwd.join(raw_path);
    let metadata = fs::symlink_metadata(&input_path)?;
    if command.directory || !metadata.is_dir() {
      file_entries.push(entry_for_path(raw_path, &input_path, command)?);
    } else {
      directory_targets.push((raw_path.clone(), input_path));
    }
  }

  if !file_entries.is_empty() {
    render_entries(&mut out, &file_entries, command, false, color_enabled);
  }

  let multiple_directories = directory_targets.len() > 1;
  let show_directory_headers =
    command.recursive || multiple_directories || !file_entries.is_empty();
  for (index, (raw_path, input_path)) in directory_targets.iter().enumerate() {
    if !out.is_empty() && (index > 0 || !file_entries.is_empty()) {
      out.push('\n');
    }
    if show_directory_headers {
      out.push_str(raw_path);
      out.push_str(":\n");
    }
    render_directory(
      &mut out,
      cwd,
      raw_path,
      input_path,
      command,
      show_directory_headers,
      color_enabled,
    )?;
  }

  Ok(out)
}

fn render_directory(
  out: &mut String,
  cwd: &Path,
  raw_path: &str,
  path: &Path,
  command: &LsCommand,
  multiple: bool,
  color_enabled: bool,
) -> io::Result<()> {
  let entries = read_directory_entries(path, command)?;
  render_entries(out, &entries, command, true, color_enabled);

  if command.recursive {
    let mut first = true;
    for entry in &entries {
      if !entry.file_type.is_dir() {
        continue;
      }
      let base_name = Path::new(&entry.display_name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&entry.display_name);
      if matches!(base_name, "." | "..") {
        continue;
      }
      if !first || !entries.is_empty() || multiple {
        out.push('\n');
      }
      first = false;

      let nested_display = if raw_path == "." {
        entry.display_name.clone()
      } else {
        format!("{raw_path}/{}", entry.display_name)
      };
      out.push_str(&nested_display);
      out.push_str(":\n");
      render_directory(
        out,
        cwd,
        &nested_display,
        &cwd.join(&nested_display),
        command,
        true,
        color_enabled,
      )?;
    }
  }

  Ok(())
}

fn read_directory_entries(
  path: &Path,
  command: &LsCommand,
) -> io::Result<Vec<LsEntry>> {
  let mut entries = Vec::new();
  if command.all {
    entries.push(entry_for_path(".", path, command)?);
    entries.push(entry_for_path("..", &path.join(".."), command)?);
  }
  for entry in fs::read_dir(path)? {
    let entry = entry?;
    let file_name = entry.file_name().to_string_lossy().into_owned();
    if should_skip_entry(&file_name, command) {
      continue;
    }
    entries.push(entry_for_path(&file_name, &entry.path(), command)?);
  }
  entries.sort_by(|left, right| left.display_name.cmp(&right.display_name));
  Ok(entries)
}

fn should_skip_entry(name: &str, command: &LsCommand) -> bool {
  if command.all {
    return false;
  }
  if command.almost_all {
    return matches!(name, "." | "..");
  }
  name.starts_with('.')
}

fn entry_for_path(
  display_name: &str,
  path: &Path,
  command: &LsCommand,
) -> io::Result<LsEntry> {
  let metadata = fs::symlink_metadata(path)?;
  let file_type = metadata.file_type();
  let need_long = command.long;
  let need_mode = need_long || command.classify || command.color;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    Ok(LsEntry {
      display_name: display_name.to_owned(),
      full_path: path.to_path_buf(),
      file_type,
      mode: if need_mode { metadata.mode() } else { 0 },
      attr_marker: if need_long {
        attr_marker(path, file_type.is_symlink())
      } else {
        ' '
      },
      nlink: if need_long { metadata.nlink() } else { 0 },
      user: if need_long {
        lookup_user_name(metadata.uid())
      } else {
        String::new()
      },
      group: if need_long {
        lookup_group_name(metadata.gid())
      } else {
        String::new()
      },
      size: if need_long { metadata.len() } else { 0 },
      blocks: if need_long { metadata.blocks() as u64 } else { 0 },
      mtime_secs: if need_long { metadata.mtime() } else { 0 },
      symlink_target: if need_long {
        symlink_target(path, file_type.is_symlink())
      } else {
        None
      },
    })
  }

  #[cfg(not(unix))]
  {
    Ok(LsEntry {
      display_name: display_name.to_owned(),
      full_path: path.to_path_buf(),
      file_type,
      mode: if need_mode { metadata.permissions().mode() } else { 0 },
      attr_marker: ' ',
      nlink: if need_long { 1 } else { 0 },
      user: if need_long { "0".into() } else { String::new() },
      group: if need_long { "0".into() } else { String::new() },
      size: if need_long { metadata.len() } else { 0 },
      blocks: 0,
      mtime_secs: if need_long { 0 } else { 0 },
      symlink_target: None,
    })
  }
}

fn flush_ls_output(
  ctx: &AppContext,
  stdout: &lio::api::resource::Resource,
  out: &mut String,
) -> io::Result<()> {
  if out.is_empty() {
    return Ok(());
  }
  let bytes = std::mem::take(out).into_bytes();
  io_util::write_all(ctx.lio(), stdout, bytes)
}

fn render_entries(
  out: &mut String,
  entries: &[LsEntry],
  command: &LsCommand,
  show_total: bool,
  color_enabled: bool,
) {
  if command.long {
    if show_total && !entries.is_empty() {
      out.push_str(&format!("total {}\n", total_blocks(entries)));
    }
    let nlink_width = entries
      .iter()
      .map(|entry| entry.nlink.to_string().len())
      .max()
      .unwrap_or(1);
    let user_width =
      entries.iter().map(|entry| entry.user.len()).max().unwrap_or(1);
    let group_width =
      entries.iter().map(|entry| entry.group.len()).max().unwrap_or(1);
    let size_width = entries
      .iter()
      .map(|entry| render_size(entry.size, command.human_readable).len())
      .max()
      .unwrap_or(1);
    for entry in entries {
      let name = classify_name(entry, command.classify, color_enabled);
      let rendered_name = if let Some(target) = &entry.symlink_target {
        format!("{name} -> {target}")
      } else {
        name
      };
      let size = render_size(entry.size, command.human_readable);
      out.push_str(&format!(
        "{}{} {:>nlink_width$} {:<user_width$}  {:<group_width$}  {:>size_width$} {} {}\n",
        mode_string(entry),
        entry.attr_marker,
        entry.nlink,
        entry.user,
        entry.group,
        size,
        format_mtime(entry.mtime_secs, command.full_time),
        rendered_name
      ));
    }
    return;
  }

  let separator = if command.one { "\n" } else { "\n" };
  for (index, entry) in entries.iter().enumerate() {
    if index > 0 {
      out.push_str(separator);
    }
    out.push_str(&classify_name(entry, command.classify, color_enabled));
  }
  if !entries.is_empty() {
    out.push('\n');
  }
}

fn total_blocks(entries: &[LsEntry]) -> u64 {
  entries.iter().map(|entry| entry.blocks).sum()
}

fn classify_name(
  entry: &LsEntry,
  classify: bool,
  color_enabled: bool,
) -> String {
  let base_name = if color_enabled {
    colorize_entry_name(entry)
  } else {
    entry.display_name.clone()
  };
  if !classify {
    return base_name;
  }

  let suffix = if entry.file_type.is_dir() {
    "/"
  } else if entry.file_type.is_symlink() {
    "@"
  } else if entry.mode & 0o111 != 0 {
    "*"
  } else {
    ""
  };
  format!("{}{}", base_name, suffix)
}

fn mode_string(entry: &LsEntry) -> String {
  let mut out = String::with_capacity(10);
  out.push(if entry.file_type.is_dir() {
    'd'
  } else if entry.file_type.is_symlink() {
    'l'
  } else {
    '-'
  });

  let permissions = entry.mode & 0o777;
  for shift in [6, 3, 0] {
    let bits = (permissions >> shift) & 0o7;
    out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
    out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
    out.push(if bits & 0o1 != 0 { 'x' } else { '-' });
  }
  out
}

fn symlink_target(path: &Path, is_symlink: bool) -> Option<String> {
  if !is_symlink {
    return None;
  }
  fs::read_link(path).ok().map(|target| target.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn lookup_user_name(uid: u32) -> String {
  unsafe {
    let passwd = libc::getpwuid(uid);
    if passwd.is_null() {
      return uid.to_string();
    }
    CStr::from_ptr((*passwd).pw_name).to_string_lossy().into_owned()
  }
}

#[cfg(not(unix))]
fn lookup_user_name(uid: u32) -> String {
  uid.to_string()
}

#[cfg(unix)]
fn lookup_group_name(gid: u32) -> String {
  unsafe {
    let group = libc::getgrgid(gid);
    if group.is_null() {
      return gid.to_string();
    }
    CStr::from_ptr((*group).gr_name).to_string_lossy().into_owned()
  }
}

#[cfg(not(unix))]
fn lookup_group_name(gid: u32) -> String {
  gid.to_string()
}

#[cfg(unix)]
fn attr_marker(path: &Path, is_symlink: bool) -> char {
  #[cfg(target_vendor = "apple")]
  {
    let Ok(cpath) = CString::new(path.as_os_str().to_string_lossy().as_bytes())
    else {
      return ' ';
    };
    let options = if is_symlink { libc::XATTR_NOFOLLOW } else { 0 };
    let len = unsafe {
      libc::listxattr(cpath.as_ptr(), std::ptr::null_mut(), 0, options)
    };
    if len > 0 { '@' } else { ' ' }
  }

  #[cfg(not(target_vendor = "apple"))]
  {
    let _ = (path, is_symlink);
    ' '
  }
}

#[cfg(not(unix))]
fn attr_marker(_path: &Path, _is_symlink: bool) -> char {
  ' '
}

fn format_mtime(secs: i64, full_time: bool) -> String {
  if full_time {
    return format_time(secs, "%b %e %H:%M:%S %Y");
  }
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap_or(Duration::ZERO)
    .as_secs() as i64;
  let recent = secs <= now + 3600 && secs >= now - 60 * 60 * 24 * 30 * 6;
  format_time(secs, if recent { "%b %e %H:%M" } else { "%b %e  %Y" })
}

fn render_size(size: u64, human_readable: bool) -> String {
  if !human_readable {
    return size.to_string();
  }
  human_readable_size(size)
}

fn human_readable_size(size: u64) -> String {
  const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
  let mut value = size as f64;
  let mut unit = 0usize;
  while value >= 1024.0 && unit + 1 < UNITS.len() {
    value /= 1024.0;
    unit += 1;
  }
  if unit == 0 {
    format!("{size}B")
  } else if value >= 10.0 {
    format!("{value:.0}{}", UNITS[unit])
  } else {
    format!("{value:.1}{}", UNITS[unit])
  }
}

fn colorize_entry_name(entry: &LsEntry) -> String {
  let color = if entry.file_type.is_dir() {
    Some(ANSI_DIRECTORY)
  } else if entry.file_type.is_symlink() {
    Some(ANSI_SYMLINK)
  } else if entry.mode & 0o111 != 0 {
    Some(ANSI_EXECUTABLE)
  } else {
    None
  };

  match color {
    Some(color) => format!("{color}{}{}", entry.display_name, ANSI_RESET),
    None => entry.display_name.clone(),
  }
}

const ANSI_DIRECTORY: &str = "\x1b[34m";
const ANSI_SYMLINK: &str = "\x1b[36m";
const ANSI_EXECUTABLE: &str = "\x1b[32m";
const ANSI_RESET: &str = "\x1b[0m";

fn format_time(secs: i64, format: &str) -> String {
  #[cfg(unix)]
  unsafe {
    let mut time = secs as libc::time_t;
    let mut tm = std::mem::zeroed::<libc::tm>();
    if libc::localtime_r(&mut time, &mut tm).is_null() {
      return secs.to_string();
    }
    let c_format = CString::new(format).expect("valid strftime format");
    let mut buf = [0u8; 64];
    let written = libc::strftime(
      buf.as_mut_ptr().cast(),
      buf.len(),
      c_format.as_ptr(),
      &tm,
    );
    if written == 0 {
      return secs.to_string();
    }
    String::from_utf8_lossy(&buf[..written]).into_owned()
  }

  #[cfg(not(unix))]
  {
    let _ = format;
    secs.to_string()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

  struct TempDir {
    path: PathBuf,
  }

  impl TempDir {
    fn new(prefix: &str) -> Self {
      let path = std::env::temp_dir().join(format!(
        "busybox-ls-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
          .duration_since(std::time::UNIX_EPOCH)
          .unwrap()
          .as_nanos()
      ));
      fs::create_dir_all(&path).unwrap();
      Self { path }
    }

    fn write(&self, relative: &str, contents: &[u8]) {
      let path = self.path.join(relative);
      if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
      }
      fs::write(path, contents).unwrap();
    }

    fn mkdir(&self, relative: &str) {
      fs::create_dir_all(self.path.join(relative)).unwrap();
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.path);
    }
  }

  #[test]
  fn parse_ls_flags() {
    let parsed = LsCommand::parse(&[
      "-alFGhRT".into(),
      "-d".into(),
      "a".into(),
      "b".into(),
    ])
    .unwrap();
    assert!(parsed.all);
    assert!(parsed.long);
    assert!(parsed.human_readable);
    assert!(parsed.classify);
    assert!(parsed.recursive);
    assert!(parsed.directory);
    assert!(parsed.color);
    assert!(parsed.full_time);
    assert_eq!(parsed.paths, vec!["a", "b"]);
  }

  #[test]
  fn parse_ls_combined_short_aliases() {
    let parsed = LsCommand::parse(&["-aGR".into(), "dir".into()]).unwrap();
    assert!(parsed.all);
    assert!(parsed.color);
    assert!(parsed.recursive);
    assert_eq!(parsed.paths, vec!["dir"]);
  }

  #[test]
  fn parse_ls_defaults_to_current_directory() {
    let parsed = LsCommand::parse(&[]).unwrap();
    assert_eq!(parsed.paths, vec!["."]);
  }

  #[test]
  fn ls_hides_dotfiles_by_default() {
    let dir = TempDir::new("hidden-default");
    dir.write("visible.txt", b"x");
    dir.write(".hidden.txt", b"x");
    let output = render_ls(dir.path.as_path(), &LsCommand::default()).unwrap();
    assert_eq!(output, "visible.txt\n");
  }

  #[test]
  fn ls_all_and_almost_all_behave() {
    let dir = TempDir::new("all");
    dir.write(".hidden.txt", b"x");
    dir.write("visible.txt", b"x");

    let output = render_ls(
      dir.path.as_path(),
      &LsCommand { all: true, paths: vec![".".into()], ..LsCommand::default() },
    )
    .unwrap();
    assert!(output.contains(".\n") || output.contains(" .\n"));
    assert!(output.contains("..\n") || output.contains(" ..\n"));
    assert!(output.contains(".hidden.txt"));

    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        almost_all: true,
        paths: vec![".".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(!output.contains("\n.\n"));
    assert!(!output.contains("\n..\n"));
    assert!(output.contains(".hidden.txt"));
  }

  #[test]
  fn ls_directory_lists_directory_itself_with_d() {
    let dir = TempDir::new("directory");
    dir.mkdir("subdir");
    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        directory: true,
        paths: vec!["subdir".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert_eq!(output, "subdir\n");
  }

  #[test]
  fn ls_classify_marks_directories_and_executables() {
    let dir = TempDir::new("classify");
    dir.mkdir("subdir");
    dir.write("run.sh", b"#!/bin/sh\n");
    let mut perms =
      fs::metadata(dir.path.join("run.sh")).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(dir.path.join("run.sh"), perms).unwrap();

    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        classify: true,
        paths: vec![".".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(output.contains("run.sh*"));
    assert!(output.contains("subdir/"));
  }

  #[test]
  fn ls_recursive_descends_into_subdirectories() {
    let dir = TempDir::new("recursive");
    dir.mkdir("sub");
    dir.write("sub/file.txt", b"x");
    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        recursive: true,
        paths: vec![".".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(output.contains("sub:\n"));
    assert!(output.contains("file.txt"));
  }

  #[test]
  fn ls_long_format_includes_mode_and_size() {
    let dir = TempDir::new("long");
    dir.write("file.txt", b"hello");
    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        long: true,
        paths: vec![".".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(output.contains("-rw"));
    assert!(output.contains("file.txt"));
    assert!(output.contains("5"));
  }

  #[test]
  fn ls_long_format_human_readable_sizes() {
    let dir = TempDir::new("human");
    dir.write("file.txt", &[0_u8; 1536]);
    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        long: true,
        human_readable: true,
        paths: vec![".".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(output.contains("1.5K"));
  }

  #[test]
  fn ls_full_time_includes_seconds_and_year() {
    let rendered = format_mtime(0, true);
    assert!(rendered.contains(":00:00"));
    assert!(rendered.ends_with("1970"));
  }

  #[test]
  fn ls_long_directory_total_uses_full_block_sum() {
    let entries = vec![
      LsEntry {
        display_name: "a".into(),
        full_path: PathBuf::from("a"),
        file_type: fs::symlink_metadata(".").unwrap().file_type(),
        mode: 0,
        attr_marker: ' ',
        nlink: 1,
        user: "u".into(),
        group: "g".into(),
        size: 0,
        blocks: 8,
        mtime_secs: 0,
        symlink_target: None,
      },
      LsEntry {
        display_name: "b".into(),
        full_path: PathBuf::from("b"),
        file_type: fs::symlink_metadata(".").unwrap().file_type(),
        mode: 0,
        attr_marker: ' ',
        nlink: 1,
        user: "u".into(),
        group: "g".into(),
        size: 0,
        blocks: 4,
        mtime_secs: 0,
        symlink_target: None,
      },
    ];
    assert_eq!(total_blocks(&entries), 12);
  }

  #[test]
  fn ls_long_files_do_not_get_section_headers_or_totals() {
    let dir = TempDir::new("long-files");
    dir.write("a.txt", b"a");
    dir.write("b.txt", b"b");
    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        long: true,
        paths: vec!["a.txt".into(), "b.txt".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(!output.contains("a.txt:\n"));
    assert!(!output.contains("b.txt:\n"));
    assert!(!output.contains("total "));
  }

  #[test]
  fn ls_ld_directory_renders_entry_without_directory_listing_header() {
    let dir = TempDir::new("ld");
    dir.mkdir("subdir");
    let output = render_ls(
      dir.path.as_path(),
      &LsCommand {
        long: true,
        directory: true,
        paths: vec!["subdir".into()],
        ..LsCommand::default()
      },
    )
    .unwrap();
    assert!(!output.contains("subdir:\n"));
    assert!(!output.contains("total "));
    assert!(output.contains(" subdir\n"));
  }

  #[test]
  fn ls_color_wraps_directory_names() {
    let dir = TempDir::new("color");
    dir.mkdir("subdir");
    let entry =
      entry_for_path("subdir", &dir.path.join("subdir"), &LsCommand::default())
        .unwrap();
    assert_eq!(classify_name(&entry, false, true), "\u{1b}[34msubdir\u{1b}[0m");
  }
}
