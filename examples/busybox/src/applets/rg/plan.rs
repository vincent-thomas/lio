use std::io;

use super::*;
use crate::applets::rg::render::should_show_line_numbers;

impl SearchPlan {
  pub fn new(config: SearchConfig, targets: Vec<SearchTarget>) -> Self {
    Self { config, targets }
  }

  pub fn from_command(command: &RgCommand) -> Self {
    let targets = if command.paths.is_empty() {
      vec![SearchTarget::File(".".into())]
    } else {
      command.paths.iter().cloned().map(SearchTarget::File).collect()
    };

    Self {
      config: SearchConfig {
        pattern_spec: command.pattern_spec.clone(),
        traversal: command.traversal.clone(),
        search: command.search.clone(),
        output: command.output.clone(),
        context: command.context,
        sort: command.sort,
        match_mode: command.match_mode,
      },
      targets,
    }
  }

  pub(crate) fn include_path_in_results(
    &self,
    runtime: &SearchRuntime,
  ) -> bool {
    match self.config.output.filename_mode {
      FilenameMode::Always => true,
      FilenameMode::Never => false,
      FilenameMode::Auto => {
        !matches!(self.targets.as_slice(), [SearchTarget::Stdin])
          && !(self.targets.is_empty() && !runtime.stdin_is_tty)
      }
    }
  }

  pub(crate) fn effective_match_mode(&self) -> MatchMode {
    MatchMode::effective(&self.config.output, self.config.match_mode)
  }

  pub(crate) fn classify_explicit_auto_filename(
    &self,
    ctx: &crate::app::AppContext,
    runtime: &SearchRuntime,
  ) -> io::Result<bool> {
    if !self.supports_explicit_auto_filename_suppression() {
      return Ok(false);
    }
    let [SearchTarget::File(path)] = self.targets.as_slice() else {
      return Ok(false);
    };
    self.has_default_auto_filename_target(ctx, runtime, path)
  }

  pub(crate) fn supports_explicit_auto_filename_suppression(&self) -> bool {
    self.has_default_auto_filename_output()
      && self.has_default_auto_filename_pattern()
      && self.has_default_auto_filename_search()
      && self.has_default_auto_filename_traversal()
      && self.has_default_auto_filename_context_and_sort()
  }

  pub(crate) fn should_suppress_auto_filename(
    &self,
    explicit_auto_filename: bool,
    outcomes: &[SearchOutcome],
  ) -> bool {
    explicit_auto_filename
      && matches!(
        outcomes,
        [SearchOutcome::MatchedLine(MatchRecord { spans, .. })]
          if spans.len() == 1
      )
  }

  pub(crate) fn supports_streaming_file_search(&self) -> bool {
    !self.config.search.null_data
      && !self.config.search.passthru
      && !matches!(self.config.search.binary_mode, SearchBinaryMode::Report)
  }

  pub(crate) fn supports_core_unordered_execute(&self) -> bool {
    self.supports_streaming_file_search()
      && self.effective_match_mode() == MatchMode::Standard
      && !self.config.search.files_mode
      && !self.config.search.invert_match
      && !self.config.search.passthru
      && self.config.context.before == 0
      && self.config.context.after == 0
      && !self.config.output.json
      && !self.config.output.only_matching
      && !self.config.output.vimgrep
      && !self.config.output.print0
      && !self.config.output.null_path_terminator
      && self.config.sort.kind == SortKind::None
      && !self.config.sort.reverse
      && !self.config.search.stats
  }

  pub fn presentation(
    &self,
    runtime: &SearchRuntime,
  ) -> io::Result<PresentationSpec> {
    let color_enabled = match self.config.output.color_mode {
      ColorMode::Always => true,
      ColorMode::Never => false,
      ColorMode::Auto => runtime.stdout_is_tty,
    };

    Ok(PresentationSpec {
      color_enabled,
      path_terminator: if self.config.output.print0
        || self.config.output.null_path_terminator
      {
        b'\0'
      } else {
        b'\n'
      },
      heading_mode: runtime.stdout_is_tty
        && self.effective_match_mode() == MatchMode::Standard
        && !self.config.output.vimgrep
        && self.config.output.filename_mode != FilenameMode::Never,
      default_line_number: should_show_line_numbers(
        self.config.output.line_number_mode,
        runtime.stdout_is_tty,
        self.effective_match_mode(),
      ),
    })
  }
  fn has_default_auto_filename_target(
    &self,
    ctx: &crate::app::AppContext,
    runtime: &SearchRuntime,
    path: &str,
  ) -> io::Result<bool> {
    if self.config.output.filename_mode != FilenameMode::Auto {
      return Ok(false);
    }
    super::util::path_is_explicit_file(ctx, &runtime.cwd, path)
  }

  fn has_default_auto_filename_output(&self) -> bool {
    self.config.output.line_number_mode == LineNumberMode::Auto
      && !self.config.output.print0
      && !self.config.output.json
      && self.config.output.color_mode == ColorMode::Auto
      && !self.config.output.only_matching
      && !self.config.output.include_zero
      && !self.config.output.vimgrep
  }

  fn has_default_auto_filename_pattern(&self) -> bool {
    self.config.pattern_spec.mode == PatternMode::Regex
      && self.config.pattern_spec.case_mode == CaseMode::Sensitive
      && self.config.pattern_spec.patterns.len() == 1
      && !self.config.pattern_spec.word_regexp
      && !self.config.pattern_spec.line_regexp
      && !super::util::contains_regex_meta(&self.config.pattern_spec.text)
  }

  fn has_default_auto_filename_search(&self) -> bool {
    self.config.match_mode == MatchMode::Standard
      && !self.config.search.invert_match
      && self.config.search.max_count.is_none()
      && !self.config.search.text
      && !self.config.search.files_mode
      && !self.config.search.stats
      && !self.config.search.quiet
      && !self.config.search.passthru
  }

  fn has_default_auto_filename_traversal(&self) -> bool {
    self.config.traversal.globs.is_empty()
      && !self.config.traversal.hidden
      && !self.config.traversal.no_ignore
  }

  fn has_default_auto_filename_context_and_sort(&self) -> bool {
    self.config.context.before == 0
      && self.config.context.after == 0
      && self.config.sort.kind == SortKind::None
      && !self.config.sort.reverse
  }
}
