use std::{ffi::CString, io};

use crate::{
  app::AppContext, applets::support::tty_size, command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct MoreCommand {
  pub files: Vec<String>,
}

impl Command for MoreCommand {
  fn name() -> &'static str {
    "more"
  }

  fn summary() -> &'static str {
    "Page through text one screen at a time."
  }

  fn usage() -> &'static str {
    "more [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let content = read_inputs(ctx, &self.files)?;
    let label = self.files.first().map(String::as_str);
    page_text(ctx, &content, PagerKind::More, label)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PagerKind {
  More,
  Less,
}

pub(crate) fn page_text(
  ctx: &AppContext,
  text: &str,
  kind: PagerKind,
  label: Option<&str>,
) -> io::Result<()> {
  let lines = split_lines(text);
  if lines.is_empty() {
    return Ok(());
  }

  let (tty_rows, tty_cols) = tty_size();
  let page_lines = tty_rows.saturating_sub(1).max(1);
  let width = tty_cols.max(1);
  let mut tty = PagerTty::new()?;
  let mut viewport_start = 0usize;
  let mut current_end = 0usize;
  let mut prompt_mode = PromptMode::Initial;
  let mut redraw = false;
  let mut first_paint = true;

  loop {
    current_end = if first_paint || redraw {
      render_page(ctx, &lines, viewport_start, page_lines, width, redraw)?.end
    } else {
      current_end
    };
    first_paint = false;

    if viewport_start == current_end {
      break;
    }

    if current_end >= lines.len() {
      break;
    }

    let action = prompt_for_action(ctx, &mut tty, kind, label, prompt_mode)?;
    clear_prompt(ctx)?;
    match action {
      PagerAction::Quit => break,
      PagerAction::Ignore => {}
      PagerAction::Line => {
        let next_end =
          render_page(ctx, &lines, current_end, 1, width, false)?.end;
        if next_end > current_end {
          current_end = next_end;
          viewport_start =
            rewind_page_start(&lines, current_end, page_lines, width);
        }
        redraw = false;
      }
      PagerAction::BackLine => {
        viewport_start = rewind_page_start(&lines, viewport_start, 1, width);
        redraw = true;
      }
      PagerAction::Page => {
        let next_end =
          render_page(ctx, &lines, current_end, page_lines, width, false)?.end;
        if next_end > current_end {
          current_end = next_end;
          viewport_start =
            rewind_page_start(&lines, current_end, page_lines, width);
        }
        redraw = false;
      }
      PagerAction::BackPage => {
        viewport_start =
          rewind_page_start(&lines, viewport_start, page_lines, width);
        redraw = true;
      }
      PagerAction::Start => {
        viewport_start = 0;
        redraw = true;
      }
      PagerAction::End => {
        viewport_start = end_of_page_start(&lines, page_lines, width);
        redraw = true;
      }
    }
    prompt_mode = PromptMode::Continue;
  }

  Ok(())
}

fn render_page(
  ctx: &AppContext,
  lines: &[String],
  start: usize,
  page_rows: usize,
  tty_cols: usize,
  redraw: bool,
) -> io::Result<RenderedPage> {
  let (start, end) = page_range(lines, start, page_rows, tty_cols);
  if redraw {
    clear_rendered_page(ctx)?;
  }
  if start < end {
    io_util::write_all(
      ctx.lio(),
      &ctx.stdout(),
      lines[start..end].concat().into_bytes(),
    )?;
  }
  Ok(RenderedPage { end })
}

fn page_range(
  lines: &[String],
  next_line: usize,
  page_rows: usize,
  tty_cols: usize,
) -> (usize, usize) {
  let start = next_line.min(lines.len());
  let mut end = start;
  let mut used_rows = 0usize;

  while end < lines.len() {
    let line_rows = rendered_rows(&lines[end], tty_cols);
    if end > start && used_rows + line_rows > page_rows {
      break;
    }
    used_rows += line_rows;
    end += 1;
    if used_rows >= page_rows {
      break;
    }
  }

  (start, end)
}

fn clear_rendered_page(ctx: &AppContext) -> io::Result<()> {
  io_util::write_all(ctx.lio(), &ctx.stdout(), b"\x1b[H\x1b[2J".to_vec())
}

struct RenderedPage {
  end: usize,
}

fn rewind_page_start(
  lines: &[String],
  current_start: usize,
  page_rows: usize,
  tty_cols: usize,
) -> usize {
  let mut start = current_start;
  let mut used_rows = 0usize;

  while start > 0 {
    let prev = start - 1;
    let line_rows = rendered_rows(&lines[prev], tty_cols);
    if start < current_start && used_rows + line_rows > page_rows {
      break;
    }
    used_rows += line_rows;
    start = prev;
    if used_rows >= page_rows {
      break;
    }
  }

  start
}

fn rendered_rows(line: &str, tty_cols: usize) -> usize {
  let width = tty_cols.max(1);
  let text = line.strip_suffix('\n').unwrap_or(line);
  let chars = text.chars().count();
  chars.max(1).div_ceil(width)
}

fn end_of_page_start(
  lines: &[String],
  page_rows: usize,
  tty_cols: usize,
) -> usize {
  let mut start = lines.len();
  let mut used_rows = 0usize;

  while start > 0 {
    let prev = start - 1;
    let line_rows = rendered_rows(&lines[prev], tty_cols);
    if start < lines.len() && used_rows + line_rows > page_rows {
      break;
    }
    used_rows += line_rows;
    start = prev;
    if used_rows >= page_rows {
      break;
    }
  }

  start
}

pub(crate) fn split_lines(text: &str) -> Vec<String> {
  if text.is_empty() {
    return Vec::new();
  }

  let mut lines: Vec<String> =
    text.split_inclusive('\n').map(ToOwned::to_owned).collect();
  if !text.ends_with('\n') {
    let covered: usize = lines.iter().map(String::len).sum();
    if covered < text.len() {
      lines.push(text[covered..].to_string());
    }
  }
  lines
}

fn read_inputs(ctx: &AppContext, files: &[String]) -> io::Result<String> {
  if files.is_empty() {
    return io_util::read_to_string_fd(ctx.lio(), &ctx.stdin());
  }

  let mut out = String::new();
  for path in files {
    out.push_str(&io_util::read_to_string(ctx.lio(), Some(path))?);
  }
  Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PagerAction {
  Quit,
  Ignore,
  Line,
  BackLine,
  Page,
  BackPage,
  Start,
  End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptMode {
  Initial,
  Continue,
}

fn prompt_for_action(
  ctx: &AppContext,
  tty: &mut PagerTty,
  kind: PagerKind,
  label: Option<&str>,
  mode: PromptMode,
) -> io::Result<PagerAction> {
  let prompt = prompt_text(kind, label, mode);
  io_util::write_all(ctx.lio(), &ctx.stdout(), prompt.as_bytes().to_vec())?;

  let key = tty.read_key()?;
  Ok(match key.as_deref() {
    Some("\u{3}") | Some("q") | Some("Q") => PagerAction::Quit,
    Some("\n") | Some("\r") | Some("j") | Some("J") | Some("B") | Some("b")
      if kind == PagerKind::More =>
    {
      PagerAction::Line
    }
    Some("\n") | Some("\r") | Some("j") | Some("J") => PagerAction::Line,
    Some("k") | Some("K") | Some("y") | Some("Y") | Some("A") => {
      PagerAction::BackLine
    }
    Some(" ") | Some("f") | Some("F") | Some("z") | Some("Z") => {
      PagerAction::Page
    }
    Some("b") | Some("B") if kind == PagerKind::Less => PagerAction::BackPage,
    Some("g") if kind == PagerKind::Less => PagerAction::Start,
    Some("G") if kind == PagerKind::Less => PagerAction::End,
    Some(key) if is_down_arrow(key) => PagerAction::Line,
    Some(key) if is_up_arrow(key) => PagerAction::BackLine,
    Some(key) if key.starts_with('\u{1b}') => PagerAction::Ignore,
    _ => PagerAction::Ignore,
  })
}

fn is_up_arrow(key: &str) -> bool {
  key.starts_with('\u{1b}') && key.ends_with('A')
}

fn is_down_arrow(key: &str) -> bool {
  key.starts_with('\u{1b}') && key.ends_with('B')
}

fn prompt_text(
  kind: PagerKind,
  label: Option<&str>,
  mode: PromptMode,
) -> String {
  match kind {
    PagerKind::Less => ":".to_string(),
    PagerKind::More => match mode {
      PromptMode::Initial => {
        format!("\x1b[30;47m{}\x1b[0m", label.unwrap_or("--More--"))
      }
      PromptMode::Continue => ":".to_string(),
    },
  }
}

struct PagerTty {
  fd: i32,
  original: libc::termios,
}

impl PagerTty {
  fn new() -> io::Result<Self> {
    let tty_path = CString::new("/dev/tty")?;
    let fd = unsafe { libc::open(tty_path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
      return Err(io::Error::last_os_error());
    }

    let original = get_termios(fd)?;
    let mut raw = original;
    raw.c_iflag &=
      !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON)
        as libc::tcflag_t;
    raw.c_cflag |= libc::CS8 as libc::tcflag_t;
    raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG)
      as libc::tcflag_t;
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    set_termios(fd, &raw)?;

    Ok(Self { fd, original })
  }

  fn read_key(&mut self) -> io::Result<Option<String>> {
    read_single_tty_key(self.fd)
  }
}

impl Drop for PagerTty {
  fn drop(&mut self) {
    let _ = set_termios(self.fd, &self.original);
    unsafe {
      libc::close(self.fd);
    }
  }
}

fn read_single_tty_key(fd: i32) -> io::Result<Option<String>> {
  let mut buf = [0u8; 16];
  let used = read_tty_bytes(fd, &mut buf[..1])?;
  if used == 0 {
    return Ok(None);
  }

  if buf[0] != 0x1b {
    return Ok(std::str::from_utf8(&buf[..1]).ok().map(ToOwned::to_owned));
  }

  let mut used = 1usize;
  let mut idle_polls = 0usize;
  while used < buf.len() && idle_polls < 6 {
    if !poll_tty_readable(fd, 10)? {
      idle_polls += 1;
      continue;
    }
    idle_polls = 0;
    let n = read_tty_bytes(fd, &mut buf[used..used + 1])?;
    if n == 0 {
      break;
    }
    used += n;
    if is_escape_sequence_complete(&buf[..used]) {
      break;
    }
  }

  Ok(std::str::from_utf8(&buf[..used]).ok().map(ToOwned::to_owned))
}

fn is_escape_sequence_complete(buf: &[u8]) -> bool {
  if buf.len() <= 1 || buf[0] != 0x1b {
    return true;
  }

  match buf[1] {
    b'[' => buf[2..].last().is_some_and(|byte| (0x40..=0x7e).contains(byte)),
    b'O' => buf.len() >= 3,
    _ => true,
  }
}

fn read_tty_bytes(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
  let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
  if n < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(n as usize)
}

fn poll_tty_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
  let mut poll_fd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
  let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
  if ready < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(ready > 0 && (poll_fd.revents & libc::POLLIN) != 0)
}

fn get_termios(fd: i32) -> io::Result<libc::termios> {
  let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
  if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(termios)
}

fn set_termios(fd: i32, termios: &libc::termios) -> io::Result<()> {
  if unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) } != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn clear_prompt(ctx: &AppContext) -> io::Result<()> {
  io_util::write_all(ctx.lio(), &ctx.stdout(), b"\r\x1b[2K\r".to_vec())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn split_lines_preserves_trailing_partial_line() {
    let lines = split_lines("a\nb");
    assert_eq!(lines, vec!["a\n", "b"]);
  }

  #[test]
  fn parse_more_accepts_optional_files() {
    let parsed = MoreCommand::parse(&["a".into(), "b".into()]).unwrap();
    assert_eq!(parsed.files, vec!["a", "b"]);
  }

  #[test]
  fn page_range_uses_requested_chunk_size() {
    let lines = vec!["a\n".to_string(), "b\n".to_string(), "c\n".to_string()];
    assert_eq!(page_range(&lines, 0, 2, 80), (0, 2));
    assert_eq!(page_range(&lines, 2, 1, 80), (2, 3));
    assert_eq!(page_range(&lines, 3, 3, 80), (3, 3));
  }

  #[test]
  fn page_range_accounts_for_wrapped_lines() {
    let lines =
      vec!["12345\n".to_string(), "12\n".to_string(), "1234\n".to_string()];
    assert_eq!(page_range(&lines, 0, 3, 4), (0, 2));
  }

  #[test]
  fn rewind_page_start_accounts_for_wrapped_lines() {
    let lines = vec![
      "12345\n".to_string(),
      "12\n".to_string(),
      "1234\n".to_string(),
      "1\n".to_string(),
    ];
    assert_eq!(rewind_page_start(&lines, 2, 3, 4), 0);
    assert_eq!(rewind_page_start(&lines, 3, 3, 4), 1);
  }

  #[test]
  fn more_initial_prompt_uses_file_label() {
    assert_eq!(
      prompt_text(PagerKind::More, Some("flake.nix"), PromptMode::Initial),
      "\u{1b}[30;47mflake.nix\u{1b}[0m"
    );
    assert_eq!(
      prompt_text(PagerKind::More, Some("flake.nix"), PromptMode::Continue),
      ":"
    );
  }

  #[test]
  fn end_of_page_start_accounts_for_wrapped_lines() {
    let lines = vec![
      "12345\n".to_string(),
      "12\n".to_string(),
      "1234\n".to_string(),
      "1\n".to_string(),
    ];
    assert_eq!(end_of_page_start(&lines, 3, 4), 1);
    assert_eq!(end_of_page_start(&lines, 4, 4), 1);
  }
}
