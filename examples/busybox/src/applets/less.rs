use std::{io, num::NonZeroUsize};

use crate::{
  app::AppContext,
  applets::{
    more::{
      footer_text, is_quit_key, is_scroll_down_key, is_scroll_up_key,
      split_lines,
    },
    support::{read_key_from_tty_raw, tty_size},
  },
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct LessCommand {
  pub quit_if_one_screen: bool,
  pub no_init: bool,
  pub quit_at_eof: bool,
  pub start_line: Option<NonZeroUsize>,
  pub show_line_numbers: bool,
  pub chop_long_lines: bool,
  pub files: Vec<String>,
}

const HORIZONTAL_SCROLL_STEP: usize = 10;
const CHOP_MARKER: &str = "\x1b[30;47m>\x1b[0m";
const END_MARKER: &str = "\x1b[30;47m(END)\x1b[0m";

impl Command for LessCommand {
  fn name() -> &'static str {
    "less"
  }

  fn summary() -> &'static str {
    "View text with forward and backward paging."
  }

  fn usage() -> &'static str {
    "less [-F] [-X] [-E] [-N] [-S] [+line] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut quit_if_one_screen = false;
    let mut no_init = false;
    let mut quit_at_eof = false;
    let mut start_line = None;
    let mut show_line_numbers = false;
    let mut chop_long_lines = false;
    let mut files = Vec::new();

    for arg in args {
      match arg.as_str() {
        "-F" => quit_if_one_screen = true,
        "-X" => no_init = true,
        "-E" => quit_at_eof = true,
        "-N" => show_line_numbers = true,
        "-S" => chop_long_lines = true,
        _ => {
          if let Some(value) = arg.strip_prefix('+') {
            if !value.is_empty() && start_line.is_none() {
              start_line = Some(parse_start_line(value)?);
              continue;
            }
          }
          files.push(arg.clone());
        }
      }
    }

    Ok(Self {
      quit_if_one_screen,
      no_init,
      quit_at_eof,
      start_line,
      show_line_numbers,
      chop_long_lines,
      files,
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let content = if self.files.is_empty() {
      io_util::read_to_string_fd(ctx.lio(), &ctx.stdin())?
    } else {
      self.files.iter().try_fold(String::new(), |mut acc, path| {
        acc.push_str(&io_util::read_to_string(ctx.lio(), Some(path))?);
        Ok::<_, io::Error>(acc)
      })?
    };
    let label = self.files.first().map(String::as_str);
    page_text_alt(ctx, &content, label, self)
  }
}

struct AltScreenGuard<'a> {
  ctx: &'a AppContext,
  enabled: bool,
}

impl<'a> AltScreenGuard<'a> {
  fn enter(ctx: &'a AppContext, enabled: bool) -> io::Result<Self> {
    if enabled {
      io_util::write_all(ctx.lio(), &ctx.stdout(), b"\x1b[?1049h".to_vec())?;
    }
    Ok(Self { ctx, enabled })
  }
}

impl Drop for AltScreenGuard<'_> {
  fn drop(&mut self) {
    if self.enabled {
      let _ = io_util::write_all(
        self.ctx.lio(),
        &self.ctx.stdout(),
        b"\x1b[?1049l".to_vec(),
      );
    }
  }
}

fn page_text_alt(
  ctx: &AppContext,
  text: &str,
  label: Option<&str>,
  options: &LessCommand,
) -> io::Result<()> {
  let lines = split_lines(text);
  let mut tty = tty_size();
  let page_rows = tty.0.saturating_sub(1).max(1);
  let width = tty.1.max(1);

  if options.quit_if_one_screen
    && rendered_lines_rows_for_less(&lines, width, options) <= page_rows
  {
    let rendered = render_content(&lines, 0, width, 0, options);
    return io_util::write_all(ctx.lio(), &ctx.stdout(), rendered.into_bytes());
  }

  let _guard = AltScreenGuard::enter(ctx, !options.no_init)?;

  let mut show_initial_footer = true;
  let mut horizontal_offset = 0usize;
  let mut max_start =
    last_page_start_for_less(&lines, tty, horizontal_offset, options);
  let mut start = initial_start(options.start_line, max_start);
  let mut first_render = true;
  render_less_page(
    ctx,
    &lines,
    start,
    max_start,
    tty,
    horizontal_offset,
    show_initial_footer,
    label,
    options,
    first_render,
  )?;
  first_render = false;

  loop {
    let new_tty = tty_size();
    if new_tty != tty {
      tty = new_tty;
      max_start =
        last_page_start_for_less(&lines, tty, horizontal_offset, options);
      start = start.min(max_start);
      render_less_page(
        ctx,
        &lines,
        start,
        max_start,
        tty,
        horizontal_offset,
        show_initial_footer,
        label,
        options,
        first_render,
      )?;
    }

    let Some(key) = read_key_from_tty_raw(ctx.lio())? else {
      continue;
    };

    if is_quit_key(&key) {
      if options.no_init {
        io_util::write_all(ctx.lio(), &ctx.stdout(), b"\r\x1b[K".to_vec())?;
      }
      return Ok(());
    }

    if is_scroll_down_key(&key) {
      let next = if start < max_start { start + 1 } else { start };
      if next == start {
        if options.quit_at_eof {
          return Ok(());
        }
        continue;
      }
      start = next;
      show_initial_footer = false;
      render_less_page(
        ctx,
        &lines,
        start,
        max_start,
        tty,
        horizontal_offset,
        show_initial_footer,
        label,
        options,
        first_render,
      )?;
      continue;
    }

    if is_scroll_up_key(&key) {
      let next = start.saturating_sub(1);
      if next == start {
        continue;
      }
      start = next;
      show_initial_footer = false;
      render_less_page(
        ctx,
        &lines,
        start,
        max_start,
        tty,
        horizontal_offset,
        show_initial_footer,
        label,
        options,
        first_render,
      )?;
      continue;
    }

    if is_scroll_right_key(&key) {
      horizontal_offset =
        horizontal_offset.saturating_add(HORIZONTAL_SCROLL_STEP);
      max_start =
        last_page_start_for_less(&lines, tty, horizontal_offset, options);
      start = start.min(max_start);
      show_initial_footer = false;
      render_less_page(
        ctx,
        &lines,
        start,
        max_start,
        tty,
        horizontal_offset,
        show_initial_footer,
        label,
        options,
        first_render,
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
        last_page_start_for_less(&lines, tty, horizontal_offset, options);
      start = start.min(max_start);
      show_initial_footer = false;
      render_less_page(
        ctx,
        &lines,
        start,
        max_start,
        tty,
        horizontal_offset,
        show_initial_footer,
        label,
        options,
        first_render,
      )?;
      continue;
    }
  }
}

fn render_less_page(
  ctx: &AppContext,
  lines: &[&str],
  start: usize,
  max_start: usize,
  tty: (usize, usize),
  horizontal_offset: usize,
  show_initial_footer: bool,
  label: Option<&str>,
  options: &LessCommand,
  first_render: bool,
) -> io::Result<()> {
  let page_rows = tty.0.saturating_sub(1).max(1);
  let width = tty.1.max(1);
  let rendered =
    render_page(lines, start, page_rows, width, horizontal_offset, options);

  io_util::write_all(
    ctx.lio(),
    &ctx.stdout(),
    render_less_frame(
      &rendered,
      show_initial_footer,
      label,
      start >= max_start,
      should_redraw_in_place(options.no_init, first_render),
    )
    .into_bytes(),
  )
}

fn less_footer_text(
  show_initial_footer: bool,
  label: Option<&str>,
  at_end: bool,
) -> String {
  if at_end {
    END_MARKER.to_string()
  } else {
    footer_text(show_initial_footer, label)
  }
}

fn render_less_frame(
  rendered: &str,
  show_initial_footer: bool,
  label: Option<&str>,
  at_end: bool,
  redraw_in_place: bool,
) -> String {
  let footer = less_footer_text(show_initial_footer, label, at_end);
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

fn render_page(
  lines: &[&str],
  start: usize,
  max_rows: usize,
  tty_cols: usize,
  horizontal_offset: usize,
  options: &LessCommand,
) -> String {
  let number_width = line_number_width(lines.len(), options.show_line_numbers);
  let mut out = String::new();
  let mut used_rows = 0usize;

  for (index, line) in lines.iter().enumerate().skip(start.min(lines.len())) {
    let rows = render_line(
      line,
      index,
      tty_cols,
      number_width,
      horizontal_offset,
      options,
    );
    if used_rows + rows.len() > max_rows {
      break;
    }
    for row in rows {
      out.push_str(&row);
      out.push('\n');
      used_rows += 1;
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

fn render_content(
  lines: &[&str],
  start: usize,
  tty_cols: usize,
  horizontal_offset: usize,
  options: &LessCommand,
) -> String {
  let number_width = line_number_width(lines.len(), options.show_line_numbers);
  let mut out = String::new();

  for (index, line) in lines.iter().enumerate().skip(start.min(lines.len())) {
    let rows = render_line(
      line,
      index,
      tty_cols,
      number_width,
      horizontal_offset,
      options,
    );
    for row in rows {
      out.push_str(&row);
      out.push('\n');
    }
  }

  out
}

fn render_line(
  line: &str,
  index: usize,
  tty_cols: usize,
  number_width: usize,
  horizontal_offset: usize,
  options: &LessCommand,
) -> Vec<String> {
  let prefix = if options.show_line_numbers {
    format!("{:>width$} ", index + 1, width = number_width)
  } else {
    String::new()
  };
  let available = tty_cols.saturating_sub(prefix.chars().count()).max(1);
  let text = line.strip_suffix('\n').unwrap_or(line);
  let chars: Vec<char> = text.chars().collect();

  if options.chop_long_lines {
    return vec![render_chopped_line(
      &chars,
      &prefix,
      available,
      horizontal_offset,
    )];
  }

  if chars.is_empty() {
    return vec![prefix];
  }

  let visible = if horizontal_offset >= chars.len() {
    &[][..]
  } else {
    &chars[horizontal_offset..]
  };

  if visible.is_empty() {
    return vec![prefix];
  }

  let mut rows = Vec::new();
  let mut offset = 0usize;
  while offset < visible.len() {
    let end = (offset + available).min(visible.len());
    let body: String = visible[offset..end].iter().collect();
    rows.push(format!("{prefix}{body}"));
    offset = end;
  }
  rows
}

fn render_chopped_line(
  chars: &[char],
  prefix: &str,
  available: usize,
  horizontal_offset: usize,
) -> String {
  if available == 0 {
    return prefix.to_string();
  }

  let remaining = chars.len().saturating_sub(horizontal_offset);
  let right_chopped = remaining > available;

  let mut body_width = available;
  if right_chopped {
    body_width = body_width.saturating_sub(1);
  }

  let body: String =
    chars.iter().skip(horizontal_offset).take(body_width).collect();

  let mut out = String::with_capacity(prefix.len() + body.len() + 16);
  out.push_str(prefix);
  out.push_str(&body);
  if right_chopped {
    out.push_str(CHOP_MARKER);
  }
  out
}

fn line_number_width(line_count: usize, show_line_numbers: bool) -> usize {
  if !show_line_numbers { 0 } else { line_count.max(1).to_string().len() }
}

fn rendered_lines_rows_for_less(
  lines: &[&str],
  tty_cols: usize,
  options: &LessCommand,
) -> usize {
  let number_width = line_number_width(lines.len(), options.show_line_numbers);
  lines
    .iter()
    .enumerate()
    .map(|(index, line)| {
      render_line(line, index, tty_cols, number_width, 0, options).len()
    })
    .sum()
}

fn last_page_start_for_less(
  lines: &[&str],
  tty: (usize, usize),
  horizontal_offset: usize,
  options: &LessCommand,
) -> usize {
  if lines.is_empty() {
    return 0;
  }

  let page_rows = tty.0.saturating_sub(1).max(1);
  let width = tty.1.max(1);
  let number_width = line_number_width(lines.len(), options.show_line_numbers);
  let mut used_rows = 0usize;
  let mut start = lines.len();

  while start > 0 {
    let rows = render_line(
      &lines[start - 1],
      start - 1,
      width,
      number_width,
      horizontal_offset,
      options,
    )
    .len();
    if used_rows + rows > page_rows {
      break;
    }
    start -= 1;
    used_rows += rows;
  }

  start
}

fn parse_start_line(value: &str) -> io::Result<NonZeroUsize> {
  value.parse::<NonZeroUsize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("less: invalid start line '{value}'"),
    )
  })
}

fn initial_start(start_line: Option<NonZeroUsize>, max_start: usize) -> usize {
  start_line
    .map(|line| line.get().saturating_sub(1).min(max_start))
    .unwrap_or(0)
}

fn should_redraw_in_place(no_init: bool, first_render: bool) -> bool {
  !no_init || !first_render
}

fn is_scroll_right_key(key: &str) -> bool {
  matches!(key, "\u{1b}[C")
}

fn is_scroll_left_key(key: &str) -> bool {
  matches!(key, "\u{1b}[D")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_less_accepts_optional_files() {
    let parsed = LessCommand::parse(&["a".into(), "b".into()]).unwrap();
    assert_eq!(parsed.files, vec!["a", "b"]);
    assert!(!parsed.quit_if_one_screen);
    assert!(!parsed.no_init);
    assert!(!parsed.quit_at_eof);
    assert_eq!(parsed.start_line, None);
    assert!(!parsed.show_line_numbers);
    assert!(!parsed.chop_long_lines);
  }

  #[test]
  fn parse_less_supports_flags_and_start_line() {
    let parsed = LessCommand::parse(&[
      "-F".into(),
      "-X".into(),
      "-E".into(),
      "-N".into(),
      "-S".into(),
      "+10".into(),
      "a".into(),
    ])
    .unwrap();
    assert!(parsed.quit_if_one_screen);
    assert!(parsed.no_init);
    assert!(parsed.quit_at_eof);
    assert_eq!(parsed.start_line, NonZeroUsize::new(10));
    assert!(parsed.show_line_numbers);
    assert!(parsed.chop_long_lines);
    assert_eq!(parsed.files, vec!["a"]);
  }

  #[test]
  fn render_line_numbers_prefix_rows() {
    let cmd = LessCommand { show_line_numbers: true, ..LessCommand::default() };
    assert_eq!(render_line("abc\n", 1, 10, 2, 0, &cmd), vec![" 2 abc"]);
  }

  #[test]
  fn render_line_chops_when_s_flag_is_set() {
    let cmd = LessCommand { chop_long_lines: true, ..LessCommand::default() };
    assert_eq!(
      render_line("abcdef\n", 0, 4, 0, 0, &cmd),
      vec![format!("abc{CHOP_MARKER}")]
    );
  }

  #[test]
  fn render_line_wraps_without_s_flag() {
    let cmd = LessCommand::default();
    assert_eq!(render_line("abcdef\n", 0, 4, 0, 0, &cmd), vec!["abcd", "ef"]);
  }

  #[test]
  fn render_line_honors_horizontal_offset_in_both_modes() {
    let cmd = LessCommand { chop_long_lines: true, ..LessCommand::default() };
    assert_eq!(
      render_line("abcdef\n", 0, 4, 0, 2, &cmd),
      vec!["cdef".to_string()]
    );

    let wrapped = LessCommand::default();
    assert_eq!(render_line("abcdef\n", 0, 4, 0, 2, &wrapped), vec!["cdef"]);
  }

  #[test]
  fn render_line_numbers_and_chop_marker_coexist() {
    let cmd = LessCommand {
      show_line_numbers: true,
      chop_long_lines: true,
      ..LessCommand::default()
    };
    assert_eq!(
      render_line("abcdef\n", 1, 8, 2, 0, &cmd),
      vec![format!(" 2 abcd{CHOP_MARKER}")]
    );
  }

  #[test]
  fn horizontal_scroll_keys_match_arrow_keys() {
    assert!(is_scroll_right_key("\u{1b}[C"));
    assert!(is_scroll_left_key("\u{1b}[D"));
    assert!(!is_scroll_right_key("\u{1b}[B"));
    assert!(!is_scroll_left_key("\u{1b}[A"));
  }

  #[test]
  fn horizontal_scroll_step_is_ten_columns_in_both_directions() {
    assert_eq!(HORIZONTAL_SCROLL_STEP, 10);
  }

  #[test]
  fn initial_start_uses_one_based_lines_and_clamps() {
    assert_eq!(initial_start(None, 4), 0);
    assert_eq!(initial_start(NonZeroUsize::new(1), 4), 0);
    assert_eq!(initial_start(NonZeroUsize::new(3), 4), 2);
    assert_eq!(initial_start(NonZeroUsize::new(10), 4), 4);
  }

  #[test]
  fn no_init_only_skips_the_first_clear_home() {
    assert!(!should_redraw_in_place(true, true));
    assert!(should_redraw_in_place(true, false));
    assert!(should_redraw_in_place(false, true));
  }

  #[test]
  fn rendered_lines_rows_for_less_counts_wrapped_rows() {
    let cmd = LessCommand::default();
    let lines = split_lines("abcdef\n12\n");
    assert_eq!(rendered_lines_rows_for_less(&lines, 4, &cmd), 3);
  }

  #[test]
  fn last_page_start_for_less_respects_horizontal_offset_in_wrapped_mode() {
    let cmd = LessCommand::default();
    let lines = split_lines("abcdef\n12\n");
    assert_eq!(last_page_start_for_less(&lines, (3, 4), 0, &cmd), 1);
    assert_eq!(last_page_start_for_less(&lines, (3, 4), 2, &cmd), 0);
  }

  #[test]
  fn render_content_does_not_pad_to_terminal_height() {
    let cmd = LessCommand::default();
    let lines = split_lines("a\nb\n");
    assert_eq!(render_content(&lines, 0, 80, 0, &cmd), "a\nb\n");
  }

  #[test]
  fn footer_shows_end_marker_at_eof() {
    assert_eq!(less_footer_text(true, Some("file"), true), END_MARKER);
    assert_eq!(less_footer_text(false, None, true), END_MARKER);
  }

  #[test]
  fn footer_uses_existing_behavior_when_not_at_eof() {
    assert_eq!(
      less_footer_text(true, Some("file"), false),
      footer_text(true, Some("file"))
    );
    assert_eq!(less_footer_text(false, None, false), ":");
  }
}
