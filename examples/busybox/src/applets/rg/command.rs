use std::{fs, io};

use super::*;
use crate::{app::AppContext, command::Command};

impl Default for SortSpec {
  fn default() -> Self {
    Self { kind: SortKind::None, reverse: false }
  }
}

impl RgCommand {
  pub(super) fn parse_args(args: &[String]) -> io::Result<Self> {
    let mut pattern = None;
    let mut patterns = Vec::new();
    let mut paths = Vec::new();
    let mut globs = Vec::new();
    let mut hidden = false;
    let mut no_ignore = false;
    let mut pattern_mode = PatternMode::Regex;
    let mut case_mode = CaseMode::Sensitive;
    let mut word_regexp = false;
    let mut line_regexp = false;
    let mut invert_match = false;
    let mut max_count = None;
    let mut threads = None;
    let mut treat_binary_as_text = false;
    let mut files_mode = false;
    let mut stats = false;
    let mut quiet = false;
    let mut passthru = false;
    let mut line_number_mode = LineNumberMode::Auto;
    let mut filename_mode = FilenameMode::Auto;
    let mut color_mode = ColorMode::Auto;
    let mut json = false;
    let mut print0 = false;
    let mut only_matching = false;
    let mut include_zero = false;
    let mut vimgrep = false;
    let mut context = ContextSpec::default();
    let mut sort = SortSpec::default();
    let mut match_mode = MatchMode::Standard;
    let mut positional_are_paths = false;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
      let arg = &args[index];
      if !positional_only {
        match arg.as_str() {
          "--" => {
            positional_only = true;
            index += 1;
            continue;
          }
          "-F" | "--fixed-strings" => {
            pattern_mode = PatternMode::FixedStrings;
            index += 1;
            continue;
          }
          "-i" | "--ignore-case" => {
            case_mode = CaseMode::Ignore;
            index += 1;
            continue;
          }
          "-s" | "--case-sensitive" => {
            case_mode = CaseMode::Sensitive;
            index += 1;
            continue;
          }
          "-S" | "--smart-case" => {
            case_mode = CaseMode::Smart;
            index += 1;
            continue;
          }
          "-v" | "--invert-match" => {
            invert_match = true;
            index += 1;
            continue;
          }
          "-w" | "--word-regexp" => {
            word_regexp = true;
            line_regexp = false;
            index += 1;
            continue;
          }
          "-x" | "--line-regexp" => {
            line_regexp = true;
            word_regexp = false;
            index += 1;
            continue;
          }
          "-e" | "--regexp" => {
            let value = required_arg(args, index, "--regexp")?;
            patterns.push(value.clone());
            positional_are_paths = true;
            index += 2;
            continue;
          }
          "-f" | "--file" => {
            let value = required_arg(args, index, "--file")?;
            patterns.extend(read_pattern_file(value)?);
            positional_are_paths = true;
            index += 2;
            continue;
          }
          "-m" | "--max-count" => {
            max_count =
              Some(parse_required_usize_flag(args, index, "--max-count")?);
            index += 2;
            continue;
          }
          "--threads" => {
            threads =
              Some(parse_required_usize_flag(args, index, "--threads")?);
            index += 2;
            continue;
          }
          value if value.starts_with("--threads=") => {
            threads = Some(parse_usize_flag(
              "--threads",
              &value["--threads=".len()..],
            )?);
            index += 1;
            continue;
          }
          "-A" | "--after-context" => {
            context.after =
              parse_required_usize_flag(args, index, "--after-context")?;
            index += 2;
            continue;
          }
          "-B" | "--before-context" => {
            context.before =
              parse_required_usize_flag(args, index, "--before-context")?;
            index += 2;
            continue;
          }
          "-C" | "--context" => {
            let amount = parse_required_usize_flag(args, index, "--context")?;
            context.before = amount;
            context.after = amount;
            index += 2;
            continue;
          }
          "-a" | "--text" => {
            treat_binary_as_text = true;
            index += 1;
            continue;
          }
          "-q" | "--quiet" => {
            quiet = true;
            index += 1;
            continue;
          }
          "--passthru" | "--passthrough" => {
            passthru = true;
            index += 1;
            continue;
          }
          "--files" => {
            files_mode = true;
            positional_are_paths = true;
            index += 1;
            continue;
          }
          "--stats" => {
            stats = true;
            index += 1;
            continue;
          }
          "--json" => {
            json = true;
            color_mode = ColorMode::Never;
            index += 1;
            continue;
          }
          "-n" | "--line-number" => {
            line_number_mode = LineNumberMode::Always;
            index += 1;
            continue;
          }
          "-N" | "--no-line-number" => {
            line_number_mode = LineNumberMode::Never;
            index += 1;
            continue;
          }
          "-H" | "--with-filename" => {
            filename_mode = FilenameMode::Always;
            index += 1;
            continue;
          }
          "-I" | "--no-filename" => {
            filename_mode = FilenameMode::Never;
            index += 1;
            continue;
          }
          "-c" | "--count" => {
            match_mode = MatchMode::Count;
            index += 1;
            continue;
          }
          "--count-matches" => {
            match_mode = MatchMode::CountMatches;
            index += 1;
            continue;
          }
          "-l" | "--files-with-matches" => {
            match_mode = MatchMode::FilesWithMatches;
            index += 1;
            continue;
          }
          "-L" | "--files-without-match" => {
            match_mode = MatchMode::FilesWithoutMatch;
            index += 1;
            continue;
          }
          "-g" | "--glob" => {
            let value = required_arg(args, index, "--glob")?;
            globs.push(value.clone());
            index += 2;
            continue;
          }
          "--hidden" => {
            hidden = true;
            index += 1;
            continue;
          }
          "--no-ignore" => {
            no_ignore = true;
            index += 1;
            continue;
          }
          "-0" | "--null" => {
            print0 = true;
            index += 1;
            continue;
          }
          "-o" | "--only-matching" => {
            only_matching = true;
            index += 1;
            continue;
          }
          "--vimgrep" => {
            vimgrep = true;
            index += 1;
            continue;
          }
          "--include-zero" => {
            include_zero = true;
            index += 1;
            continue;
          }
          "--no-color" => {
            color_mode = ColorMode::Never;
            index += 1;
            continue;
          }
          "--color" => {
            color_mode = parse_required_color_mode(args, index, "--color")?;
            index += 2;
            continue;
          }
          value if value.starts_with("--color=") => {
            color_mode = parse_color_mode(&value["--color=".len()..])?;
            index += 1;
            continue;
          }
          "--sort" => {
            sort = parse_required_sort_spec(args, index, "--sort", false)?;
            index += 2;
            continue;
          }
          value if value.starts_with("--sort=") => {
            sort = SortSpec {
              kind: parse_sort_kind(&value["--sort=".len()..])?,
              reverse: false,
            };
            index += 1;
            continue;
          }
          "--sortr" => {
            sort = parse_required_sort_spec(args, index, "--sortr", true)?;
            index += 2;
            continue;
          }
          value if value.starts_with("--sortr=") => {
            sort = SortSpec {
              kind: parse_sort_kind(&value["--sortr=".len()..])?,
              reverse: true,
            };
            index += 1;
            continue;
          }
          value if value.starts_with('-') => {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              format!("rg: unsupported flag {value}"),
            ));
          }
          _ => {}
        }
      }

      if !positional_are_paths && pattern.is_none() {
        pattern = Some(arg.clone());
        patterns.push(arg.clone());
      } else {
        paths.push(arg.clone());
      }
      index += 1;
    }

    let pattern = if let Some(pattern) = pattern {
      pattern
    } else if let Some(pattern) = patterns.first() {
      pattern.clone()
    } else if files_mode {
      String::new()
    } else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "rg: missing search pattern",
      ));
    };

    Ok(Self {
      pattern: pattern.clone(),
      paths: paths.clone(),
      globs: globs.clone(),
      traversal: TraversalSpec {
        hidden,
        no_ignore,
        paths,
        globs,
        include_globs: Vec::new(),
        exclude_globs: Vec::new(),
        exclude_dirs: Vec::new(),
      },
      pattern_spec: PatternSpec {
        text: pattern,
        patterns,
        mode: pattern_mode,
        case_mode,
        word_regexp,
        line_regexp,
      },
      search: SearchSpec {
        invert_match,
        max_count,
        threads,
        text: treat_binary_as_text,
        suppress_errors: false,
        binary_mode: SearchBinaryMode::Skip,
        null_data: false,
        files_mode,
        stats,
        quiet,
        passthru,
      },
      output: OutputSpec {
        filename_mode,
        color_mode,
        line_number_mode,
        json,
        print0,
        null_path_terminator: false,
        only_matching,
        include_zero,
        vimgrep,
      },
      context,
      sort,
      match_mode,
    })
  }

  pub(super) fn effective_match_mode(&self) -> MatchMode {
    MatchMode::effective(&self.output, self.match_mode)
  }

  pub(super) fn matched_anything(&self, stats: SearchStats) -> bool {
    if self.search.files_mode {
      return true;
    }

    match self.effective_match_mode() {
      MatchMode::FilesWithoutMatch => {
        stats.files_searched > stats.files_with_matches
      }
      MatchMode::Standard
      | MatchMode::Count
      | MatchMode::CountMatches
      | MatchMode::FilesWithMatches => stats.matched_lines > 0,
    }
  }

  pub(super) fn plan_for_runtime(&self, runtime: &SearchRuntime) -> SearchPlan {
    let mut plan = SearchPlan::from_command(self);
    if self.paths.is_empty() && !runtime.stdin_is_tty {
      plan.targets = vec![SearchTarget::Stdin];
    }
    plan
  }
}

impl Command for RgCommand {
  fn name() -> &'static str {
    "rg"
  }

  fn summary() -> &'static str {
    "Search recursively for lines matching a pattern."
  }

  fn usage() -> &'static str {
    "rg [options] <pattern> [path ...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Self::parse_args(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let runtime = SearchRuntime::from_context(ctx)?;
    let plan = self.plan_for_runtime(&runtime);
    let engine = SearchEngine::default();
    let stats = engine.execute_cli_pipeline(ctx, &plan, &runtime)?;

    if self.matched_anything(stats) {
      Ok(())
    } else {
      Err(crate::exit_with_status(1))
    }
  }
}

pub(super) fn parse_color_mode(value: &str) -> io::Result<ColorMode> {
  match value {
    "always" => Ok(ColorMode::Always),
    "auto" => Ok(ColorMode::Auto),
    "never" => Ok(ColorMode::Never),
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("rg: unsupported color mode {value}"),
    )),
  }
}

pub(super) fn parse_usize_flag(flag: &str, value: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("rg: {flag} requires a non-negative integer"),
    )
  })
}

pub(super) fn parse_sort_kind(value: &str) -> io::Result<SortKind> {
  match value {
    "none" => Ok(SortKind::None),
    "path" => Ok(SortKind::Path),
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("rg: unsupported sort mode {value}"),
    )),
  }
}

pub(super) fn read_pattern_file(path: &str) -> io::Result<Vec<String>> {
  let contents = fs::read_to_string(path)?;
  Ok(contents.lines().map(str::to_owned).collect())
}

fn required_arg<'a>(
  args: &'a [String],
  index: usize,
  flag: &str,
) -> io::Result<&'a String> {
  args.get(index + 1).ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("rg: {flag} requires an argument"),
    )
  })
}

fn parse_required_usize_flag(
  args: &[String],
  index: usize,
  flag: &str,
) -> io::Result<usize> {
  parse_usize_flag(flag, required_arg(args, index, flag)?)
}

fn parse_required_color_mode(
  args: &[String],
  index: usize,
  flag: &str,
) -> io::Result<ColorMode> {
  let value = required_arg(args, index, flag).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("rg: {flag} requires a value"),
    )
  })?;
  parse_color_mode(value)
}

fn parse_required_sort_spec(
  args: &[String],
  index: usize,
  flag: &str,
  reverse: bool,
) -> io::Result<SortSpec> {
  Ok(SortSpec {
    kind: parse_sort_kind(required_arg(args, index, flag)?)?,
    reverse,
  })
}
