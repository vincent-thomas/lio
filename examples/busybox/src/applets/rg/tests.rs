use std::{
  fs,
  path::{Path, PathBuf},
  time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::{app::AppContext, command::Command};

struct TempDir {
  path: PathBuf,
}

struct CwdGuard {
  previous: PathBuf,
}

impl CwdGuard {
  fn change_to(path: &Path) -> Self {
    let previous = std::env::current_dir()
      .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    std::env::set_current_dir(path).unwrap();
    Self { previous }
  }
}

impl Drop for CwdGuard {
  fn drop(&mut self) {
    let _ = std::env::set_current_dir(&self.previous);
  }
}

impl TempDir {
  fn new(prefix: &str) -> Self {
    let unique =
      SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path =
      std::env::temp_dir().join(format!("busybox-rg-{prefix}-{unique}"));
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

#[test]
fn parse_minimal_command() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  assert_eq!(cmd.pattern, "needle");
  assert!(cmd.paths.is_empty());
  assert_eq!(cmd.pattern_spec.mode, PatternMode::Regex);
  assert_eq!(cmd.pattern_spec.case_mode, CaseMode::Sensitive);
  assert_eq!(cmd.output.filename_mode, FilenameMode::Auto);
  assert_eq!(cmd.output.color_mode, ColorMode::Auto);
  assert_eq!(cmd.match_mode, MatchMode::Standard);
  assert_eq!(cmd.search.threads, None);
}

#[test]
fn parse_threads_flag() {
  let cmd =
    RgCommand::parse(&["--threads".into(), "3".into(), "needle".into()])
      .unwrap();
  assert_eq!(cmd.search.threads, Some(3));

  let cmd = RgCommand::parse(&["--threads=2".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.search.threads, Some(2));
}

#[test]
fn behavior_exit_status_reports_match_for_standard_searches() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  assert!(cmd.matched_anything(SearchStats {
    matched_lines: 1,
    files_with_matches: 1,
    files_searched: 1,
    ..SearchStats::default()
  }));
  assert!(!cmd.matched_anything(SearchStats {
    files_searched: 1,
    ..SearchStats::default()
  }));
}

#[test]
fn behavior_exit_status_reports_match_for_files_without_match_mode() {
  let cmd = RgCommand::parse(&["-L".into(), "needle".into()]).unwrap();
  assert!(cmd.matched_anything(SearchStats {
    files_searched: 3,
    files_with_matches: 2,
    ..SearchStats::default()
  }));
  assert!(!cmd.matched_anything(SearchStats {
    files_searched: 3,
    files_with_matches: 3,
    ..SearchStats::default()
  }));
}

#[test]
fn behavior_exit_status_ignores_include_zero_without_matches() {
  let cmd =
    RgCommand::parse(&["-c".into(), "--include-zero".into(), "needle".into()])
      .unwrap();
  assert!(!cmd.matched_anything(SearchStats {
    files_searched: 1,
    ..SearchStats::default()
  }));
}

#[test]
fn behavior_exit_status_files_mode_always_succeeds_without_errors() {
  let cmd = RgCommand::parse(&["--files".into()]).unwrap();
  assert!(cmd.matched_anything(SearchStats::default()));
}

#[test]
fn parse_flag_rich_command() {
  let cmd = RgCommand::parse(&[
    "-F".into(),
    "-S".into(),
    "-n".into(),
    "-H".into(),
    "-g".into(),
    "*.rs".into(),
    "--hidden".into(),
    "--no-ignore".into(),
    "--color=always".into(),
    "main(".into(),
    "src".into(),
    "tests".into(),
  ])
  .unwrap();
  assert_eq!(cmd.pattern_spec.mode, PatternMode::FixedStrings);
  assert_eq!(cmd.pattern_spec.case_mode, CaseMode::Smart);
  assert_eq!(cmd.output.line_number_mode, LineNumberMode::Always);
  assert_eq!(cmd.output.filename_mode, FilenameMode::Always);
  assert_eq!(cmd.output.color_mode, ColorMode::Always);
  assert_eq!(cmd.globs, vec!["*.rs"]);
  assert!(cmd.traversal.hidden);
  assert!(cmd.traversal.no_ignore);
  assert_eq!(cmd.paths, vec!["src", "tests"]);
}

#[test]
fn parse_match_mode_flags() {
  let cmd = RgCommand::parse(&["-c".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.match_mode, MatchMode::Count);

  let cmd =
    RgCommand::parse(&["--count-matches".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.match_mode, MatchMode::CountMatches);

  let cmd = RgCommand::parse(&["-l".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.match_mode, MatchMode::FilesWithMatches);

  let cmd = RgCommand::parse(&["-L".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.match_mode, MatchMode::FilesWithoutMatch);
}

#[test]
fn parse_last_case_flag_wins() {
  let cmd =
    RgCommand::parse(&["-i".into(), "-S".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.pattern_spec.case_mode, CaseMode::Smart);

  let cmd =
    RgCommand::parse(&["-S".into(), "-i".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.pattern_spec.case_mode, CaseMode::Ignore);

  let cmd =
    RgCommand::parse(&["-i".into(), "-s".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.pattern_spec.case_mode, CaseMode::Sensitive);
}

#[test]
fn parse_last_filename_flag_wins() {
  let cmd =
    RgCommand::parse(&["-H".into(), "-I".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.output.filename_mode, FilenameMode::Never);

  let cmd =
    RgCommand::parse(&["-I".into(), "-H".into(), "needle".into()]).unwrap();
  assert_eq!(cmd.output.filename_mode, FilenameMode::Always);
}

#[test]
fn parse_last_color_flag_wins() {
  let cmd = RgCommand::parse(&[
    "--color=always".into(),
    "--no-color".into(),
    "needle".into(),
  ])
  .unwrap();
  assert_eq!(cmd.output.color_mode, ColorMode::Never);

  let cmd = RgCommand::parse(&[
    "--no-color".into(),
    "--color=always".into(),
    "needle".into(),
  ])
  .unwrap();
  assert_eq!(cmd.output.color_mode, ColorMode::Always);
}

#[test]
fn parse_accumulates_multiple_globs() {
  let cmd = RgCommand::parse(&[
    "-g".into(),
    "*.rs".into(),
    "--glob".into(),
    "*.toml".into(),
    "needle".into(),
  ])
  .unwrap();
  assert_eq!(cmd.globs, vec!["*.rs", "*.toml"]);
  assert_eq!(cmd.traversal.globs, vec!["*.rs", "*.toml"]);
}

#[test]
fn parse_output_flags_are_reflected_in_output_spec() {
  let cmd = RgCommand::parse(&[
    "-n".into(),
    "-0".into(),
    "-H".into(),
    "--color=always".into(),
    "needle".into(),
  ])
  .unwrap();
  assert_eq!(cmd.output.line_number_mode, LineNumberMode::Always);
  assert!(cmd.output.print0);
  assert_eq!(cmd.output.filename_mode, FilenameMode::Always);
  assert_eq!(cmd.output.color_mode, ColorMode::Always);
}

#[test]
fn parse_json_output_flag_disables_color_and_enables_json() {
  let cmd = RgCommand::parse(&[
    "--color=always".into(),
    "--json".into(),
    "needle".into(),
  ])
  .unwrap();
  assert!(cmd.output.json);
  assert_eq!(cmd.output.color_mode, ColorMode::Never);
}

#[test]
fn parse_extended_flags_are_reflected_in_specs() {
  let cmd = RgCommand::parse(&[
    "-v".into(),
    "-w".into(),
    "-m".into(),
    "3".into(),
    "-a".into(),
    "-o".into(),
    "--include-zero".into(),
    "-N".into(),
    "needle".into(),
  ])
  .unwrap();
  assert!(cmd.search.invert_match);
  assert_eq!(cmd.search.max_count, Some(3));
  assert!(cmd.search.text);
  assert!(cmd.output.only_matching);
  assert!(cmd.output.include_zero);
  assert_eq!(cmd.output.line_number_mode, LineNumberMode::Never);
  assert!(cmd.pattern_spec.word_regexp);
  assert!(!cmd.pattern_spec.line_regexp);
}

#[test]
fn parse_quiet_passthru_and_vimgrep_flags() {
  let cmd = RgCommand::parse(&[
    "-q".into(),
    "--passthru".into(),
    "--vimgrep".into(),
    "needle".into(),
  ])
  .unwrap();
  assert!(cmd.search.quiet);
  assert!(cmd.search.passthru);
  assert!(cmd.output.vimgrep);
}

#[test]
fn parse_context_sort_files_and_stats_flags() {
  let cmd = RgCommand::parse(&[
    "-A".into(),
    "2".into(),
    "-B".into(),
    "1".into(),
    "--stats".into(),
    "--files".into(),
    "--sort=path".into(),
    ".".into(),
  ])
  .unwrap();
  assert_eq!(cmd.context.before, 1);
  assert_eq!(cmd.context.after, 2);
  assert!(cmd.search.stats);
  assert!(cmd.search.files_mode);
  assert_eq!(cmd.sort.kind, SortKind::Path);
  assert!(!cmd.sort.reverse);
  assert_eq!(cmd.paths, vec!["."]);
}

#[test]
fn parse_files_mode_allows_missing_pattern() {
  let cmd = RgCommand::parse(&["--files".into()]).unwrap();
  assert!(cmd.search.files_mode);
  assert_eq!(cmd.pattern, "");
}

#[test]
fn parse_regexp_and_pattern_file_turn_positionals_into_paths() {
  let dir = TempDir::new("pattern-file");
  dir.write("patterns.txt", b"alpha\nbeta\n");
  let _cwd = CwdGuard::change_to(dir.path());

  let cmd = RgCommand::parse(&[
    "-e".into(),
    "gamma".into(),
    "-f".into(),
    "patterns.txt".into(),
    "src".into(),
  ])
  .unwrap();

  assert_eq!(cmd.pattern_spec.patterns, vec!["gamma", "alpha", "beta"]);
  assert_eq!(cmd.paths, vec!["src"]);
}

#[test]
fn parse_traversal_flags_are_reflected_in_traversal_spec() {
  let cmd = RgCommand::parse(&[
    "--hidden".into(),
    "--no-ignore".into(),
    "needle".into(),
    "src".into(),
  ])
  .unwrap();
  assert!(cmd.traversal.hidden);
  assert!(cmd.traversal.no_ignore);
  assert_eq!(cmd.traversal.paths, vec!["src"]);
}

#[test]
fn parse_double_dash_treats_rest_as_positional() {
  let cmd =
    RgCommand::parse(&["--".into(), "-not-a-flag".into(), "src".into()])
      .unwrap();
  assert_eq!(cmd.pattern, "-not-a-flag");
  assert_eq!(cmd.paths, vec!["src"]);
}

#[test]
fn parse_missing_pattern_is_an_error() {
  let err = RgCommand::parse(&[]).unwrap_err();
  assert!(err.to_string().contains("missing search pattern"));
}

#[test]
fn parse_unknown_flag_is_an_error() {
  let err = RgCommand::parse(&["--wat".into(), "needle".into()]).unwrap_err();
  assert!(err.to_string().contains("unsupported flag"));
}

#[test]
fn parse_glob_requires_value() {
  let err = RgCommand::parse(&["-g".into()]).unwrap_err();
  assert!(err.to_string().contains("--glob requires an argument"));
}

#[test]
fn parse_regexp_requires_value() {
  let err = RgCommand::parse(&["-e".into()]).unwrap_err();
  assert!(err.to_string().contains("--regexp requires an argument"));
}

#[test]
fn parse_pattern_file_requires_value() {
  let err = RgCommand::parse(&["-f".into()]).unwrap_err();
  assert!(err.to_string().contains("--file requires an argument"));
}

#[test]
fn parse_max_count_requires_integer() {
  let err = RgCommand::parse(&["-m".into(), "wat".into(), "needle".into()])
    .unwrap_err();
  assert!(
    err.to_string().contains("--max-count requires a non-negative integer")
  );
}

#[test]
fn parse_sort_requires_value() {
  let err = RgCommand::parse(&["--sort".into()]).unwrap_err();
  assert!(err.to_string().contains("--sort requires an argument"));
}

#[test]
fn parse_rejects_unknown_sort_mode() {
  let err =
    RgCommand::parse(&["--sort=wat".into(), "needle".into()]).unwrap_err();
  assert!(err.to_string().contains("unsupported sort mode"));
}

#[test]
fn parse_color_requires_value() {
  let err = RgCommand::parse(&["--color".into()]).unwrap_err();
  assert!(err.to_string().contains("--color requires a value"));
}

#[test]
fn parse_rejects_unknown_color_mode() {
  let err =
    RgCommand::parse(&["--color=wat".into(), "needle".into()]).unwrap_err();
  assert!(err.to_string().contains("unsupported color mode"));
}

#[test]
fn search_plan_defaults_to_current_directory_target() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  assert_eq!(plan.targets, vec![SearchTarget::File(".".into())]);
  assert_eq!(plan.config.pattern_spec.text, "needle");
}

#[test]
fn search_plan_preserves_explicit_paths() {
  let cmd =
    RgCommand::parse(&["needle".into(), "src".into(), "tests".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  assert_eq!(
    plan.targets,
    vec![SearchTarget::File("src".into()), SearchTarget::File("tests".into())]
  );
}

#[test]
fn render_json_output_emits_match_and_summary_events() {
  let cmd = RgCommand::parse(&["--json".into(), "needle".into()]).unwrap();
  let rendered = render_outcomes(
    &[
      SearchOutcome::json_begin(Some("./src/main.rs".into())),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./src/main.rs".into()),
          7,
          b"find needle here".to_vec(),
          vec![MatchSpan::new(5, 11).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::json_end(
        Some("./src/main.rs".into()),
        17,
        1,
        1,
        true,
        std::time::Duration::from_nanos(42),
      ),
    ],
    &cmd,
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    },
  );

  let output = String::from_utf8(rendered).unwrap();
  let mut lines = output.lines();
  let begin =
    serde_json::from_str::<serde_json::Value>(lines.next().unwrap()).unwrap();
  assert_eq!(begin["type"], "begin");
  assert_eq!(begin["data"]["path"]["text"], "src/main.rs");

  let first =
    serde_json::from_str::<serde_json::Value>(lines.next().unwrap()).unwrap();
  assert_eq!(first["type"], "match");
  assert_eq!(first["data"]["path"]["text"], "src/main.rs");
  assert_eq!(first["data"]["line_number"], 7);
  assert_eq!(first["data"]["absolute_offset"], 0);
  assert_eq!(first["data"]["submatches"][0]["match"]["text"], "needle");

  let end =
    serde_json::from_str::<serde_json::Value>(lines.next().unwrap()).unwrap();
  assert_eq!(end["type"], "end");
  assert_eq!(end["data"]["path"]["text"], "src/main.rs");
  assert_eq!(end["data"]["stats"]["searches"], 1);
  assert_eq!(end["data"]["stats"]["searches_with_match"], 1);
  assert_eq!(end["data"]["stats"]["bytes_searched"], 17);
  assert!(end["data"]["stats"]["bytes_printed"].as_u64().unwrap() > 0);
  assert_eq!(end["data"]["binary_offset"], serde_json::Value::Null);

  let summary =
    serde_json::from_str::<serde_json::Value>(lines.next().unwrap()).unwrap();
  assert_eq!(summary["type"], "summary");
  assert_eq!(summary["data"]["stats"]["matches"], 0);
  assert_eq!(summary["data"]["stats"]["searches"], 0);
  assert_eq!(summary["data"]["stats"]["searches_with_match"], 0);
  assert!(summary["data"]["stats"]["bytes_printed"].as_u64().is_some());
  assert_eq!(summary["data"]["elapsed_total"]["secs"], 0);
  assert!(
    summary["data"]["elapsed_total"]["human"].as_str().unwrap().ends_with('s')
  );
  assert!(lines.next().is_none());
}

#[test]
fn render_streaming_json_matches_buffered_json_output() {
  let cmd = RgCommand::parse(&["--json".into(), "needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let outcomes = vec![
    SearchOutcome::json_begin(Some("./src/main.rs".into())),
    SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./src/main.rs".into()),
        7,
        b"find needle here".to_vec(),
        vec![MatchSpan::new(5, 11).unwrap()],
      )
      .unwrap(),
    ),
    SearchOutcome::json_end(
      Some("./src/main.rs".into()),
      17,
      1,
      1,
      true,
      std::time::Duration::from_nanos(42),
    ),
  ];
  let presentation = PresentationSpec {
    color_enabled: false,
    path_terminator: b'\n',
    heading_mode: false,
    default_line_number: false,
  };
  let stats = SearchStats {
    matches: 1,
    matched_lines: 1,
    files_with_matches: 1,
    files_searched: 1,
    bytes_searched: 17,
  };

  let buffered = presentation.render_plan_output(
    &plan,
    &outcomes,
    stats,
    std::time::Duration::from_nanos(10),
    std::time::Duration::from_nanos(20),
  );
  let streamed = super::render::stream_render_plan_output(
    presentation,
    &plan,
    &outcomes,
    stats,
    std::time::Duration::from_nanos(10),
    std::time::Duration::from_nanos(20),
  );

  assert_eq!(streamed, buffered);
}

#[test]
fn render_streaming_heading_matches_buffered_heading_output() {
  let cmd = RgCommand::parse(&["needle".into(), "src".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let outcomes = vec![
    SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./src/a.rs".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    ),
    SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./src/a.rs".into()),
        2,
        b"second needle".to_vec(),
        vec![MatchSpan::new(7, 13).unwrap()],
      )
      .unwrap(),
    ),
    SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./src/b.rs".into()),
        3,
        b"needle again".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    ),
  ];
  let presentation = PresentationSpec {
    color_enabled: false,
    path_terminator: b'\n',
    heading_mode: true,
    default_line_number: true,
  };

  let buffered = presentation.render_plan_output(
    &plan,
    &outcomes,
    SearchStats::default(),
    std::time::Duration::ZERO,
    std::time::Duration::ZERO,
  );
  let streamed = super::render::stream_render_plan_output(
    presentation,
    &plan,
    &outcomes,
    SearchStats::default(),
    std::time::Duration::ZERO,
    std::time::Duration::ZERO,
  );

  assert_eq!(streamed, buffered);
}

#[test]
fn render_direct_plain_matches_buffered_stream_output() {
  let cmd = RgCommand::parse(&["needle".into(), "src".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let presentation = PresentationSpec {
    color_enabled: false,
    path_terminator: b'\n',
    heading_mode: false,
    default_line_number: false,
  };
  let outcomes = vec![
    SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        Some("./src/a.rs".into()),
        2,
        14,
        b"needle".to_vec(),
        Vec::new(),
      )
      .unwrap(),
    ),
    SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        Some("./src/b.rs".into()),
        4,
        33,
        b"second".to_vec(),
        Vec::new(),
      )
      .unwrap(),
    ),
  ];

  let buffered = super::render::stream_render_plan_output(
    presentation,
    &plan,
    &outcomes,
    SearchStats::default(),
    std::time::Duration::ZERO,
    std::time::Duration::ZERO,
  );
  let direct = super::render::stream_render_plain_matches_output(
    presentation,
    &plan,
    &[
      (Some("./src/a.rs"), 2, 14, &b"needle"[..]),
      (Some("./src/b.rs"), 4, 33, &b"second"[..]),
    ],
  );

  assert_eq!(direct, buffered);
}

#[test]
fn render_direct_plain_matches_buffered_heading_output() {
  let cmd = RgCommand::parse(&["needle".into(), "src".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let presentation = PresentationSpec {
    color_enabled: false,
    path_terminator: b'\n',
    heading_mode: true,
    default_line_number: true,
  };
  let outcomes = vec![
    SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        Some("./src/a.rs".into()),
        2,
        14,
        b"needle".to_vec(),
        Vec::new(),
      )
      .unwrap(),
    ),
    SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        Some("./src/a.rs".into()),
        4,
        33,
        b"second".to_vec(),
        Vec::new(),
      )
      .unwrap(),
    ),
  ];

  let buffered = super::render::stream_render_plan_output(
    presentation,
    &plan,
    &outcomes,
    SearchStats::default(),
    std::time::Duration::ZERO,
    std::time::Duration::ZERO,
  );
  let direct = super::render::stream_render_plain_matches_output(
    presentation,
    &plan,
    &[
      (Some("./src/a.rs"), 2, 14, &b"needle"[..]),
      (Some("./src/a.rs"), 4, 33, &b"second"[..]),
    ],
  );

  assert_eq!(direct, buffered);
}

#[test]
fn search_plan_copies_all_specs_from_command() {
  let cmd = RgCommand::parse(&[
    "-F".into(),
    "-i".into(),
    "-n".into(),
    "-H".into(),
    "-0".into(),
    "--hidden".into(),
    "--no-ignore".into(),
    "-g".into(),
    "*.rs".into(),
    "-c".into(),
    "needle".into(),
    "src".into(),
  ])
  .unwrap();
  let plan = SearchPlan::from_command(&cmd);
  assert_eq!(plan.config.pattern_spec, cmd.pattern_spec);
  assert_eq!(plan.config.traversal, cmd.traversal);
  assert_eq!(plan.config.output, cmd.output);
  assert_eq!(plan.config.context, cmd.context);
  assert_eq!(plan.config.sort, cmd.sort);
  assert_eq!(plan.config.match_mode, cmd.match_mode);
}

#[test]
fn match_span_rejects_empty_or_backwards_ranges() {
  let err = MatchSpan::new(3, 3).unwrap_err();
  assert!(err.to_string().contains("invalid match span"));

  let err = MatchSpan::new(4, 2).unwrap_err();
  assert!(err.to_string().contains("invalid match span"));
}

#[test]
fn match_record_validates_line_number_and_span_bounds() {
  let err = MatchRecord::new(None, 0, b"hello".to_vec(), vec![]).unwrap_err();
  assert!(err.to_string().contains("line numbers are 1-based"));

  let err = MatchRecord::new(
    None,
    1,
    b"hello".to_vec(),
    vec![MatchSpan { start: 0, end: 6 }],
  );
  assert!(err.unwrap_err().to_string().contains("ends past line length"));
}

#[test]
fn match_record_rejects_unsorted_or_overlapping_spans() {
  let err = MatchRecord::new(
    None,
    1,
    b"abcdef".to_vec(),
    vec![MatchSpan::new(2, 4).unwrap(), MatchSpan::new(1, 2).unwrap()],
  )
  .unwrap_err();
  assert!(err.to_string().contains("sorted and non-overlapping"));

  let err = MatchRecord::new(
    None,
    1,
    b"abcdef".to_vec(),
    vec![MatchSpan::new(0, 3).unwrap(), MatchSpan::new(2, 4).unwrap()],
  )
  .unwrap_err();
  assert!(err.to_string().contains("sorted and non-overlapping"));
}

#[test]
fn match_record_accepts_valid_spans() {
  let record = MatchRecord::new(
    Some("src/lib.rs".into()),
    7,
    b"hello world".to_vec(),
    vec![MatchSpan::new(0, 5).unwrap(), MatchSpan::new(6, 11).unwrap()],
  )
  .unwrap();
  assert_eq!(record.path.as_deref(), Some("src/lib.rs"));
  assert_eq!(record.line_number, 7);
  assert_eq!(record.absolute_offset, 0);
  assert_eq!(record.spans.len(), 2);
}

#[test]
fn match_record_allows_zero_spans_for_non_highlighted_records() {
  let record =
    MatchRecord::new(Some("a.txt".into()), 1, b"line".to_vec(), vec![])
      .unwrap();
  assert!(record.spans.is_empty());
}

#[test]
fn search_result_emitter_collects_structured_outcomes_in_order() {
  let mut emitter = SearchResultEmitter::new();
  emitter.emit_match(
    MatchRecord::new(
      Some("src/lib.rs".into()),
      12,
      b"fn main()".to_vec(),
      vec![MatchSpan::new(3, 7).unwrap()],
    )
    .unwrap(),
  );
  emitter.emit_count(Some("src/lib.rs".into()), 3);
  emitter.emit_file_match("src/lib.rs");
  emitter.emit_file_without_match("README.md");

  assert_eq!(
    emitter.outcomes(),
    &[
      SearchOutcome::MatchedLine(MatchRecord {
        path: Some("src/lib.rs".into()),
        line_number: 12,
        absolute_offset: 0,
        line: b"fn main()".to_vec(),
        spans: vec![MatchSpan { start: 3, end: 7 }],
      }),
      SearchOutcome::Count { path: Some("src/lib.rs".into()), count: 3 },
      SearchOutcome::FileMatch("src/lib.rs".into()),
      SearchOutcome::FileWithoutMatch("README.md".into()),
    ]
  );
}

#[test]
fn search_result_emitter_into_outcomes_consumes_all_emitted_values() {
  let mut emitter = SearchResultEmitter::new();
  emitter.emit_count(None, 2);
  emitter.emit_file_match("src/lib.rs");
  assert_eq!(
    emitter.into_outcomes(),
    vec![
      SearchOutcome::Count { path: None, count: 2 },
      SearchOutcome::FileMatch("src/lib.rs".into()),
    ]
  );
}

#[test]
fn search_outcome_constructors_build_structured_values() {
  let record = MatchRecord::new(
    None,
    2,
    b"needle".to_vec(),
    vec![MatchSpan::new(0, 6).unwrap()],
  )
  .unwrap();
  assert_eq!(
    SearchOutcome::matched_line(record.clone()),
    SearchOutcome::MatchedLine(record)
  );
  assert_eq!(
    SearchOutcome::count(Some("a.txt".into()), 1),
    SearchOutcome::Count { path: Some("a.txt".into()), count: 1 }
  );
  assert_eq!(
    SearchOutcome::file_match("a.txt"),
    SearchOutcome::FileMatch("a.txt".into())
  );
  assert_eq!(
    SearchOutcome::file_without_match("b.txt"),
    SearchOutcome::FileWithoutMatch("b.txt".into())
  );
}

#[test]
fn behavior_searches_stdin_when_no_paths_are_provided() {
  let dir = TempDir::new("stdin");
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let actual = SearchEngine::default()
    .search(&cmd, &stdin_runtime(dir.path(), b"first\nneedle here\nlast\n"))
    .unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        None,
        2,
        6,
        b"needle here".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_searches_current_directory_when_no_stdin_and_no_paths_are_provided()
{
  let dir = TempDir::new("cwd");
  dir.write("a.txt", b"needle\n");
  dir.write("nested/b.txt", b"nope\nneedle twice\n");
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./a.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("./nested/b.txt".into()),
          2,
          5,
          b"needle twice".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_searches_files_recursively_under_directory_paths() {
  let dir = TempDir::new("recursive");
  dir.write("root/src/lib.rs", b"fn needle() {}\n");
  dir.write("root/tests/test.rs", b"needle\n");
  let cmd = RgCommand::parse(&["needle".into(), "root".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("root/src/lib.rs".into()),
          1,
          b"fn needle() {}".to_vec(),
          vec![MatchSpan::new(3, 9).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("root/tests/test.rs".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[cfg(unix)]
#[test]
fn behavior_permission_denied_on_file_open_is_skipped_silently() {
  let dir = TempDir::new("permission-denied-file");
  dir.write("visible.txt", b"testing\n");
  dir.write("secret.txt", b"testing\n");
  let secret = dir.path().join("secret.txt");
  let original_mode = fs::metadata(&secret).unwrap().permissions().mode();
  fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();

  let cmd = RgCommand::parse(&["testing".into(), ".".into()]).unwrap();
  let outcomes =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();

  fs::set_permissions(&secret, fs::Permissions::from_mode(original_mode))
    .unwrap();

  assert_eq!(outcomes.len(), 1);
  let rendered = format!("{:?}", outcomes);
  assert!(rendered.contains("visible.txt"));
  assert!(!rendered.contains("secret.txt"));
}

#[test]
fn behavior_cli_parallel_search_matches_deterministic_engine_results() {
  let dir = TempDir::new("parallel-cli");
  for index in 0..12 {
    let body = if index % 3 == 0 {
      format!("hit {index}\nneedle\n")
    } else {
      format!("hit {index}\nmiss\n")
    };
    dir.write(&format!("tree/file-{index}.txt"), body.as_bytes());
  }

  let cmd = RgCommand::parse(&["needle".into(), "tree".into()]).unwrap();
  let runtime = runtime(dir.path());
  let mut expected = SearchEngine::default().search(&cmd, &runtime).unwrap();

  let _cwd = CwdGuard::change_to(dir.path());
  let ctx = AppContext::new().unwrap();
  let plan = cmd.plan_for_runtime(&runtime);
  let (mut actual, _) =
    SearchEngine::default().search_cli(&ctx, &plan, &runtime).unwrap();

  expected.sort_by_key(outcome_sort_key);
  actual.sort_by_key(outcome_sort_key);
  assert_eq!(actual, expected);
}

#[test]
fn behavior_cli_streaming_context_matches_buffered_engine_results() {
  let dir = TempDir::new("streaming-context");
  dir.write("sample.txt", b"zero\none needle\ntwo\nthree needle\nfour\n");

  let cmd = RgCommand::parse(&[
    "-C".into(),
    "1".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let runtime = runtime(dir.path());
  let expected = SearchEngine::default().search(&cmd, &runtime).unwrap();

  let _cwd = CwdGuard::change_to(dir.path());
  let ctx = AppContext::new().unwrap();
  let plan = cmd.plan_for_runtime(&runtime);
  let (actual, _) =
    SearchEngine::default().search_cli(&ctx, &plan, &runtime).unwrap();

  assert_eq!(actual, expected);
}

#[test]
fn behavior_cli_streaming_context_honors_max_count_with_trailing_context() {
  let dir = TempDir::new("streaming-context-max");
  dir.write("sample.txt", b"zero\none needle\ntwo needle\nthree\n");

  let cmd = RgCommand::parse(&[
    "-C".into(),
    "1".into(),
    "-m".into(),
    "1".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let runtime = runtime(dir.path());
  let expected = SearchEngine::default().search(&cmd, &runtime).unwrap();

  let _cwd = CwdGuard::change_to(dir.path());
  let ctx = AppContext::new().unwrap();
  let plan = cmd.plan_for_runtime(&runtime);
  let (actual, _) =
    SearchEngine::default().search_cli(&ctx, &plan, &runtime).unwrap();

  assert_eq!(actual, expected);
}

fn outcome_sort_key(outcome: &SearchOutcome) -> (String, usize, usize) {
  match outcome {
    SearchOutcome::MatchedLine(record) | SearchOutcome::ContextLine(record) => {
      (
        record.path.clone().unwrap_or_default(),
        record.line_number,
        record.absolute_offset,
      )
    }
    SearchOutcome::BinaryMatch { path } => {
      (path.clone().unwrap_or_default(), 0, 0)
    }
    SearchOutcome::Count { path, count } => {
      (path.clone().unwrap_or_default(), *count, 0)
    }
    SearchOutcome::FileMatch(path) | SearchOutcome::FileWithoutMatch(path) => {
      (path.clone(), 0, 0)
    }
    SearchOutcome::JsonBegin { path } | SearchOutcome::JsonEnd { path, .. } => {
      (path.clone().unwrap_or_default(), 0, 0)
    }
    SearchOutcome::ContextSeparator => (String::new(), 0, 0),
  }
}

#[test]
fn behavior_regex_mode_matches_lines_by_default() {
  let dir = TempDir::new("regex");
  dir.write("sample.txt", b"abc123\nabc\n");
  let cmd = RgCommand::parse(&["abc\\d+".into(), "sample.txt".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("sample.txt".into()),
        1,
        b"abc123".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_invalid_regex_reports_error() {
  let dir = TempDir::new("invalid-regex");
  dir.write("sample.txt", b"needle\n");
  let cmd = RgCommand::parse(&["[".into(), "sample.txt".into()]).unwrap();
  let err =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap_err();
  assert!(err.to_string().contains("regex"));
}

#[test]
fn behavior_fixed_string_mode_treats_metacharacters_literally() {
  let dir = TempDir::new("fixed");
  dir.write("sample.txt", b"abc123\nabc\\d+\n");
  let cmd =
    RgCommand::parse(&["-F".into(), "abc\\d+".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        Some("sample.txt".into()),
        2,
        7,
        b"abc\\d+".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_regexp_patterns_match_as_alternatives() {
  let dir = TempDir::new("multi-pattern");
  dir.write("sample.txt", b"alpha\nbeta\ngamma\n");
  let cmd = RgCommand::parse(&[
    "-e".into(),
    "alpha".into(),
    "-e".into(),
    "gamma".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"alpha".to_vec(),
          vec![MatchSpan::new(0, 5).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          3,
          11,
          b"gamma".to_vec(),
          vec![MatchSpan::new(0, 5).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_standard_mode_emits_multiple_spans_for_one_line() {
  let dir = TempDir::new("multi-span");
  dir.write("sample.txt", b"needle and needle\n");
  let cmd = RgCommand::parse(&["needle".into(), "sample.txt".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("sample.txt".into()),
        1,
        b"needle and needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap(), MatchSpan::new(11, 17).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_invert_match_prints_non_matching_lines() {
  let dir = TempDir::new("invert");
  dir.write("sample.txt", b"keep\nneedle\nalso keep\n");
  let cmd =
    RgCommand::parse(&["-v".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"keep".to_vec(),
          vec![]
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          3,
          12,
          b"also keep".to_vec(),
          vec![],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_word_regexp_requires_word_boundaries() {
  let dir = TempDir::new("word-regexp");
  dir.write("sample.txt", b"one stone\none tone\n");
  let cmd = RgCommand::parse(&["-w".into(), "one".into(), "sample.txt".into()])
    .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"one stone".to_vec(),
          vec![MatchSpan::new(0, 3).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          2,
          10,
          b"one tone".to_vec(),
          vec![MatchSpan::new(0, 3).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );

  let cmd = RgCommand::parse(&["-w".into(), "ton".into(), "sample.txt".into()])
    .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert!(actual.is_empty());
}

#[test]
fn behavior_line_regexp_matches_entire_line() {
  let dir = TempDir::new("line-regexp");
  dir.write("sample.txt", b"needle\nneedle here\n");
  let cmd =
    RgCommand::parse(&["-x".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("sample.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_ignore_case_forces_case_insensitive_matching() {
  let dir = TempDir::new("ignore-case");
  dir.write("sample.txt", b"Needle\n");
  let cmd =
    RgCommand::parse(&["-i".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("sample.txt".into()),
        1,
        b"Needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_smart_case_only_ignores_case_for_all_lowercase_patterns() {
  let dir = TempDir::new("smart-case");
  dir.write("sample.txt", b"Needle\nneedle\n");

  let lower =
    RgCommand::parse(&["-S".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let lower_actual =
    SearchEngine::default().search(&lower, &runtime(dir.path())).unwrap();
  assert_eq!(
    lower_actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"Needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          2,
          7,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );

  let upper =
    RgCommand::parse(&["-S".into(), "Needle".into(), "sample.txt".into()])
      .unwrap();
  let upper_actual =
    SearchEngine::default().search(&upper, &runtime(dir.path())).unwrap();
  assert_eq!(
    upper_actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("sample.txt".into()),
        1,
        b"Needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_with_filename_always_prints_file_prefixes() {
  let dir = TempDir::new("with-filename");
  dir.write("single.txt", b"needle\n");
  let cmd =
    RgCommand::parse(&["-H".into(), "needle".into(), "single.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("single.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_auto_filename_omits_path_for_single_file_target() {
  let dir = TempDir::new("auto-filename-single");
  dir.write("single.txt", b"needle\n");
  let cmd = RgCommand::parse(&["needle".into(), "single.txt".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        None,
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()]
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_auto_filename_includes_path_for_multiple_targets() {
  let dir = TempDir::new("auto-filename-multi");
  dir.write("a.txt", b"needle\n");
  dir.write("b.txt", b"needle\n");
  let cmd =
    RgCommand::parse(&["needle".into(), "a.txt".into(), "b.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("a.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("b.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_no_filename_suppresses_file_prefixes_even_with_multiple_paths() {
  let dir = TempDir::new("no-filename");
  dir.write("a.txt", b"needle\n");
  dir.write("b.txt", b"needle\n");
  let cmd = RgCommand::parse(&[
    "-I".into(),
    "needle".into(),
    "a.txt".into(),
    "b.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          None,
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()]
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          None,
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()]
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_line_number_prints_one_based_line_numbers() {
  let dir = TempDir::new("line-number");
  dir.write("sample.txt", b"skip\nskip\nneedle\n");
  let cmd =
    RgCommand::parse(&["-n".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new_with_offset(
        Some("sample.txt".into()),
        3,
        10,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_count_prints_match_counts_per_input() {
  let dir = TempDir::new("count");
  dir.write("a.txt", b"needle\nneedle\n");
  dir.write("b.txt", b"needle\n");
  let cmd = RgCommand::parse(&[
    "-c".into(),
    "needle".into(),
    "a.txt".into(),
    "b.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::Count { path: Some("a.txt".into()), count: 2 },
      SearchOutcome::Count { path: Some("b.txt".into()), count: 1 },
    ]
  );
}

#[test]
fn behavior_count_matches_counts_individual_matches() {
  let dir = TempDir::new("count-matches");
  dir.write("sample.txt", b"needle needle\nneedle\n");
  let cmd = RgCommand::parse(&[
    "--count-matches".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::Count { path: Some("sample.txt".into()), count: 3 }]
  );
}

#[test]
fn behavior_only_matching_emits_each_match_separately() {
  let dir = TempDir::new("only-matching");
  dir.write("sample.txt", b"alpha needle omega needle\n");
  let cmd =
    RgCommand::parse(&["-o".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          1,
          6,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          1,
          19,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_include_zero_keeps_zero_counts() {
  let dir = TempDir::new("include-zero");
  dir.write("sample.txt", b"nope\n");
  let cmd = RgCommand::parse(&[
    "-c".into(),
    "--include-zero".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::Count { path: Some("sample.txt".into()), count: 0 }]
  );
}

#[test]
fn behavior_max_count_limits_matching_lines_per_file() {
  let dir = TempDir::new("max-count");
  dir.write("sample.txt", b"needle\nneedle\nneedle\n");
  let cmd = RgCommand::parse(&[
    "-m".into(),
    "2".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(actual.len(), 2);
}

#[test]
fn behavior_text_searches_binary_files() {
  let dir = TempDir::new("text-binary");
  dir.write("binary.bin", b"\0needle\0");
  let cmd =
    RgCommand::parse(&["-a".into(), "needle".into(), "binary.bin".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("binary.bin".into()),
        1,
        b"\0needle\0".to_vec(),
        vec![MatchSpan::new(1, 7).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_context_emits_surrounding_lines_and_separators() {
  let dir = TempDir::new("context");
  dir.write("sample.txt", b"zero\none\nneedle\nafter\nskip\nneedle two\nsix\n");
  let cmd = RgCommand::parse(&[
    "-C".into(),
    "1".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::ContextLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          2,
          5,
          b"one".to_vec(),
          vec![],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          3,
          9,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::ContextLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          4,
          16,
          b"after".to_vec(),
          vec![],
        )
        .unwrap(),
      ),
      SearchOutcome::ContextLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          5,
          22,
          b"skip".to_vec(),
          vec![],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          6,
          27,
          b"needle two".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::ContextLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          7,
          38,
          b"six".to_vec(),
          vec![],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_files_lists_searchable_files() {
  let dir = TempDir::new("files-mode");
  dir.write("a.txt", b"alpha\n");
  dir.write("nested/b.txt", b"beta\n");
  let cmd = RgCommand::parse(&["--files".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::FileMatch("./a.txt".into()),
      SearchOutcome::FileMatch("./nested/b.txt".into()),
    ]
  );
}

#[test]
fn behavior_sort_path_orders_results() {
  let dir = TempDir::new("sort-path");
  dir.write("b.txt", b"needle\n");
  dir.write("a.txt", b"needle\n");
  let cmd =
    RgCommand::parse(&["--sort=path".into(), "needle".into(), ".".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./a.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./b.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_sortr_path_reverses_order() {
  let dir = TempDir::new("sortr-path");
  dir.write("b.txt", b"needle\n");
  dir.write("a.txt", b"needle\n");
  let cmd =
    RgCommand::parse(&["--sortr=path".into(), "needle".into(), ".".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./b.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./a.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_standard_mode_returns_empty_when_there_are_no_matches() {
  let dir = TempDir::new("no-matches");
  dir.write("sample.txt", b"nope\n");
  let cmd = RgCommand::parse(&["needle".into(), "sample.txt".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert!(actual.is_empty());
}

#[test]
fn behavior_files_with_matches_stops_after_first_match_per_file() {
  let dir = TempDir::new("files-with-matches");
  dir.write("a.txt", b"needle\nneedle\n");
  dir.write("b.txt", b"nope\n");
  let cmd = RgCommand::parse(&[
    "-l".into(),
    "needle".into(),
    "a.txt".into(),
    "b.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(actual, vec![SearchOutcome::FileMatch("a.txt".into())]);
}

#[test]
fn behavior_files_without_match_only_prints_non_matching_files() {
  let dir = TempDir::new("files-without-match");
  dir.write("a.txt", b"needle\n");
  dir.write("b.txt", b"nope\n");
  let cmd = RgCommand::parse(&[
    "-L".into(),
    "needle".into(),
    "a.txt".into(),
    "b.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(actual, vec![SearchOutcome::FileWithoutMatch("b.txt".into())]);
}

#[test]
fn behavior_glob_limits_search_to_matching_paths() {
  let dir = TempDir::new("glob");
  dir.write("src/lib.rs", b"needle\n");
  dir.write("src/lib.toml", b"needle\n");
  let cmd = RgCommand::parse(&[
    "-g".into(),
    "*.rs".into(),
    "needle".into(),
    "src".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("src/lib.rs".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_hidden_is_excluded_by_default() {
  let dir = TempDir::new("hidden-default");
  dir.write(".hidden.txt", b"needle\n");
  dir.write("visible.txt", b"needle\n");
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./visible.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_hidden_includes_dotfiles_and_dotdirs() {
  let dir = TempDir::new("hidden");
  dir.write(".hidden.txt", b"needle\n");
  dir.write(".config/file.txt", b"needle\n");
  let cmd = RgCommand::parse(&["--hidden".into(), "needle".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./.hidden.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./.config/file.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_no_ignore_disables_gitignore_and_ignore_file_filtering() {
  let dir = TempDir::new("no-ignore");
  dir.write(".gitignore", b"ignored.txt\n");
  dir.write("ignored.txt", b"needle\n");
  dir.write("tracked.txt", b"needle\n");

  let cmd =
    RgCommand::parse(&["--no-ignore".into(), "needle".into(), ".".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./ignored.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("./tracked.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_default_respects_gitignore() {
  let dir = TempDir::new("gitignore-default");
  dir.write(".gitignore", b"ignored.txt\n");
  dir.write("ignored.txt", b"needle\n");
  dir.write("tracked.txt", b"needle\n");
  let cmd = RgCommand::parse(&["needle".into(), ".".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./tracked.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}

#[test]
fn behavior_null_prints_nul_terminated_paths_for_file_listing_modes() {
  let cmd = RgCommand::parse(&["-0".into(), "needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let presentation = plan.presentation(&runtime(Path::new("."))).unwrap();
  assert_eq!(presentation.path_terminator, b'\0');
}

#[test]
fn behavior_default_presentation_uses_newline_terminator() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let presentation = plan.presentation(&runtime(Path::new("."))).unwrap();
  assert_eq!(presentation.path_terminator, b'\n');
}

#[test]
fn behavior_color_auto_only_enables_colors_on_tty() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let tty_runtime = SearchRuntime {
    cwd: PathBuf::from("."),
    stdin: None,
    stdin_is_tty: true,
    stdout_is_tty: true,
  };
  let pipe_runtime = SearchRuntime {
    cwd: PathBuf::from("."),
    stdin: None,
    stdin_is_tty: true,
    stdout_is_tty: false,
  };
  assert_eq!(
    plan.presentation(&tty_runtime).unwrap(),
    PresentationSpec {
      color_enabled: true,
      path_terminator: b'\n',
      heading_mode: true,
      default_line_number: true,
    }
  );
  assert_eq!(
    plan.presentation(&pipe_runtime).unwrap(),
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    }
  );
}

#[test]
fn behavior_color_always_forces_match_highlighting() {
  let cmd =
    RgCommand::parse(&["--color=always".into(), "needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let runtime = SearchRuntime {
    cwd: PathBuf::from("."),
    stdin: None,
    stdin_is_tty: true,
    stdout_is_tty: false,
  };
  assert_eq!(
    plan.presentation(&runtime).unwrap(),
    PresentationSpec {
      color_enabled: true,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    }
  );
}

#[test]
fn behavior_no_color_disables_color_even_on_tty() {
  let cmd = RgCommand::parse(&["--no-color".into(), "needle".into()]).unwrap();
  let plan = SearchPlan::from_command(&cmd);
  let runtime = SearchRuntime {
    cwd: PathBuf::from("."),
    stdin: None,
    stdin_is_tty: true,
    stdout_is_tty: true,
  };
  assert_eq!(
    plan.presentation(&runtime).unwrap(),
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: true,
      default_line_number: true,
    }
  );
}

#[test]
fn behavior_tty_standard_output_uses_heading_style() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let output = render_outcomes(
    &[
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("a.txt".into()),
          1,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("b.txt".into()),
          3,
          b"needle again".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
    ],
    &cmd,
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: true,
      default_line_number: true,
    },
  );

  assert_eq!(
    String::from_utf8(output).unwrap(),
    "a.txt\n1:needle\n\nb.txt\n3:needle again\n"
  );
}

#[test]
fn behavior_tty_render_strips_leading_dot_slash_from_paths() {
  let cmd = RgCommand::parse(&["needle".into()]).unwrap();
  let output = render_outcomes(
    &[SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./a.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )],
    &cmd,
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: true,
      default_line_number: true,
    },
  );

  assert_eq!(String::from_utf8(output).unwrap(), "a.txt\n1:needle\n");
}

#[test]
fn behavior_colorized_stream_output_matches_rg_palette() {
  let cmd = RgCommand::parse(&[
    "-n".into(),
    "-H".into(),
    "--color=always".into(),
    "needle".into(),
  ])
  .unwrap();
  let output = render_outcomes(
    &[SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("a.txt".into()),
        2,
        b"alpha needle omega".to_vec(),
        vec![MatchSpan::new(6, 12).unwrap()],
      )
      .unwrap(),
    )],
    &cmd,
    PresentationSpec {
      color_enabled: true,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    },
  );

  assert_eq!(
    String::from_utf8(output).unwrap(),
    "\u{1b}[0m\u{1b}[35ma.txt\u{1b}[0m:\u{1b}[0m\u{1b}[32m2\u{1b}[0m:alpha \u{1b}[0m\u{1b}[1m\u{1b}[31mneedle\u{1b}[0m omega\n"
  );
}

#[test]
fn behavior_context_stream_render_uses_hyphen_delimiters() {
  let cmd = RgCommand::parse(&["-n".into(), "needle".into()]).unwrap();
  let output = render_outcomes(
    &[
      SearchOutcome::ContextLine(
        MatchRecord::new(Some("a.txt".into()), 1, b"before".to_vec(), vec![])
          .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("a.txt".into()),
          2,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::ContextSeparator,
    ],
    &cmd,
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    },
  );
  assert_eq!(
    String::from_utf8(output).unwrap(),
    "a.txt-1-before\na.txt:2:needle\n--\n"
  );
}

#[test]
fn behavior_stream_render_strips_leading_dot_slash_from_paths() {
  let cmd = RgCommand::parse(&["-n".into(), "needle".into()]).unwrap();
  let output = render_outcomes(
    &[SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./a.txt".into()),
        2,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )],
    &cmd,
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    },
  );

  assert_eq!(String::from_utf8(output).unwrap(), "a.txt:2:needle\n");
}

#[test]
fn behavior_stats_are_appended_to_output() {
  let cmd = RgCommand::parse(&["--stats".into(), "needle".into()]).unwrap();
  let output = PresentationSpec {
    color_enabled: false,
    path_terminator: b'\n',
    heading_mode: false,
    default_line_number: false,
  }
  .render_output(
    &cmd,
    &[SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("a.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )],
    SearchStats {
      matches: 1,
      matched_lines: 1,
      files_with_matches: 1,
      files_searched: 2,
      bytes_searched: 42,
    },
    std::time::Duration::from_micros(1_000),
    std::time::Duration::from_micros(2_000),
  );
  let rendered = String::from_utf8(output).unwrap();
  assert!(rendered.contains("1 matches"));
  assert!(rendered.contains("1 matched lines"));
  assert!(rendered.contains("1 files contained matches"));
  assert!(rendered.contains("2 files searched"));
  assert!(rendered.contains("42 bytes searched"));
  assert!(rendered.contains("0.001000 seconds spent searching"));
  assert!(rendered.contains("0.002000 seconds total"));
}

#[test]
fn behavior_quiet_suppresses_output_even_when_match_exists() {
  let dir = TempDir::new("quiet");
  dir.write("sample.txt", b"needle\n");
  let cmd =
    RgCommand::parse(&["-q".into(), "needle".into(), "sample.txt".into()])
      .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert!(actual.is_empty());
}

#[test]
fn behavior_passthru_emits_non_matching_lines_as_context_lines() {
  let dir = TempDir::new("passthru");
  dir.write("sample.txt", b"before\nneedle\nafter\n");
  let cmd = RgCommand::parse(&[
    "--passthru".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::ContextLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"before".to_vec(),
          vec![]
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          2,
          7,
          b"needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::ContextLine(
        MatchRecord::new_with_offset(
          Some("sample.txt".into()),
          3,
          14,
          b"after".to_vec(),
          vec![],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_vimgrep_emits_one_result_per_match_with_columns() {
  let dir = TempDir::new("vimgrep");
  dir.write("sample.txt", b"needle and needle\n");
  let cmd = RgCommand::parse(&[
    "--vimgrep".into(),
    "needle".into(),
    "sample.txt".into(),
  ])
  .unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"needle and needle".to_vec(),
          vec![MatchSpan::new(0, 6).unwrap()],
        )
        .unwrap(),
      ),
      SearchOutcome::MatchedLine(
        MatchRecord::new(
          Some("sample.txt".into()),
          1,
          b"needle and needle".to_vec(),
          vec![MatchSpan::new(11, 17).unwrap()],
        )
        .unwrap(),
      ),
    ]
  );
}

#[test]
fn behavior_vimgrep_render_includes_column() {
  let cmd = RgCommand::parse(&["--vimgrep".into(), "needle".into()]).unwrap();
  let output = render_outcomes(
    &[SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("a.txt".into()),
        2,
        b"alpha needle omega".to_vec(),
        vec![MatchSpan::new(6, 12).unwrap()],
      )
      .unwrap(),
    )],
    &cmd,
    PresentationSpec {
      color_enabled: false,
      path_terminator: b'\n',
      heading_mode: false,
      default_line_number: false,
    },
  );
  assert_eq!(
    String::from_utf8(output).unwrap(),
    "a.txt:2:7:alpha needle omega\n"
  );
}

#[test]
fn behavior_binary_files_are_skipped_or_reported_consistently() {
  let dir = TempDir::new("binary");
  dir.write("binary.bin", b"\0needle\0");
  dir.write("text.txt", b"needle\n");
  let cmd = RgCommand::parse(&["needle".into(), ".".into()]).unwrap();
  let actual =
    SearchEngine::default().search(&cmd, &runtime(dir.path())).unwrap();
  assert_eq!(
    actual,
    vec![SearchOutcome::MatchedLine(
      MatchRecord::new(
        Some("./text.txt".into()),
        1,
        b"needle".to_vec(),
        vec![MatchSpan::new(0, 6).unwrap()],
      )
      .unwrap(),
    )]
  );
}
