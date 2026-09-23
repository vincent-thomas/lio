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
  Literal { needle: Vec<u8> },
  Regex { regex: regex::bytes::Regex, prefilter: Option<LiteralPrefilter> },
}

#[derive(Debug, Clone)]
enum LiteralPrefilter {
  One(memmem::Finder<'static>),
  Many(regex::bytes::Regex),
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

    let mandatory_literals =
      if case_insensitive { None } else { extract_mandatory_literals(spec) };
    let prefilter = build_literal_prefilter(mandatory_literals)?;

    Ok(Self { kind: MatcherKind::Regex { regex, prefilter } })
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
      MatcherKind::Regex { regex, prefilter } => {
        if !matches_literal_prefilter(line, prefilter.as_ref()) {
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
      MatcherKind::Regex { regex, prefilter } => {
        matches_literal_prefilter(bytes, prefilter.as_ref())
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
      MatcherKind::Regex { prefilter, .. } => {
        find_prefilter_candidate(haystack, prefilter.as_ref())
          .map(CandidateLineMatch::Candidate)
      }
    }
  }

  pub(super) fn has_candidate_line_search(&self) -> bool {
    match &self.kind {
      MatcherKind::Literal { .. } => true,
      MatcherKind::Regex { prefilter, .. } => prefilter.is_some(),
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
      MatcherKind::Regex { regex, prefilter } => {
        if matches_literal_prefilter(bytes, prefilter.as_ref()) {
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
    PatternMode::Regex => regex_literal_needle(&spec.patterns[0]),
  }
}

fn extract_mandatory_literals(spec: &PatternSpec) -> Option<Vec<Vec<u8>>> {
  spec
    .patterns
    .iter()
    .map(|pattern| match spec.mode {
      PatternMode::FixedStrings => {
        (!pattern.is_empty()).then(|| pattern.as_bytes().to_vec())
      }
      PatternMode::Regex => mandatory_literal(pattern),
    })
    .collect()
}

/// Returns one literal that every match of the pattern must contain.
///
/// This deliberately recognizes only a small, easy-to-prove subset. In
/// particular, alternation and repetition that may match zero times disable
/// the prefilter rather than risking a false negative.
fn mandatory_literal(pattern: &str) -> Option<Vec<u8>> {
  let chars: Vec<char> = pattern.chars().collect();
  let mut runs = Vec::new();
  let mut current = String::new();
  let mut index = 0;
  // States: immediately after opening, after an optional leading caret, or body.
  let mut class_states = Vec::new();

  while index < chars.len() {
    let ch = chars[index];

    if let Some(state) = class_states.last_mut() {
      if ch == '\\' {
        *state = 2;
        index += 2;
        continue;
      }
      match ch {
        '^' if *state == 0 => *state = 1,
        ']' if *state < 2 => *state = 2,
        ']' => {
          class_states.pop();
        }
        '[' => {
          *state = 2;
          class_states.push(0);
        }
        _ => *state = 2,
      }
      index += 1;
      continue;
    }

    match ch {
      '[' => {
        push_literal_run(&mut runs, &mut current);
        class_states.push(0);
        index += 1;
      }
      '\\' => {
        let escaped = *chars.get(index + 1)?;
        if escaped.is_ascii_alphanumeric() {
          // These escapes consume one character or assert a boundary. Other
          // alphanumeric escapes (such as hex and Unicode escapes) have more
          // syntax that this deliberately small scanner does not parse.
          if matches!(
            escaped,
            'd' | 'D' | 's' | 'S' | 'w' | 'W' | 'b' | 'B' | 'A' | 'z'
          ) {
            push_literal_run(&mut runs, &mut current);
            index += 2;
            continue;
          }
          return None;
        }
        push_literal_char(&mut runs, &mut current, escaped);
        index += 2;
      }
      '|' | '*' | '?' => return None,
      '{' => {
        push_literal_run(&mut runs, &mut current);
        let close = chars[index + 1..]
          .iter()
          .position(|ch| *ch == '}')
          .map(|offset| index + 1 + offset)?;
        let repetition: String = chars[index + 1..close].iter().collect();
        let minimum = repetition
          .split_once(',')
          .map_or(repetition.as_str(), |(minimum, _)| minimum);
        if minimum.parse::<usize>().ok()? == 0 {
          return None;
        }
        index = close + 1;
      }
      '.' | '+' | '(' | ')' | '}' | '^' | '$' => {
        push_literal_run(&mut runs, &mut current);
        index += 1;
      }
      _ => {
        push_literal_char(&mut runs, &mut current, ch);
        index += 1;
      }
    }
  }

  push_literal_run(&mut runs, &mut current);
  runs.into_iter().max_by_key(Vec::len)
}

fn push_literal_char(runs: &mut Vec<Vec<u8>>, current: &mut String, ch: char) {
  if ch.is_alphanumeric() || ch == '_' {
    current.push(ch);
  } else {
    push_literal_run(runs, current);
  }
}

fn push_literal_run(runs: &mut Vec<Vec<u8>>, current: &mut String) {
  if !current.is_empty() {
    runs.push(std::mem::take(current).into_bytes());
  }
}

fn regex_literal_needle(pattern: &str) -> Option<Vec<u8>> {
  let mut literal = String::with_capacity(pattern.len());
  let mut chars = pattern.chars();
  while let Some(ch) = chars.next() {
    if ch == '\\' {
      let escaped = chars.next()?;
      if escaped.is_ascii_alphanumeric() {
        return None;
      }
      literal.push(escaped);
    } else if matches!(
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
      return None;
    } else {
      literal.push(ch);
    }
  }
  Some(literal.into_bytes())
}
fn matches_literal_prefilter(
  haystack: &[u8],
  prefilter: Option<&LiteralPrefilter>,
) -> bool {
  prefilter.is_none_or(|prefilter| match prefilter {
    LiteralPrefilter::One(literal) => literal.find(haystack).is_some(),
    LiteralPrefilter::Many(regex) => regex.is_match(haystack),
  })
}

fn find_prefilter_candidate(
  haystack: &[u8],
  prefilter: Option<&LiteralPrefilter>,
) -> Option<usize> {
  match prefilter? {
    LiteralPrefilter::One(literal) => literal.find(haystack),
    LiteralPrefilter::Many(regex) => regex.find(haystack).map(|m| m.start()),
  }
}

fn build_literal_prefilter(
  literals: Option<Vec<Vec<u8>>>,
) -> io::Result<Option<LiteralPrefilter>> {
  let Some(literals) = literals else {
    return Ok(None);
  };
  if literals.is_empty() {
    return Ok(None);
  }
  if literals.len() == 1 {
    return Ok(Some(LiteralPrefilter::One(
      memmem::Finder::new(&literals.into_iter().next().unwrap()).into_owned(),
    )));
  }

  let pattern = literals
    .iter()
    .map(|literal| regex::escape(&String::from_utf8_lossy(literal)))
    .collect::<Vec<_>>()
    .join("|");
  let regex = RegexBuilder::new(&pattern)
    .build()
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
  Ok(Some(LiteralPrefilter::Many(regex)))
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

  #[test]
  fn canonical_regex_uses_one_mandatory_literal() {
    let matcher =
      CompiledMatcher::new(&spec("NEEDLE_[0-9]{4}::[A-Za-z]{8}::payload"))
        .unwrap();

    let literal = match &matcher.kind {
      MatcherKind::Regex {
        prefilter: Some(LiteralPrefilter::One(literal)),
        ..
      } => {
        assert!(
          literal.needle() == b"NEEDLE_" || literal.needle() == b"payload"
        );
        literal
      }
      kind => panic!("expected one-literal prefilter, got {kind:?}"),
    };
    let matching = b"NEEDLE_1234::abcdefgh::payload";
    assert_eq!(
      matcher.find_candidate_line(matching),
      literal.find(matching).map(CandidateLineMatch::Candidate)
    );
    assert!(matcher.is_match(matching));
    assert!(!matcher.is_match(b"payload without the required structure"));
  }

  #[test]
  fn unsafe_regex_constructs_disable_prefilter() {
    for pattern in ["foo|bar", "foo*bar", "foo?bar", "foo{0}bar", "foo{0,3}bar"]
    {
      let matcher = CompiledMatcher::new(&spec(pattern)).unwrap();
      assert!(
        !matcher.has_candidate_line_search(),
        "unexpected prefilter for {pattern}"
      );
    }
  }

  #[test]
  fn character_class_contents_are_not_candidate_literals() {
    let matcher = CompiledMatcher::new(&spec("[A-Za-z]{8}::payload")).unwrap();
    match &matcher.kind {
      MatcherKind::Regex {
        prefilter: Some(LiteralPrefilter::One(literal)),
        ..
      } => assert_eq!(literal.needle(), b"payload"),
      kind => panic!("expected one-literal prefilter, got {kind:?}"),
    }
  }

  #[test]
  fn multiple_patterns_require_one_literal_from_each() {
    let mut patterns = spec("unused");
    patterns.patterns = vec!["alpha[0-9]+".to_owned(), "beta.+".to_owned()];
    let matcher = CompiledMatcher::new(&patterns).unwrap();
    assert!(matches!(
      matcher.kind,
      MatcherKind::Regex { prefilter: Some(LiteralPrefilter::Many(_)), .. }
    ));
    assert!(matcher.is_match(b"alpha7"));
    assert!(matcher.is_match(b"beta!"));

    patterns.patterns.push("[0-9]+".to_owned());
    let matcher = CompiledMatcher::new(&patterns).unwrap();
    assert!(!matcher.has_candidate_line_search());
    assert!(matcher.is_match(b"123"));
  }

  #[test]
  fn mandatory_literal_prefilter_matches_regex_on_tricky_constructs() {
    let cases: &[(&str, &[&[u8]])] = &[
      ("((foo)(bar))baz", &[b"foobarbaz", b"barbaz", b"xxfoobarbazyy"]),
      ("longliteral|x", &[b"longliteral", b"x", b"neither"]),
      ("(?:foo)?bar", &[b"bar", b"foobar", b"foo"]),
      ("ab{0,2}c", &[b"ac", b"abc", b"abbc", b"abbbc"]),
      (r"[ab]\d\x66oo", &[b"a1foo", b"b9foo", b"aXfoo"]),
      (r"\p{Greek}+", &["Ω".as_bytes(), b"Greek", "λδ".as_bytes()]),
      ("(?i)foo", &[b"FOO", b"foo", b"bar"]),
      ("(?x)foo bar", &[b"foobar", b"foo bar"]),
      (r"\bfoo\b", &[b" foo ", b"foobar", b"foo!"]),
      (r"foo\Bbar", &[b"foobar", b"foo bar"]),
      ("[]abc]", &[b"]", b"a", b"x"]),
      ("[^]abc]", &[b"x", b"]", b"a"]),
      (r"[\]]", &[b"]", b"[", b"x"]),
    ];

    for (pattern, haystacks) in cases {
      let matcher = CompiledMatcher::new(&spec(pattern)).unwrap();
      let reference = RegexBuilder::new(pattern).build().unwrap();
      for haystack in *haystacks {
        assert_eq!(
          matcher.is_match(haystack),
          reference.is_match(haystack),
          "pattern {pattern:?}, haystack {haystack:?}"
        );
        assert_eq!(
          !matcher.line_spans(haystack).unwrap().is_empty(),
          reference.is_match(haystack),
          "span mismatch for pattern {pattern:?}, haystack {haystack:?}"
        );
      }
    }
  }

  #[test]
  fn fixed_string_prefilter_handles_empty_unicode_and_regex_syntax() {
    let mut patterns = spec("unused");
    patterns.mode = PatternMode::FixedStrings;
    patterns.patterns =
      vec![String::new(), r"\d.+".to_owned(), "naïve".to_owned()];
    let matcher = CompiledMatcher::new(&patterns).unwrap();
    for haystack in
      [b"".as_slice(), b"plain", r"\d.+".as_bytes(), "naïve".as_bytes()]
    {
      assert!(matcher.is_match(haystack), "empty fixed string must match");
    }
    assert!(!matcher.has_candidate_line_search());
  }

  #[test]
  fn mandatory_literal_classification_is_conservative_at_boundaries() {
    assert_eq!(mandatory_literal("((alpha)(beta))"), Some(b"alpha".to_vec()));
    for pattern in [
      "alpha|b",
      "(?:alpha)?b",
      "[αβ]",
      r"\p{Greek}+",
      "(?i)alpha",
      "(?x)alpha beta",
      r"\b",
    ] {
      assert_eq!(mandatory_literal(pattern), None, "pattern {pattern:?}");
    }
    assert_eq!(mandatory_literal(r"\bneedle\b"), Some(b"needle".to_vec()));
    assert_eq!(mandatory_literal("[]abc]"), None);
    assert_eq!(mandatory_literal("[^]abc]"), None);
    assert_eq!(mandatory_literal("[]abc]needle"), Some(b"needle".to_vec()));
    assert_eq!(mandatory_literal("[^]abc]needle"), Some(b"needle".to_vec()));
  }

  #[test]
  fn multiple_fixed_strings_are_escaped_in_prefilter() {
    let mut patterns = spec("unused");
    patterns.mode = PatternMode::FixedStrings;
    patterns.patterns = vec!["a.b".to_owned(), "c+d".to_owned()];
    let matcher = CompiledMatcher::new(&patterns).unwrap();

    assert!(matcher.is_match(b"a.b"));
    assert!(matcher.is_match(b"c+d"));
    assert!(!matcher.is_match(b"axb"));
    assert!(!matcher.is_match(b"ccd"));
  }
}
