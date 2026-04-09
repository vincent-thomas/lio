use std::{
  ffi::CStr,
  io,
  sync::atomic::{AtomicBool, Ordering},
  time::{Duration, Instant},
};

use crate::{
  app::AppContext,
  applets::support::tty_size,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util, process as process_util,
  },
};

#[derive(Debug, Clone)]
pub struct WatchCommand {
  pub interval: f64,
  pub command: String,
  pub args: Vec<String>,
}

impl Default for WatchCommand {
  fn default() -> Self {
    Self { interval: 2.0, command: String::new(), args: Vec::new() }
  }
}

impl Command for WatchCommand {
  fn name() -> &'static str {
    "watch"
  }

  fn summary() -> &'static str {
    "Execute a program periodically."
  }

  fn usage() -> &'static str {
    "watch [-n seconds] <command> [args...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[FlagSpec {
      name: "interval",
      short: &['n'],
      long: &[],
      takes_value: true,
    }];
    let parsed =
      FlagParser::new("watch", SPECS).parse(args).map_err(|err| {
        if err.kind() == io::ErrorKind::InvalidInput
          && err.to_string().contains("missing value for '-n'")
        {
          io::Error::new(io::ErrorKind::InvalidInput, "watch: missing interval")
        } else {
          err
        }
      })?;

    let Some(program) = parsed.positional().first() else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "watch: missing command",
      ));
    };
    let mut command = Self::default();
    if let Some(value) = parsed.get_flag_value("interval") {
      command.interval = value.parse::<f64>().map_err(|_| {
        io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("watch: invalid interval '{value}'"),
        )
      })?;
      if !command.interval.is_finite() || command.interval <= 0.0 {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "watch: interval must be a positive number",
        ));
      }
    }
    command.command = program.clone();
    command.args = parsed.positional()[1..].to_vec();
    Ok(command)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let _sigint = SigintGuard::enter()?;
    let _guard = AltScreenGuard::enter(ctx)?;
    loop {
      if interrupted() {
        return Ok(());
      }
      let started = Instant::now();
      let (output, status) =
        process_util::run_command_capture(ctx, &self.command, &self.args)?;
      if interrupted() {
        return Ok(());
      }
      let elapsed = started.elapsed();
      let frame = render_watch_frame(self, &output, status, elapsed);
      io_util::write_all(ctx.lio(), &ctx.stdout(), frame)?;

      sleep_interruptibly(ctx, Duration::from_secs_f64(self.interval))?;
    }
  }
}

struct AltScreenGuard<'a> {
  ctx: &'a AppContext,
}

impl<'a> AltScreenGuard<'a> {
  fn enter(ctx: &'a AppContext) -> io::Result<Self> {
    io_util::write_all(ctx.lio(), &ctx.stdout(), b"\x1b[?1049h".to_vec())?;
    Ok(Self { ctx })
  }
}

impl Drop for AltScreenGuard<'_> {
  fn drop(&mut self) {
    let _ = io_util::write_all(
      self.ctx.lio(),
      &self.ctx.stdout(),
      b"\x1b[?1049l".to_vec(),
    );
  }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: libc::c_int) {
  INTERRUPTED.store(true, Ordering::SeqCst);
}

struct SigintGuard {
  previous: libc::sigaction,
}

impl SigintGuard {
  fn enter() -> io::Result<Self> {
    INTERRUPTED.store(false, Ordering::SeqCst);
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_flags = 0;
    action.sa_sigaction = handle_sigint as *const () as usize;
    let mut previous = unsafe { std::mem::zeroed::<libc::sigaction>() };
    let empty = unsafe { libc::sigemptyset(&mut action.sa_mask) };
    if empty != 0 {
      return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::sigaction(libc::SIGINT, &action, &mut previous) };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(Self { previous })
  }
}

impl Drop for SigintGuard {
  fn drop(&mut self) {
    unsafe {
      libc::sigaction(libc::SIGINT, &self.previous, std::ptr::null_mut());
    }
    INTERRUPTED.store(false, Ordering::SeqCst);
  }
}

fn interrupted() -> bool {
  INTERRUPTED.load(Ordering::SeqCst)
}

fn sleep_interruptibly(ctx: &AppContext, duration: Duration) -> io::Result<()> {
  let deadline = Instant::now() + duration;
  while Instant::now() < deadline {
    if interrupted() {
      return Ok(());
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let step = remaining.min(Duration::from_millis(50));
    let rx = lio::api::sleep(step).with_lio(ctx.lio()).send();
    io_util::run(ctx.lio(), rx)?;
  }
  Ok(())
}

fn render_watch_frame(
  command: &WatchCommand,
  output: &[u8],
  status: process_util::ChildStatus,
  elapsed: Duration,
) -> Vec<u8> {
  let (rows, width) = tty_size();
  let width = width.max(1);
  let body_rows = rows.saturating_sub(2);
  let command_text = format_command(command);
  let top_left = format!("Every {:.1}s: {command_text}", command.interval);
  let top_right = format!("{}: {}", hostname(), current_time_string());
  let bottom_right =
    format!("in {:.3}s ({})", elapsed.as_secs_f64(), status_code(status));

  let mut frame = Vec::new();
  frame.extend_from_slice(b"\x1b[2J\x1b[H");
  push_line(&mut frame, &compose_header_line(&top_left, &top_right, width));
  push_line(&mut frame, &compose_header_line("", &bottom_right, width));

  let rendered_output = render_output(output, body_rows, width);
  frame.extend_from_slice(rendered_output.as_bytes());
  frame
}

fn render_output(output: &[u8], rows: usize, width: usize) -> String {
  if rows == 0 {
    return String::new();
  }

  let mut rendered = String::new();
  let text = String::from_utf8_lossy(output);
  for line in text.lines().take(rows) {
    push_string_line(&mut rendered, &truncate_display(line, width));
  }
  rendered
}

fn format_command(command: &WatchCommand) -> String {
  let mut rendered = command.command.clone();
  for arg in &command.args {
    rendered.push(' ');
    rendered.push_str(arg);
  }
  rendered
}

fn compose_header_line(left: &str, right: &str, width: usize) -> String {
  let left = truncate_display(left, width);
  let right = truncate_display(right, width);
  let left_len = left.chars().count();
  let right_len = right.chars().count();

  if right.is_empty() {
    return left;
  }
  if left.is_empty() {
    return format!("{:>width$}", right, width = width);
  }
  if left_len + 1 + right_len > width {
    let available_left = width.saturating_sub(right_len + 1);
    let left = truncate_display(&left, available_left);
    return format!(
      "{left}{:spaces$}{right}",
      "",
      spaces = width.saturating_sub(left.chars().count() + right_len)
    );
  }

  format!("{left}{:spaces$}{right}", "", spaces = width - left_len - right_len)
}

fn truncate_display(input: &str, width: usize) -> String {
  input.chars().take(width).collect()
}

fn push_line(buf: &mut Vec<u8>, line: &str) {
  buf.extend_from_slice(line.as_bytes());
  buf.push(b'\n');
}

fn push_string_line(buf: &mut String, line: &str) {
  buf.push_str(line);
  buf.push('\n');
}

fn status_code(status: process_util::ChildStatus) -> i32 {
  match status {
    process_util::ChildStatus::Exited(code) => code,
    process_util::ChildStatus::Signaled(signal) => 128 + signal,
    process_util::ChildStatus::Other(raw) => raw,
  }
}

fn hostname() -> String {
  let mut buf = [0u8; 256];
  let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
  if rc != 0 {
    return String::from("unknown");
  }

  let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
  String::from_utf8_lossy(&buf[..len]).into_owned()
}

fn current_time_string() -> String {
  let mut now = 0;
  unsafe {
    libc::time(&mut now);
  }
  let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
  let local = unsafe { libc::localtime_r(&now, &mut tm) };
  if local.is_null() {
    return String::from("--:--:--");
  }

  let mut buf = [0i8; 16];
  let fmt = c"%H:%M:%S";
  let written =
    unsafe { libc::strftime(buf.as_mut_ptr(), buf.len(), fmt.as_ptr(), &tm) };
  if written == 0 {
    return String::from("--:--:--");
  }

  unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_watch_supports_interval_flag() {
    let parsed = WatchCommand::parse(&[
      "-n".into(),
      "0.5".into(),
      "echo".into(),
      "hi".into(),
    ])
    .unwrap();
    assert_eq!(parsed.interval, 0.5);
    assert_eq!(parsed.command, "echo");
    assert_eq!(parsed.args, vec!["hi"]);
  }

  #[test]
  fn parse_watch_requires_command() {
    assert!(WatchCommand::parse(&[]).is_err());
    assert!(WatchCommand::parse(&["-n".into(), "1".into()]).is_err());
  }

  #[test]
  fn header_line_right_aligns_status() {
    let rendered = compose_header_line("Every 2.0s: ls", "host: 16:42:34", 40);
    assert_eq!(rendered.chars().count(), 40);
    assert!(rendered.starts_with("Every 2.0s: ls"));
    assert!(rendered.ends_with("host: 16:42:34"));
  }

  #[test]
  fn render_output_clips_rows() {
    let rendered = render_output(b"a\nb\nc\n", 2, 80);
    assert_eq!(rendered, "a\nb\n");
  }
}
