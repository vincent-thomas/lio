use std::io;

use memchr::memmem;
use regex::bytes::RegexBuilder;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CandidateLineMatch {
  Confirmed(usize),
  Candidate(usize),
}

#[derive(Debug, Clone)]
enum MatcherKind {
  Literal {
    needle: Vec<u8>,
  },
  Regex {
    regex: regex::bytes::Regex,
    fast_line_regex: Option<regex::bytes::Regex>,
  },
}

#[derive(Debug, Clone)]
pub(super) struct CompiledMatcher {
  kind: MatcherKind,
}

pub(super) struct WorkerMatcher {
  matcher: CompiledMatcher,
  line_spans: Vec<MatchSpan>,
}

impl CompiledMatcher {
  pub(super) fn new(spec: &PatternSpec) -> io::Result<Self> {
    let case_insensitive = match spec.case_mode {
      CaseMode::Sensitive => false,
      CaseMode::Ignore => true,
      CaseMode::Smart => !spec
        .patterns
        .iter()
        .any(|pattern| pattern.chars().any(|ch| ch.is_ascii_uppercase())),
    };

    if let Some(needle) = exact_literal_needle(spec, case_insensitive) {
      return Ok(Self { kind: MatcherKind::Literal { needle } });
    }

    let pattern = build_combined_pattern(spec);
    let regex = RegexBuilder::new(&pattern)
      .case_insensitive(case_insensitive)
      .build()
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

    let candidate_literals = if case_insensitive {
      Vec::new()
    } else {
      extract_candidate_literals(spec)
    };
    let fast_line_regex = build_fast_line_regex(&candidate_literals)?;

    Ok(Self { kind: MatcherKind::Regex { regex, fast_line_regex } })
  }

  pub(super) fn line_spans(&self, line: &[u8]) -> io::Result<Vec<MatchSpan>> {
    let mut spans = Vec::new();
    self.fill_line_spans(line, &mut spans)?;
    Ok(spans)
  }

  pub(super) fn fill_line_spans(
    &self,
    line: &[u8],
    spans: &mut Vec<MatchSpan>,
  ) -> io::Result<()> {
    spans.clear();
    match &self.kind {
      MatcherKind::Literal { needle } => {
        for (start, end) in find_literal_ranges(line, needle) {
          spans.push(MatchSpan::new(start, end)?);
        }
        Ok(())
      }
      MatcherKind::Regex { regex, fast_line_regex } => {
        if !matches_fast_line_regex(line, fast_line_regex.as_ref()) {
          return Ok(());
        }
        for m in regex.find_iter(line) {
          spans.push(MatchSpan::new(m.start(), m.end())?);
        }
        Ok(())
      }
    }
  }

  pub(super) fn is_match(&self, bytes: &[u8]) -> bool {
    match &self.kind {
      MatcherKind::Literal { needle } => memmem::find(bytes, needle).is_some(),
      MatcherKind::Regex { regex, fast_line_regex } => {
        matches_fast_line_regex(bytes, fast_line_regex.as_ref())
          && regex.is_match(bytes)
      }
    }
  }

  pub(super) fn find_candidate_line(
    &self,
    haystack: &[u8],
  ) -> Option<CandidateLineMatch> {
    match &self.kind {
      MatcherKind::Literal { needle } => {
        memmem::find(haystack, needle).map(CandidateLineMatch::Confirmed)
      }
      MatcherKind::Regex { fast_line_regex, .. } => {
        find_fast_line_candidate(haystack, fast_line_regex.as_ref())
          .map(CandidateLineMatch::Candidate)
      }
    }
  }

  pub(super) fn has_candidate_line_search(&self) -> bool {
    match &self.kind {
      MatcherKind::Literal { .. } => true,
      MatcherKind::Regex { fast_line_regex, .. } => fast_line_regex.is_some(),
    }
  }

  pub(super) fn visit_match_ranges(
    &self,
    bytes: &[u8],
    mut visit: impl FnMut(usize, usize),
  ) {
    match &self.kind {
      MatcherKind::Literal { needle } => {
        for (start, end) in find_literal_ranges(bytes, needle) {
          visit(start, end);
        }
      }
      MatcherKind::Regex { regex, fast_line_regex } => {
        if matches_fast_line_regex(bytes, fast_line_regex.as_ref()) {
          for m in regex.find_iter(bytes) {
            visit(m.start(), m.end());
          }
        }
      }
    }
  }
}

impl WorkerMatcher {
  pub(super) fn new(matcher: CompiledMatcher) -> Self {
    Self { matcher, line_spans: Vec::new() }
  }

  pub(super) fn is_match(&self, bytes: &[u8]) -> bool {
    self.matcher.is_match(bytes)
  }

  pub(super) fn find_candidate_line(
    &self,
    haystack: &[u8],
  ) -> Option<CandidateLineMatch> {
    self.matcher.find_candidate_line(haystack)
  }

  pub(super) fn has_candidate_line_search(&self) -> bool {
    self.matcher.has_candidate_line_search()
  }

  pub(super) fn line_spans(&mut self, line: &[u8]) -> io::Result<&[MatchSpan]> {
    self.matcher.fill_line_spans(line, &mut self.line_spans)?;
    Ok(&self.line_spans)
  }
}

fn exact_literal_needle(
  spec: &PatternSpec,
  case_insensitive: bool,
) -> Option<Vec<u8>> {
  if case_insensitive
    || spec.patterns.len() != 1
    || spec.word_regexp
    || spec.line_regexp
  {
    return None;
  }

  match spec.mode {
    PatternMode::FixedStrings => Some(spec.patterns[0].as_bytes().to_vec()),
    PatternMode::Regex
      if !super::util::contains_regex_meta(&spec.patterns[0]) =>
    {
      Some(unescape_literal(&spec.patterns[0]).into_bytes())
    }
    PatternMode::Regex => None,
  }
}

fn extract_candidate_literals(spec: &PatternSpec) -> Vec<Vec<u8>> {
  if spec.line_regexp {
    return Vec::new();
  }

  match spec.mode {
    PatternMode::FixedStrings => spec
      .patterns
      .iter()
      .filter(|pattern| !pattern.is_empty())
      .map(|pattern| pattern.as_bytes().to_vec())
      .collect(),
    PatternMode::Regex => {
      let mut literals = Vec::new();
      for pattern in &spec.patterns {
        for literal in literal_runs(pattern) {
          if literal.len() >= 2 && !literals.iter().any(|prev| prev == &literal)
          {
            literals.push(literal);
          }
        }
      }
      literals.sort_by(|left, right| right.len().cmp(&left.len()));
      literals.truncate(8);
      literals
    }
  }
}

fn literal_runs(pattern: &str) -> Vec<Vec<u8>> {
  let mut runs = Vec::new();
  let mut current = String::new();
  let mut escaped = false;

  for ch in pattern.chars() {
    if escaped {
      if ch.is_ascii_alphanumeric() || ch == '_' {
        if !current.is_empty() {
          runs.push(current.clone().into_bytes());
        }
        current.clear();
      } else {
        current.push(ch);
      }
      escaped = false;
      continue;
    }

    if ch == '\\' {
      escaped = true;
      continue;
    }

    if matches!(
      ch,
      '.'
        | '+'
        | '*'
        | '?'
        | '('
        | ')'
        | '['
        | ']'
        | '{'
        | '}'
        | '|'
        | '^'
        | '$'
    ) {
      if !current.is_empty() {
        runs.push(current.clone().into_bytes());
      }
      current.clear();
      continue;
    }

    current.push(ch);
  }

  if !current.is_empty() {
    runs.push(current.into_bytes());
  }
  runs
}

fn unescape_literal(pattern: &str) -> String {
  let mut out = String::with_capacity(pattern.len());
  let mut escaped = false;
  for ch in pattern.chars() {
    if escaped {
      out.push(ch);
      escaped = false;
    } else if ch == '\\' {
      escaped = true;
    } else {
      out.push(ch);
    }
  }
  out
}

fn matches_fast_line_regex(
  haystack: &[u8],
  fast_line_regex: Option<&regex::bytes::Regex>,
) -> bool {
  fast_line_regex.is_none_or(|regex| regex.is_match(haystack))
}

fn find_fast_line_candidate(
  haystack: &[u8],
  fast_line_regex: Option<&regex::bytes::Regex>,
) -> Option<usize> {
  fast_line_regex.and_then(|regex| regex.find(haystack).map(|m| m.start()))
}

fn build_fast_line_regex(
  literals: &[Vec<u8>],
) -> io::Result<Option<regex::bytes::Regex>> {
  if literals.is_empty() {
    return Ok(None);
  }

  let pattern = literals
    .iter()
    .map(|literal| regex::escape(&String::from_utf8_lossy(literal)))
    .collect::<Vec<_>>()
    .join("|");
  let regex = RegexBuilder::new(&pattern)
    .build()
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
  Ok(Some(regex))
}

fn build_combined_pattern(spec: &PatternSpec) -> String {
  let parts: Vec<String> = spec
    .patterns
    .iter()
    .map(|pattern| {
      let base = match spec.mode {
        PatternMode::Regex => pattern.clone(),
        PatternMode::FixedStrings => regex::escape(pattern),
      };

      if spec.line_regexp {
        format!("^(?:{base})$")
      } else if spec.word_regexp {
        format!(r"\b(?:{base})\b")
      } else {
        base
      }
    })
    .collect();

  if parts.len() == 1 {
    parts.into_iter().next().unwrap_or_default()
  } else {
    parts
      .into_iter()
      .map(|pattern| format!("(?:{pattern})"))
      .collect::<Vec<_>>()
      .join("|")
  }
}

fn find_literal_ranges<'a>(
  haystack: &'a [u8],
  needle: &'a [u8],
) -> impl Iterator<Item = (usize, usize)> + 'a {
  let finder = memmem::Finder::new(needle);
  let mut offset = 0usize;
  std::iter::from_fn(move || {
    if offset > haystack.len() {
      return None;
    }
    let start = finder.find(&haystack[offset..])?;
    let start = offset + start;
    let end = start + needle.len();
    offset = end.max(start + 1);
    Some((start, end))
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn spec(pattern: &str) -> PatternSpec {
    PatternSpec {
      text: pattern.to_owned(),
      patterns: vec![pattern.to_owned()],
      mode: PatternMode::Regex,
      case_mode: CaseMode::Sensitive,
      word_regexp: false,
      line_regexp: false,
    }
  }

  #[test]
  fn literal_regex_uses_literal_matching() {
    let matcher = CompiledMatcher::new(&spec("testing")).unwrap();
    assert!(matcher.is_match(b"alpha testing beta"));
    assert!(!matcher.is_match(b"alpha beta"));
    let mut ranges = Vec::new();
    matcher.visit_match_ranges(b"testing testing", |start, end| {
      ranges.push((start, end));
    });
    assert_eq!(ranges, vec![(0, 7), (8, 15)]);
  }

  #[test]
  fn regex_prefilter_preserves_regex_semantics() {
    let matcher = CompiledMatcher::new(&spec("test.*ing")).unwrap();
    assert!(matcher.is_match(b"testing"));
    assert!(matcher.is_match(b"test___ing"));
    assert!(!matcher.is_match(b"toast___ing"));
  }

  #[test]
  fn word_regexp_keeps_candidate_literal_prefilter() {
    let matcher = CompiledMatcher::new(&PatternSpec {
      text: "[A-Z]+_SUSPEND".to_owned(),
      patterns: vec!["[A-Z]+_SUSPEND".to_owned()],
      mode: PatternMode::Regex,
      case_mode: CaseMode::Sensitive,
      word_regexp: true,
      line_regexp: false,
    })
    .unwrap();

    assert!(matcher.has_candidate_line_search());
    assert!(matcher.is_match(b" PM_SUSPEND "));
    assert!(!matcher.is_match(b"pm_suspend"));
  }
}
