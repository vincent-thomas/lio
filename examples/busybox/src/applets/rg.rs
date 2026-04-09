use std::io;
use std::path::PathBuf;

mod command;
mod glob;
mod matcher;
mod outcome;
mod plan;
mod render;
mod runtime;
mod search;
mod util;
mod walker;
mod worker;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgCommand {
  pub pattern: String,
  pub paths: Vec<String>,
  pub globs: Vec<String>,
  pub traversal: TraversalSpec,
  pub pattern_spec: PatternSpec,
  pub search: SearchSpec,
  pub output: OutputSpec,
  pub context: ContextSpec,
  pub sort: SortSpec,
  pub match_mode: MatchMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchConfig {
  pub pattern_spec: PatternSpec,
  pub traversal: TraversalSpec,
  pub search: SearchSpec,
  pub output: OutputSpec,
  pub context: ContextSpec,
  pub sort: SortSpec,
  pub match_mode: MatchMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternSpec {
  pub text: String,
  pub patterns: Vec<String>,
  pub mode: PatternMode,
  pub case_mode: CaseMode,
  pub word_regexp: bool,
  pub line_regexp: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSpec {
  pub invert_match: bool,
  pub max_count: Option<usize>,
  pub threads: Option<usize>,
  pub text: bool,
  pub suppress_errors: bool,
  pub binary_mode: SearchBinaryMode,
  pub null_data: bool,
  pub files_mode: bool,
  pub stats: bool,
  pub quiet: bool,
  pub passthru: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternMode {
  Regex,
  FixedStrings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseMode {
  Sensitive,
  Ignore,
  Smart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraversalSpec {
  pub hidden: bool,
  pub no_ignore: bool,
  pub paths: Vec<String>,
  pub globs: Vec<String>,
  pub include_globs: Vec<String>,
  pub exclude_globs: Vec<String>,
  pub exclude_dirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputSpec {
  pub filename_mode: FilenameMode,
  pub color_mode: ColorMode,
  pub line_number_mode: LineNumberMode,
  pub json: bool,
  pub print0: bool,
  pub null_path_terminator: bool,
  pub only_matching: bool,
  pub include_zero: bool,
  pub vimgrep: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextSpec {
  pub before: usize,
  pub after: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
  pub kind: SortKind,
  pub reverse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKind {
  None,
  Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilenameMode {
  Auto,
  Always,
  Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
  Auto,
  Always,
  Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineNumberMode {
  Auto,
  Always,
  Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
  Standard,
  Count,
  CountMatches,
  FilesWithMatches,
  FilesWithoutMatch,
}

impl MatchMode {
  pub(crate) fn effective(
    output: &OutputSpec,
    configured_mode: MatchMode,
  ) -> MatchMode {
    if output.json {
      MatchMode::Standard
    } else if output.only_matching && configured_mode == MatchMode::Count {
      MatchMode::CountMatches
    } else {
      configured_mode
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchBinaryMode {
  Skip,
  Text,
  Report,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchTarget {
  Stdin,
  File(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPlan {
  pub config: SearchConfig,
  pub targets: Vec<SearchTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRuntime {
  pub cwd: PathBuf,
  pub stdin: Option<Vec<u8>>,
  pub stdin_is_tty: bool,
  pub stdout_is_tty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchRecord {
  pub path: Option<String>,
  pub line_number: usize,
  pub absolute_offset: usize,
  pub line: Vec<u8>,
  pub spans: Vec<MatchSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSpan {
  pub start: usize,
  pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchOutcome {
  JsonBegin {
    path: Option<String>,
  },
  MatchedLine(MatchRecord),
  BinaryMatch {
    path: Option<String>,
  },
  ContextLine(MatchRecord),
  ContextSeparator,
  Count {
    path: Option<String>,
    count: usize,
  },
  FileMatch(String),
  FileWithoutMatch(String),
  JsonEnd {
    path: Option<String>,
    bytes_searched: usize,
    matches: usize,
    matched_lines: usize,
    has_match: bool,
    elapsed: std::time::Duration,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationSpec {
  pub color_enabled: bool,
  pub path_terminator: u8,
  pub heading_mode: bool,
  pub default_line_number: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SearchStats {
  matches: usize,
  matched_lines: usize,
  files_with_matches: usize,
  files_searched: usize,
  bytes_searched: usize,
}

#[derive(Debug, Default)]
pub struct SearchEngine;

#[derive(Debug, Default)]
pub struct SearchResultEmitter {
  outcomes: Vec<SearchOutcome>,
}

pub trait SearchOutcomeSink {
  fn emit_outcome(&mut self, outcome: SearchOutcome) -> io::Result<()>;

  fn emit_match_line(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
    spans: &[MatchSpan],
  ) -> io::Result<()> {
    self.emit_match(MatchRecord::new_with_offset(
      path.map(str::to_owned),
      line_number,
      absolute_offset,
      line.to_vec(),
      spans.to_vec(),
    )?)
  }

  fn emit_plain_match(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
  ) -> io::Result<()> {
    self.emit_match(MatchRecord::new_with_offset(
      path.map(str::to_owned),
      line_number,
      absolute_offset,
      line.to_vec(),
      Vec::new(),
    )?)
  }

  fn emit_match(&mut self, record: MatchRecord) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::matched_line(record))
  }

  fn emit_binary_match(&mut self, path: Option<String>) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::binary_match(path))
  }

  fn emit_json_begin(&mut self, path: Option<String>) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::json_begin(path))
  }

  fn emit_context(&mut self, record: MatchRecord) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::context_line(record))
  }

  fn emit_context_line(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
  ) -> io::Result<()> {
    self.emit_context(MatchRecord::new_with_offset(
      path.map(str::to_owned),
      line_number,
      absolute_offset,
      line.to_vec(),
      Vec::new(),
    )?)
  }

  fn emit_context_separator(&mut self) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::context_separator())
  }

  fn emit_count(
    &mut self,
    path: Option<String>,
    count: usize,
  ) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::count(path, count))
  }

  fn emit_file_match(&mut self, path: String) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::file_match(path))
  }

  fn emit_file_without_match(&mut self, path: String) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::file_without_match(path))
  }

  fn emit_json_end(
    &mut self,
    path: Option<String>,
    bytes_searched: usize,
    matches: usize,
    matched_lines: usize,
    has_match: bool,
    elapsed: std::time::Duration,
  ) -> io::Result<()> {
    self.emit_outcome(SearchOutcome::json_end(
      path,
      bytes_searched,
      matches,
      matched_lines,
      has_match,
      elapsed,
    ))
  }

  fn flush_file(&mut self) -> io::Result<()> {
    Ok(())
  }
}

#[cfg(test)]
fn render_outcomes(
  outcomes: &[SearchOutcome],
  command: &RgCommand,
  presentation: PresentationSpec,
) -> Vec<u8> {
  let plan = SearchPlan::from_command(command);
  presentation.render_plan_output(
    &plan,
    outcomes,
    SearchStats::default(),
    std::time::Duration::ZERO,
    std::time::Duration::ZERO,
  )
}

#[cfg(test)]
mod tests;
