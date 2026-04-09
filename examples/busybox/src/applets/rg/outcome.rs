use std::io;

use super::*;

impl MatchSpan {
  pub fn new(start: usize, end: usize) -> io::Result<Self> {
    if start >= end {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("rg: invalid match span {start}..{end}"),
      ));
    }

    Ok(Self { start, end })
  }
}

impl MatchRecord {
  pub fn new(
    path: Option<String>,
    line_number: usize,
    line: Vec<u8>,
    spans: Vec<MatchSpan>,
  ) -> io::Result<Self> {
    Self::new_with_offset(path, line_number, 0, line, spans)
  }

  pub fn new_with_offset(
    path: Option<String>,
    line_number: usize,
    absolute_offset: usize,
    line: Vec<u8>,
    spans: Vec<MatchSpan>,
  ) -> io::Result<Self> {
    if line_number == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rg: line numbers are 1-based",
      ));
    }

    let line_len = line.len();
    let mut previous_end = 0usize;
    for span in &spans {
      if span.end > line_len {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("rg: match span ends past line length: {}", span.end),
        ));
      }
      if span.start < previous_end {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "rg: match spans must be sorted and non-overlapping",
        ));
      }
      previous_end = span.end;
    }

    Ok(Self { path, line_number, absolute_offset, line, spans })
  }
}

impl SearchOutcome {
  pub fn json_begin(path: Option<String>) -> Self {
    Self::JsonBegin { path }
  }

  pub fn matched_line(record: MatchRecord) -> Self {
    Self::MatchedLine(record)
  }

  pub fn binary_match(path: Option<String>) -> Self {
    Self::BinaryMatch { path }
  }

  pub fn context_line(record: MatchRecord) -> Self {
    Self::ContextLine(record)
  }

  pub fn context_separator() -> Self {
    Self::ContextSeparator
  }

  pub fn count(path: Option<String>, count: usize) -> Self {
    Self::Count { path, count }
  }

  pub fn file_match(path: impl Into<String>) -> Self {
    Self::FileMatch(path.into())
  }

  pub fn file_without_match(path: impl Into<String>) -> Self {
    Self::FileWithoutMatch(path.into())
  }

  pub fn json_end(
    path: Option<String>,
    bytes_searched: usize,
    matches: usize,
    matched_lines: usize,
    has_match: bool,
    elapsed: std::time::Duration,
  ) -> Self {
    Self::JsonEnd {
      path,
      bytes_searched,
      matches,
      matched_lines,
      has_match,
      elapsed,
    }
  }
}

impl SearchResultEmitter {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn emit_match(&mut self, record: MatchRecord) {
    self.outcomes.push(SearchOutcome::matched_line(record));
  }

  pub fn emit_binary_match(&mut self, path: Option<String>) {
    self.outcomes.push(SearchOutcome::binary_match(path));
  }

  pub fn emit_json_begin(&mut self, path: Option<String>) {
    self.outcomes.push(SearchOutcome::json_begin(path));
  }

  pub fn emit_context(&mut self, record: MatchRecord) {
    self.outcomes.push(SearchOutcome::context_line(record));
  }

  pub fn emit_context_separator(&mut self) {
    self.outcomes.push(SearchOutcome::context_separator());
  }

  pub fn emit_count(&mut self, path: Option<String>, count: usize) {
    self.outcomes.push(SearchOutcome::count(path, count));
  }

  pub fn emit_file_match(&mut self, path: impl Into<String>) {
    self.outcomes.push(SearchOutcome::file_match(path));
  }

  pub fn emit_file_without_match(&mut self, path: impl Into<String>) {
    self.outcomes.push(SearchOutcome::file_without_match(path));
  }

  pub fn emit_json_end(
    &mut self,
    path: Option<String>,
    bytes_searched: usize,
    matches: usize,
    matched_lines: usize,
    has_match: bool,
    elapsed: std::time::Duration,
  ) {
    self.outcomes.push(SearchOutcome::json_end(
      path,
      bytes_searched,
      matches,
      matched_lines,
      has_match,
      elapsed,
    ));
  }

  pub fn outcomes(&self) -> &[SearchOutcome] {
    &self.outcomes
  }

  pub fn into_outcomes(self) -> Vec<SearchOutcome> {
    self.outcomes
  }
}

impl SearchOutcomeSink for SearchResultEmitter {
  fn emit_outcome(&mut self, outcome: SearchOutcome) -> io::Result<()> {
    self.outcomes.push(outcome);
    Ok(())
  }
}

pub(super) fn suppress_paths(outcomes: &mut [SearchOutcome]) {
  for outcome in outcomes {
    match outcome {
      SearchOutcome::JsonBegin { path } => *path = None,
      SearchOutcome::MatchedLine(record)
      | SearchOutcome::ContextLine(record) => record.path = None,
      SearchOutcome::BinaryMatch { path } => *path = None,
      SearchOutcome::Count { path, .. } => *path = None,
      SearchOutcome::JsonEnd { path, .. } => *path = None,
      SearchOutcome::ContextSeparator
      | SearchOutcome::FileMatch(_)
      | SearchOutcome::FileWithoutMatch(_) => {}
    }
  }
}
