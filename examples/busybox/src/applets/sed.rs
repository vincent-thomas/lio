use std::{fs, io, num::NonZeroU64};

use crate::{app::AppContext, command::Command, util::io as io_util};
use regex::RegexBuilder;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SedCommand {
  pub quiet: bool,
  pub extended_regex: bool,
  pub scripts: Vec<SedScriptSource>,
  pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedRunPlan {
  pub quiet: bool,
  pub script: SedScript,
  pub files: Vec<String>,
}

#[derive(Debug, Clone)]
struct SedCompiledProgram {
  statements: Vec<SedCompiledStatement>,
}

#[derive(Debug, Clone)]
struct SedCompiledStatement {
  guards: Vec<SedSelectorGuard>,
  op: SedCompiledOp,
}

#[derive(Debug, Clone)]
struct SedSelectorGuard {
  state_id: usize,
  selector: SedSelector,
  sticky: bool,
}

#[derive(Debug, Clone)]
enum SedCompiledOp {
  Statement(SedStatementKind),
  Label(String),
}

#[derive(Debug, Clone, Default)]
struct SedRangeState {
  active: bool,
  start_line: Option<u64>,
  zero_consumed: bool,
  just_started: bool,
}

#[derive(Debug, Clone)]
struct SedRuntimeInput {
  text: String,
  file: String,
  line_number: u64,
  is_last: bool,
}

#[derive(Debug, Clone, Default)]
struct SedRuntime {
  hold_space: String,
  range_states: Vec<SedRangeState>,
  read_line_states: std::collections::HashMap<usize, SedQueuedLines>,
  last_regex: Option<SedRegex>,
}

#[derive(Debug, Clone, Default)]
struct SedQueuedLines {
  lines: Vec<String>,
  offset: usize,
}

#[derive(Debug, Clone)]
struct SedCycleState {
  pattern_space: String,
  current_file: String,
  line_number: u64,
  is_last: bool,
  next_input_index: usize,
  append_queue: Vec<String>,
  suppressed_default_print: bool,
  substituted: bool,
  sticky_guard_matches: std::collections::HashMap<usize, bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SedFlow {
  Continue,
  RestartCycle,
  NextCycle,
  Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyText {
  pub head: char,
  pub tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedScriptSource {
  Expression(NonEmptyText),
  File(SedPath),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SedScript {
  pub statements: Vec<SedStatement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedStatement {
  pub selector: SedSelector,
  pub command: SedStatementKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedSelector {
  Any,
  Addressed { addresses: SedAddresses, negated: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedAddresses {
  Single(SedSingleAddress),
  Range(SedRangeAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedSingleAddress {
  Line(NonZeroU64),
  LastLine,
  Regex(SedRegex),
  Step(SedStepAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedStepAddress {
  pub first: SedStepStart,
  pub step: NonZeroU64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedStepStart {
  Zero,
  Line(NonZeroU64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedRangeAddress {
  pub start: SedRangeStart,
  pub end: SedRangeEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedRangeStart {
  Zero,
  Address(SedSingleAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedRangeEnd {
  Address(SedSingleAddress),
  NextLines(NonZeroU64),
  NextMultiple(NonZeroU64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedRegex {
  pub delimiter: char,
  pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedLabel {
  pub text: NonEmptyText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedPath {
  pub text: NonEmptyText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedStatementKind {
  Substitute(SedSubstitute),
  Transliterate(SedTransliterate),
  AppendText(SedTextBlock),
  InsertText(SedTextBlock),
  ChangeText(SedTextBlock),
  Block(Vec<SedStatement>),
  Label(SedLabel),
  Branch(SedBranch),
  PrintPattern,
  PrintFirstLine,
  DeletePattern,
  DeleteFirstLine,
  NextLine,
  AppendNextLine,
  PrintLineNumber,
  CopyPatternToHold,
  AppendPatternToHold,
  CopyHoldToPattern,
  AppendHoldToPattern,
  ExchangePatternAndHold,
  ZapPattern,
  ListPattern { wrap: Option<NonZeroU64> },
  Quit(SedQuit),
  ReadFile { path: SedPath },
  ReadLineFromFile { path: SedPath },
  WriteFile { path: SedPath },
  WriteFirstLine { path: SedPath },
  PrintCurrentFile,
  VersionCheck { version: Option<NonEmptyText> },
  Execute(SedExecute),
  Comment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedBranch {
  pub kind: SedBranchKind,
  pub target: SedBranchTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SedBranchKind {
  Unconditional,
  OnSubstitute,
  OnNoSubstitute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedBranchTarget {
  EndOfScript,
  Label(SedLabel),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedQuit {
  pub kind: SedQuitKind,
  pub code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SedQuitKind {
  PrintPattern,
  Silent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SedExecute {
  PatternSpace,
  Command(NonEmptyText),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedTextBlock {
  pub first: String,
  pub rest: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedSubstitute {
  pub delimiter: char,
  pub pattern: String,
  pub replacement: String,
  pub flags: SedSubstituteFlags,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SedSubstituteFlags {
  pub global: bool,
  pub print: bool,
  pub occurrence: Option<NonZeroU64>,
  pub write: Option<SedPath>,
  pub ignore_case: bool,
  pub multi_line: bool,
  pub evaluate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedTransliterate {
  pub delimiter: char,
  pub source: String,
  pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SedParser<'a> {
  input: &'a str,
  offset: usize,
}

impl SedScript {
  pub fn parse(script: &str) -> io::Result<Self> {
    SedParser::new(script).parse_script()
  }

  pub fn parse_many<'a, I>(scripts: I) -> io::Result<Self>
  where
    I: IntoIterator<Item = &'a str>,
  {
    let mut combined = SedScript::default();
    for script in scripts {
      combined.statements.extend(Self::parse(script)?.statements);
    }
    Ok(combined)
  }
}

impl<'a> SedParser<'a> {
  pub fn new(input: &'a str) -> Self {
    Self { input, offset: 0 }
  }

  pub fn parse_script(&mut self) -> io::Result<SedScript> {
    self.parse_script_until(None)
  }

  pub fn parse_statement(&mut self) -> io::Result<SedStatement> {
    self.skip_inline_ws();
    let selector = self.parse_selector()?;
    let command = self.parse_command_kind()?;
    if !matches!(selector, SedSelector::Any)
      && matches!(command, SedStatementKind::Comment(_))
    {
      return Err(invalid_input("addressed comments are not supported"));
    }
    Ok(SedStatement { selector, command })
  }

  pub fn parse_selector(&mut self) -> io::Result<SedSelector> {
    self.skip_inline_ws();
    let start = self.offset;
    let Some(first) = self.peek_char() else {
      return Ok(SedSelector::Any);
    };

    let zero_range_start = if self.peek_char() == Some('0') {
      let checkpoint = self.offset;
      self.bump_char();
      self.skip_inline_ws();
      let is_zero_range = self.peek_char() == Some(',');
      self.offset = checkpoint;
      is_zero_range
    } else {
      false
    };

    let first_address =
      if zero_range_start { None } else { self.try_parse_single_address()? };

    if !zero_range_start && first_address.is_none() {
      return Ok(SedSelector::Any);
    }

    self.skip_inline_ws();
    let addresses = if zero_range_start || self.peek_char() == Some(',') {
      if zero_range_start {
        self.bump_char();
        if self.bump_char() != Some(',') {
          return Err(invalid_input("invalid zero range"));
        }
      } else {
        self.bump_char();
      }
      self.skip_inline_ws();
      let start = if zero_range_start {
        SedRangeStart::Zero
      } else {
        match first_address
          .ok_or_else(|| invalid_input("missing range start"))?
        {
          SedSingleAddress::Step(SedStepAddress {
            first: SedStepStart::Zero,
            ..
          }) => SedRangeStart::Zero,
          other => SedRangeStart::Address(other),
        }
      };
      let end = self.parse_range_end()?;
      SedAddresses::Range(SedRangeAddress { start, end })
    } else {
      SedAddresses::Single(
        first_address.ok_or_else(|| invalid_input("missing address"))?,
      )
    };

    self.skip_inline_ws();
    let negated = if self.peek_char() == Some('!') {
      self.bump_char();
      true
    } else {
      false
    };

    self.skip_inline_ws();
    match self.peek_char() {
      Some(c) if !matches!(c, '\n' | ';' | '}') => {}
      _ => {
        self.offset = start;
        return Err(invalid_input("selector missing command"));
      }
    }

    if first == '#' {
      self.offset = start;
      Ok(SedSelector::Any)
    } else {
      Ok(SedSelector::Addressed { addresses, negated })
    }
  }

  pub fn parse_command_kind(&mut self) -> io::Result<SedStatementKind> {
    self.skip_inline_ws();
    let Some(command) = self.peek_char() else {
      return Err(invalid_input("missing command"));
    };

    if command == '#' {
      self.bump_char();
      let comment = self.take_until_statement_end();
      return Ok(SedStatementKind::Comment(comment));
    }

    let command =
      self.bump_char().ok_or_else(|| invalid_input("missing command"))?;

    match command {
      '{' => Ok(SedStatementKind::Block(self.parse_block_statements()?)),
      '}' => Err(invalid_input("unexpected closing brace")),
      ':' => Ok(SedStatementKind::Label(SedLabel {
        text: self.parse_nonempty_rest_of_segment()?,
      })),
      'b' | 't' | 'T' => Ok(SedStatementKind::Branch(SedBranch {
        kind: match command {
          'b' => SedBranchKind::Unconditional,
          't' => SedBranchKind::OnSubstitute,
          'T' => SedBranchKind::OnNoSubstitute,
          _ => unreachable!(),
        },
        target: self.parse_optional_label_target()?,
      })),
      'p' => Ok(SedStatementKind::PrintPattern),
      'P' => Ok(SedStatementKind::PrintFirstLine),
      'd' => Ok(SedStatementKind::DeletePattern),
      'D' => Ok(SedStatementKind::DeleteFirstLine),
      'n' => Ok(SedStatementKind::NextLine),
      'N' => Ok(SedStatementKind::AppendNextLine),
      '=' => Ok(SedStatementKind::PrintLineNumber),
      'h' => Ok(SedStatementKind::CopyPatternToHold),
      'H' => Ok(SedStatementKind::AppendPatternToHold),
      'g' => Ok(SedStatementKind::CopyHoldToPattern),
      'G' => Ok(SedStatementKind::AppendHoldToPattern),
      'x' => Ok(SedStatementKind::ExchangePatternAndHold),
      'z' => Ok(SedStatementKind::ZapPattern),
      'l' => Ok(SedStatementKind::ListPattern {
        wrap: self.parse_optional_nonzero_u64()?,
      }),
      'q' | 'Q' => Ok(SedStatementKind::Quit(SedQuit {
        kind: if command == 'q' {
          SedQuitKind::PrintPattern
        } else {
          SedQuitKind::Silent
        },
        code: self.parse_optional_i32()?,
      })),
      'r' => Ok(SedStatementKind::ReadFile {
        path: SedPath { text: self.parse_required_spaced_text()? },
      }),
      'R' => Ok(SedStatementKind::ReadLineFromFile {
        path: SedPath { text: self.parse_required_spaced_text()? },
      }),
      'w' => Ok(SedStatementKind::WriteFile {
        path: SedPath { text: self.parse_required_spaced_text()? },
      }),
      'W' => Ok(SedStatementKind::WriteFirstLine {
        path: SedPath { text: self.parse_required_spaced_text()? },
      }),
      'F' => Ok(SedStatementKind::PrintCurrentFile),
      'v' => Ok(SedStatementKind::VersionCheck {
        version: self.parse_optional_nonempty_text()?,
      }),
      'e' => Ok(SedStatementKind::Execute(self.parse_execute_command()?)),
      'a' | 'i' | 'c' => {
        let block = self.parse_text_block()?;
        Ok(match command {
          'a' => SedStatementKind::AppendText(block),
          'i' => SedStatementKind::InsertText(block),
          'c' => SedStatementKind::ChangeText(block),
          _ => unreachable!(),
        })
      }
      's' => Ok(SedStatementKind::Substitute(self.parse_substitute()?)),
      'y' => Ok(SedStatementKind::Transliterate(self.parse_transliterate()?)),
      _ => Err(invalid_input("unknown command")),
    }
  }

  fn parse_script_until(
    &mut self,
    until: Option<char>,
  ) -> io::Result<SedScript> {
    let mut statements = Vec::new();

    loop {
      self.skip_statement_separators();
      if self.eof() {
        if until.is_some() {
          return Err(invalid_input("unclosed block"));
        }
        break;
      }

      if let Some(end) = until {
        if self.peek_char() == Some(end) {
          self.bump_char();
          break;
        }
      } else if self.peek_char() == Some('}') {
        return Err(invalid_input("unexpected closing brace"));
      }

      statements.push(self.parse_statement()?);

      if matches!(self.peek_char(), Some(';' | '\n')) {
        continue;
      }

      if let Some(end) = until {
        if self.peek_char() == Some(end) {
          self.bump_char();
          break;
        }
      }

      if self.starts_statement() {
        continue;
      }

      if !self.eof() {
        return Err(invalid_input("trailing garbage"));
      }
    }

    Ok(SedScript { statements })
  }

  fn parse_block_statements(&mut self) -> io::Result<Vec<SedStatement>> {
    Ok(self.parse_script_until(Some('}'))?.statements)
  }

  fn parse_optional_label_target(&mut self) -> io::Result<SedBranchTarget> {
    let raw = self.take_until_statement_end();
    if raw.is_empty() {
      return Ok(SedBranchTarget::EndOfScript);
    }
    if raw.trim().is_empty() {
      return Err(invalid_input("empty label target"));
    }
    Ok(SedBranchTarget::Label(SedLabel {
      text: parse_nonempty_text(raw.trim())?,
    }))
  }

  fn parse_optional_nonzero_u64(&mut self) -> io::Result<Option<NonZeroU64>> {
    let raw = self.take_until_statement_end();
    if raw.is_empty() {
      return Ok(None);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      return Ok(None);
    }
    let value: u64 =
      trimmed.parse().map_err(|_| invalid_input("invalid number"))?;
    Ok(Some(
      NonZeroU64::new(value)
        .ok_or_else(|| invalid_input("expected non-zero number"))?,
    ))
  }

  fn parse_optional_i32(&mut self) -> io::Result<Option<i32>> {
    let raw = self.take_until_statement_end();
    if raw.is_empty() {
      return Ok(None);
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      return Ok(None);
    }
    let mut parts = trimmed.split_whitespace();
    let first =
      parts.next().ok_or_else(|| invalid_input("invalid quit code"))?;
    if parts.next().is_some() {
      return Err(invalid_input("duplicate quit code"));
    }
    let value =
      first.parse::<i32>().map_err(|_| invalid_input("invalid quit code"))?;
    Ok(Some(value))
  }

  fn parse_execute_command(&mut self) -> io::Result<SedExecute> {
    let raw = self.take_until_statement_end();
    if raw.is_empty() {
      return Ok(SedExecute::PatternSpace);
    }
    if raw.trim().is_empty() {
      return Err(invalid_input("empty execute payload"));
    }
    Ok(SedExecute::Command(parse_nonempty_text(raw.trim_start())?))
  }

  fn parse_text_block(&mut self) -> io::Result<SedTextBlock> {
    if self.bump_char() != Some('\\') || self.bump_char() != Some('\n') {
      return Err(invalid_input("text commands require newline payload"));
    }

    let mut lines = Vec::new();
    loop {
      let line = self.take_until_newline_or_eof();
      if line.contains(';') {
        return Err(invalid_input("invalid text payload"));
      }
      lines.push(line);
      if self.peek_char() == Some('\n') {
        let checkpoint = self.offset;
        self.bump_char();
        let mut probe = self.clone();
        probe.skip_inline_ws();
        if probe.eof() || probe.peek_char() == Some('}') {
          break;
        }
        if probe.starts_statement() {
          break;
        }
        self.offset = checkpoint + '\n'.len_utf8();
        continue;
      } else {
        break;
      }
    }

    let mut iter = lines.into_iter();
    let first =
      iter.next().ok_or_else(|| invalid_input("missing text payload"))?;
    Ok(SedTextBlock { first, rest: iter.collect() })
  }

  fn parse_substitute(&mut self) -> io::Result<SedSubstitute> {
    let delimiter =
      self.bump_char().ok_or_else(|| invalid_input("missing delimiter"))?;
    let pattern = self.parse_delimited_field(delimiter)?;
    let replacement = self.parse_delimited_field(delimiter)?;
    let flags = self.parse_substitute_flags()?;
    Ok(SedSubstitute { delimiter, pattern, replacement, flags })
  }

  fn parse_substitute_flags(&mut self) -> io::Result<SedSubstituteFlags> {
    let mut flags = SedSubstituteFlags::default();
    loop {
      match self.peek_char() {
        None | Some('\n' | ';' | '}') => break,
        Some('g') => {
          if flags.global {
            return Err(invalid_input("duplicate substitute flag"));
          }
          self.bump_char();
          flags.global = true;
        }
        Some('p') => {
          if flags.print {
            return Err(invalid_input("duplicate substitute flag"));
          }
          self.bump_char();
          flags.print = true;
        }
        Some('I') => {
          if flags.ignore_case {
            return Err(invalid_input("duplicate substitute flag"));
          }
          self.bump_char();
          flags.ignore_case = true;
        }
        Some('M') => {
          if flags.multi_line {
            return Err(invalid_input("duplicate substitute flag"));
          }
          self.bump_char();
          flags.multi_line = true;
        }
        Some('e') => {
          if flags.evaluate {
            return Err(invalid_input("duplicate substitute flag"));
          }
          self.bump_char();
          flags.evaluate = true;
        }
        Some('w') => {
          if flags.write.is_some() {
            return Err(invalid_input("duplicate substitute write flag"));
          }
          self.bump_char();
          if !self
            .peek_char()
            .is_some_and(|c| c == ' ' || c == '\t' || c == '\r')
          {
            return Err(invalid_input("substitute write flag requires path"));
          }
          self.skip_inline_ws();
          let start = self.offset;
          while self.peek_char().is_some_and(|c| {
            !matches!(c, '\n' | ';' | '}')
              && !matches!(c, 'g' | 'p' | 'I' | 'M' | 'e')
          }) {
            self.bump_char();
          }
          let path =
            parse_nonempty_text(self.input[start..self.offset].trim())?;
          flags.write = Some(SedPath { text: path });
        }
        Some(c) if c.is_ascii_digit() => {
          if flags.occurrence.is_some() {
            return Err(invalid_input(
              "duplicate or conflicting substitute flag",
            ));
          }
          let start = self.offset;
          while self.peek_char().is_some_and(|value| value.is_ascii_digit()) {
            self.bump_char();
          }
          let occurrence = self.input[start..self.offset]
            .parse::<u64>()
            .map_err(|_| invalid_input("expected number"))?;
          flags.occurrence = Some(
            NonZeroU64::new(occurrence)
              .ok_or_else(|| invalid_input("expected non-zero occurrence"))?,
          );
        }
        _ => return Err(invalid_input("invalid substitute flag")),
      }
    }
    Ok(flags)
  }

  fn parse_transliterate(&mut self) -> io::Result<SedTransliterate> {
    let delimiter =
      self.bump_char().ok_or_else(|| invalid_input("missing delimiter"))?;
    let source = self.parse_delimited_field(delimiter)?;
    let target = self.parse_delimited_field(delimiter)?;
    if source.is_empty() || source.chars().count() != target.chars().count() {
      return Err(invalid_input("invalid transliterate arguments"));
    }
    Ok(SedTransliterate { delimiter, source, target })
  }

  fn parse_range_end(&mut self) -> io::Result<SedRangeEnd> {
    match self.peek_char() {
      Some('+') => {
        self.bump_char();
        let number = self.parse_nonzero_number()?;
        Ok(SedRangeEnd::NextLines(number))
      }
      Some('~') => {
        self.bump_char();
        let number = self.parse_nonzero_number()?;
        Ok(SedRangeEnd::NextMultiple(number))
      }
      _ => Ok(SedRangeEnd::Address(
        self
          .try_parse_single_address()?
          .ok_or_else(|| invalid_input("invalid range end"))?,
      )),
    }
  }

  fn try_parse_single_address(
    &mut self,
  ) -> io::Result<Option<SedSingleAddress>> {
    let checkpoint = self.offset;
    let Some(ch) = self.peek_char() else {
      return Ok(None);
    };

    let address = match ch {
      '$' => {
        self.bump_char();
        SedSingleAddress::LastLine
      }
      '/' => SedSingleAddress::Regex(self.parse_regex()?),
      '0'..='9' => {
        let number = self.parse_number()?;
        match self.peek_char() {
          Some('~') => {
            self.bump_char();
            let step = self.parse_nonzero_number()?;
            let first = if number == 0 {
              SedStepStart::Zero
            } else {
              SedStepStart::Line(
                NonZeroU64::new(number)
                  .ok_or_else(|| invalid_input("line 0 is invalid"))?,
              )
            };
            SedSingleAddress::Step(SedStepAddress { first, step })
          }
          _ => SedSingleAddress::Line(
            NonZeroU64::new(number)
              .ok_or_else(|| invalid_input("line 0 is invalid"))?,
          ),
        }
      }
      _ => {
        self.offset = checkpoint;
        return Ok(None);
      }
    };

    Ok(Some(address))
  }

  fn parse_regex(&mut self) -> io::Result<SedRegex> {
    let delimiter = self
      .bump_char()
      .ok_or_else(|| invalid_input("missing regex delimiter"))?;
    let pattern = self.parse_delimited_field(delimiter)?;
    Ok(SedRegex { delimiter, pattern })
  }

  fn parse_delimited_field(&mut self, delimiter: char) -> io::Result<String> {
    let mut out = String::new();
    let mut escaped = false;
    loop {
      let Some(ch) = self.bump_char() else {
        return Err(invalid_input("unterminated delimited field"));
      };
      if escaped {
        out.push(ch);
        escaped = false;
        continue;
      }
      if ch == '\\' {
        out.push(ch);
        escaped = true;
        continue;
      }
      if ch == delimiter {
        break;
      }
      if ch == '\n' {
        return Err(invalid_input("unterminated delimited field"));
      }
      out.push(ch);
    }
    Ok(out)
  }

  fn parse_nonempty_rest_of_segment(&mut self) -> io::Result<NonEmptyText> {
    let raw = self.take_until_statement_end();
    let trimmed = raw.trim();
    parse_nonempty_text(trimmed)
  }

  fn parse_optional_nonempty_text(
    &mut self,
  ) -> io::Result<Option<NonEmptyText>> {
    let raw = self.take_until_statement_end();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      return Ok(None);
    }
    Ok(Some(parse_nonempty_text(trimmed)?))
  }

  fn parse_required_spaced_text(&mut self) -> io::Result<NonEmptyText> {
    if !self.peek_char().is_some_and(|c| c == ' ' || c == '\t' || c == '\r') {
      return Err(invalid_input("expected whitespace before argument"));
    }
    self.parse_nonempty_rest_of_segment()
  }

  fn parse_nonzero_number(&mut self) -> io::Result<NonZeroU64> {
    let value = self.parse_number()?;
    NonZeroU64::new(value)
      .ok_or_else(|| invalid_input("expected non-zero number"))
  }

  fn parse_number(&mut self) -> io::Result<u64> {
    let start = self.offset;
    while self.peek_char().is_some_and(|c| c.is_ascii_digit()) {
      self.bump_char();
    }
    if self.offset == start {
      return Err(invalid_input("expected number"));
    }
    self.input[start..self.offset]
      .parse::<u64>()
      .map_err(|_| invalid_input("invalid number"))
  }

  fn take_until_statement_end(&mut self) -> String {
    let start = self.offset;
    while !matches!(self.peek_char(), None | Some('\n' | ';' | '}')) {
      self.bump_char();
    }
    self.input[start..self.offset].to_string()
  }

  fn take_until_newline_or_eof(&mut self) -> String {
    let start = self.offset;
    while !matches!(self.peek_char(), None | Some('\n')) {
      self.bump_char();
    }
    self.input[start..self.offset].to_string()
  }

  fn skip_statement_separators(&mut self) {
    while matches!(self.peek_char(), Some('\n' | ';')) {
      self.bump_char();
    }
  }

  fn skip_inline_ws(&mut self) {
    while self.peek_char().is_some_and(|c| c == ' ' || c == '\t' || c == '\r') {
      self.bump_char();
    }
  }

  fn peek_char(&self) -> Option<char> {
    self.input[self.offset..].chars().next()
  }

  fn bump_char(&mut self) -> Option<char> {
    let ch = self.peek_char()?;
    self.offset += ch.len_utf8();
    Some(ch)
  }

  fn eof(&self) -> bool {
    self.offset >= self.input.len()
  }

  fn starts_statement(&self) -> bool {
    let mut probe = self.clone();
    probe.skip_inline_ws();

    let zero_range_start = if probe.peek_char() == Some('0') {
      let checkpoint = probe.offset;
      probe.bump_char();
      probe.skip_inline_ws();
      let ok = probe.peek_char() == Some(',');
      probe.offset = checkpoint;
      ok
    } else {
      false
    };

    if zero_range_start {
      probe.bump_char();
      probe.bump_char();
      probe.skip_inline_ws();
      if matches!(probe.peek_char(), Some('+' | '~')) {
        probe.bump_char();
        if !probe.peek_char().is_some_and(|c| c.is_ascii_digit()) {
          return false;
        }
        while probe.peek_char().is_some_and(|c| c.is_ascii_digit()) {
          probe.bump_char();
        }
      } else if probe.try_parse_single_address().ok().flatten().is_none() {
        return false;
      }
      probe.skip_inline_ws();
    } else if probe.try_parse_single_address().ok().flatten().is_some() {
      probe.skip_inline_ws();
      if probe.peek_char() == Some(',') {
        probe.bump_char();
        probe.skip_inline_ws();
        if matches!(probe.peek_char(), Some('+' | '~')) {
          probe.bump_char();
          if !probe.peek_char().is_some_and(|c| c.is_ascii_digit()) {
            return false;
          }
          while probe.peek_char().is_some_and(|c| c.is_ascii_digit()) {
            probe.bump_char();
          }
        } else if probe.try_parse_single_address().ok().flatten().is_none() {
          return false;
        }
        probe.skip_inline_ws();
      }
      if probe.peek_char() == Some('!') {
        probe.bump_char();
        probe.skip_inline_ws();
      }
    }

    let Some(command) = probe.bump_char() else {
      return false;
    };

    match command {
      '{' | '}' | '#' | 'p' | 'P' | 'd' | 'D' | 'n' | 'N' | '=' | 'h' | 'H'
      | 'g' | 'G' | 'x' | 'z' => true,
      'b' | 't' | 'T' | 'q' | 'Q' | 'l' | 'e' => true,
      'r' | 'R' | 'w' | 'W' => {
        probe.peek_char().is_some_and(|c| c == ' ' || c == '\t' || c == '\r')
      }
      'a' | 'i' | 'c' => probe.peek_char() == Some('\\'),
      's' | 'y' => probe.peek_char().is_some(),
      'F' | 'v' => true,
      ':' => !matches!(probe.peek_char(), None | Some('\n' | ';' | '}')),
      _ => false,
    }
  }
}

impl SedCommand {
  pub fn parse_invocation(args: &[String]) -> io::Result<Self> {
    let mut quiet = false;
    let mut extended_regex = false;
    let mut scripts = Vec::new();
    let mut files = Vec::new();
    let mut index = 0;

    while index < args.len() {
      match args[index].as_str() {
        "-n" => {
          quiet = true;
          index += 1;
        }
        "-E" | "-r" | "--regexp-extended" => {
          extended_regex = true;
          index += 1;
        }
        "-e" => {
          let Some(script) = args.get(index + 1) else {
            return Err(invalid_input("missing -e argument"));
          };
          scripts
            .push(SedScriptSource::Expression(parse_nonempty_text(script)?));
          index += 2;
        }
        "-f" => {
          let Some(path) = args.get(index + 1) else {
            return Err(invalid_input("missing -f argument"));
          };
          scripts.push(SedScriptSource::File(SedPath {
            text: parse_nonempty_text(path)?,
          }));
          index += 2;
        }
        value if value.starts_with('-') && value != "-" => {
          return Err(invalid_input("unknown option"));
        }
        _ => {
          if scripts.is_empty() {
            scripts.push(SedScriptSource::Expression(parse_nonempty_text(
              &args[index],
            )?));
            index += 1;
          }
          files.extend(args[index..].iter().cloned());
          break;
        }
      }
    }

    if scripts.is_empty() {
      return Err(invalid_input("missing script"));
    }

    Ok(Self { quiet, extended_regex, scripts, files })
  }

  pub fn parse_plan(&self) -> io::Result<SedRunPlan> {
    self.parse_plan_with_reader(|path| fs::read_to_string(path))
  }

  fn parse_plan_with_ctx(&self, ctx: &AppContext) -> io::Result<SedRunPlan> {
    self.parse_plan_with_reader(|path| {
      io_util::read_to_string(ctx.lio(), Some(path))
    })
  }

  fn parse_plan_with_reader(
    &self,
    mut read_script: impl FnMut(&str) -> io::Result<String>,
  ) -> io::Result<SedRunPlan> {
    let mut sources = Vec::with_capacity(self.scripts.len());
    for source in &self.scripts {
      match source {
        SedScriptSource::Expression(text) => sources.push(text.to_string()),
        SedScriptSource::File(path) => {
          sources.push(read_script(&path.as_str())?)
        }
      }
    }

    Ok(SedRunPlan {
      quiet: self.quiet,
      script: SedScript::parse_many(sources.iter().map(String::as_str))?,
      files: self.files.clone(),
    })
  }

  fn execute_plan(
    &self,
    ctx: &AppContext,
    plan: &SedRunPlan,
  ) -> io::Result<Vec<u8>> {
    let inputs = load_runtime_inputs(ctx, &plan.files)?;
    let program = SedCompiledProgram::compile(&plan.script)?;
    SedRuntime::default().run_program(ctx, &program, &inputs, plan.quiet)
  }
}

fn parse_nonempty_text(value: &str) -> io::Result<NonEmptyText> {
  let mut chars = value.chars();
  let head =
    chars.next().ok_or_else(|| invalid_input("expected non-empty text"))?;
  Ok(NonEmptyText { head, tail: chars.collect() })
}

fn invalid_input(message: &str) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidInput, message)
}

impl NonEmptyText {
  pub fn as_str(&self) -> String {
    let mut out = String::with_capacity(self.tail.len() + self.head.len_utf8());
    out.push(self.head);
    out.push_str(&self.tail);
    out
  }
}

impl std::fmt::Display for NonEmptyText {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.as_str())
  }
}

impl SedPath {
  pub fn as_str(&self) -> String {
    self.text.as_str()
  }
}

impl SedCompiledProgram {
  fn compile(script: &SedScript) -> io::Result<Self> {
    let mut compiler =
      SedCompiler { statements: Vec::new(), next_selector_state_id: 0 };
    compiler.compile_statements(&script.statements, &[])?;
    compiler.resolve_branches()?;
    Ok(Self { statements: compiler.statements })
  }
}

struct SedCompiler {
  statements: Vec<SedCompiledStatement>,
  next_selector_state_id: usize,
}

impl SedCompiler {
  fn compile_statements(
    &mut self,
    statements: &[SedStatement],
    inherited_guards: &[SedSelectorGuard],
  ) -> io::Result<()> {
    for statement in statements {
      let mut guards = inherited_guards.to_vec();
      for guard in &mut guards {
        guard.sticky = true;
      }
      if !matches!(statement.selector, SedSelector::Any) {
        guards.push(SedSelectorGuard {
          state_id: self.next_selector_state_id,
          selector: statement.selector.clone(),
          sticky: !inherited_guards.is_empty(),
        });
        self.next_selector_state_id += 1;
      }

      match &statement.command {
        SedStatementKind::Block(inner) => {
          self.compile_statements(inner, &guards)?
        }
        SedStatementKind::Label(label) => {
          self.statements.push(SedCompiledStatement {
            guards,
            op: SedCompiledOp::Label(label.text.to_string()),
          });
        }
        other => {
          self.statements.push(SedCompiledStatement {
            guards,
            op: SedCompiledOp::Statement(other.clone()),
          });
        }
      }
    }
    Ok(())
  }

  fn resolve_branches(&mut self) -> io::Result<()> {
    let mut labels = std::collections::HashMap::new();
    for (index, statement) in self.statements.iter().enumerate() {
      if let SedCompiledOp::Label(name) = &statement.op {
        labels.entry(name.clone()).or_insert(index);
      }
    }

    for statement in &mut self.statements {
      let SedCompiledOp::Statement(SedStatementKind::Branch(branch)) =
        &mut statement.op
      else {
        continue;
      };
      if let SedBranchTarget::Label(label) = &branch.target {
        if !labels.contains_key(&label.text.to_string()) {
          return Err(invalid_input("unknown branch label"));
        }
      }
    }
    Ok(())
  }
}

impl SedRuntime {
  fn run_program(
    mut self,
    ctx: &AppContext,
    program: &SedCompiledProgram,
    inputs: &[SedRuntimeInput],
    quiet: bool,
  ) -> io::Result<Vec<u8>> {
    let mut output = String::new();
    let mut index = 0usize;
    let label_targets = collect_label_targets(program);

    while index < inputs.len() {
      let input = &inputs[index];
      let mut cycle = SedCycleState {
        pattern_space: input.text.clone(),
        current_file: input.file.clone(),
        line_number: input.line_number,
        is_last: input.is_last,
        next_input_index: index + 1,
        append_queue: Vec::new(),
        suppressed_default_print: false,
        substituted: false,
        sticky_guard_matches: std::collections::HashMap::new(),
      };

      let mut pc = 0usize;
      let mut quit = false;

      while pc < program.statements.len() {
        let statement = &program.statements[pc];
        if !self.statement_matches(statement, &mut cycle)? {
          pc += 1;
          continue;
        }

        let flow = self.execute_statement(
          ctx,
          statement,
          &mut cycle,
          &mut output,
          program,
          inputs,
          &label_targets,
          quiet,
          &mut pc,
        )?;

        match flow {
          SedFlow::Continue => pc += 1,
          SedFlow::RestartCycle => {
            pc = 0;
            cycle.substituted = false;
            cycle.suppressed_default_print = false;
            cycle.sticky_guard_matches.clear();
          }
          SedFlow::NextCycle => break,
          SedFlow::Quit => {
            quit = true;
            break;
          }
        }
      }

      if !cycle.suppressed_default_print && !quiet {
        output.push_str(&cycle.pattern_space);
      }
      for queued in cycle.append_queue {
        output.push_str(&queued);
      }

      index = cycle.next_input_index;
      if quit {
        break;
      }
    }

    Ok(output.into_bytes())
  }

  fn statement_matches(
    &mut self,
    statement: &SedCompiledStatement,
    cycle: &mut SedCycleState,
  ) -> io::Result<bool> {
    for guard in &statement.guards {
      if !self.selector_matches(guard, cycle)? {
        return Ok(false);
      }
    }
    Ok(true)
  }

  fn selector_matches(
    &mut self,
    guard: &SedSelectorGuard,
    cycle: &mut SedCycleState,
  ) -> io::Result<bool> {
    if guard.sticky {
      if let Some(matched) = cycle.sticky_guard_matches.get(&guard.state_id) {
        return Ok(*matched);
      }
    }
    match &guard.selector {
      SedSelector::Any => Ok(true),
      SedSelector::Addressed { addresses, negated } => {
        let matched = match addresses {
          SedAddresses::Single(single) => {
            self.single_address_matches(single, cycle)?
          }
          SedAddresses::Range(range) => {
            self.range_address_matches(guard.state_id, range, cycle)?
          }
        };
        let matched = if *negated { !matched } else { matched };
        if guard.sticky {
          cycle.sticky_guard_matches.insert(guard.state_id, matched);
        }
        Ok(matched)
      }
    }
  }

  fn single_address_matches(
    &mut self,
    address: &SedSingleAddress,
    cycle: &SedCycleState,
  ) -> io::Result<bool> {
    match address {
      SedSingleAddress::Line(line) => Ok(cycle.line_number == line.get()),
      SedSingleAddress::LastLine => Ok(cycle.is_last),
      SedSingleAddress::Regex(regex) => {
        let resolved = self.resolve_regex(regex)?;
        Ok(
          compile_regex(&resolved, false, false)?
            .is_match(strip_terminal_newline(&cycle.pattern_space)),
        )
      }
      SedSingleAddress::Step(step) => {
        let first = match step.first {
          SedStepStart::Zero => 0,
          SedStepStart::Line(line) => line.get(),
        };
        let line = cycle.line_number;
        Ok(line >= first && (line - first) % step.step.get() == 0)
      }
    }
  }

  fn range_address_matches(
    &mut self,
    state_id: usize,
    range: &SedRangeAddress,
    cycle: &SedCycleState,
  ) -> io::Result<bool> {
    if self.range_states.len() <= state_id {
      self.range_states.resize_with(state_id + 1, SedRangeState::default);
    }

    self.range_states[state_id].just_started = false;
    let state_active = self.range_states[state_id].active;
    let start_matches = if state_active {
      false
    } else {
      match &range.start {
        SedRangeStart::Zero => !self.range_states[state_id].zero_consumed,
        SedRangeStart::Address(address) => {
          self.single_address_matches(address, cycle)?
        }
      }
    };

    let mut started_this_line = false;
    if start_matches {
      let state = &mut self.range_states[state_id];
      state.active = true;
      state.start_line = Some(cycle.line_number);
      state.just_started = true;
      if matches!(range.start, SedRangeStart::Zero) {
        state.zero_consumed = true;
      }
      started_this_line = true;
    }

    if !self.range_states[state_id].active {
      return Ok(false);
    }

    let start_line = self.range_states[state_id].start_line;
    let should_end = match &range.end {
      SedRangeEnd::Address(address) => {
        if started_this_line {
          matches!(address, SedSingleAddress::Line(line) if line.get() == cycle.line_number)
        } else {
          self.single_address_matches(address, cycle)?
        }
      }
      SedRangeEnd::NextLines(count) => {
        start_line.is_some_and(|start| cycle.line_number >= start + count.get())
      }
      SedRangeEnd::NextMultiple(count) => cycle.line_number % count.get() == 0,
    };

    if should_end {
      let state = &mut self.range_states[state_id];
      state.active = false;
      state.start_line = None;
    }

    Ok(true)
  }

  #[allow(clippy::too_many_arguments)]
  fn execute_statement(
    &mut self,
    ctx: &AppContext,
    statement: &SedCompiledStatement,
    cycle: &mut SedCycleState,
    output: &mut String,
    _program: &SedCompiledProgram,
    inputs: &[SedRuntimeInput],
    label_targets: &std::collections::HashMap<String, usize>,
    quiet: bool,
    pc: &mut usize,
  ) -> io::Result<SedFlow> {
    match &statement.op {
      SedCompiledOp::Label(_) => Ok(SedFlow::Continue),
      SedCompiledOp::Statement(kind) => match kind {
        SedStatementKind::Substitute(substitute) => {
          let changed = apply_substitute(
            ctx,
            &mut cycle.pattern_space,
            substitute,
            self.resolve_regex(&SedRegex {
              delimiter: substitute.delimiter,
              pattern: substitute.pattern.clone(),
            })?,
          )?;
          cycle.substituted |= changed;
          if changed && substitute.flags.print {
            output.push_str(&cycle.pattern_space);
          }
          Ok(SedFlow::Continue)
        }
        SedStatementKind::Transliterate(transliterate) => {
          transliterate_pattern(&mut cycle.pattern_space, transliterate);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::AppendText(block) => {
          cycle.append_queue.push(render_text_block(block));
          Ok(SedFlow::Continue)
        }
        SedStatementKind::InsertText(block) => {
          output.push_str(&render_text_block(block));
          Ok(SedFlow::Continue)
        }
        SedStatementKind::ChangeText(block) => {
          if self.should_emit_change_text(statement) {
            output.push_str(&render_text_block(block));
          }
          cycle.append_queue.clear();
          cycle.suppressed_default_print = true;
          Ok(SedFlow::NextCycle)
        }
        SedStatementKind::Branch(branch) => {
          self.execute_branch(branch, cycle, label_targets, pc)
        }
        SedStatementKind::PrintPattern => {
          output.push_str(&cycle.pattern_space);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::PrintFirstLine => {
          output.push_str(first_line(&cycle.pattern_space));
          Ok(SedFlow::Continue)
        }
        SedStatementKind::DeletePattern => {
          cycle.append_queue.clear();
          cycle.suppressed_default_print = true;
          Ok(SedFlow::NextCycle)
        }
        SedStatementKind::DeleteFirstLine => {
          cycle.append_queue.clear();
          cycle.suppressed_default_print = true;
          if delete_first_line(&mut cycle.pattern_space) {
            Ok(SedFlow::RestartCycle)
          } else {
            Ok(SedFlow::NextCycle)
          }
        }
        SedStatementKind::NextLine => {
          self.execute_next_line(cycle, output, quiet, inputs)
        }
        SedStatementKind::AppendNextLine => {
          self.execute_append_next_line(cycle, inputs)
        }
        SedStatementKind::PrintLineNumber => {
          output.push_str(&format!("{}\n", cycle.line_number));
          Ok(SedFlow::Continue)
        }
        SedStatementKind::CopyPatternToHold => {
          self.hold_space = cycle.pattern_space.clone();
          Ok(SedFlow::Continue)
        }
        SedStatementKind::AppendPatternToHold => {
          self.hold_space.push('\n');
          self.hold_space.push_str(&cycle.pattern_space);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::CopyHoldToPattern => {
          cycle.pattern_space = self.hold_space.clone();
          Ok(SedFlow::Continue)
        }
        SedStatementKind::AppendHoldToPattern => {
          cycle.pattern_space.push('\n');
          cycle.pattern_space.push_str(&self.hold_space);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::ExchangePatternAndHold => {
          std::mem::swap(&mut cycle.pattern_space, &mut self.hold_space);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::ZapPattern => {
          cycle.pattern_space.clear();
          Ok(SedFlow::Continue)
        }
        SedStatementKind::ListPattern { wrap } => {
          output.push_str(&render_list_pattern(&cycle.pattern_space, *wrap));
          Ok(SedFlow::Continue)
        }
        SedStatementKind::Quit(quit) => {
          cycle.suppressed_default_print = true;
          if matches!(quit.kind, SedQuitKind::PrintPattern) && !quiet {
            output.push_str(&cycle.pattern_space);
          }
          Ok(SedFlow::Quit)
        }
        SedStatementKind::ReadFile { path } => {
          cycle
            .append_queue
            .push(io_util::read_to_string(ctx.lio(), Some(&path.as_str()))?);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::ReadLineFromFile { path } => {
          if !self.read_line_states.contains_key(pc) {
            self.read_line_states.insert(
              *pc,
              SedQueuedLines {
                lines: split_preserving_lines(&io_util::read_to_string(
                  ctx.lio(),
                  Some(&path.as_str()),
                )?),
                offset: 0,
              },
            );
          }
          let entry = self
            .read_line_states
            .get_mut(pc)
            .expect("read line state must exist");
          if let Some(line) = entry.lines.get(entry.offset) {
            cycle.append_queue.push(line.clone());
            entry.offset += 1;
          }
          Ok(SedFlow::Continue)
        }
        SedStatementKind::WriteFile { path } => {
          append_to_path(ctx, path, &cycle.pattern_space)?;
          Ok(SedFlow::Continue)
        }
        SedStatementKind::WriteFirstLine { path } => {
          append_to_path(ctx, path, first_line(&cycle.pattern_space))?;
          Ok(SedFlow::Continue)
        }
        SedStatementKind::PrintCurrentFile => {
          output.push_str(&cycle.current_file);
          output.push('\n');
          Ok(SedFlow::Continue)
        }
        SedStatementKind::VersionCheck { version: _ } => Ok(SedFlow::Continue),
        SedStatementKind::Execute(command) => {
          cycle
            .append_queue
            .push(execute_shell_command(command, &cycle.pattern_space)?);
          Ok(SedFlow::Continue)
        }
        SedStatementKind::Comment(_) => Ok(SedFlow::Continue),
        SedStatementKind::Block(_) | SedStatementKind::Label(_) => {
          Ok(SedFlow::Continue)
        }
      },
    }
  }

  fn execute_branch(
    &mut self,
    branch: &SedBranch,
    cycle: &mut SedCycleState,
    label_targets: &std::collections::HashMap<String, usize>,
    pc: &mut usize,
  ) -> io::Result<SedFlow> {
    let should_branch = match branch.kind {
      SedBranchKind::Unconditional => true,
      SedBranchKind::OnSubstitute => cycle.substituted,
      SedBranchKind::OnNoSubstitute => !cycle.substituted,
    };
    if !matches!(branch.kind, SedBranchKind::Unconditional) {
      cycle.substituted = false;
    }
    if !should_branch {
      return Ok(SedFlow::Continue);
    }

    match &branch.target {
      SedBranchTarget::EndOfScript => Ok(SedFlow::NextCycle),
      SedBranchTarget::Label(label) => {
        *pc = *label_targets
          .get(&label.text.to_string())
          .ok_or_else(|| invalid_input("unknown branch label"))?;
        Ok(SedFlow::Continue)
      }
    }
  }

  fn execute_next_line(
    &mut self,
    cycle: &mut SedCycleState,
    output: &mut String,
    quiet: bool,
    inputs: &[SedRuntimeInput],
  ) -> io::Result<SedFlow> {
    if !quiet {
      output.push_str(&cycle.pattern_space);
    }
    cycle.suppressed_default_print = true;
    let Some(next) = inputs.get(cycle.next_input_index) else {
      return Ok(SedFlow::Quit);
    };
    cycle.pattern_space = next.text.clone();
    cycle.current_file = next.file.clone();
    cycle.line_number = next.line_number;
    cycle.is_last = next.is_last;
    cycle.next_input_index += 1;
    cycle.suppressed_default_print = false;
    cycle.substituted = false;
    Ok(SedFlow::Continue)
  }

  fn execute_append_next_line(
    &mut self,
    cycle: &mut SedCycleState,
    inputs: &[SedRuntimeInput],
  ) -> io::Result<SedFlow> {
    let Some(next) = inputs.get(cycle.next_input_index) else {
      cycle.suppressed_default_print = true;
      return Ok(SedFlow::Quit);
    };
    if !cycle.pattern_space.is_empty() && !cycle.pattern_space.ends_with('\n') {
      cycle.pattern_space.push('\n');
    }
    cycle.pattern_space.push_str(&next.text);
    cycle.current_file = next.file.clone();
    cycle.line_number = next.line_number;
    cycle.is_last = next.is_last;
    cycle.next_input_index += 1;
    cycle.substituted = false;
    Ok(SedFlow::Continue)
  }

  fn should_emit_change_text(&self, statement: &SedCompiledStatement) -> bool {
    for guard in &statement.guards {
      if matches!(
        guard.selector,
        SedSelector::Addressed { addresses: SedAddresses::Range(_), .. }
      ) {
        return self
          .range_states
          .get(guard.state_id)
          .is_some_and(|state| state.just_started);
      }
    }
    true
  }

  fn resolve_regex(&mut self, regex: &SedRegex) -> io::Result<SedRegex> {
    if regex.pattern.is_empty() {
      self
        .last_regex
        .clone()
        .ok_or_else(|| invalid_input("no previous regular expression"))
    } else {
      self.last_regex = Some(regex.clone());
      Ok(regex.clone())
    }
  }
}

fn collect_label_targets(
  program: &SedCompiledProgram,
) -> std::collections::HashMap<String, usize> {
  let mut labels = std::collections::HashMap::new();
  for (index, statement) in program.statements.iter().enumerate() {
    if let SedCompiledOp::Label(name) = &statement.op {
      labels.entry(name.clone()).or_insert(index);
    }
  }
  labels
}

fn load_runtime_inputs(
  ctx: &AppContext,
  files: &[String],
) -> io::Result<Vec<SedRuntimeInput>> {
  let mut inputs = Vec::new();
  let files =
    if files.is_empty() { vec!["-".to_string()] } else { files.to_vec() };

  let mut line_number = 1u64;
  for file in files {
    let content = if file == "-" {
      io_util::read_to_string_fd(ctx.lio(), &ctx.stdin())?
    } else {
      io_util::read_to_string(ctx.lio(), Some(&file))?
    };
    for line in split_preserving_lines(&content) {
      inputs.push(SedRuntimeInput {
        text: line,
        file: file.clone(),
        line_number,
        is_last: false,
      });
      line_number += 1;
    }
  }
  if let Some(last) = inputs.last_mut() {
    last.is_last = true;
  }
  Ok(inputs)
}

fn split_preserving_lines(input: &str) -> Vec<String> {
  let mut lines: Vec<String> =
    input.split_inclusive('\n').map(str::to_owned).collect();
  if lines.is_empty() && !input.is_empty() {
    lines.push(input.to_string());
  }
  lines
}

#[cfg(test)]
fn build_runtime_inputs(input: &str) -> Vec<SedRuntimeInput> {
  let mut inputs = Vec::new();
  for (index, line) in split_preserving_lines(input).into_iter().enumerate() {
    inputs.push(SedRuntimeInput {
      text: line,
      file: "-".into(),
      line_number: index as u64 + 1,
      is_last: false,
    });
  }
  if let Some(last) = inputs.last_mut() {
    last.is_last = true;
  }
  inputs
}

fn first_line(pattern: &str) -> &str {
  let end = pattern.find('\n').map(|index| index + 1).unwrap_or(pattern.len());
  &pattern[..end]
}

fn strip_terminal_newline(text: &str) -> &str {
  text.strip_suffix('\n').unwrap_or(text)
}

fn delete_first_line(pattern: &mut String) -> bool {
  if let Some(index) = pattern.find('\n') {
    pattern.replace_range(..index + 1, "");
    true
  } else {
    pattern.clear();
    false
  }
}

fn render_text_block(block: &SedTextBlock) -> String {
  let mut out = String::new();
  out.push_str(&block.first);
  out.push('\n');
  for line in &block.rest {
    out.push_str(line);
    out.push('\n');
  }
  out
}

fn render_list_pattern(pattern: &str, wrap: Option<NonZeroU64>) -> String {
  let mut out = String::new();
  let wrap = wrap.map(|value| value.get() as usize);
  for line in pattern.split_inclusive('\n') {
    let stripped = line.strip_suffix('\n').unwrap_or(line);
    let mut rendered = stripped.escape_default().to_string();
    rendered.push('$');
    push_wrapped_list_line(&mut out, &rendered, wrap);
  }
  if !pattern.ends_with('\n') {
    push_wrapped_list_line(&mut out, "$", wrap);
  }
  out
}

fn push_wrapped_list_line(
  out: &mut String,
  rendered: &str,
  wrap: Option<usize>,
) {
  let Some(width) = wrap else {
    out.push_str(rendered);
    out.push('\n');
    return;
  };

  if rendered.len() <= width {
    out.push_str(rendered);
    out.push('\n');
    return;
  }

  let segment_len = width.saturating_sub(1).max(1);
  let mut remaining = rendered;
  while remaining.len() > width {
    let split_at = segment_len.min(remaining.len());
    out.push_str(&remaining[..split_at]);
    out.push('\\');
    out.push('\n');
    remaining = &remaining[split_at..];
  }
  out.push_str(remaining);
  out.push('\n');
}

fn compile_regex(
  regex: &SedRegex,
  ignore_case: bool,
  multi_line: bool,
) -> io::Result<regex::Regex> {
  RegexBuilder::new(&regex.pattern)
    .case_insensitive(ignore_case)
    .multi_line(multi_line)
    .build()
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

fn transliterate_pattern(
  pattern: &mut String,
  transliterate: &SedTransliterate,
) {
  let map: std::collections::HashMap<char, char> =
    transliterate.source.chars().zip(transliterate.target.chars()).collect();
  *pattern =
    pattern.chars().map(|ch| map.get(&ch).copied().unwrap_or(ch)).collect();
}

fn apply_substitute(
  ctx: &AppContext,
  pattern: &mut String,
  substitute: &SedSubstitute,
  resolved_regex: SedRegex,
) -> io::Result<bool> {
  let regex = compile_regex(
    &resolved_regex,
    substitute.flags.ignore_case,
    substitute.flags.multi_line,
  )?;
  validate_replacement_backrefs(&regex, &substitute.replacement)?;

  let replaced = if substitute.flags.global {
    apply_substitute_global(
      &regex,
      pattern,
      &substitute.replacement,
      substitute
        .flags
        .occurrence
        .map(|value| value.get() as usize)
        .unwrap_or(1),
    )
  } else if let Some(occurrence) = substitute.flags.occurrence {
    apply_substitute_nth(
      &regex,
      pattern,
      &substitute.replacement,
      occurrence.get() as usize,
    )
  } else {
    apply_substitute_nth(&regex, pattern, &substitute.replacement, 1)
  };

  if let Some((mut updated, changed)) = replaced {
    if changed && substitute.flags.evaluate {
      updated = strip_trailing_command_newlines(&execute_shell_command(
        &SedExecute::PatternSpace,
        &updated,
      )?);
    }
    *pattern = updated;
    if substitute.flags.print && changed {
      // Print flag is handled by execution caller via changed status if needed later.
    }
    if let Some(path) = &substitute.flags.write {
      append_to_path(ctx, path, pattern)?;
    }
    Ok(changed)
  } else {
    Ok(false)
  }
}

fn validate_replacement_backrefs(
  regex: &regex::Regex,
  replacement: &str,
) -> io::Result<()> {
  let mut chars = replacement.chars().peekable();
  let max_group = regex.captures_len().saturating_sub(1);
  while let Some(ch) = chars.next() {
    if ch != '\\' {
      continue;
    }
    if let Some(next) = chars.peek().copied() {
      if next.is_ascii_digit() {
        let index = chars.next().unwrap().to_digit(10).unwrap() as usize;
        if index > max_group {
          return Err(invalid_input("undefined backreference"));
        }
      } else {
        chars.next();
      }
    }
  }
  Ok(())
}

fn apply_substitute_global(
  regex: &regex::Regex,
  input: &str,
  replacement: &str,
  start_at: usize,
) -> Option<(String, bool)> {
  let mut out = String::new();
  let mut last_end = 0;
  let mut changed = false;
  let mut match_index = 0usize;
  for caps in regex.captures_iter(input) {
    let m = caps.get(0)?;
    out.push_str(&input[last_end..m.start()]);
    match_index += 1;
    if match_index >= start_at {
      out.push_str(&render_replacement(replacement, &caps));
      changed = true;
    } else {
      out.push_str(m.as_str());
    }
    last_end = m.end();
    if m.start() == m.end() && last_end < input.len() {
      out.push_str(&input[last_end..last_end + 1]);
      last_end += 1;
    }
  }
  if !changed {
    return None;
  }
  out.push_str(&input[last_end..]);
  Some((out, true))
}

fn strip_trailing_command_newlines(output: &str) -> String {
  output.trim_end_matches('\n').to_string()
}

fn apply_substitute_nth(
  regex: &regex::Regex,
  input: &str,
  replacement: &str,
  nth: usize,
) -> Option<(String, bool)> {
  let mut out = String::new();
  let last_end = 0;
  let mut current = 0usize;
  let mut changed = false;
  for caps in regex.captures_iter(input) {
    let m = caps.get(0)?;
    current += 1;
    if current == nth {
      out.push_str(&input[last_end..m.start()]);
      out.push_str(&render_replacement(replacement, &caps));
      out.push_str(&input[m.end()..]);
      changed = true;
      break;
    }
  }
  if !changed {
    return None;
  }
  Some((out, true))
}

fn render_replacement(replacement: &str, caps: &regex::Captures<'_>) -> String {
  let mut out = String::new();
  let mut chars = replacement.chars().peekable();
  while let Some(ch) = chars.next() {
    match ch {
      '&' => out.push_str(caps.get(0).map(|m| m.as_str()).unwrap_or("")),
      '\\' => match chars.peek().copied() {
        Some(next) if next.is_ascii_digit() => {
          let index = chars.next().unwrap().to_digit(10).unwrap() as usize;
          if let Some(group) = caps.get(index) {
            out.push_str(group.as_str());
          }
        }
        Some(next) => {
          out.push(next);
          chars.next();
        }
        None => out.push('\\'),
      },
      _ => out.push(ch),
    }
  }
  out
}

fn append_to_path(
  ctx: &AppContext,
  path: &SedPath,
  contents: &str,
) -> io::Result<()> {
  let cpath = std::ffi::CString::new(path.as_str())?;
  let fd = io_util::run(
    ctx.lio(),
    lio::api::openat(
      &ctx.cwd(),
      cpath,
      libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
      0o666,
    )
    .with_lio(ctx.lio())
    .send(),
  )?;
  io_util::write_all(ctx.lio(), &fd, contents.as_bytes().to_vec())
}

fn execute_shell_command(
  command: &SedExecute,
  pattern_space: &str,
) -> io::Result<String> {
  let command = match command {
    SedExecute::PatternSpace => pattern_space.to_string(),
    SedExecute::Command(command) => command.to_string(),
  };
  let output =
    std::process::Command::new("sh").arg("-c").arg(&command).output()?;
  Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl Command for SedCommand {
  fn name() -> &'static str {
    "sed"
  }

  fn summary() -> &'static str {
    "Stream editor."
  }

  fn usage() -> &'static str {
    "sed [-n] [-e script]... [script] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Self::parse_invocation(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let plan = self.parse_plan_with_ctx(ctx)?;
    let output = self.execute_plan(ctx, &plan)?;
    io_util::write_all(ctx.lio(), &ctx.stdout(), output)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
  };

  fn nz(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
  }

  fn text(value: &str) -> NonEmptyText {
    let mut chars = value.chars();
    let head = chars.next().unwrap();
    NonEmptyText { head, tail: chars.collect() }
  }

  fn expr(script: &str) -> SedScriptSource {
    SedScriptSource::Expression(text(script))
  }

  fn temp_script_path(name: &str) -> PathBuf {
    let nanos =
      SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    env::temp_dir().join(format!("lio-sed-{name}-{nanos}.sed"))
  }

  fn run_script(script: &str, input: &str, quiet: bool) -> String {
    let script = SedScript::parse(script).unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs(input);
    run_program_on_inputs(&program, &inputs, quiet)
  }

  fn run_script_err(script: &str, input: &str, quiet: bool) -> io::Error {
    let script = SedScript::parse(script).unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs(input);
    let ctx = AppContext::new().unwrap();
    SedRuntime::default()
      .run_program(&ctx, &program, &inputs, quiet)
      .unwrap_err()
  }

  fn run_program_on_inputs(
    program: &SedCompiledProgram,
    inputs: &[SedRuntimeInput],
    quiet: bool,
  ) -> String {
    let ctx = AppContext::new().unwrap();
    String::from_utf8(
      SedRuntime::default().run_program(&ctx, program, inputs, quiet).unwrap(),
    )
    .unwrap()
  }

  fn named_inputs(file: &str, input: &str) -> Vec<SedRuntimeInput> {
    let mut inputs = build_runtime_inputs(input);
    for entry in &mut inputs {
      entry.file = file.into();
    }
    inputs
  }

  fn line(value: u64) -> SedSingleAddress {
    SedSingleAddress::Line(nz(value))
  }

  fn path(value: &str) -> SedPath {
    SedPath { text: text(value) }
  }

  fn label(value: &str) -> SedLabel {
    SedLabel { text: text(value) }
  }

  fn regex(delimiter: char, pattern: &str) -> SedSingleAddress {
    SedSingleAddress::Regex(SedRegex { delimiter, pattern: pattern.into() })
  }

  fn single(address: SedSingleAddress) -> SedSelector {
    SedSelector::Addressed {
      addresses: SedAddresses::Single(address),
      negated: false,
    }
  }

  fn negated(address: SedSingleAddress) -> SedSelector {
    SedSelector::Addressed {
      addresses: SedAddresses::Single(address),
      negated: true,
    }
  }

  fn range(start: SedRangeStart, end: SedRangeEnd) -> SedSelector {
    SedSelector::Addressed {
      addresses: SedAddresses::Range(SedRangeAddress { start, end }),
      negated: false,
    }
  }

  fn sub(
    delimiter: char,
    pattern: &str,
    replacement: &str,
    flags: SedSubstituteFlags,
  ) -> SedStatementKind {
    SedStatementKind::Substitute(SedSubstitute {
      delimiter,
      pattern: pattern.into(),
      replacement: replacement.into(),
      flags,
    })
  }

  #[test]
  fn parse_invocation_supports_quiet_scripts_and_files() {
    let parsed = SedCommand::parse_invocation(&[
      "-n".into(),
      "-e".into(),
      "s/foo/bar/g".into(),
      "-e".into(),
      "1,3d".into(),
      "input.txt".into(),
      "more.txt".into(),
    ])
    .unwrap();

    assert_eq!(
      parsed,
      SedCommand {
        quiet: true,
        extended_regex: false,
        scripts: vec![expr("s/foo/bar/g"), expr("1,3d")],
        files: vec!["input.txt".into(), "more.txt".into()],
      }
    );
  }

  #[test]
  fn parse_invocation_treats_first_non_flag_as_script() {
    let parsed =
      SedCommand::parse_invocation(&["s/foo/bar/".into(), "input.txt".into()])
        .unwrap();

    assert_eq!(
      parsed,
      SedCommand {
        quiet: false,
        extended_regex: false,
        scripts: vec![expr("s/foo/bar/")],
        files: vec!["input.txt".into()],
      }
    );
  }

  #[test]
  fn parse_plan_combines_expression_and_file_sources() {
    let script_path = temp_script_path("plan-combines");
    fs::write(&script_path, "2d\n3q").unwrap();

    let command = SedCommand {
      quiet: true,
      extended_regex: false,
      scripts: vec![
        expr("1p"),
        SedScriptSource::File(SedPath {
          text: text(script_path.to_str().unwrap()),
        }),
      ],
      files: vec!["input.txt".into(), "more.txt".into()],
    };

    let plan = command.parse_plan().unwrap();
    let _ = fs::remove_file(&script_path);

    assert_eq!(
      plan,
      SedRunPlan {
        quiet: true,
        script: SedScript {
          statements: vec![
            SedStatement {
              selector: single(line(1)),
              command: SedStatementKind::PrintPattern,
            },
            SedStatement {
              selector: single(line(2)),
              command: SedStatementKind::DeletePattern,
            },
            SedStatement {
              selector: single(line(3)),
              command: SedStatementKind::Quit(SedQuit {
                kind: SedQuitKind::PrintPattern,
                code: None,
              }),
            },
          ],
        },
        files: vec!["input.txt".into(), "more.txt".into()],
      }
    );
  }

  #[test]
  fn parse_plan_preserves_empty_file_list_for_stdin() {
    let command = SedCommand {
      quiet: false,
      extended_regex: false,
      scripts: vec![expr("p")],
      files: vec![],
    };

    let plan = command.parse_plan().unwrap();
    assert!(plan.files.is_empty());
    assert_eq!(plan.script, SedScript::parse("p").unwrap());
  }

  #[test]
  fn execute_default_print_and_delete_behave_per_cycle() {
    assert_eq!(run_script("d", "a\nb\n", false), "");
    assert_eq!(run_script("p", "a\nb\n", false), "a\na\nb\nb\n");
    assert_eq!(run_script("p", "a\nb\n", true), "a\nb\n");
  }

  #[test]
  fn execute_substitute_and_address_filters_apply() {
    assert_eq!(
      run_script("s/foo/bar/g", "foo foo\nbaz\n", false),
      "bar bar\nbaz\n"
    );
    assert_eq!(run_script("2s/foo/bar/", "foo\nfoo\n", false), "foo\nbar\n");
    assert_eq!(run_script("/skip/d", "keep\nskip\n", false), "keep\n");
  }

  #[test]
  fn execute_insert_append_and_change_emit_expected_text() {
    assert_eq!(run_script("1i\\\nHEAD", "body\n", false), "HEAD\nbody\n");
    assert_eq!(run_script("a\\\nTAIL", "body\n", false), "body\nTAIL\n");
    assert_eq!(run_script("c\\\nREPLACED", "body\n", false), "REPLACED\n");
  }

  #[test]
  fn execute_hold_space_and_transliterate_commands_work() {
    assert_eq!(run_script("h;g", "abc\n", false), "abc\n");
    assert_eq!(run_script("H;g", "abc\n", false), "\nabc\n");
    assert_eq!(run_script("h;G", "abc\n", false), "abc\n\nabc\n");
    assert_eq!(run_script("y/abc/xyz/", "cab\n", false), "zxy\n");
    assert_eq!(run_script("z", "abc\n", false), "");
  }

  #[test]
  fn execute_branch_and_line_number_commands_work() {
    assert_eq!(run_script("s/a/b/;t done;d;:done;p", "a\nx\n", true), "b\n");
    assert_eq!(run_script("=", "one\ntwo\n", false), "1\none\n2\ntwo\n");
    assert_eq!(
      run_script("s/a/b/;t done;:done;T fail;p;:fail;d", "a\n", true),
      ""
    );
  }

  #[test]
  fn execute_next_and_multiline_pattern_commands_work() {
    assert_eq!(run_script("n;p", "one\ntwo\n", false), "one\ntwo\ntwo\n");
    assert_eq!(run_script("N;P;D", "one\ntwo\n", true), "one\n");
  }

  #[test]
  fn execute_substitute_print_flag_and_write_first_line_work() {
    assert_eq!(run_script("s/foo/bar/p", "foo\n", false), "bar\nbar\n");

    let out_path = temp_script_path("write-first-line");
    let script =
      SedScript::parse(&format!("N\nW {}", out_path.display())).unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs("one\ntwo\n");
    let ctx = AppContext::new().unwrap();
    let _ =
      SedRuntime::default().run_program(&ctx, &program, &inputs, true).unwrap();
    let written = fs::read_to_string(&out_path).unwrap();
    let _ = fs::remove_file(&out_path);
    assert_eq!(written, "one\n");
  }

  #[test]
  fn execute_step_and_range_addresses_work() {
    assert_eq!(run_script("1~2p", "a\nb\nc\nd\n", true), "a\nc\n");
    assert_eq!(run_script("2,3d", "a\nb\nc\nd\n", false), "a\nd\n");
    assert_eq!(run_script("0,/^$/d", "a\n\nb\n", false), "b\n");
    assert_eq!(run_script("2,+1d", "a\nb\nc\nd\n", false), "a\nd\n");
    assert_eq!(run_script("2,~3d", "a\nb\nc\nd\ne\n", false), "a\nd\ne\n");
    assert_eq!(run_script("$p", "a\nb\n", true), "b\n");
    assert_eq!(run_script("/skip/!p", "keep\nskip\n", true), "keep\n");
  }

  #[test]
  fn execute_read_and_write_file_commands_work() {
    let read_path = temp_script_path("read-file");
    let write_path = temp_script_path("write-file");
    fs::write(&read_path, "tail-1\ntail-2\n").unwrap();

    let script = SedScript::parse(&format!(
      "r {}\nw {}",
      read_path.display(),
      write_path.display()
    ))
    .unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs("body\n");
    let output = run_program_on_inputs(&program, &inputs, false);
    let written = fs::read_to_string(&write_path).unwrap();

    let _ = fs::remove_file(&read_path);
    let _ = fs::remove_file(&write_path);

    assert_eq!(output, "body\ntail-1\ntail-2\n");
    assert_eq!(written, "body\n");
  }

  #[test]
  fn execute_read_line_from_file_consumes_one_line_per_cycle() {
    let read_path = temp_script_path("read-line-file");
    fs::write(&read_path, "x\ny\n").unwrap();

    let script =
      SedScript::parse(&format!("R {}", read_path.display())).unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs("a\nb\n");
    let output = run_program_on_inputs(&program, &inputs, false);

    let _ = fs::remove_file(&read_path);

    assert_eq!(output, "a\nx\nb\ny\n");
  }

  #[test]
  fn execute_print_current_file_and_list_pattern_work() {
    let script = SedScript::parse("F\nl").unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = named_inputs("sample.txt", "a\tb\n");
    let output = run_program_on_inputs(&program, &inputs, true);
    assert_eq!(output, "sample.txt\na\\tb$\n");
  }

  #[test]
  fn execute_quit_and_no_substitute_branch_work() {
    assert_eq!(run_script("q", "a\nb\n", false), "a\n");
    assert_eq!(run_script("Q", "a\nb\n", false), "");
    assert_eq!(run_script("s/x/y/;T no;d;:no;p", "x\nz\n", true), "z\n");
  }

  #[test]
  fn gnu_change_command_replaces_address_range_once() {
    assert_eq!(run_script("2,3c\\\nX", "1\n2\n3\n4\n", false), "1\nX\n4\n");
  }

  #[test]
  fn gnu_substitute_supports_number_and_global_together() {
    let script = SedScript::parse("s/foo/bar/2g").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: SedSelector::Any,
          command: sub(
            '/',
            "foo",
            "bar",
            SedSubstituteFlags {
              global: true,
              occurrence: Some(nz(2)),
              ..SedSubstituteFlags::default()
            },
          ),
        }],
      }
    );
  }

  #[test]
  fn gnu_substitute_number_and_global_replaces_from_nth_match_forward() {
    assert_eq!(
      run_script("s/foo/bar/2g", "foo foo foo\n", false),
      "foo bar bar\n"
    );
  }

  #[test]
  fn gnu_substitute_supports_multi_digit_occurrence_flags() {
    let script = SedScript::parse("s/foo/bar/12").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: SedSelector::Any,
          command: sub(
            '/',
            "foo",
            "bar",
            SedSubstituteFlags {
              occurrence: Some(nz(12)),
              ..SedSubstituteFlags::default()
            },
          ),
        }],
      }
    );
  }

  #[test]
  fn gnu_substitute_multi_digit_occurrence_replaces_only_that_match() {
    let input = "x x x x x x x x x x x x\n".replace('x', "foo");
    let expected = "foo foo foo foo foo foo foo foo foo foo foo bar\n";
    assert_eq!(run_script("s/foo/bar/12", &input, false), expected);
  }

  #[test]
  fn gnu_substitute_evaluates_shell_command_from_replacement() {
    assert_eq!(run_script("s/.*/printf hi/e", "x\n", false), "hi");
  }

  #[test]
  fn gnu_substitute_e_and_p_print_evaluated_result_once_in_quiet_mode() {
    assert_eq!(run_script("s/.*/printf hi/ep", "x\n", true), "hi");
  }

  #[test]
  fn gnu_substitute_backreferences_and_ampersand_work_in_replacement() {
    assert_eq!(
      run_script(r"s/(foo) (bar)/[\1]-<&>-\&/", "foo bar\n", false),
      "[foo]-<foo bar>-&\n"
    );
  }

  #[test]
  fn gnu_d_command_discards_pending_append_text_for_that_cycle() {
    assert_eq!(run_script("a\\\nTAIL\nd", "body\n", false), "");
  }

  #[test]
  fn gnu_empty_substitute_regex_reuses_previous_substitute_pattern() {
    assert_eq!(run_script("s/foo/bar/;s//baz/", "foofoo\n", false), "barbaz\n");
  }

  #[test]
  fn gnu_empty_substitute_regex_can_reuse_previous_address_pattern() {
    assert_eq!(run_script("/foo/s//bar/", "foo\nxxx\n", false), "bar\nxxx\n");
  }

  #[test]
  fn gnu_empty_address_regex_reuses_previous_address_pattern() {
    assert_eq!(
      run_script("/foo/p;//d", "foo\nbar\nfoo\n", false),
      "foo\nbar\nfoo\n"
    );
  }

  #[test]
  fn gnu_empty_address_regex_can_reuse_previous_substitute_pattern() {
    assert_eq!(
      run_script("s/foo/bar/;//p", "foofoo\n", false),
      "barfoo\nbarfoo\n"
    );
  }

  #[test]
  fn gnu_first_empty_regex_reports_error() {
    let err = run_script_err("//p", "a\n", false);
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = run_script_err("s//x/", "a\n", false);
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn gnu_n_command_at_eof_keeps_current_line_output() {
    assert_eq!(run_script("n", "one\n", false), "one\n");
  }

  #[test]
  fn gnu_n_append_next_line_at_eof_suppresses_output() {
    assert_eq!(run_script("N", "one\n", false), "");
    assert_eq!(run_script("N;p", "one\n", false), "");
  }

  #[test]
  fn gnu_delete_first_line_restarts_cycle_with_remaining_pattern_space() {
    assert_eq!(run_script("1{\nN\nD\n}", "a\nb\nc\n", false), "b\nc\n");
  }

  #[test]
  fn gnu_insert_and_append_text_preserve_output_order() {
    assert_eq!(run_script("i\\\nI\na\\\nA", "x\n", false), "I\nx\nA\n");
  }

  #[test]
  fn gnu_read_and_append_queued_output_follow_command_order() {
    let read_path = temp_script_path("read-before-append");
    fs::write(&read_path, "R\n").unwrap();
    let script =
      SedScript::parse(&format!("r {}\na\\\nA", read_path.display())).unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs("x\n");
    let output = run_program_on_inputs(&program, &inputs, false);
    let _ = fs::remove_file(&read_path);
    assert_eq!(output, "x\nR\nA\n");
  }

  #[test]
  fn gnu_append_and_read_queued_output_follow_command_order() {
    let read_path = temp_script_path("append-before-read");
    fs::write(&read_path, "R\n").unwrap();
    let script =
      SedScript::parse(&format!("a\\\nA\nr {}", read_path.display())).unwrap();
    let program = SedCompiledProgram::compile(&script).unwrap();
    let inputs = build_runtime_inputs("x\n");
    let output = run_program_on_inputs(&program, &inputs, false);
    let _ = fs::remove_file(&read_path);
    assert_eq!(output, "x\nA\nR\n");
  }

  #[test]
  fn gnu_multiline_print_cycle_at_eof_matches_reference_behavior() {
    assert_eq!(run_script("N;P;D", "a\n", true), "");
    assert_eq!(run_script("N;P;D", "a\nb\n", true), "a\n");
  }

  #[test]
  fn gnu_replacement_allows_literal_backslash() {
    assert_eq!(run_script(r"s/(foo)/\\/", "foo\n", false), "\\\n");
  }

  #[test]
  fn gnu_unmatched_optional_backreference_expands_to_empty() {
    assert_eq!(run_script(r"s/(a)?b/X\1Y/", "b\n", false), "XY\n");
  }

  #[test]
  fn gnu_undefined_backreference_reports_error() {
    let err = run_script_err(r"s/foo/\1/", "foo\n", false);
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = run_script_err(r"s/(foo)/\2/", "foo\n", false);
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn gnu_test_branch_uses_any_prior_substitute_since_last_reset() {
    assert_eq!(
      run_script("s/a/b/;s/x/y/;t hit;d;:hit;p", "a\nx\nz\n", true),
      "b\ny\n"
    );
  }

  #[test]
  fn gnu_no_substitute_branch_uses_any_prior_substitute_since_last_reset() {
    assert_eq!(
      run_script(
        "s/a/b/;s/x/y/;T miss;p;d;:miss;s/.*/MISS/;p",
        "a\nx\nz\n",
        true
      ),
      "b\ny\nMISS\n"
    );
  }

  #[test]
  fn gnu_test_branch_resets_substitute_state_after_check() {
    assert_eq!(
      run_script(
        "s/a/b/;t hit;b end;:hit;t stale;p;d;:stale;s/.*/bad/;p;:end",
        "a\n",
        true
      ),
      "b\n"
    );
  }

  #[test]
  fn gnu_append_next_line_resets_substitute_state_for_test_branches() {
    assert_eq!(run_script("s/a/b/;N;t hit;d;:hit;p", "a\nz\n", true), "");
    assert_eq!(
      run_script("s/a/b/;N;T miss;d;:miss;p", "a\nz\n", true),
      "b\nz\n"
    );
  }

  #[test]
  fn gnu_list_command_wraps_long_output() {
    assert_eq!(run_script("l 3", "abcdef\n", true), "ab\\\ncd\\\nef$\n");
  }

  #[test]
  fn parse_invocation_accepts_extended_regex_flags() {
    let parsed = SedCommand::parse_invocation(&[
      "-E".into(),
      "-e".into(),
      "s/foo/bar/".into(),
      "input.txt".into(),
    ])
    .unwrap();

    assert_eq!(
      parsed,
      SedCommand {
        quiet: false,
        extended_regex: true,
        scripts: vec![expr("s/foo/bar/")],
        files: vec!["input.txt".into()],
      }
    );

    let parsed = SedCommand::parse_invocation(&[
      "--regexp-extended".into(),
      "s/foo/bar/".into(),
    ])
    .unwrap();
    assert!(parsed.extended_regex);
  }

  #[test]
  fn parse_single_delete_statement() {
    let script = SedScript::parse("d").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: SedSelector::Any,
          command: SedStatementKind::DeletePattern,
        }],
      }
    );
  }

  #[test]
  fn parse_multiple_statements_split_by_newline_and_semicolon() {
    let script = SedScript::parse("p;d\nq").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::DeletePattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Quit(SedQuit {
              kind: SedQuitKind::PrintPattern,
              code: None,
            }),
          },
        ],
      }
    );
  }

  #[test]
  fn parse_numeric_last_line_and_regex_addresses() {
    let script = SedScript::parse("1p\n$p\n/needle/d").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: single(line(1)),
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: single(SedSingleAddress::LastLine),
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: single(regex('/', "needle")),
            command: SedStatementKind::DeletePattern,
          },
        ],
      }
    );
  }

  #[test]
  fn parse_step_addresses_and_zero_address_ranges() {
    let script = SedScript::parse("1~2p\n0,/^$/d").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: single(SedSingleAddress::Step(SedStepAddress {
              first: SedStepStart::Line(nz(1)),
              step: nz(2),
            })),
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: range(
              SedRangeStart::Zero,
              SedRangeEnd::Address(regex('/', "^$")),
            ),
            command: SedStatementKind::DeletePattern,
          },
        ],
      }
    );
  }

  #[test]
  fn parse_range_addresses_with_numeric_regex_plus_and_tilde_forms() {
    let script =
      SedScript::parse("1,5p\n/start/,/end/d\n3,+2p\n4,~8d").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: range(
              SedRangeStart::Address(line(1)),
              SedRangeEnd::Address(line(5)),
            ),
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: range(
              SedRangeStart::Address(regex('/', "start")),
              SedRangeEnd::Address(regex('/', "end")),
            ),
            command: SedStatementKind::DeletePattern,
          },
          SedStatement {
            selector: range(
              SedRangeStart::Address(line(3)),
              SedRangeEnd::NextLines(nz(2)),
            ),
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: range(
              SedRangeStart::Address(line(4)),
              SedRangeEnd::NextMultiple(nz(8)),
            ),
            command: SedStatementKind::DeletePattern,
          },
        ],
      }
    );
  }

  #[test]
  fn parse_negated_addressed_statement() {
    let script = SedScript::parse("/skip/!p").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: negated(regex('/', "skip")),
          command: SedStatementKind::PrintPattern,
        }],
      }
    );
  }

  #[test]
  fn parse_substitute_with_core_flags() {
    let script = SedScript::parse("s/foo/bar/gp").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: SedSelector::Any,
          command: sub(
            '/',
            "foo",
            "bar",
            SedSubstituteFlags {
              global: true,
              print: true,
              ..SedSubstituteFlags::default()
            },
          ),
        }],
      }
    );
  }

  #[test]
  fn parse_substitute_with_custom_delimiter_escaping_and_extended_flags() {
    let script =
      SedScript::parse(r#"s#foo\/bar#baz\#qux#2w out.txtIMe"#).unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: SedSelector::Any,
          command: sub(
            '#',
            r#"foo\/bar"#,
            r#"baz\#qux"#,
            SedSubstituteFlags {
              occurrence: Some(nz(2)),
              write: Some(path("out.txt")),
              ignore_case: true,
              multi_line: true,
              evaluate: true,
              ..SedSubstituteFlags::default()
            },
          ),
        }],
      }
    );
  }

  #[test]
  fn parse_transliterate_command() {
    let script = SedScript::parse("y/abc/xyz/").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: SedSelector::Any,
          command: SedStatementKind::Transliterate(SedTransliterate {
            delimiter: '/',
            source: "abc".into(),
            target: "xyz".into(),
          }),
        }],
      }
    );
  }

  #[test]
  fn parse_text_commands_with_multiline_payloads() {
    let script =
      SedScript::parse("a\\\nhello\nworld\n1i\\\nheader\n$c\\\nfooter")
        .unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::AppendText(SedTextBlock {
              first: "hello".into(),
              rest: vec!["world".into()],
            }),
          },
          SedStatement {
            selector: single(line(1)),
            command: SedStatementKind::InsertText(SedTextBlock {
              first: "header".into(),
              rest: vec![],
            }),
          },
          SedStatement {
            selector: single(SedSingleAddress::LastLine),
            command: SedStatementKind::ChangeText(SedTextBlock {
              first: "footer".into(),
              rest: vec![],
            }),
          },
        ],
      }
    );
  }

  #[test]
  fn parse_labels_and_branches() {
    let script = SedScript::parse(":top\nb done\nt retry\nT fail").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Label(label("top")),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Branch(SedBranch {
              kind: SedBranchKind::Unconditional,
              target: SedBranchTarget::Label(label("done")),
            }),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Branch(SedBranch {
              kind: SedBranchKind::OnSubstitute,
              target: SedBranchTarget::Label(label("retry")),
            }),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Branch(SedBranch {
              kind: SedBranchKind::OnNoSubstitute,
              target: SedBranchTarget::Label(label("fail")),
            }),
          },
        ],
      }
    );
  }

  #[test]
  fn parse_unlabeled_branch_is_explicitly_represented() {
    let script = SedScript::parse("b\nt\nT").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Branch(SedBranch {
              kind: SedBranchKind::Unconditional,
              target: SedBranchTarget::EndOfScript,
            }),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Branch(SedBranch {
              kind: SedBranchKind::OnSubstitute,
              target: SedBranchTarget::EndOfScript,
            }),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Branch(SedBranch {
              kind: SedBranchKind::OnNoSubstitute,
              target: SedBranchTarget::EndOfScript,
            }),
          },
        ],
      }
    );
  }

  #[test]
  fn parse_blocks_and_nested_statements() {
    let script = SedScript::parse("1,5{p;d\n/start/s/foo/bar/}").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![SedStatement {
          selector: range(
            SedRangeStart::Address(line(1)),
            SedRangeEnd::Address(line(5)),
          ),
          command: SedStatementKind::Block(vec![
            SedStatement {
              selector: SedSelector::Any,
              command: SedStatementKind::PrintPattern,
            },
            SedStatement {
              selector: SedSelector::Any,
              command: SedStatementKind::DeletePattern,
            },
            SedStatement {
              selector: single(regex('/', "start")),
              command: sub('/', "foo", "bar", SedSubstituteFlags::default()),
            },
          ]),
        }],
      }
    );
  }

  #[test]
  fn parse_pattern_space_hold_space_and_flow_commands() {
    let script = SedScript::parse("g;G;h;H;x;n;N;d;D;p;P;z;=;F").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::CopyHoldToPattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::AppendHoldToPattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::CopyPatternToHold,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::AppendPatternToHold,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::ExchangePatternAndHold,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::NextLine,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::AppendNextLine,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::DeletePattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::DeleteFirstLine,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::PrintFirstLine,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::ZapPattern,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::PrintLineNumber,
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::PrintCurrentFile,
          },
        ],
      }
    );
  }

  #[test]
  fn parse_read_write_execute_quit_and_list_commands() {
    let script = SedScript::parse(
      "r input.txt\nR fifo\nw out.txt\nW first.txt\ne echo hi\ne\nq 7\nQ 9\nl 120\nv\nv 4.9",
    )
    .unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::ReadFile { path: path("input.txt") },
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::ReadLineFromFile { path: path("fifo") },
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::WriteFile { path: path("out.txt") },
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::WriteFirstLine {
              path: path("first.txt"),
            },
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Execute(SedExecute::Command(text(
              "echo hi"
            ))),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Execute(SedExecute::PatternSpace),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Quit(SedQuit {
              kind: SedQuitKind::PrintPattern,
              code: Some(7),
            }),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Quit(SedQuit {
              kind: SedQuitKind::Silent,
              code: Some(9),
            }),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::ListPattern { wrap: Some(nz(120)) },
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::VersionCheck { version: None },
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::VersionCheck {
              version: Some(text("4.9")),
            },
          },
        ],
      }
    );
  }

  #[test]
  fn parse_comment_statement_and_ignores_its_trailing_text() {
    let script = SedScript::parse("# comment about script\np").unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::Comment(" comment about script".into()),
          },
          SedStatement {
            selector: SedSelector::Any,
            command: SedStatementKind::PrintPattern,
          },
        ],
      }
    );
  }

  #[test]
  fn parse_many_concatenates_script_sources_in_order() {
    let script = SedScript::parse_many(["1p", "2d", "3q"]).unwrap();
    assert_eq!(
      script,
      SedScript {
        statements: vec![
          SedStatement {
            selector: single(line(1)),
            command: SedStatementKind::PrintPattern,
          },
          SedStatement {
            selector: single(line(2)),
            command: SedStatementKind::DeletePattern,
          },
          SedStatement {
            selector: single(line(3)),
            command: SedStatementKind::Quit(SedQuit {
              kind: SedQuitKind::PrintPattern,
              code: None,
            }),
          },
        ],
      }
    );
  }

  #[test]
  fn parse_selector_parses_none_single_and_negated_range_forms() {
    let mut parser = SedParser::new("p");
    assert_eq!(parser.parse_selector().unwrap(), SedSelector::Any);

    let mut parser = SedParser::new("/x/p");
    assert_eq!(parser.parse_selector().unwrap(), single(regex('/', "x")));

    let mut parser = SedParser::new("1,~4!d");
    assert_eq!(
      parser.parse_selector().unwrap(),
      SedSelector::Addressed {
        addresses: SedAddresses::Range(SedRangeAddress {
          start: SedRangeStart::Address(line(1)),
          end: SedRangeEnd::NextMultiple(nz(4)),
        }),
        negated: true,
      }
    );
  }

  #[test]
  fn parse_command_kind_parses_branch_substitute_and_text_forms_directly() {
    let mut parser = SedParser::new("b done");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      SedStatementKind::Branch(SedBranch {
        kind: SedBranchKind::Unconditional,
        target: SedBranchTarget::Label(label("done")),
      })
    );

    let mut parser = SedParser::new("s/foo/bar/2gp");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      sub(
        '/',
        "foo",
        "bar",
        SedSubstituteFlags {
          occurrence: Some(nz(2)),
          global: true,
          print: true,
          ..SedSubstituteFlags::default()
        },
      )
    );

    let mut parser = SedParser::new("a\\\nhello\nworld");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      SedStatementKind::AppendText(SedTextBlock {
        first: "hello".into(),
        rest: vec!["world".into()],
      })
    );
  }

  #[test]
  fn parse_statement_preserves_selector_command_pairing() {
    let mut parser = SedParser::new("/foo/!s/bar/baz/g");
    assert_eq!(
      parser.parse_statement().unwrap(),
      SedStatement {
        selector: negated(regex('/', "foo")),
        command: sub(
          '/',
          "bar",
          "baz",
          SedSubstituteFlags { global: true, ..SedSubstituteFlags::default() },
        ),
      }
    );
  }

  #[test]
  fn parse_statement_handles_whitespace_after_addresses_and_commands() {
    let mut parser = SedParser::new("  1, 5  s#foo#bar#g");
    assert_eq!(
      parser.parse_statement().unwrap(),
      SedStatement {
        selector: range(
          SedRangeStart::Address(line(1)),
          SedRangeEnd::Address(line(5)),
        ),
        command: sub(
          '#',
          "foo",
          "bar",
          SedSubstituteFlags { global: true, ..SedSubstituteFlags::default() },
        ),
      }
    );
  }

  #[test]
  fn parse_statement_respects_semicolon_as_statement_terminator_not_payload() {
    let mut parser = SedParser::new(r#"s;foo;bar;g;d"#);
    assert_eq!(
      parser.parse_statement().unwrap(),
      SedStatement {
        selector: SedSelector::Any,
        command: sub(
          ';',
          "foo",
          "bar",
          SedSubstituteFlags { global: true, ..SedSubstituteFlags::default() },
        ),
      }
    );
  }

  #[test]
  fn parse_selector_accepts_step_address_starting_at_zero() {
    let mut parser = SedParser::new("0~3p");
    assert_eq!(
      parser.parse_selector().unwrap(),
      single(SedSingleAddress::Step(SedStepAddress {
        first: SedStepStart::Zero,
        step: nz(3),
      }))
    );
  }

  #[test]
  fn parse_command_kind_parses_comments_and_silent_quit_directly() {
    let mut parser = SedParser::new("# note");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      SedStatementKind::Comment(" note".into())
    );

    let mut parser = SedParser::new("Q 42");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      SedStatementKind::Quit(SedQuit {
        kind: SedQuitKind::Silent,
        code: Some(42),
      })
    );

    let mut parser = SedParser::new("F");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      SedStatementKind::PrintCurrentFile
    );

    let mut parser = SedParser::new("v 4.9");
    assert_eq!(
      parser.parse_command_kind().unwrap(),
      SedStatementKind::VersionCheck { version: Some(text("4.9")) }
    );
  }

  #[test]
  fn parse_rejects_unknown_command_letters() {
    let err = SedScript::parse("Y").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_invalid_step_address_zero_step_and_zero_line() {
    let err = SedScript::parse("1~0p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("0p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_invalid_range_end_forms() {
    let err = SedScript::parse("1,+0p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("1,~0p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("1,$~2p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_substitute_write_flag_without_path() {
    let err = SedScript::parse("s/foo/bar/w").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_execute_command_with_empty_explicit_payload() {
    let err = SedScript::parse("e ").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_read_write_commands_without_paths() {
    for script in ["r", "R", "w", "W"] {
      let err = SedScript::parse(script).unwrap_err();
      assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
  }

  #[test]
  fn parse_rejects_list_command_with_zero_wrap_width() {
    let err = SedScript::parse("l 0").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_text_commands_separated_with_semicolon_payload_confusion() {
    let err = SedScript::parse("a\\\nhello;p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_selector_without_following_command() {
    for script in ["1", "/x/", "1,5", "/x/!"] {
      let err = SedScript::parse(script).unwrap_err();
      assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
  }

  #[test]
  fn parse_rejects_blocks_with_trailing_garbage_after_close() {
    let err = SedScript::parse("{p}garbage").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_duplicate_quit_codes_and_non_numeric_codes() {
    let err = SedScript::parse("q 1 2").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("Q nope").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_transliterate_with_mismatched_lengths_or_missing_fields() {
    let err = SedScript::parse("y/ab/c/").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("y//").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_addressed_comments_when_scope_disallows_them() {
    let err = SedScript::parse("1# comment").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_invocation_rejects_missing_e_argument_and_empty_script_set() {
    let err = SedCommand::parse_invocation(&["-e".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedCommand::parse_invocation(&[]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_invocation_rejects_missing_script_file_operand() {
    let err = SedCommand::parse_invocation(&["-f".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_unterminated_regex_addresses_and_y_delimiters() {
    let err = SedScript::parse("/foo d").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("y/foo/bar").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_duplicate_or_conflicting_substitute_flags() {
    let err = SedScript::parse("s/foo/bar/2g3").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("s/foo/bar/ww out").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_empty_or_malformed_label_references() {
    let err = SedScript::parse(":").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("b ").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_dangling_negation_and_range_suffixes() {
    let err = SedScript::parse("1!").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("1,+p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("1,~d").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_text_commands_without_required_newline_payload() {
    let err = SedScript::parse("a\\").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("1i\\").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_unbalanced_nested_blocks_and_extra_closing_brace() {
    let err = SedScript::parse("{1{p}}}").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("}").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_unterminated_substitute_delimiters() {
    let err = SedScript::parse("s/foo/bar").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_unclosed_block() {
    let err = SedScript::parse("{p;d").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_invalid_address_forms() {
    let err = SedScript::parse("~2p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    let err = SedScript::parse("1,,3p").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn parse_rejects_invalid_substitute_flags() {
    let err = SedScript::parse("s/foo/bar/gg").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }
}
