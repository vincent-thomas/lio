use super::*;
use serde_json::{Map, Value, json};
use std::io;

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_PATH: &str = "\x1b[35m";
const ANSI_LINE_NUMBER: &str = "\x1b[32m";
const ANSI_MATCH: &str = "\x1b[1m\x1b[31m";
const INITIAL_STREAM_FLUSH_THRESHOLD: usize = 256 * 1024;
const STREAM_FLUSH_THRESHOLD: usize = 1024 * 1024;

pub(super) trait ByteSink {
  fn write_chunk(&mut self, bytes: Vec<u8>) -> io::Result<()>;
}

pub(super) struct AppByteSink<'a> {
  ctx: &'a crate::app::AppContext,
}

impl<'a> AppByteSink<'a> {
  pub(super) fn new(ctx: &'a crate::app::AppContext) -> Self {
    Self { ctx }
  }
}

impl ByteSink for AppByteSink<'_> {
  fn write_chunk(&mut self, bytes: Vec<u8>) -> io::Result<()> {
    crate::util::io::write_all(self.ctx.lio(), &self.ctx.stdout(), bytes)
  }
}

#[cfg(test)]
#[derive(Default)]
struct VecByteSink {
  bytes: Vec<u8>,
}

#[cfg(test)]
impl ByteSink for VecByteSink {
  fn write_chunk(&mut self, bytes: Vec<u8>) -> io::Result<()> {
    self.bytes.extend_from_slice(&bytes);
    Ok(())
  }
}

pub(super) struct StreamingRenderer<'a, W: ByteSink> {
  presentation: PresentationSpec,
  plan: &'a SearchPlan,
  writer: W,
  buf: Vec<u8>,
  bytes_written: usize,
  current_path: Option<String>,
  show_line_number: bool,
  current_file_start: usize,
}

impl SearchStats {
  fn append_to(
    self,
    out: &mut Vec<u8>,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
  ) {
    let bytes_printed = out.len();
    if !out.is_empty() && !out.ends_with(b"\n\n") {
      out.push(b'\n');
    }
    append_stats_line(out, &format!("{} matches", self.matches));
    append_stats_line(out, &format!("{} matched lines", self.matched_lines));
    append_stats_line(
      out,
      &format!("{} files contained matches", self.files_with_matches),
    );
    append_stats_line(out, &format!("{} files searched", self.files_searched));
    append_stats_line(out, &format!("{} bytes printed", bytes_printed));
    append_stats_line(out, &format!("{} bytes searched", self.bytes_searched));
    append_stats_line(
      out,
      &format!("{:.6} seconds spent searching", search_elapsed.as_secs_f64()),
    );
    append_stats_line(
      out,
      &format!("{:.6} seconds total", total_elapsed.as_secs_f64()),
    );
  }
}

impl PresentationSpec {
  #[cfg(test)]
  pub(super) fn render_output(
    self,
    command: &RgCommand,
    outcomes: &[SearchOutcome],
    stats: SearchStats,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
  ) -> Vec<u8> {
    self.render_plan_output(
      &SearchPlan::from_command(command),
      outcomes,
      stats,
      search_elapsed,
      total_elapsed,
    )
  }

  #[cfg(test)]
  pub(super) fn render_plan_output(
    self,
    plan: &SearchPlan,
    outcomes: &[SearchOutcome],
    stats: SearchStats,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
  ) -> Vec<u8> {
    if plan.config.search.quiet {
      return Vec::new();
    }
    if plan.config.output.json {
      return self.render_json_outcomes(
        outcomes,
        stats,
        search_elapsed,
        total_elapsed,
      );
    }
    let mut out = if self.heading_mode {
      self.render_heading_outcomes(outcomes, plan)
    } else {
      self.render_stream_outcomes(outcomes, plan)
    };
    if plan.config.search.stats && !plan.config.search.files_mode {
      stats.append_to(&mut out, search_elapsed, total_elapsed);
    }
    out
  }

  #[cfg(test)]
  fn render_json_outcomes(
    self,
    outcomes: &[SearchOutcome],
    stats: SearchStats,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
  ) -> Vec<u8> {
    let mut out = Vec::new();
    let mut current_file_start = 0usize;

    for outcome in outcomes {
      let value = match outcome {
        SearchOutcome::JsonBegin { path } => json_begin_value(path.as_deref()),
        SearchOutcome::MatchedLine(record) => json_match_value(record),
        SearchOutcome::BinaryMatch { .. } => continue,
        SearchOutcome::ContextLine(record) => json_context_value(record),
        SearchOutcome::ContextSeparator => continue,
        SearchOutcome::Count { .. } => continue,
        SearchOutcome::FileMatch(_) => continue,
        SearchOutcome::FileWithoutMatch(_) => continue,
        SearchOutcome::JsonEnd {
          path,
          bytes_searched,
          matches,
          matched_lines,
          has_match,
          elapsed,
        } => json_end_value(
          path.as_deref(),
          *bytes_searched,
          *matches,
          *matched_lines,
          *has_match,
          *elapsed,
        ),
      };
      if let SearchOutcome::JsonBegin { .. } = outcome {
        current_file_start = out.len();
      }

      let mut bytes = serde_json::to_vec(&value).expect("json render");
      if let SearchOutcome::JsonEnd { .. } = outcome {
        let mut end_value = value;
        if let Some(stats) = end_value
          .get_mut("data")
          .and_then(|data| data.get_mut("stats"))
          .and_then(serde_json::Value::as_object_mut)
        {
          stats.insert(
            "bytes_printed".into(),
            json!(out.len().saturating_sub(current_file_start)),
          );
        }
        bytes = serde_json::to_vec(&end_value).expect("json render");
      }
      out.extend_from_slice(&bytes);
      out.push(b'\n');
    }

    let bytes_printed = out.len();
    let mut summary_stats = Map::new();
    summary_stats.insert("bytes_printed".into(), json!(bytes_printed));
    summary_stats.insert("bytes_searched".into(), json!(stats.bytes_searched));
    summary_stats.insert("elapsed".into(), json_duration(search_elapsed));
    summary_stats.insert("matched_lines".into(), json!(stats.matched_lines));
    summary_stats.insert("matches".into(), json!(stats.matches));
    summary_stats.insert("searches".into(), json!(stats.files_searched));
    summary_stats
      .insert("searches_with_match".into(), json!(stats.files_with_matches));

    let mut summary_data = Map::new();
    summary_data.insert("elapsed_total".into(), json_duration(total_elapsed));
    summary_data.insert("stats".into(), Value::Object(summary_stats));

    let mut summary = Map::new();
    summary.insert("data".into(), Value::Object(summary_data));
    summary.insert("type".into(), json!("summary"));
    out.extend_from_slice(
      serde_json::to_string(&Value::Object(summary))
        .expect("json summary")
        .as_bytes(),
    );
    out.push(b'\n');

    out
  }

  #[cfg(test)]
  fn render_heading_outcomes(
    self,
    outcomes: &[SearchOutcome],
    plan: &SearchPlan,
  ) -> Vec<u8> {
    let mut out = Vec::new();
    let mut current_path: Option<&str> = None;
    let show_line_number = should_show_line_numbers(
      plan.config.output.line_number_mode,
      self.default_line_number,
      plan.effective_match_mode(),
    );

    for outcome in outcomes {
      match outcome {
        SearchOutcome::JsonBegin { .. } => {}
        SearchOutcome::MatchedLine(record) => {
          let path = record.path.as_deref();
          if path != current_path {
            if !out.is_empty() {
              out.push(b'\n');
            }
            if let Some(path) = path {
              self.push_path(&mut out, path);
              out.push(b'\n');
            }
            current_path = path;
          }

          if show_line_number {
            self.push_colored_segment(
              &mut out,
              record.line_number.to_string().as_bytes(),
              ANSI_LINE_NUMBER,
            );
            out.push(b':');
          }
          self.push_highlighted_line(&mut out, &record.line, &record.spans);
          out.push(b'\n');
        }
        SearchOutcome::BinaryMatch { path } => {
          if let Some(path) = path {
            self.push_path(&mut out, path);
            out.push(b'\n');
          }
        }
        SearchOutcome::ContextLine(record) => {
          if show_line_number {
            self.push_colored_segment(
              &mut out,
              record.line_number.to_string().as_bytes(),
              ANSI_LINE_NUMBER,
            );
            out.push(b'-');
          }
          out.extend_from_slice(&record.line);
          out.push(b'\n');
        }
        SearchOutcome::ContextSeparator => out.extend_from_slice(b"--\n"),
        SearchOutcome::Count { .. }
        | SearchOutcome::FileMatch(_)
        | SearchOutcome::FileWithoutMatch(_)
        | SearchOutcome::JsonEnd { .. } => {
          self.render_stream_outcome(&mut out, outcome, plan, false);
        }
      }
    }

    out
  }

  #[cfg(test)]
  fn render_stream_outcomes(
    self,
    outcomes: &[SearchOutcome],
    plan: &SearchPlan,
  ) -> Vec<u8> {
    let mut out = Vec::new();
    let show_line_number = should_show_line_numbers(
      plan.config.output.line_number_mode,
      false,
      plan.effective_match_mode(),
    );
    for outcome in outcomes {
      self.render_stream_outcome(&mut out, outcome, plan, show_line_number);
    }
    out
  }

  fn render_stream_outcome(
    self,
    out: &mut Vec<u8>,
    outcome: &SearchOutcome,
    plan: &SearchPlan,
    show_line_number: bool,
  ) {
    match outcome {
      SearchOutcome::JsonBegin { .. } | SearchOutcome::JsonEnd { .. } => {}
      SearchOutcome::MatchedLine(record) => {
        if plan.config.output.vimgrep {
          if let Some(path) = &record.path {
            self.push_path(out, path);
            out.push(b':');
          }
          self.push_colored_segment(
            out,
            record.line_number.to_string().as_bytes(),
            ANSI_LINE_NUMBER,
          );
          out.push(b':');
          let column = record.spans.first().map_or(1, |span| span.start + 1);
          out.extend_from_slice(column.to_string().as_bytes());
          out.push(b':');
          self.push_highlighted_line(out, &record.line, &record.spans);
          out.push(b'\n');
          return;
        }
        if let Some(path) = &record.path {
          self.push_path(out, path);
          out.push(b':');
        }
        if show_line_number {
          self.push_colored_segment(
            out,
            record.line_number.to_string().as_bytes(),
            ANSI_LINE_NUMBER,
          );
          out.push(b':');
        }
        self.push_highlighted_line(out, &record.line, &record.spans);
        out.push(b'\n');
      }
      SearchOutcome::BinaryMatch { path } => {
        if let Some(path) = path {
          self.push_path(out, path);
          out.push(b'\n');
        }
      }
      SearchOutcome::ContextLine(record) => {
        if let Some(path) = &record.path {
          self.push_path(out, path);
          out.push(b'-');
        }
        if show_line_number {
          self.push_colored_segment(
            out,
            record.line_number.to_string().as_bytes(),
            ANSI_LINE_NUMBER,
          );
          out.push(b'-');
        }
        out.extend_from_slice(&record.line);
        out.push(b'\n');
      }
      SearchOutcome::ContextSeparator => out.extend_from_slice(b"--\n"),
      SearchOutcome::Count { path, count } => {
        if let Some(path) = path {
          self.push_path(out, path);
          out.push(b':');
        }
        out.extend_from_slice(count.to_string().as_bytes());
        out.push(b'\n');
      }
      SearchOutcome::FileMatch(path)
      | SearchOutcome::FileWithoutMatch(path) => {
        self.push_path(out, path);
        out.push(self.path_terminator);
      }
    }
  }

  fn push_path(self, out: &mut Vec<u8>, path: &str) {
    let rendered = path.strip_prefix("./").unwrap_or(path);
    self.push_colored_segment(out, rendered.as_bytes(), ANSI_PATH);
  }

  fn push_colored_segment(self, out: &mut Vec<u8>, bytes: &[u8], ansi: &str) {
    if self.color_enabled {
      out.extend_from_slice(ANSI_RESET.as_bytes());
      out.extend_from_slice(ansi.as_bytes());
    }
    out.extend_from_slice(bytes);
    if self.color_enabled {
      out.extend_from_slice(ANSI_RESET.as_bytes());
    }
  }

  fn push_highlighted_line(
    self,
    out: &mut Vec<u8>,
    line: &[u8],
    spans: &[MatchSpan],
  ) {
    if !self.color_enabled || spans.is_empty() {
      out.extend_from_slice(line);
      return;
    }

    let mut cursor = 0usize;
    for span in spans {
      if cursor < span.start {
        out.extend_from_slice(&line[cursor..span.start]);
      }
      out.extend_from_slice(ANSI_RESET.as_bytes());
      out.extend_from_slice(ANSI_MATCH.as_bytes());
      out.extend_from_slice(&line[span.start..span.end]);
      out.extend_from_slice(ANSI_RESET.as_bytes());
      cursor = span.end;
    }

    if cursor < line.len() {
      out.extend_from_slice(&line[cursor..]);
    }
  }
}

#[cfg(test)]
fn json_begin_value(path: Option<&str>) -> Value {
  json!({
    "type": "begin",
    "data": {
      "path": path.map(json_path)
    }
  })
}

#[cfg(test)]
fn json_match_value(record: &MatchRecord) -> Value {
  json!({
    "type": "match",
    "data": {
      "path": record.path.as_deref().map(json_path),
      "lines": { "text": format!("{}\n", String::from_utf8_lossy(&record.line)) },
      "line_number": record.line_number,
      "absolute_offset": record.absolute_offset,
      "submatches": record.spans.iter().map(|span| json!({
        "match": { "text": String::from_utf8_lossy(&record.line[span.start..span.end]) },
        "start": span.start,
        "end": span.end
      })).collect::<Vec<_>>()
    }
  })
}

#[cfg(test)]
fn json_context_value(record: &MatchRecord) -> Value {
  json!({
    "type": "context",
    "data": {
      "path": record.path.as_deref().map(json_path),
      "lines": { "text": format!("{}\n", String::from_utf8_lossy(&record.line)) },
      "line_number": record.line_number,
      "absolute_offset": record.absolute_offset,
      "submatches": []
    }
  })
}

#[cfg(test)]
fn json_end_value(
  path: Option<&str>,
  bytes_searched: usize,
  matches: usize,
  matched_lines: usize,
  has_match: bool,
  elapsed: std::time::Duration,
) -> Value {
  json!({
    "type": "end",
    "data": {
      "path": path.map(json_path),
      "binary_offset": serde_json::Value::Null,
      "stats": {
        "elapsed": json_duration(elapsed),
        "searches": 1,
        "searches_with_match": usize::from(has_match),
        "bytes_searched": bytes_searched,
        "bytes_printed": 0,
        "matched_lines": matched_lines,
        "matches": matches
      }
    }
  })
}

impl<'a, W: ByteSink> StreamingRenderer<'a, W> {
  pub(super) fn new(
    presentation: PresentationSpec,
    plan: &'a SearchPlan,
    writer: W,
  ) -> Self {
    let show_line_number = should_show_line_numbers(
      plan.config.output.line_number_mode,
      if presentation.heading_mode {
        presentation.default_line_number
      } else {
        false
      },
      plan.effective_match_mode(),
    );
    Self {
      presentation,
      plan,
      writer,
      buf: Vec::with_capacity(Self::initial_buffer_capacity()),
      bytes_written: 0,
      current_path: None,
      show_line_number,
      current_file_start: 0,
    }
  }

  fn emit_outcome(&mut self, outcome: &SearchOutcome) -> io::Result<()> {
    if self.plan.config.search.quiet {
      return Ok(());
    }

    if self.plan.config.output.json {
      self.render_json_outcome(outcome)?;
    } else if self.presentation.heading_mode {
      self.render_heading_outcome(outcome);
    } else {
      self.presentation.render_stream_outcome(
        &mut self.buf,
        outcome,
        self.plan,
        self.show_line_number,
      );
    }
    self.flush_if_needed()
  }

  pub(super) fn finish(
    mut self,
    stats: SearchStats,
    search_elapsed: std::time::Duration,
    total_elapsed: std::time::Duration,
  ) -> io::Result<W> {
    if !self.plan.config.search.quiet {
      if self.plan.config.output.json {
        let bytes_printed = self.bytes_written + self.buf.len();
        let mut summary_stats = Map::new();
        summary_stats.insert("bytes_printed".into(), json!(bytes_printed));
        summary_stats
          .insert("bytes_searched".into(), json!(stats.bytes_searched));
        summary_stats.insert("elapsed".into(), json_duration(search_elapsed));
        summary_stats
          .insert("matched_lines".into(), json!(stats.matched_lines));
        summary_stats.insert("matches".into(), json!(stats.matches));
        summary_stats.insert("searches".into(), json!(stats.files_searched));
        summary_stats.insert(
          "searches_with_match".into(),
          json!(stats.files_with_matches),
        );

        let mut summary_data = Map::new();
        summary_data
          .insert("elapsed_total".into(), json_duration(total_elapsed));
        summary_data.insert("stats".into(), Value::Object(summary_stats));

        let mut summary = Map::new();
        summary.insert("data".into(), Value::Object(summary_data));
        summary.insert("type".into(), json!("summary"));
        self.buf.extend_from_slice(
          serde_json::to_string(&Value::Object(summary))
            .expect("json summary")
            .as_bytes(),
        );
        self.buf.push(b'\n');
      } else if self.plan.config.search.stats
        && !self.plan.config.search.files_mode
      {
        stats.append_to(&mut self.buf, search_elapsed, total_elapsed);
      }
    }
    self.flush()?;
    Ok(self.writer)
  }

  fn flush_if_needed(&mut self) -> io::Result<()> {
    let threshold = self.flush_threshold();
    if self.buf.len() >= threshold {
      self.flush()?;
    }
    Ok(())
  }

  fn flush(&mut self) -> io::Result<()> {
    if self.buf.is_empty() {
      return Ok(());
    }
    let next_capacity = self.next_buffer_capacity();
    let bytes =
      std::mem::replace(&mut self.buf, Vec::with_capacity(next_capacity));
    self.bytes_written += bytes.len();
    self.writer.write_chunk(bytes)
  }

  fn flush_threshold(&self) -> usize {
    if self.bytes_written == 0 {
      INITIAL_STREAM_FLUSH_THRESHOLD
    } else {
      STREAM_FLUSH_THRESHOLD
    }
  }

  fn initial_buffer_capacity() -> usize {
    STREAM_FLUSH_THRESHOLD
  }

  fn next_buffer_capacity(&self) -> usize {
    self.flush_threshold().max(STREAM_FLUSH_THRESHOLD)
  }

  fn render_json_outcome(&mut self, outcome: &SearchOutcome) -> io::Result<()> {
    let value = match outcome {
      SearchOutcome::JsonBegin { path } => {
        self.current_file_start = self.bytes_written + self.buf.len();
        json!({
          "type": "begin",
          "data": {
            "path": path.as_deref().map(json_path)
          }
        })
      }
      SearchOutcome::MatchedLine(record) => json!({
        "type": "match",
        "data": {
          "path": record.path.as_deref().map(json_path),
          "lines": { "text": format!("{}\n", String::from_utf8_lossy(&record.line)) },
          "line_number": record.line_number,
          "absolute_offset": record.absolute_offset,
          "submatches": record.spans.iter().map(|span| json!({
            "match": { "text": String::from_utf8_lossy(&record.line[span.start..span.end]) },
            "start": span.start,
            "end": span.end
          })).collect::<Vec<_>>()
        }
      }),
      SearchOutcome::BinaryMatch { .. } => return Ok(()),
      SearchOutcome::ContextLine(record) => json!({
        "type": "context",
        "data": {
          "path": record.path.as_deref().map(json_path),
          "lines": { "text": format!("{}\n", String::from_utf8_lossy(&record.line)) },
          "line_number": record.line_number,
          "absolute_offset": record.absolute_offset,
          "submatches": []
        }
      }),
      SearchOutcome::ContextSeparator => return Ok(()),
      SearchOutcome::Count { .. } => return Ok(()),
      SearchOutcome::FileMatch(_) => return Ok(()),
      SearchOutcome::FileWithoutMatch(_) => return Ok(()),
      SearchOutcome::JsonEnd {
        path,
        bytes_searched,
        matches,
        matched_lines,
        has_match,
        elapsed,
      } => {
        let bytes_printed =
          self.bytes_written + self.buf.len() - self.current_file_start;
        json!({
          "type": "end",
          "data": {
            "path": path.as_deref().map(json_path),
            "binary_offset": serde_json::Value::Null,
            "stats": {
              "elapsed": json_duration(*elapsed),
              "searches": 1,
              "searches_with_match": usize::from(*has_match),
              "bytes_searched": bytes_searched,
              "bytes_printed": bytes_printed,
              "matched_lines": matched_lines,
              "matches": matches
            }
          }
        })
      }
    };

    self.buf.extend_from_slice(
      serde_json::to_string(&value).expect("json render").as_bytes(),
    );
    self.buf.push(b'\n');
    Ok(())
  }

  fn render_plain_match_direct(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    line: &[u8],
  ) -> io::Result<()> {
    if self.plan.config.search.quiet {
      return Ok(());
    }

    if self.presentation.heading_mode {
      if path != self.current_path.as_deref() {
        if !self.buf.is_empty() {
          self.buf.push(b'\n');
        }
        if let Some(path) = path {
          self.presentation.push_path(&mut self.buf, path);
          self.buf.push(b'\n');
        }
        self.current_path = path.map(str::to_owned);
      }

      if self.show_line_number {
        self.presentation.push_colored_segment(
          &mut self.buf,
          line_number.to_string().as_bytes(),
          ANSI_LINE_NUMBER,
        );
        self.buf.push(b':');
      }
      self.buf.extend_from_slice(line);
      self.buf.push(b'\n');
    } else {
      if let Some(path) = path {
        self.presentation.push_path(&mut self.buf, path);
        self.buf.push(b':');
      }
      if self.show_line_number {
        self.presentation.push_colored_segment(
          &mut self.buf,
          line_number.to_string().as_bytes(),
          ANSI_LINE_NUMBER,
        );
        self.buf.push(b':');
      }
      self.buf.extend_from_slice(line);
      self.buf.push(b'\n');
    }

    self.flush_if_needed()
  }

  fn render_vimgrep_lines(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    _absolute_offset: usize,
    line: &[u8],
    spans: &[MatchSpan],
  ) {
    for span in spans {
      if let Some(path) = path {
        self.presentation.push_path(&mut self.buf, path);
        self.buf.push(b':');
      }
      self.presentation.push_colored_segment(
        &mut self.buf,
        line_number.to_string().as_bytes(),
        ANSI_LINE_NUMBER,
      );
      self.buf.push(b':');
      self.buf.extend_from_slice((span.start + 1).to_string().as_bytes());
      self.buf.push(b':');
      self.presentation.push_highlighted_line(
        &mut self.buf,
        line,
        std::slice::from_ref(span),
      );
      self.buf.push(b'\n');
    }
  }

  fn render_only_matching_lines(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
    spans: &[MatchSpan],
  ) {
    let _ = absolute_offset;
    for span in spans {
      if let Some(path) = path {
        self.presentation.push_path(&mut self.buf, path);
        self.buf.push(b':');
      }
      if self.show_line_number {
        self.presentation.push_colored_segment(
          &mut self.buf,
          line_number.to_string().as_bytes(),
          ANSI_LINE_NUMBER,
        );
        self.buf.push(b':');
      }
      self.buf.extend_from_slice(&line[span.start..span.end]);
      self.buf.push(b'\n');
    }
  }

  fn render_heading_outcome(&mut self, outcome: &SearchOutcome) {
    match outcome {
      SearchOutcome::JsonBegin { .. } => {}
      SearchOutcome::MatchedLine(record) => {
        let path = record.path.clone();
        if path != self.current_path {
          if !self.buf.is_empty() {
            self.buf.push(b'\n');
          }
          if let Some(path) = path.as_deref() {
            self.presentation.push_path(&mut self.buf, path);
            self.buf.push(b'\n');
          }
          self.current_path = path;
        }

        if self.show_line_number {
          self.presentation.push_colored_segment(
            &mut self.buf,
            record.line_number.to_string().as_bytes(),
            ANSI_LINE_NUMBER,
          );
          self.buf.push(b':');
        }
        self.presentation.push_highlighted_line(
          &mut self.buf,
          &record.line,
          &record.spans,
        );
        self.buf.push(b'\n');
      }
      SearchOutcome::BinaryMatch { path } => {
        if let Some(path) = path {
          self.presentation.push_path(&mut self.buf, path);
          self.buf.push(b'\n');
        }
      }
      SearchOutcome::ContextLine(record) => {
        if self.show_line_number {
          self.presentation.push_colored_segment(
            &mut self.buf,
            record.line_number.to_string().as_bytes(),
            ANSI_LINE_NUMBER,
          );
          self.buf.push(b'-');
        }
        self.buf.extend_from_slice(&record.line);
        self.buf.push(b'\n');
      }
      SearchOutcome::ContextSeparator => self.buf.extend_from_slice(b"--\n"),
      SearchOutcome::Count { .. }
      | SearchOutcome::FileMatch(_)
      | SearchOutcome::FileWithoutMatch(_)
      | SearchOutcome::JsonEnd { .. } => {
        self.presentation.render_stream_outcome(
          &mut self.buf,
          outcome,
          self.plan,
          false,
        );
      }
    }
  }
}

impl<W: ByteSink> SearchOutcomeSink for StreamingRenderer<'_, W> {
  fn emit_outcome(&mut self, outcome: SearchOutcome) -> io::Result<()> {
    Self::emit_outcome(self, &outcome)
  }

  fn emit_match_line(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
    spans: &[MatchSpan],
  ) -> io::Result<()> {
    if self.plan.config.search.quiet {
      return Ok(());
    }

    if self.plan.config.output.json {
      let value = json!({
        "type": "match",
        "data": {
          "path": path.map(json_path),
          "lines": { "text": format!("{}\n", String::from_utf8_lossy(line)) },
          "line_number": line_number,
          "absolute_offset": absolute_offset,
          "submatches": spans.iter().map(|span| json!({
            "match": { "text": String::from_utf8_lossy(&line[span.start..span.end]) },
            "start": span.start,
            "end": span.end
          })).collect::<Vec<_>>()
        }
      });
      self.buf.extend_from_slice(
        serde_json::to_string(&value).expect("json render").as_bytes(),
      );
      self.buf.push(b'\n');
      return self.flush_if_needed();
    }

    if self.presentation.heading_mode {
      if path != self.current_path.as_deref() {
        if !self.buf.is_empty() {
          self.buf.push(b'\n');
        }
        if let Some(path) = path {
          self.presentation.push_path(&mut self.buf, path);
          self.buf.push(b'\n');
        }
        self.current_path = path.map(str::to_owned);
      }

      if self.plan.config.output.vimgrep {
        self.render_vimgrep_lines(
          path,
          line_number,
          absolute_offset,
          line,
          spans,
        );
      } else if self.plan.config.output.only_matching
        && !self.plan.config.search.invert_match
      {
        self.render_only_matching_lines(
          path,
          line_number,
          absolute_offset,
          line,
          spans,
        );
      } else {
        if self.show_line_number {
          self.presentation.push_colored_segment(
            &mut self.buf,
            line_number.to_string().as_bytes(),
            ANSI_LINE_NUMBER,
          );
          self.buf.push(b':');
        }
        self.presentation.push_highlighted_line(&mut self.buf, line, spans);
        self.buf.push(b'\n');
      }
    } else if self.plan.config.output.vimgrep {
      self.render_vimgrep_lines(
        path,
        line_number,
        absolute_offset,
        line,
        spans,
      );
    } else if self.plan.config.output.only_matching
      && !self.plan.config.search.invert_match
    {
      self.render_only_matching_lines(
        path,
        line_number,
        absolute_offset,
        line,
        spans,
      );
    } else {
      if let Some(path) = path {
        self.presentation.push_path(&mut self.buf, path);
        self.buf.push(b':');
      }
      if self.show_line_number {
        self.presentation.push_colored_segment(
          &mut self.buf,
          line_number.to_string().as_bytes(),
          ANSI_LINE_NUMBER,
        );
        self.buf.push(b':');
      }
      self.presentation.push_highlighted_line(&mut self.buf, line, spans);
      self.buf.push(b'\n');
    }

    self.flush_if_needed()
  }

  fn emit_plain_match(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    _absolute_offset: usize,
    line: &[u8],
  ) -> io::Result<()> {
    Self::render_plain_match_direct(self, path, line_number, line)
  }

  fn emit_context_line(
    &mut self,
    path: Option<&str>,
    line_number: usize,
    absolute_offset: usize,
    line: &[u8],
  ) -> io::Result<()> {
    let _ = absolute_offset;
    if self.plan.config.search.quiet {
      return Ok(());
    }

    if self.plan.config.output.json {
      let value = json!({
        "type": "context",
        "data": {
          "path": path.map(json_path),
          "lines": { "text": format!("{}\n", String::from_utf8_lossy(line)) },
          "line_number": line_number,
          "absolute_offset": absolute_offset,
          "submatches": []
        }
      });
      self.buf.extend_from_slice(
        serde_json::to_string(&value).expect("json render").as_bytes(),
      );
      self.buf.push(b'\n');
      return self.flush_if_needed();
    }

    if self.presentation.heading_mode {
      if path != self.current_path.as_deref() {
        if !self.buf.is_empty() {
          self.buf.push(b'\n');
        }
        if let Some(path) = path {
          self.presentation.push_path(&mut self.buf, path);
          self.buf.push(b'\n');
        }
        self.current_path = path.map(str::to_owned);
      }
      if self.show_line_number {
        self.presentation.push_colored_segment(
          &mut self.buf,
          line_number.to_string().as_bytes(),
          ANSI_LINE_NUMBER,
        );
        self.buf.push(b'-');
      }
      self.buf.extend_from_slice(line);
      self.buf.push(b'\n');
    } else {
      if let Some(path) = path {
        self.presentation.push_path(&mut self.buf, path);
        self.buf.push(b'-');
      }
      if self.show_line_number {
        self.presentation.push_colored_segment(
          &mut self.buf,
          line_number.to_string().as_bytes(),
          ANSI_LINE_NUMBER,
        );
        self.buf.push(b'-');
      }
      self.buf.extend_from_slice(line);
      self.buf.push(b'\n');
    }

    self.flush_if_needed()
  }

  fn flush_file(&mut self) -> io::Result<()> {
    self.flush()
  }
}

#[cfg(test)]
pub(super) fn stream_render_plan_output(
  presentation: PresentationSpec,
  plan: &SearchPlan,
  outcomes: &[SearchOutcome],
  stats: SearchStats,
  search_elapsed: std::time::Duration,
  total_elapsed: std::time::Duration,
) -> Vec<u8> {
  let writer = VecByteSink::default();
  let mut renderer = StreamingRenderer::new(presentation, plan, writer);
  for outcome in outcomes {
    renderer.emit_outcome(outcome).expect("stream render");
  }
  renderer
    .finish(stats, search_elapsed, total_elapsed)
    .expect("stream render finish")
    .bytes
}

#[cfg(test)]
pub(super) fn stream_render_plain_matches_output(
  presentation: PresentationSpec,
  plan: &SearchPlan,
  matches: &[(Option<&str>, usize, usize, &[u8])],
) -> Vec<u8> {
  let writer = VecByteSink::default();
  let mut renderer = StreamingRenderer::new(presentation, plan, writer);
  for (path, line_number, absolute_offset, line) in matches {
    SearchOutcomeSink::emit_plain_match(
      &mut renderer,
      *path,
      *line_number,
      *absolute_offset,
      line,
    )
    .expect("plain stream render");
  }
  renderer
    .finish(
      SearchStats::default(),
      std::time::Duration::ZERO,
      std::time::Duration::ZERO,
    )
    .expect("plain stream render finish")
    .bytes
}

fn json_duration(duration: std::time::Duration) -> serde_json::Value {
  json!({
    "human": format!("{:.6}s", duration.as_secs_f64()),
    "nanos": duration.as_nanos(),
    "secs": duration.as_secs(),
  })
}

fn append_stats_line(out: &mut Vec<u8>, line: &str) {
  out.extend_from_slice(line.as_bytes());
  out.push(b'\n');
}

fn json_path(path: &str) -> serde_json::Value {
  json!({ "text": path.strip_prefix("./").unwrap_or(path) })
}

pub(super) fn should_show_line_numbers(
  mode: LineNumberMode,
  tty_default: bool,
  match_mode: MatchMode,
) -> bool {
  if match_mode != MatchMode::Standard {
    return false;
  }

  match mode {
    LineNumberMode::Always => true,
    LineNumberMode::Never => false,
    LineNumberMode::Auto => tty_default,
  }
}
