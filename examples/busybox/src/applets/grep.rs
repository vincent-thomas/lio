use std::{fs, io, path::Path};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::{
  app::AppContext,
  applets::rg::{
    CaseMode, ColorMode, ContextSpec, FilenameMode, LineNumberMode, MatchMode,
    OutputSpec, PatternMode, PatternSpec, SearchBinaryMode, SearchConfig,
    SearchEngine, SearchOutcome, SearchPlan, SearchRuntime, SearchSpec,
    SearchTarget, SortKind, SortSpec, TraversalSpec,
  },
  command::Command,
  util::{
    cwd as cwd_util,
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

const GREP_FLAG_SPECS: &[FlagSpec<'static>] = &[
  FlagSpec {
    name: "fixed",
    short: &['F'],
    long: &["fixed-strings"],
    takes_value: false,
  },
  FlagSpec {
    name: "ignore-case",
    short: &['i'],
    long: &["ignore-case"],
    takes_value: false,
  },
  FlagSpec {
    name: "invert",
    short: &['v'],
    long: &["invert-match"],
    takes_value: false,
  },
  FlagSpec {
    name: "word",
    short: &['w'],
    long: &["word-regexp"],
    takes_value: false,
  },
  FlagSpec {
    name: "line",
    short: &['x'],
    long: &["line-regexp"],
    takes_value: false,
  },
  FlagSpec {
    name: "regexp",
    short: &['e'],
    long: &["regexp"],
    takes_value: true,
  },
  FlagSpec { name: "file", short: &['f'], long: &["file"], takes_value: true },
  FlagSpec {
    name: "max-count",
    short: &['m'],
    long: &["max-count"],
    takes_value: true,
  },
  FlagSpec {
    name: "line-number",
    short: &['n'],
    long: &["line-number"],
    takes_value: false,
  },
  FlagSpec {
    name: "with-filename",
    short: &['H'],
    long: &["with-filename"],
    takes_value: false,
  },
  FlagSpec {
    name: "no-filename",
    short: &['h'],
    long: &["no-filename"],
    takes_value: false,
  },
  FlagSpec {
    name: "count",
    short: &['c'],
    long: &["count"],
    takes_value: false,
  },
  FlagSpec {
    name: "files-with-matches",
    short: &['l'],
    long: &["files-with-matches"],
    takes_value: false,
  },
  FlagSpec {
    name: "files-without-match",
    short: &['L'],
    long: &["files-without-match"],
    takes_value: false,
  },
  FlagSpec {
    name: "quiet",
    short: &['q'],
    long: &["quiet", "silent"],
    takes_value: false,
  },
  FlagSpec {
    name: "only-matching",
    short: &['o'],
    long: &["only-matching"],
    takes_value: false,
  },
  FlagSpec {
    name: "after-context",
    short: &['A'],
    long: &["after-context"],
    takes_value: true,
  },
  FlagSpec {
    name: "before-context",
    short: &['B'],
    long: &["before-context"],
    takes_value: true,
  },
  FlagSpec {
    name: "context",
    short: &['C'],
    long: &["context"],
    takes_value: true,
  },
  FlagSpec { name: "text", short: &['a'], long: &["text"], takes_value: false },
  FlagSpec {
    name: "no-messages",
    short: &['s'],
    long: &["no-messages"],
    takes_value: false,
  },
  FlagSpec {
    name: "byte-offset",
    short: &['b'],
    long: &["byte-offset"],
    takes_value: false,
  },
  FlagSpec {
    name: "binary-without-match",
    short: &['I'],
    long: &[],
    takes_value: false,
  },
  FlagSpec {
    name: "recursive",
    short: &['r', 'R'],
    long: &["recursive", "dereference-recursive"],
    takes_value: false,
  },
  FlagSpec {
    name: "extended",
    short: &['E'],
    long: &["extended-regexp"],
    takes_value: false,
  },
  FlagSpec {
    name: "basic",
    short: &['G'],
    long: &["basic-regexp"],
    takes_value: false,
  },
  FlagSpec {
    name: "color",
    short: &[],
    long: &["color", "colour"],
    takes_value: true,
  },
  FlagSpec {
    name: "binary-files",
    short: &[],
    long: &["binary-files"],
    takes_value: true,
  },
  FlagSpec {
    name: "null-data",
    short: &['z'],
    long: &["null-data"],
    takes_value: false,
  },
  FlagSpec { name: "null", short: &['Z'], long: &["null"], takes_value: false },
  FlagSpec {
    name: "include",
    short: &[],
    long: &["include"],
    takes_value: true,
  },
  FlagSpec {
    name: "exclude",
    short: &[],
    long: &["exclude"],
    takes_value: true,
  },
  FlagSpec {
    name: "exclude-dir",
    short: &[],
    long: &["exclude-dir"],
    takes_value: true,
  },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepCommand {
  pub patterns: Vec<String>,
  pub paths: Vec<String>,
  pub fixed_strings: bool,
  pub ignore_case: bool,
  pub invert_match: bool,
  pub word_regexp: bool,
  pub line_regexp: bool,
  pub max_count: Option<usize>,
  pub line_number: bool,
  pub byte_offset: bool,
  pub filename_mode: FilenameMode,
  pub color_mode: ColorMode,
  pub match_mode: MatchMode,
  pub quiet: bool,
  pub only_matching: bool,
  pub context: ContextSpec,
  pub text: bool,
  pub suppress_messages: bool,
  pub binary_mode: SearchBinaryMode,
  pub null_data: bool,
  pub null_path_terminator: bool,
  pub include_globs: Vec<String>,
  pub exclude_globs: Vec<String>,
  pub exclude_dirs: Vec<String>,
  pub recursive: bool,
  pub extended_regexp: bool,
}

impl Command for GrepCommand {
  fn name() -> &'static str {
    "grep"
  }

  fn summary() -> &'static str {
    "Search for lines matching a pattern."
  }

  fn usage() -> &'static str {
    "grep [options] <pattern> [file ...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Self::parse_args(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let runtime = self.runtime_from_context(ctx)?;
    let plan = self.build_plan(&runtime.cwd)?;
    let outcomes = SearchEngine::default().search_plan(&plan, &runtime)?;
    let output = self.render_outcomes(&outcomes);
    io_util::write_all(ctx.lio(), &ctx.stdout(), output)?;

    if self.matched_anything(&outcomes) {
      Ok(())
    } else {
      Err(crate::exit_with_status(1))
    }
  }
}

impl GrepCommand {
  pub fn parse_args(args: &[String]) -> io::Result<Self> {
    let parsed = FlagParser::new("grep", GREP_FLAG_SPECS).parse(args)?;
    let mut patterns = parsed.get_flag_values("regexp").to_vec();
    for path in parsed.get_flag_values("file") {
      patterns.extend(read_pattern_file(path)?);
    }

    let positionals = parsed.positional();
    let paths = if patterns.is_empty() {
      let Some(pattern) = positionals.first() else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "grep: missing search pattern",
        ));
      };
      patterns.push(pattern.clone());
      positionals[1..].to_vec()
    } else {
      positionals.to_vec()
    };

    if patterns.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "grep: missing search pattern",
      ));
    }

    let mut context = ContextSpec::default();
    if let Some(value) = parsed.get_flag_value("before-context") {
      context.before = parse_usize_flag("grep", "--before-context", value)?;
    }
    if let Some(value) = parsed.get_flag_value("after-context") {
      context.after = parse_usize_flag("grep", "--after-context", value)?;
    }
    if let Some(value) = parsed.get_flag_value("context") {
      let amount = parse_usize_flag("grep", "--context", value)?;
      context.before = amount;
      context.after = amount;
    }

    let max_count = parsed
      .get_flag_value("max-count")
      .map(|v| parse_usize_flag("grep", "--max-count", v))
      .transpose()?;

    let color_mode = match parsed.get_flag_value("color") {
      Some(value) => parse_color_mode(value)?,
      None => ColorMode::Never,
    };

    let binary_mode = if parsed.get_flag_exists("text") {
      SearchBinaryMode::Text
    } else if parsed.get_flag_exists("binary-without-match") {
      SearchBinaryMode::Skip
    } else if let Some(value) = parsed.get_flag_value("binary-files") {
      parse_binary_mode(value)?
    } else {
      SearchBinaryMode::Report
    };

    let extended_regexp = grep_uses_extended_regexp(args);

    let filename_mode = if parsed.get_flag_exists("no-filename") {
      FilenameMode::Never
    } else if parsed.get_flag_exists("with-filename") {
      FilenameMode::Always
    } else {
      FilenameMode::Auto
    };

    let match_mode = if parsed.get_flag_exists("files-without-match") {
      MatchMode::FilesWithoutMatch
    } else if parsed.get_flag_exists("files-with-matches") {
      MatchMode::FilesWithMatches
    } else if parsed.get_flag_exists("count") {
      MatchMode::Count
    } else {
      MatchMode::Standard
    };

    Ok(Self {
      patterns,
      paths,
      fixed_strings: parsed.get_flag_exists("fixed"),
      ignore_case: parsed.get_flag_exists("ignore-case"),
      invert_match: parsed.get_flag_exists("invert"),
      word_regexp: parsed.get_flag_exists("word"),
      line_regexp: parsed.get_flag_exists("line"),
      max_count,
      line_number: parsed.get_flag_exists("line-number"),
      byte_offset: parsed.get_flag_exists("byte-offset"),
      filename_mode,
      color_mode,
      match_mode,
      quiet: parsed.get_flag_exists("quiet"),
      only_matching: parsed.get_flag_exists("only-matching"),
      context,
      text: parsed.get_flag_exists("text"),
      suppress_messages: parsed.get_flag_exists("no-messages"),
      binary_mode,
      null_data: parsed.get_flag_exists("null-data"),
      null_path_terminator: parsed.get_flag_exists("null"),
      include_globs: parsed.get_flag_values("include").to_vec(),
      exclude_globs: parsed.get_flag_values("exclude").to_vec(),
      exclude_dirs: parsed.get_flag_values("exclude-dir").to_vec(),
      recursive: parsed.get_flag_exists("recursive"),
      extended_regexp,
    })
  }

  fn runtime_from_context(
    &self,
    ctx: &AppContext,
  ) -> io::Result<SearchRuntime> {
    let read_stdin = self.paths.is_empty();
    Ok(SearchRuntime {
      cwd: cwd_util::current_working_directory(ctx)?,
      stdin: if read_stdin {
        Some(io_util::read_to_bytes_fd(ctx.lio(), &ctx.stdin())?)
      } else {
        None
      },
      stdin_is_tty: !read_stdin,
      stdout_is_tty: stdout_is_tty(ctx),
    })
  }

  fn build_plan(&self, cwd: &Path) -> io::Result<SearchPlan> {
    if !self.recursive {
      for path in &self.paths {
        if fs::metadata(cwd.join(path))
          .map(|meta| meta.is_dir())
          .unwrap_or(false)
        {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("grep: {path}: is a directory"),
          ));
        }
      }
    }

    let show_paths = match self.filename_mode {
      FilenameMode::Always => true,
      FilenameMode::Never => false,
      FilenameMode::Auto => self.paths.len() > 1 || self.recursive,
    };

    let config = SearchConfig {
      pattern_spec: PatternSpec {
        text: self.patterns.first().cloned().unwrap_or_default(),
        patterns: self
          .patterns
          .iter()
          .map(|pattern| {
            if self.fixed_strings || self.extended_regexp {
              pattern.clone()
            } else {
              basic_regex_to_extended(pattern)
            }
          })
          .collect(),
        mode: if self.fixed_strings {
          PatternMode::FixedStrings
        } else {
          PatternMode::Regex
        },
        case_mode: if self.ignore_case {
          CaseMode::Ignore
        } else {
          CaseMode::Sensitive
        },
        word_regexp: self.word_regexp,
        line_regexp: self.line_regexp,
      },
      traversal: TraversalSpec {
        hidden: false,
        no_ignore: false,
        paths: self.paths.clone(),
        globs: Vec::new(),
        include_globs: self.include_globs.clone(),
        exclude_globs: self.exclude_globs.clone(),
        exclude_dirs: self.exclude_dirs.clone(),
      },
      search: SearchSpec {
        invert_match: self.invert_match,
        max_count: self.max_count,
        threads: None,
        text: self.text,
        suppress_errors: self.suppress_messages,
        binary_mode: self.binary_mode,
        null_data: self.null_data,
        files_mode: false,
        stats: false,
        quiet: false,
        passthru: false,
      },
      output: OutputSpec {
        filename_mode: if show_paths {
          FilenameMode::Always
        } else {
          FilenameMode::Never
        },
        color_mode: self.color_mode,
        line_number_mode: if self.line_number {
          LineNumberMode::Always
        } else {
          LineNumberMode::Never
        },
        json: false,
        print0: false,
        null_path_terminator: self.null_path_terminator,
        only_matching: self.only_matching,
        include_zero: self.match_mode == MatchMode::Count,
        vimgrep: false,
      },
      context: self.context,
      sort: SortSpec { kind: SortKind::None, reverse: false },
      match_mode: self.match_mode,
    };

    let targets = if self.paths.is_empty() {
      vec![SearchTarget::Stdin]
    } else {
      self.paths.iter().cloned().map(SearchTarget::File).collect()
    };

    Ok(SearchPlan::new(config, targets))
  }

  fn matched_anything(&self, outcomes: &[SearchOutcome]) -> bool {
    match self.match_mode {
      MatchMode::Standard => outcomes.iter().any(|outcome| {
        matches!(
          outcome,
          SearchOutcome::MatchedLine(_) | SearchOutcome::BinaryMatch { .. }
        )
      }),
      MatchMode::Count => outcomes.iter().any(|outcome| match outcome {
        SearchOutcome::Count { count, .. } => *count > 0,
        _ => false,
      }),
      MatchMode::CountMatches => outcomes.iter().any(|outcome| match outcome {
        SearchOutcome::Count { count, .. } => *count > 0,
        _ => false,
      }),
      MatchMode::FilesWithMatches => outcomes
        .iter()
        .any(|outcome| matches!(outcome, SearchOutcome::FileMatch(_))),
      MatchMode::FilesWithoutMatch => outcomes
        .iter()
        .any(|outcome| matches!(outcome, SearchOutcome::FileWithoutMatch(_))),
    }
  }

  fn render_outcomes(&self, outcomes: &[SearchOutcome]) -> Vec<u8> {
    if self.quiet {
      return Vec::new();
    }

    let color_enabled = matches!(self.color_mode, ColorMode::Always);
    let mut out = Vec::new();
    for outcome in outcomes {
      match outcome {
        SearchOutcome::MatchedLine(record) => {
          push_grep_prefix(
            &mut out,
            record.path.as_deref(),
            self.line_number.then_some(record.line_number),
            self.byte_offset.then_some(record.absolute_offset),
            b':',
          );
          push_grep_line(
            &mut out,
            &record.line,
            &record.spans,
            color_enabled,
            self.null_data,
          );
        }
        SearchOutcome::BinaryMatch { path } => {
          out.extend_from_slice(b"Binary file ");
          out.extend_from_slice(
            path.as_deref().unwrap_or("(standard input)").as_bytes(),
          );
          out.extend_from_slice(b" matches\n");
        }
        SearchOutcome::ContextLine(record) => {
          push_grep_prefix(
            &mut out,
            record.path.as_deref(),
            self.line_number.then_some(record.line_number),
            self.byte_offset.then_some(record.absolute_offset),
            b'-',
          );
          push_line_bytes(&mut out, &record.line, self.null_data);
        }
        SearchOutcome::ContextSeparator => out.extend_from_slice(b"--\n"),
        SearchOutcome::Count { path, count } => {
          if let Some(path) = path {
            out.extend_from_slice(path.as_bytes());
            out.push(b':');
          }
          out.extend_from_slice(count.to_string().as_bytes());
          out.push(b'\n');
        }
        SearchOutcome::FileMatch(path)
        | SearchOutcome::FileWithoutMatch(path) => {
          out.extend_from_slice(path.as_bytes());
          out.push(if self.null_path_terminator { b'\0' } else { b'\n' });
        }
        SearchOutcome::JsonBegin { .. } | SearchOutcome::JsonEnd { .. } => {}
      }
    }

    out
  }
}

fn push_grep_prefix(
  out: &mut Vec<u8>,
  path: Option<&str>,
  line_number: Option<usize>,
  byte_offset: Option<usize>,
  separator: u8,
) {
  if let Some(path) = path {
    out.extend_from_slice(path.as_bytes());
    out.push(separator);
  }
  if let Some(line_number) = line_number {
    out.extend_from_slice(line_number.to_string().as_bytes());
    out.push(separator);
  }
  if let Some(byte_offset) = byte_offset {
    out.extend_from_slice(byte_offset.to_string().as_bytes());
    out.push(separator);
  }
}

fn push_line_bytes(out: &mut Vec<u8>, line: &[u8], null_data: bool) {
  out.extend_from_slice(line);
  let terminator = if null_data { b'\0' } else { b'\n' };
  if !line.ends_with(&[terminator]) {
    out.push(terminator);
  }
}

fn push_grep_line(
  out: &mut Vec<u8>,
  line: &[u8],
  spans: &[crate::applets::rg::MatchSpan],
  color_enabled: bool,
  null_data: bool,
) {
  const ANSI_MATCH_START: &[u8] = b"\x1b[01;31m\x1b[K";
  const ANSI_MATCH_END: &[u8] = b"\x1b[m\x1b[K";

  if !color_enabled || spans.is_empty() {
    push_line_bytes(out, line, null_data);
    return;
  }

  let mut cursor = 0usize;
  for span in spans {
    if cursor < span.start {
      out.extend_from_slice(&line[cursor..span.start]);
    }
    out.extend_from_slice(ANSI_MATCH_START);
    out.extend_from_slice(&line[span.start..span.end]);
    out.extend_from_slice(ANSI_MATCH_END);
    cursor = span.end;
  }
  if cursor < line.len() {
    out.extend_from_slice(&line[cursor..]);
  }
  let terminator = if null_data { b'\0' } else { b'\n' };
  if !line.ends_with(&[terminator]) {
    out.push(terminator);
  }
}

fn read_pattern_file(path: &str) -> io::Result<Vec<String>> {
  Ok(
    fs::read_to_string(path)?
      .split_terminator('\n')
      .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
      .collect(),
  )
}

fn parse_color_mode(value: &str) -> io::Result<ColorMode> {
  match value {
    "always" => Ok(ColorMode::Always),
    "auto" => Ok(ColorMode::Auto),
    "never" => Ok(ColorMode::Never),
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("grep: unsupported color mode {value}"),
    )),
  }
}

fn parse_binary_mode(value: &str) -> io::Result<SearchBinaryMode> {
  match value {
    "binary" => Ok(SearchBinaryMode::Report),
    "text" => Ok(SearchBinaryMode::Text),
    "without-match" => Ok(SearchBinaryMode::Skip),
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("grep: unsupported binary mode {value}"),
    )),
  }
}

fn grep_uses_extended_regexp(args: &[String]) -> bool {
  let mut extended = false;
  for arg in args {
    if arg == "-E" || arg == "--extended-regexp" {
      extended = true;
    } else if arg == "-G" || arg == "--basic-regexp" {
      extended = false;
    }
  }
  extended
}

fn basic_regex_to_extended(pattern: &str) -> String {
  let mut out = String::new();
  let mut chars = pattern.chars().peekable();
  while let Some(ch) = chars.next() {
    if ch == '\\' {
      if let Some(next) = chars.next() {
        if matches!(next, '+' | '?' | '(' | ')' | '|' | '{' | '}') {
          out.push(next);
        } else {
          out.push('\\');
          out.push(next);
        }
      } else {
        out.push('\\');
      }
      continue;
    }

    if matches!(ch, '+' | '?' | '(' | ')' | '|' | '{' | '}') {
      out.push('\\');
    }
    out.push(ch);
  }
  out
}

fn parse_usize_flag(
  applet: &str,
  flag: &str,
  value: &str,
) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("{applet}: {flag} requires a non-negative integer"),
    )
  })
}

fn stdout_is_tty(ctx: &AppContext) -> bool {
  #[cfg(unix)]
  {
    let stdout = ctx.stdout();
    unsafe { libc::isatty(stdout.as_raw_fd()) == 1 }
  }

  #[cfg(not(unix))]
  {
    let _ = ctx;
    false
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
  };

  struct TempDir {
    path: PathBuf,
  }

  impl TempDir {
    fn new(prefix: &str) -> Self {
      let unique =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
      let path =
        std::env::temp_dir().join(format!("busybox-grep-{prefix}-{unique}"));
      fs::create_dir_all(&path).unwrap();
      Self { path }
    }

    fn path(&self) -> &Path {
      &self.path
    }

    fn write(&self, relative: &str, contents: &[u8]) {
      let path = self.path.join(relative);
      if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
      }
      fs::write(path, contents).unwrap();
    }
  }

  impl Drop for TempDir {
    fn drop(&mut self) {
      let _ = fs::remove_dir_all(&self.path);
    }
  }

  fn runtime(cwd: &Path) -> SearchRuntime {
    SearchRuntime {
      cwd: cwd.to_path_buf(),
      stdin: None,
      stdin_is_tty: true,
      stdout_is_tty: false,
    }
  }

  fn stdin_runtime(cwd: &Path, input: &[u8]) -> SearchRuntime {
    SearchRuntime {
      cwd: cwd.to_path_buf(),
      stdin: Some(input.to_vec()),
      stdin_is_tty: false,
      stdout_is_tty: false,
    }
  }

  fn run_grep(
    cwd: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
  ) -> (Vec<u8>, bool) {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let command = GrepCommand::parse(&args).unwrap();
    let runtime = match stdin {
      Some(input) => stdin_runtime(cwd, input),
      None => runtime(cwd),
    };
    let plan = command.build_plan(cwd).unwrap();
    let outcomes =
      SearchEngine::default().search_plan(&plan, &runtime).unwrap();
    (command.render_outcomes(&outcomes), command.matched_anything(&outcomes))
  }

  #[test]
  fn parse_grep_collects_patterns_and_paths() {
    let cmd = GrepCommand::parse(&[
      "-in".into(),
      "-e".into(),
      "foo".into(),
      "bar".into(),
      "file.txt".into(),
    ])
    .unwrap();
    assert_eq!(cmd.patterns, vec!["foo"]);
    assert_eq!(cmd.paths, vec!["bar", "file.txt"]);
    assert!(cmd.ignore_case);
    assert!(cmd.line_number);
  }

  #[test]
  fn parse_grep_uses_first_positional_as_pattern_without_e() {
    let cmd =
      GrepCommand::parse(&["needle".into(), "a.txt".into(), "b.txt".into()])
        .unwrap();
    assert_eq!(cmd.patterns, vec!["needle"]);
    assert_eq!(cmd.paths, vec!["a.txt", "b.txt"]);
  }

  #[test]
  fn behavior_grep_reads_stdin_when_no_paths_are_given() {
    let (output, matched) =
      run_grep(Path::new("."), &["foo"], Some(b"foo\nbar\n"));
    assert_eq!(String::from_utf8(output).unwrap(), "foo\n");
    assert!(matched);
  }

  #[test]
  fn behavior_grep_renders_filename_and_line_number_prefixes() {
    let dir = TempDir::new("prefixes");
    dir.write("a.txt", b"foo\nbar\n");
    dir.write("b.txt", b"foo\n");

    let (output, matched) =
      run_grep(dir.path(), &["-n", "foo", "a.txt", "b.txt"], None);
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "a.txt:1:foo\nb.txt:1:foo\n"
    );
    assert!(matched);
  }

  #[test]
  fn behavior_grep_count_mode_prints_zero_for_files_without_matches() {
    let dir = TempDir::new("count");
    dir.write("a.txt", b"foo\n");
    dir.write("b.txt", b"bar\n");

    let (output, matched) =
      run_grep(dir.path(), &["-c", "foo", "a.txt", "b.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "a.txt:1\nb.txt:0\n");
    assert!(matched);
  }

  #[test]
  fn behavior_grep_only_matching_uses_custom_renderer() {
    let dir = TempDir::new("only-matching");
    dir.write("a.txt", b"foo food\n");

    let (output, matched) = run_grep(dir.path(), &["-o", "foo", "a.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "foo\nfoo\n");
    assert!(matched);
  }

  #[test]
  fn behavior_grep_context_uses_hyphen_delimiters() {
    let dir = TempDir::new("context");
    dir.write("a.txt", b"one\ntwo\nthree\n");

    let (output, matched) =
      run_grep(dir.path(), &["-n", "-C", "1", "two", "a.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "1-one\n2:two\n3-three\n");
    assert!(matched);
  }

  #[test]
  fn behavior_grep_files_with_and_without_matches_work() {
    let dir = TempDir::new("files");
    dir.write("a.txt", b"foo\n");
    dir.write("b.txt", b"bar\n");

    let (with_output, with_match) =
      run_grep(dir.path(), &["-l", "foo", "a.txt", "b.txt"], None);
    assert_eq!(String::from_utf8(with_output).unwrap(), "a.txt\n");
    assert!(with_match);

    let (without_output, without_match) =
      run_grep(dir.path(), &["-L", "foo", "a.txt", "b.txt"], None);
    assert_eq!(String::from_utf8(without_output).unwrap(), "b.txt\n");
    assert!(without_match);
  }

  #[test]
  fn behavior_grep_recursive_uses_search_walker() {
    let dir = TempDir::new("recursive");
    dir.write("nested/a.txt", b"foo\n");
    dir.write("nested/b.txt", b"bar\n");

    let (output, matched) =
      run_grep(dir.path(), &["-r", "foo", "nested"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "nested/a.txt:foo\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_multiple_e_patterns_are_alternatives() {
    let dir = TempDir::new("multi-e");
    dir.write("a.txt", b"foo\nbar\nbaz\n");

    let (output, matched) =
      run_grep(dir.path(), &["-e", "foo", "-e", "baz", "a.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "foo\nbaz\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_pattern_file_adds_to_e_patterns() {
    let dir = TempDir::new("pattern-file");
    dir.write("patterns.txt", b"bar\nqux\r\n");
    dir.write("a.txt", b"foo\nbar\nqux\nzap\n");

    let patterns = dir.path().join("patterns.txt");
    let (output, matched) = run_grep(
      dir.path(),
      &["-e", "foo", "-f", patterns.to_str().unwrap(), "a.txt"],
      None,
    );
    assert_eq!(String::from_utf8(output).unwrap(), "foo\nbar\nqux\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_quiet_suppresses_output_but_preserves_match_result() {
    let dir = TempDir::new("quiet");
    dir.write("a.txt", b"foo\nbar\n");

    let (output, matched) = run_grep(dir.path(), &["-q", "foo", "a.txt"], None);
    assert!(output.is_empty());
    assert!(matched);

    let (output, matched) = run_grep(dir.path(), &["-q", "zzz", "a.txt"], None);
    assert!(output.is_empty());
    assert!(!matched);
  }

  #[test]
  fn gnu_grep_word_and_line_regexp_behave_like_grep() {
    let dir = TempDir::new("word-line");
    dir.write("a.txt", b"cat\nscat\ncatfish\n");

    let (word_output, word_matched) =
      run_grep(dir.path(), &["-w", "cat", "a.txt"], None);
    assert_eq!(String::from_utf8(word_output).unwrap(), "cat\n");
    assert!(word_matched);

    let (line_output, line_matched) =
      run_grep(dir.path(), &["-x", "cat", "a.txt"], None);
    assert_eq!(String::from_utf8(line_output).unwrap(), "cat\n");
    assert!(line_matched);
  }

  #[test]
  fn gnu_grep_recursive_single_directory_still_prefixes_filenames() {
    let dir = TempDir::new("recursive-prefix");
    dir.write("nested/a.txt", b"foo\n");

    let (output, matched) =
      run_grep(dir.path(), &["-r", "foo", "nested"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "nested/a.txt:foo\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_no_messages_ignores_missing_files() {
    let dir = TempDir::new("no-messages");
    dir.write("a.txt", b"foo\n");

    let (output, matched) =
      run_grep(dir.path(), &["-s", "foo", "missing.txt", "a.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "a.txt:foo\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_include_exclude_and_exclude_dir_filter_recursive_search() {
    let dir = TempDir::new("filters");
    dir.write("src/keep.txt", b"foo\n");
    dir.write("src/skip.log", b"foo\n");
    dir.write("vendor/lib.txt", b"foo\n");

    let (output, matched) = run_grep(
      dir.path(),
      &[
        "-r",
        "--include=*.txt",
        "--exclude=skip*",
        "--exclude-dir=vendor",
        "foo",
        ".",
      ],
      None,
    );
    assert_eq!(String::from_utf8(output).unwrap(), "./src/keep.txt:foo\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_byte_offset_prints_zero_based_offset() {
    let dir = TempDir::new("byte-offset");
    dir.write("a.txt", b"aa\nfoo\n");

    let (output, matched) =
      run_grep(dir.path(), &["-b", "-n", "foo", "a.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "2:3:foo\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_color_always_highlights_matches() {
    let dir = TempDir::new("color");
    dir.write("a.txt", b"foo\n");

    let (output, matched) =
      run_grep(dir.path(), &["--color=always", "foo", "a.txt"], None);
    assert_eq!(output, b"\x1b[01;31m\x1b[Kfoo\x1b[m\x1b[K\n".to_vec());
    assert!(matched);
  }

  #[test]
  fn gnu_grep_binary_modes_report_skip_and_text() {
    let dir = TempDir::new("binary-modes");
    dir.write("bin.dat", b"\0foo\0");

    let (report_output, report_matched) =
      run_grep(dir.path(), &["foo", "bin.dat"], None);
    assert_eq!(
      String::from_utf8(report_output).unwrap(),
      "Binary file bin.dat matches\n"
    );
    assert!(report_matched);

    let (skip_output, skip_matched) = run_grep(
      dir.path(),
      &["--binary-files=without-match", "foo", "bin.dat"],
      None,
    );
    assert!(skip_output.is_empty());
    assert!(!skip_matched);

    let (text_output, text_matched) =
      run_grep(dir.path(), &["-a", "foo", "bin.dat"], None);
    assert_eq!(String::from_utf8(text_output).unwrap(), "\0foo\0\n");
    assert!(text_matched);
  }

  #[test]
  fn gnu_grep_basic_and_extended_regex_modes_differ() {
    let dir = TempDir::new("regex-modes");
    dir.write("a.txt", b"ab\nb\n");

    let (basic_output, basic_matched) =
      run_grep(dir.path(), &["a+b", "a.txt"], None);
    assert!(basic_output.is_empty());
    assert!(!basic_matched);

    let (extended_output, extended_matched) =
      run_grep(dir.path(), &["-E", "a+b", "a.txt"], None);
    assert_eq!(String::from_utf8(extended_output).unwrap(), "ab\n");
    assert!(extended_matched);

    let (escaped_basic_output, escaped_basic_matched) =
      run_grep(dir.path(), &["a\\+b", "a.txt"], None);
    assert_eq!(String::from_utf8(escaped_basic_output).unwrap(), "ab\n");
    assert!(escaped_basic_matched);
  }

  #[test]
  fn gnu_grep_null_data_uses_nul_terminated_records() {
    let (output, matched) =
      run_grep(Path::new("."), &["-z", "bar"], Some(b"foo\0bar\0baz\0"));
    assert_eq!(output, b"bar\0".to_vec());
    assert!(matched);
  }

  #[test]
  fn gnu_grep_null_output_uses_nul_for_file_lists() {
    let dir = TempDir::new("null-output");
    dir.write("a.txt", b"foo\n");
    dir.write("b.txt", b"bar\n");

    let (output, matched) =
      run_grep(dir.path(), &["-l", "-Z", "foo", "a.txt", "b.txt"], None);
    assert_eq!(output, b"a.txt\0".to_vec());
    assert!(matched);
  }

  #[test]
  fn gnu_grep_only_matching_with_byte_offset_reports_match_offsets() {
    let dir = TempDir::new("only-matching-byte");
    dir.write("a.txt", b"aafoofoo\n");

    let (output, matched) =
      run_grep(dir.path(), &["-b", "-o", "foo", "a.txt"], None);
    assert_eq!(String::from_utf8(output).unwrap(), "2:foo\n5:foo\n");
    assert!(matched);
  }

  #[test]
  fn gnu_grep_only_matching_with_null_data_keeps_nul_records() {
    let (output, matched) =
      run_grep(Path::new("."), &["-z", "-o", "foo"], Some(b"foofoo\0"));
    assert_eq!(output, b"foo\0foo\0".to_vec());
    assert!(matched);
  }
}
