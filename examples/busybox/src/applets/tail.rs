use std::{
  collections::VecDeque, ffi::CString, io, os::fd::AsRawFd,
  os::unix::fs::MetadataExt, time::Duration,
};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Copy)]
enum TailMode {
  Lines(usize),
  Bytes(usize),
  LinesFromStart(usize),
  BytesFromStart(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FollowMode {
  Descriptor,
  NameRetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
  dev: u64,
  ino: u64,
}

#[derive(Debug, Clone)]
struct FollowTarget {
  path: String,
  fd: Option<lio::api::resource::Resource>,
}

#[derive(Debug, Clone, Default)]
pub struct TailCommand {
  mode: Option<TailMode>,
  follow: Option<FollowMode>,
  pub quiet: bool,
  pub verbose: bool,
  pub files: Vec<String>,
}

impl Command for TailCommand {
  fn name() -> &'static str {
    "tail"
  }

  fn summary() -> &'static str {
    "Print the last part of files."
  }

  fn usage() -> &'static str {
    "tail [-F|-f] [-q] [-v] [-n N|-c N] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut follow = None;
    let mut quiet = false;
    let mut verbose = false;
    let mut filtered = Vec::new();
    for arg in args {
      if arg == "-f" {
        follow = Some(FollowMode::Descriptor);
      } else if arg == "-F" {
        follow = Some(FollowMode::NameRetry);
      } else if arg == "-q" {
        quiet = true;
      } else if arg == "-v" {
        verbose = true;
      } else {
        filtered.push(arg.clone());
      }
    }
    let (mode, files) =
      parse_count_mode(&filtered, TailMode::Lines(10), "tail")?;
    Ok(Self { mode: Some(mode), follow, quiet, verbose, files: files.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let mode = self.mode.expect("tail mode should be parsed");
    if self.files.is_empty() {
      tail_fd(ctx, &ctx.stdin(), mode, false)?;
      if self.follow.is_some() {
        follow_descriptor_stream(ctx, &ctx.stdin())?;
      }
      Ok(())
    } else {
      let print_headers =
        should_print_headers(self.files.len(), self.quiet, self.verbose);
      let mut follow_targets = Vec::new();
      let mut open_receivers = Vec::with_capacity(self.files.len());
      for path in &self.files {
        let cpath = CString::new(path.as_str())?;
        open_receivers.push(
          api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
            .with_lio(ctx.lio())
            .send(),
        );
      }

      for (i, (path, file)) in self
        .files
        .iter()
        .zip(io_util::run_all(ctx.lio(), open_receivers))
        .enumerate()
      {
        if should_print_headers(self.files.len(), self.quiet, self.verbose) {
          if i > 0 {
            io_util::write_all(ctx.lio(), &ctx.stdout(), b"\n".to_vec())?;
          }
          io_util::write_all(
            ctx.lio(),
            &ctx.stdout(),
            format!("==> {} <==\n", path).into_bytes(),
          )?;
        }
        let file = file?;
        tail_fd(ctx, &file, mode, false)?;
        if self.follow.is_some() {
          follow_targets
            .push(FollowTarget { path: path.clone(), fd: Some(file) });
        }
      }
      if let Some(follow_mode) = self.follow {
        follow_paths(ctx, &mut follow_targets, follow_mode, print_headers)?;
      }
      Ok(())
    }
  }
}

fn should_print_headers(file_count: usize, quiet: bool, verbose: bool) -> bool {
  if quiet {
    false
  } else if verbose {
    true
  } else {
    file_count > 1
  }
}

fn tail_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  mode: TailMode,
  follow: bool,
) -> io::Result<()> {
  let stdout = ctx.stdout();
  match mode {
    TailMode::Lines(num_lines) => {
      let output = tail_last_lines_fd(ctx, fd, num_lines)?;
      io_util::write_all(ctx.lio(), &stdout, output)?;
    }
    TailMode::Bytes(num_bytes) => {
      let output = tail_last_bytes_fd(ctx, fd, num_bytes)?;
      io_util::write_all(ctx.lio(), &stdout, output)?;
    }
    TailMode::LinesFromStart(start_line) => {
      tail_lines_from_start_stream(ctx, fd, &stdout, start_line)?;
    }
    TailMode::BytesFromStart(start_byte) => {
      tail_bytes_from_start_stream(ctx, fd, &stdout, start_byte)?;
    }
  }

  if follow {
    follow_descriptor_stream(ctx, fd)?;
  }

  Ok(())
}

fn follow_descriptor_stream(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
) -> io::Result<()> {
  let stdout = ctx.stdout();
  let mut buf = vec![0u8; 8192];
  loop {
    sleep_for_follow(ctx)?;
    rewind_if_truncated(fd)?;
    let n = read_chunk(ctx, fd, &mut buf)?;
    if n == 0 {
      continue;
    }
    write_chunk(ctx, &stdout, &mut buf, n)?;
  }
}

fn follow_paths(
  ctx: &AppContext,
  targets: &mut [FollowTarget],
  follow_mode: FollowMode,
  print_headers: bool,
) -> io::Result<()> {
  let stdout = ctx.stdout();
  let mut buf = vec![0u8; 8192];
  let mut last_output_index: Option<usize> = None;

  loop {
    sleep_for_follow(ctx)?;

    for (index, target) in targets.iter_mut().enumerate() {
      refresh_follow_target(ctx, target, follow_mode)?;
      let Some(fd) = target.fd.as_ref() else {
        continue;
      };

      let n = read_chunk(ctx, fd, &mut buf)?;
      if n == 0 {
        continue;
      }

      if print_headers && last_output_index != Some(index) {
        if last_output_index.is_some() {
          io_util::write_all(ctx.lio(), &stdout, b"\n".to_vec())?;
        }
        io_util::write_all(
          ctx.lio(),
          &stdout,
          format!("==> {} <==\n", target.path).into_bytes(),
        )?;
      }

      write_chunk(ctx, &stdout, &mut buf, n)?;
      last_output_index = Some(index);
    }
  }
}

fn refresh_follow_target(
  ctx: &AppContext,
  target: &mut FollowTarget,
  follow_mode: FollowMode,
) -> io::Result<()> {
  if follow_mode == FollowMode::Descriptor {
    if let Some(fd) = target.fd.as_ref() {
      rewind_if_truncated(fd)?;
    }
    return Ok(());
  }

  let path_identity = path_identity(ctx, &target.path)?;
  match (&target.fd, path_identity) {
    (Some(fd), Some(path_identity)) => {
      rewind_if_truncated(fd)?;
      let current_identity = file_identity(fd)?;
      if current_identity != Some(path_identity) {
        target.fd = Some(open_follow_file(ctx, &target.path)?);
      }
    }
    (Some(_), None) => {
      target.fd = None;
    }
    (None, Some(_)) => {
      target.fd = Some(open_follow_file(ctx, &target.path)?);
    }
    (None, None) => {}
  }
  Ok(())
}

fn open_follow_file(
  ctx: &AppContext,
  path: &str,
) -> io::Result<lio::api::resource::Resource> {
  let cpath = CString::new(path)?;
  io_util::run(
    ctx.lio(),
    api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
      .with_lio(ctx.lio())
      .send(),
  )
}

fn read_chunk(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  buf: &mut Vec<u8>,
) -> io::Result<usize> {
  let rx = api::read(fd, std::mem::take(buf)).with_lio(ctx.lio()).send();
  let (result, returned_buf) = io_util::run(ctx.lio(), rx);
  *buf = returned_buf;

  let n = result? as usize;
  if n == 0 && buf.len() != 8192 {
    buf.resize(8192, 0);
  }

  Ok(n)
}

fn write_chunk(
  ctx: &AppContext,
  stdout: &lio::api::resource::Resource,
  buf: &mut Vec<u8>,
  n: usize,
) -> io::Result<()> {
  if n == 0 {
    if buf.len() != 8192 {
      buf.resize(8192, 0);
    }
    return Ok(());
  }

  buf.truncate(n);
  *buf =
    io_util::write_all_reusing_buffer(ctx.lio(), stdout, std::mem::take(buf))?;
  buf.resize(8192, 0);
  Ok(())
}

fn sleep_for_follow(ctx: &AppContext) -> io::Result<()> {
  let rx = api::sleep(Duration::from_millis(100)).with_lio(ctx.lio()).send();
  io_util::run(ctx.lio(), rx)?;
  Ok(())
}

fn rewind_if_truncated(fd: &lio::api::resource::Resource) -> io::Result<()> {
  let Some(identity) = file_identity(fd)? else {
    return Ok(());
  };
  let offset = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_CUR) };
  if offset < 0 {
    return Err(io::Error::last_os_error());
  }
  let size = file_size(fd)?;
  if size < offset as u64 {
    let result = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
    if result < 0 {
      return Err(io::Error::last_os_error());
    }
  }
  let _ = identity;
  Ok(())
}

fn file_identity(
  fd: &lio::api::resource::Resource,
) -> io::Result<Option<FileIdentity>> {
  let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
  let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(Some(FileIdentity { dev: stat.st_dev as u64, ino: stat.st_ino as u64 }))
}

fn file_size(fd: &lio::api::resource::Resource) -> io::Result<u64> {
  let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
  let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut stat) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(stat.st_size as u64)
}

fn path_identity(
  ctx: &AppContext,
  path: &str,
) -> io::Result<Option<FileIdentity>> {
  let _ = ctx;
  match std::fs::metadata(path) {
    Ok(stat) => Ok(Some(FileIdentity { dev: stat.dev(), ino: stat.ino() })),
    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
    Err(err) => Err(err),
  }
}

fn parse_count_mode<'a>(
  args: &'a [String],
  default: TailMode,
  applet: &str,
) -> io::Result<(TailMode, &'a [String])> {
  if let Some(value) = args
    .first()
    .and_then(|arg| arg.strip_prefix('-'))
    .filter(|value| !value.is_empty())
    .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
  {
    let count = parse_usize_arg(value, applet)?;
    return Ok((TailMode::Lines(count), &args[1..]));
  }
  if args.len() >= 2 && args[0] == "-n" {
    if let Some(value) = args[1].strip_prefix('+') {
      let count = parse_usize_arg(value, applet)?;
      return Ok((TailMode::LinesFromStart(count), &args[2..]));
    }
    let count = parse_usize_arg(&args[1], applet)?;
    return Ok((TailMode::Lines(count), &args[2..]));
  }
  if args.len() >= 2 && args[0] == "-c" {
    if let Some(value) = args[1].strip_prefix('+') {
      let count = parse_usize_arg(value, applet)?;
      return Ok((TailMode::BytesFromStart(count), &args[2..]));
    }
    let count = parse_usize_arg(&args[1], applet)?;
    return Ok((TailMode::Bytes(count), &args[2..]));
  }
  if let Some(value) = args
    .first()
    .and_then(|arg| arg.strip_prefix('+'))
    .filter(|value| !value.is_empty())
    .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
  {
    let count = parse_usize_arg(value, applet)?;
    return Ok((TailMode::LinesFromStart(count), &args[1..]));
  }
  Ok((default, args))
}

fn parse_usize_arg(value: &str, applet: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{applet}: invalid count '{value}'"),
    )
  })
}

fn tail_last_lines_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  num_lines: usize,
) -> io::Result<Vec<u8>> {
  if num_lines == 0 {
    drain_fd(ctx, fd)?;
    return Ok(Vec::new());
  }

  let mut lines = VecDeque::with_capacity(num_lines + 1);
  let mut current = Vec::new();
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      current.push(byte);
      if byte == b'\n' {
        lines.push_back(std::mem::take(&mut current));
        if lines.len() > num_lines {
          lines.pop_front();
        }
      }
    }
  }

  if !current.is_empty() {
    lines.push_back(current);
    if lines.len() > num_lines {
      lines.pop_front();
    }
  }

  Ok(lines.into_iter().flatten().collect())
}

fn tail_last_bytes_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  num_bytes: usize,
) -> io::Result<Vec<u8>> {
  if num_bytes == 0 {
    drain_fd(ctx, fd)?;
    return Ok(Vec::new());
  }

  let mut tail = VecDeque::with_capacity(num_bytes.min(8192));
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      if tail.len() == num_bytes {
        tail.pop_front();
      }
      tail.push_back(byte);
    }
  }

  Ok(tail.into_iter().collect())
}

#[cfg(test)]
fn tail_lines_from_start_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  start_line: usize,
) -> io::Result<Vec<u8>> {
  if start_line <= 1 {
    return read_all_fd(ctx, fd);
  }

  let mut out = Vec::new();
  let mut buf = vec![0u8; 8192];
  let mut current_line = 1usize;
  let mut started = false;

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      if started {
        out.push(byte);
      }
      if byte == b'\n' {
        current_line += 1;
        if !started && current_line >= start_line {
          started = true;
        }
      }
    }
  }

  Ok(out)
}

fn tail_lines_from_start_stream(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  stdout: &lio::api::resource::Resource,
  start_line: usize,
) -> io::Result<()> {
  if start_line <= 1 {
    return stream_all_fd(ctx, fd, stdout);
  }

  let mut buf = vec![0u8; 8192];
  let mut current_line = 1usize;
  let mut started = false;
  let mut out = Vec::with_capacity(8192);

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      if started {
        out.push(byte);
        if out.len() >= 8192 {
          io_util::write_all(ctx.lio(), stdout, std::mem::take(&mut out))?;
        }
      }
      if byte == b'\n' {
        current_line += 1;
        if !started && current_line >= start_line {
          started = true;
        }
      }
    }
  }

  if !out.is_empty() {
    io_util::write_all(ctx.lio(), stdout, out)?;
  }
  Ok(())
}

#[cfg(test)]
#[allow(dead_code)]
fn tail_bytes_from_start_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  start_byte: usize,
) -> io::Result<Vec<u8>> {
  if start_byte <= 1 {
    return read_all_fd(ctx, fd);
  }

  let mut out = Vec::new();
  let mut buf = vec![0u8; 8192];
  let mut seen = 0usize;

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      seen += 1;
      if seen >= start_byte {
        out.push(byte);
      }
    }
  }

  Ok(out)
}

fn tail_bytes_from_start_stream(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  stdout: &lio::api::resource::Resource,
  start_byte: usize,
) -> io::Result<()> {
  if start_byte <= 1 {
    return stream_all_fd(ctx, fd, stdout);
  }

  let mut buf = vec![0u8; 8192];
  let mut seen = 0usize;
  let mut out = Vec::with_capacity(8192);

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      seen += 1;
      if seen >= start_byte {
        out.push(byte);
        if out.len() >= 8192 {
          io_util::write_all(ctx.lio(), stdout, std::mem::take(&mut out))?;
        }
      }
    }
  }

  if !out.is_empty() {
    io_util::write_all(ctx.lio(), stdout, out)?;
  }
  Ok(())
}

#[cfg(test)]
fn read_all_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
) -> io::Result<Vec<u8>> {
  let mut out = Vec::new();
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }
    out.extend_from_slice(&buf[..n]);
  }

  Ok(out)
}

fn stream_all_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  stdout: &lio::api::resource::Resource,
) -> io::Result<()> {
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    buf.truncate(n);
    buf = io_util::write_all_reusing_buffer(ctx.lio(), stdout, buf)?;
    buf.resize(8192, 0);
  }

  Ok(())
}

fn drain_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
) -> io::Result<()> {
  let mut buf = vec![0u8; 8192];
  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;
    if result? == 0 {
      return Ok(());
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_tail_supports_short_numeric_count() {
    let parsed = TailCommand::parse(&["-20".into(), "file".into()]).unwrap();
    assert!(matches!(parsed.mode, Some(TailMode::Lines(20))));
    assert_eq!(parsed.files, vec!["file"]);
  }

  #[test]
  fn parse_tail_supports_n_flag_count() {
    let parsed =
      TailCommand::parse(&["-n".into(), "20".into(), "file".into()]).unwrap();
    assert!(matches!(parsed.mode, Some(TailMode::Lines(20))));
    assert_eq!(parsed.files, vec!["file"]);
  }

  #[test]
  fn parse_tail_supports_plus_n_starting_line() {
    let parsed =
      TailCommand::parse(&["-n".into(), "+3".into(), "file".into()]).unwrap();
    assert!(matches!(parsed.mode, Some(TailMode::LinesFromStart(3))));
    assert_eq!(parsed.files, vec!["file"]);
  }

  #[test]
  fn parse_tail_supports_q_and_v() {
    let parsed = TailCommand::parse(&[
      "-q".into(),
      "-v".into(),
      "-f".into(),
      "file".into(),
    ])
    .unwrap();
    assert!(parsed.quiet);
    assert!(parsed.verbose);
    assert_eq!(parsed.follow, Some(FollowMode::Descriptor));
    assert_eq!(parsed.files, vec!["file"]);
  }

  #[test]
  fn parse_tail_supports_follow_by_name() {
    let parsed = TailCommand::parse(&["-F".into(), "file".into()]).unwrap();
    assert_eq!(parsed.follow, Some(FollowMode::NameRetry));
    assert_eq!(parsed.files, vec!["file"]);
  }

  #[test]
  fn tail_header_policy_matches_flags() {
    assert!(!should_print_headers(2, true, false));
    assert!(should_print_headers(1, false, true));
    assert!(should_print_headers(2, false, false));
    assert!(!should_print_headers(1, false, false));
  }

  #[test]
  fn tail_streaming_bytes_keeps_only_requested_suffix() {
    let ctx = AppContext::new().unwrap();
    let path = temp_path("tail-bytes");
    std::fs::write(&path, b"0123456789").unwrap();

    let fd = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();

    assert_eq!(tail_last_bytes_fd(&ctx, &fd, 4).unwrap(), b"6789");
    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn tail_streaming_lines_keeps_only_requested_suffix() {
    let ctx = AppContext::new().unwrap();
    let path = temp_path("tail-lines");
    std::fs::write(&path, b"a\nb\nc\nd\n").unwrap();

    let fd = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();

    assert_eq!(tail_last_lines_fd(&ctx, &fd, 2).unwrap(), b"c\nd\n");
    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn tail_streaming_lines_from_start_skips_prefix_lines() {
    let ctx = AppContext::new().unwrap();
    let path = temp_path("tail-lines-from-start");
    std::fs::write(&path, b"a\nb\nc\nd\n").unwrap();

    let fd = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();

    assert_eq!(tail_lines_from_start_fd(&ctx, &fd, 3).unwrap(), b"c\nd\n");
    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn rewind_if_truncated_resets_offset_to_start() {
    let ctx = AppContext::new().unwrap();
    let path = temp_path("tail-follow-truncate");
    std::fs::write(&path, b"abcdef").unwrap();

    let fd = io_util::run(
      ctx.lio(),
      api::openat(
        &ctx.cwd(),
        CString::new(path.to_str().unwrap()).unwrap(),
        libc::O_RDONLY,
        0,
      )
      .with_lio(ctx.lio())
      .send(),
    )
    .unwrap();

    unsafe {
      libc::lseek(fd.as_raw_fd(), 6, libc::SEEK_SET);
    }
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(2).unwrap();

    rewind_if_truncated(&fd).unwrap();
    let offset = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_CUR) };
    assert_eq!(offset, 0);

    std::fs::remove_file(path).unwrap();
  }

  #[test]
  fn refresh_follow_target_reopens_replaced_path_for_follow_by_name() {
    let ctx = AppContext::new().unwrap();
    let dir = temp_path("tail-follow-rename-dir");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("log.txt");
    let rotated = dir.join("log.txt.1");
    std::fs::write(&path, b"old\n").unwrap();

    let fd = open_follow_file(&ctx, path.to_str().unwrap()).unwrap();
    let old_identity = file_identity(&fd).unwrap();
    let mut target =
      FollowTarget { path: path.to_string_lossy().into_owned(), fd: Some(fd) };

    std::fs::rename(&path, &rotated).unwrap();
    std::fs::write(&path, b"new\n").unwrap();

    refresh_follow_target(&ctx, &mut target, FollowMode::NameRetry).unwrap();

    let new_fd = target.fd.as_ref().expect("reopened fd");
    assert_ne!(file_identity(new_fd).unwrap(), old_identity);
    assert_eq!(io_util::read_to_bytes_fd(ctx.lio(), new_fd).unwrap(), b"new\n");

    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(rotated).unwrap();
    std::fs::remove_dir(dir).unwrap();
  }

  fn temp_path(name: &str) -> std::path::PathBuf {
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
