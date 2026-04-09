use std::{io, num::NonZeroUsize};

use crate::{
  app::AppContext,
  applets::support::{read_key_from_tty_raw, tty_size},
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct MoreCommand {
  pub chop_long_lines: bool,
  pub start_line: Option<NonZeroUsize>,
  pub files: Vec<String>,
}

impl Command for MoreCommand {
  fn name() -> &'static str {
    "more"
  }

  fn summary() -> &'static str {
    "Print a file if it fits in the terminal window."
  }

  fn usage() -> &'static str {
    "more [-S] [+line] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut chop_long_lines = false;
    let mut start_line = None;
    let mut files = Vec::new();

    for arg in args {
      if arg == "-S" {
        chop_long_lines = true;
        continue;
      }
      if let Some(value) = arg.strip_prefix('+') {
        if !value.is_empty() && start_line.is_none() {
          start_line = Some(parse_start_line(value)?);
          continue;
        }
      }
      files.push(arg.clone());
    }

    Ok(Self { chop_long_lines, start_line, files })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let content = read_inputs(ctx, &self.files)?;
    let label = self.files.first().map(String::as_str);
    page_text(ctx, &content, PagerKind::More, label, self)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PagerKind {
  More,
}

const HORIZONTAL_SCROLL_STEP: usize = 10;
const CHOP_MARKER: &str = "\x1b[30;47m>\x1b[0m";

pub(crate) fn page_text(
  ctx: &AppContext,
  text: &str,
  _kind: PagerKind,
  label: Option<&str>,
  options: &MoreCommand,
) -> io::Result<()> {
  let (mut tty_rows, mut tty_cols) = tty_size();
  let mut page_rows = tty_rows.saturating_sub(1).max(1);
  let mut width = tty_cols.max(1);
  let lines = split_lines(text);
  let mut horizontal_offset = 0usize;
  let mut max_start =
    last_page_start(&lines, page_rows, width, horizontal_offset, options);

  if rendered_lines_rows(&lines, width, horizontal_offset, options) <= page_rows
  {
    return io_util::write_all(
      ctx.lio(),
      &ctx.stdout(),
      text.as_bytes().to_vec(),
    );
  }

  let mut start = initial_start(options.start_line, max_start);
  let mut show_initial_footer = true;
  let mut rendered =
    render_page(&lines, start, page_rows, width, horizontal_offset, options);
  io_util::write_all(
    ctx.lio(),
    &ctx.stdout(),
    render_more_frame(&rendered, show_initial_footer, label, false)
      .into_bytes(),
  )?;

  loop {
    let (new_rows, new_cols) = tty_size();
    if new_rows != tty_rows || new_cols != tty_cols {
      tty_rows = new_rows;
      tty_cols = new_cols;
      page_rows = tty_rows.saturating_sub(1).max(1);
      width = tty_cols.max(1);
      max_start =
        last_page_start(&lines, page_rows, width, horizontal_offset, options);
      start = start.min(max_start);
      rendered = render_page(
        &lines,
        start,
        page_rows,
        width,
        horizontal_offset,
        options,
      );
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        render_more_frame(&rendered, show_initial_footer, label, true)
          .into_bytes(),
      )?;
    }

    let Some(key) = read_key_from_tty_raw(ctx.lio())? else {
      continue;
    };

    if is_quit_key(&key) {
      io_util::write_all(ctx.lio(), &ctx.stdout(), b"\r\x1b[K".to_vec())?;
      return Ok(());
    }

    if is_scroll_down_key(&key) {
      let next = next_start(start, max_start);
      if next == start {
        io_util::write_all(ctx.lio(), &ctx.stdout(), b"\r\x1b[K".to_vec())?;
        return Ok(());
      }
      start = next;
      show_initial_footer = false;
      rendered = render_page(
        &lines,
        start,
        page_rows,
        width,
        horizontal_offset,
        options,
      );
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        render_more_frame(&rendered, show_initial_footer, label, true)
          .into_bytes(),
      )?;
      continue;
    }

    if is_scroll_up_key(&key) {
      let next = prev_start(start);
      if next == start {
        continue;
      }
      start = next;
      show_initial_footer = false;
      rendered = render_page(
        &lines,
        start,
        page_rows,
        width,
        horizontal_offset,
        options,
      );
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        render_more_frame(&rendered, show_initial_footer, label, true)
          .into_bytes(),
      )?;
      continue;
    }

    if is_scroll_right_key(&key) {
      horizontal_offset =
        horizontal_offset.saturating_add(HORIZONTAL_SCROLL_STEP);
      max_start =
        last_page_start(&lines, page_rows, width, horizontal_offset, options);
      start = start.min(max_start);
      show_initial_footer = false;
      rendered = render_page(
        &lines,
        start,
        page_rows,
        width,
        horizontal_offset,
        options,
      );
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        render_more_frame(&rendered, show_initial_footer, label, true)
          .into_bytes(),
      )?;
      continue;
    }

    if is_scroll_left_key(&key) {
      let next = horizontal_offset.saturating_sub(HORIZONTAL_SCROLL_STEP);
      if next == horizontal_offset {
        continue;
      }
      horizontal_offset = next;
      max_start =
        last_page_start(&lines, page_rows, width, horizontal_offset, options);
      start = start.min(max_start);
      show_initial_footer = false;
      rendered = render_page(
        &lines,
        start,
        page_rows,
        width,
        horizontal_offset,
        options,
      );
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        render_more_frame(&rendered, show_initial_footer, label, true)
          .into_bytes(),
      )?;
      continue;
    }

    panic!("{}", overflow_panic_message(&key));
  }
}

fn overflow_panic_message(key: &str) -> String {
  format!("more key input: {key:?}")
}

fn parse_start_line(value: &str) -> io::Result<NonZeroUsize> {
  let parsed = value.parse::<NonZeroUsize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("more: invalid start line '{value}'"),
    )
  })?;
  Ok(parsed)
}

fn initial_start(start_line: Option<NonZeroUsize>, max_start: usize) -> usize {
  start_line
    .map(|line| line.get().saturating_sub(1).min(max_start))
    .unwrap_or(0)
}

pub(crate) fn footer_text(
  show_initial_footer: bool,
  label: Option<&str>,
) -> String {
  if !show_initial_footer {
    return ":".to_string();
  }
  match label {
    Some(label) => format!("\x1b[30;47m{label}\x1b[0m"),
    None => ":".to_string(),
  }
}

fn render_more_frame(
  rendered: &str,
  show_initial_footer: bool,
  label: Option<&str>,
  redraw_in_place: bool,
) -> String {
  let footer = footer_text(show_initial_footer, label);
  let mut out = String::with_capacity(
    rendered.len() + footer.len() + if redraw_in_place { 7 } else { 0 },
  );
  if redraw_in_place {
    out.push_str("\x1b[2J\x1b[H");
  }
  out.push_str(rendered);
  out.push_str(&footer);
  out
}

#[cfg(test)]
fn rendered_text_rows(text: &str, tty_cols: usize) -> usize {
  rendered_lines_rows(&split_lines(text), tty_cols, 0, &MoreCommand::default())
}

pub(crate) fn rendered_lines_rows(
  lines: &[&str],
  tty_cols: usize,
  horizontal_offset: usize,
  options: &MoreCommand,
) -> usize {
  lines
    .iter()
    .map(|line| rendered_rows(line, tty_cols, horizontal_offset, options))
    .sum()
}

#[cfg(test)]
fn visible_prefix(text: &str, max_rows: usize, tty_cols: usize) -> String {
  let mut out = String::new();
  let mut used_rows = 0usize;

  for line in split_lines(text) {
    let line_rows = rendered_rows(&line, tty_cols, 0, &MoreCommand::default());
    if used_rows + line_rows > max_rows {
      break;
    }
    used_rows += line_rows;
    out.push_str(&line);
    if used_rows >= max_rows {
      break;
    }
  }

  out
}

pub(crate) fn render_page(
  lines: &[&str],
  start: usize,
  max_rows: usize,
  tty_cols: usize,
  horizontal_offset: usize,
  options: &MoreCommand,
) -> String {
  let mut out = String::new();
  let mut used_rows = 0usize;

  for line in &lines[start.min(lines.len())..] {
    let rendered_line = render_line(line, tty_cols, horizontal_offset, options);
    let line_rows = rendered_line.len();
    if used_rows + line_rows > max_rows {
      break;
    }
    used_rows += line_rows;
    for row in rendered_line {
      out.push_str(&row);
      out.push('\n');
    }
    if used_rows >= max_rows {
      break;
    }
  }

  while used_rows < max_rows {
    out.push('\n');
    used_rows += 1;
  }

  out
}

pub(crate) fn next_start(start: usize, max_start: usize) -> usize {
  if start < max_start { start + 1 } else { start }
}

pub(crate) fn prev_start(start: usize) -> usize {
  start.saturating_sub(1)
}

pub(crate) fn last_page_start(
  lines: &[&str],
  max_rows: usize,
  tty_cols: usize,
  horizontal_offset: usize,
  options: &MoreCommand,
) -> usize {
  if lines.is_empty() {
    return 0;
  }

  let mut used_rows = 0usize;
  let mut start = lines.len();
  while start > 0 {
    let next_rows =
      rendered_rows(&lines[start - 1], tty_cols, horizontal_offset, options);
    if used_rows + next_rows > max_rows {
      break;
    }
    start -= 1;
    used_rows += next_rows;
  }
  start
}

pub(crate) fn is_scroll_down_key(key: &str) -> bool {
  matches!(key, "\n" | "\r" | "\u{1b}[B")
}

pub(crate) fn is_scroll_up_key(key: &str) -> bool {
  matches!(key, "\u{1b}[A")
}

fn is_scroll_right_key(key: &str) -> bool {
  matches!(key, "\u{1b}[C")
}

fn is_scroll_left_key(key: &str) -> bool {
  matches!(key, "\u{1b}[D")
}

pub(crate) fn is_quit_key(key: &str) -> bool {
  matches!(key, "q" | "Q")
}

fn rendered_rows(
  line: &str,
  tty_cols: usize,
  horizontal_offset: usize,
  options: &MoreCommand,
) -> usize {
  render_line(line, tty_cols, horizontal_offset, options).len()
}

fn render_line(
  line: &str,
  tty_cols: usize,
  horizontal_offset: usize,
  options: &MoreCommand,
) -> Vec<String> {
  let available = tty_cols.max(1);
  let text = line.strip_suffix('\n').unwrap_or(line);
  let chars: Vec<char> = text.chars().collect();

  if options.chop_long_lines {
    return vec![render_chopped_line(&chars, available, horizontal_offset)];
  }

  let visible = if horizontal_offset >= chars.len() {
    &[][..]
  } else {
    &chars[horizontal_offset..]
  };

  if visible.is_empty() {
    return vec![String::new()];
  }

  let mut rows = Vec::new();
  let mut offset = 0usize;
  while offset < visible.len() {
    let end = (offset + available).min(visible.len());
    rows.push(visible[offset..end].iter().collect());
    offset = end;
  }
  rows
}

fn render_chopped_line(
  chars: &[char],
  available: usize,
  horizontal_offset: usize,
) -> String {
  let remaining = chars.len().saturating_sub(horizontal_offset);
  let right_chopped = remaining > available;
  let body_width =
    if right_chopped { available.saturating_sub(1) } else { available };
  let body: String =
    chars.iter().skip(horizontal_offset).take(body_width).collect();
  let mut out = String::with_capacity(body.len() + 16);
  out.push_str(&body);
  if right_chopped {
    out.push_str(CHOP_MARKER);
  }
  out
}

pub(crate) fn split_lines(text: &str) -> Vec<&str> {
  if text.is_empty() {
    return Vec::new();
  }

  let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
  if !text.ends_with('\n') {
    let covered: usize = lines.iter().map(|line| line.len()).sum();
    if covered < text.len() {
      lines.push(&text[covered..]);
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn split_lines_preserves_trailing_partial_line() {
    let lines = split_lines("a\nb");
    assert_eq!(lines, vec!["a\n", "b"]);
  }

  #[test]
  fn rendered_text_rows_counts_wrapped_lines() {
    assert_eq!(rendered_text_rows("12345\n12\n", 4), 3);
  }

  #[test]
  fn visible_prefix_stops_at_terminal_height() {
    assert_eq!(visible_prefix("a\nb\nc\n", 2, 80), "a\nb\n");
  }

  #[test]
  fn visible_prefix_respects_wrapped_rows() {
    assert_eq!(visible_prefix("12345\n12\n", 2, 4), "12345\n");
    assert_eq!(visible_prefix("12345\n12\n", 3, 4), "12345\n12\n");
  }

  #[test]
  fn parse_more_accepts_optional_files() {
    let parsed = MoreCommand::parse(&["a".into(), "b".into()]).unwrap();
    assert!(!parsed.chop_long_lines);
    assert_eq!(parsed.start_line, None);
    assert_eq!(parsed.files, vec!["a", "b"]);
  }

  #[test]
  fn parse_more_supports_s_flag() {
    let parsed = MoreCommand::parse(&["-S".into(), "a".into()]).unwrap();
    assert!(parsed.chop_long_lines);
    assert_eq!(parsed.files, vec!["a"]);
  }

  #[test]
  fn parse_more_accepts_plus_line_prefix() {
    let parsed = MoreCommand::parse(&["+10".into(), "a".into()]).unwrap();
    assert_eq!(parsed.start_line, NonZeroUsize::new(10));
    assert_eq!(parsed.files, vec!["a"]);
  }

  #[test]
  fn parse_more_rejects_zero_start_line() {
    let err = MoreCommand::parse(&["+0".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn footer_text_uses_label_then_plain_colon() {
    assert_eq!(footer_text(true, None), ":");
    assert_eq!(
      footer_text(true, Some("flake.nix")),
      "\x1b[30;47mflake.nix\x1b[0m"
    );
    assert_eq!(footer_text(false, Some("flake.nix")), ":");
  }

  #[test]
  fn render_more_frame_batches_redraw_and_footer() {
    assert_eq!(
      render_more_frame("body\n", false, None, true),
      "\x1b[2J\x1b[Hbody\n:"
    );
    assert_eq!(
      render_more_frame("body\n", true, Some("file"), false),
      "body\n\x1b[30;47mfile\x1b[0m"
    );
  }

  #[test]
  fn overflow_panic_message_formats_key() {
    assert_eq!(overflow_panic_message("x"), r#"more key input: "x""#);
    assert_eq!(
      overflow_panic_message("\u{1b}[A"),
      r#"more key input: "\u{1b}[A""#
    );
  }

  #[test]
  fn render_page_starts_one_line_later() {
    let lines = split_lines("a\nb\nc\n");
    assert_eq!(
      render_page(&lines, 0, 2, 80, 0, &MoreCommand::default()),
      "a\nb\n"
    );
    assert_eq!(
      render_page(&lines, 1, 2, 80, 0, &MoreCommand::default()),
      "b\nc\n"
    );
  }

  #[test]
  fn render_page_respects_wrapped_rows_from_start() {
    let lines = split_lines("12345\nxx\n");
    assert_eq!(
      render_page(&lines, 0, 2, 4, 0, &MoreCommand::default()),
      "1234\n5\n"
    );
    assert_eq!(
      render_page(&lines, 1, 2, 4, 0, &MoreCommand::default()),
      "xx\n\n"
    );
  }

  #[test]
  fn render_page_pads_short_final_page_to_footer_row() {
    let lines = split_lines("a\nb\n");
    assert_eq!(
      render_page(&lines, 1, 3, 80, 0, &MoreCommand::default()),
      "b\n\n\n"
    );
  }

  #[test]
  fn render_page_honors_horizontal_offset_without_s_flag() {
    let lines = split_lines("abcdef\n");
    assert_eq!(
      render_page(&lines, 0, 2, 4, 2, &MoreCommand::default()),
      "cdef\n\n"
    );
  }

  #[test]
  fn render_page_chops_with_s_flag() {
    let lines = split_lines("abcdef\n");
    let options = MoreCommand { chop_long_lines: true, ..Default::default() };
    assert_eq!(
      render_page(&lines, 0, 2, 4, 0, &options),
      format!("abc{CHOP_MARKER}\n\n")
    );
  }

  #[test]
  fn next_start_stops_at_last_line() {
    assert_eq!(next_start(0, 1), 1);
    assert_eq!(next_start(1, 1), 1);
  }

  #[test]
  fn prev_start_stops_at_zero() {
    assert_eq!(prev_start(0), 0);
    assert_eq!(prev_start(2), 1);
  }

  #[test]
  fn last_page_start_keeps_last_page_fullish() {
    let lines = split_lines("a\nb\nc\n");
    assert_eq!(last_page_start(&lines, 2, 80, 0, &MoreCommand::default()), 1);
    assert_eq!(last_page_start(&lines, 3, 80, 0, &MoreCommand::default()), 0);
  }

  #[test]
  fn last_page_start_respects_wrapped_rows() {
    let lines = split_lines("12345\nxx\nyy\n");
    assert_eq!(last_page_start(&lines, 2, 4, 0, &MoreCommand::default()), 1);
  }

  #[test]
  fn initial_start_uses_one_based_lines_and_clamps() {
    assert_eq!(initial_start(None, 4), 0);
    assert_eq!(initial_start(NonZeroUsize::new(1), 4), 0);
    assert_eq!(initial_start(NonZeroUsize::new(3), 4), 2);
    assert_eq!(initial_start(NonZeroUsize::new(10), 4), 4);
  }

  #[test]
  fn scroll_down_keys_match_enter_and_arrow_down() {
    assert!(is_scroll_down_key("\n"));
    assert!(is_scroll_down_key("\r"));
    assert!(is_scroll_down_key("\u{1b}[B"));
    assert!(!is_scroll_down_key("\u{1b}[A"));
  }

  #[test]
  fn scroll_up_key_matches_arrow_up() {
    assert!(is_scroll_up_key("\u{1b}[A"));
    assert!(!is_scroll_up_key("\u{1b}[B"));
    assert!(!is_scroll_up_key("\n"));
  }

  #[test]
  fn horizontal_scroll_keys_match_arrow_keys() {
    assert!(is_scroll_right_key("\u{1b}[C"));
    assert!(is_scroll_left_key("\u{1b}[D"));
    assert!(!is_scroll_right_key("\u{1b}[B"));
    assert!(!is_scroll_left_key("\u{1b}[A"));
  }

  #[test]
  fn quit_keys_match_q() {
    assert!(is_quit_key("q"));
    assert!(is_quit_key("Q"));
    assert!(!is_quit_key("\u{1b}[A"));
  }
}
