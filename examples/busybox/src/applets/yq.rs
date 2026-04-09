use std::{collections::BTreeMap, ffi::CString, io};

use indexmap::IndexMap;
use lio::api;
use serde::Deserialize;

use crate::{
  app::AppContext,
  applets::jq::{QueryParser, QueryPlan, is_truthy},
  command::Command,
  query::{EncodeOptions, Value, ValueEncoder, YamlCodec},
  util::io as io_util,
};

const LIO_READ_BATCH_SIZE: usize = 32;
const LIO_READ_BUF_SIZE: usize = 16 * 1024;
const COLOR_RESET: &str = "\x1b[0m";
const COLOR_NULL: &str = "\x1b[1;30m";
const COLOR_BOOL: &str = "\x1b[0;33m";
const COLOR_NUMBER: &str = "\x1b[0;36m";
const COLOR_STRING: &str = "\x1b[0;32m";
const COLOR_KEY: &str = "\x1b[1;34m";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct YqCommand {
  plan: QueryPlan,
  implicit_filter: bool,
  raw_output: bool,
  sort_keys: bool,
  color_output: Option<bool>,
  slurp: bool,
  stream_input: bool,
  null_input: bool,
  exit_status: bool,
  args: BTreeMap<String, Value>,
  files: Vec<String>,
}

impl Command for YqCommand {
  fn name() -> &'static str {
    "yq"
  }

  fn summary() -> &'static str {
    "Parse YAML and evaluate jq-style filters."
  }

  fn usage() -> &'static str {
    "yq [.] [file ...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    parse_command(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    if self.implicit_filter && self.files.is_empty() && stdin_is_tty(ctx) {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}\n\nUsage:\n  {}\n", Self::summary(), Self::usage()),
      ));
    }

    let mut slurped = Vec::new();
    let mut last_result = None;
    let mut rendered_output = Vec::new();
    let color_output = self.color_output.unwrap_or_else(|| stdout_is_tty(ctx));

    if self.null_input {
      let outputs = self.plan.execute(Value::Null, &self.args)?;
      if self.exit_status {
        last_result = outputs.last().map(is_truthy);
      }
      rendered_output.extend(render_yaml_outputs(
        outputs,
        self.raw_output,
        self.sort_keys,
        color_output,
      )?);
      io_util::write_all(ctx.lio(), &ctx.stdout(), rendered_output)?;
      if self.exit_status && !last_result.unwrap_or(false) {
        return Err(crate::exit_with_status(1));
      }
      return Ok(());
    }

    if self.files.is_empty() {
      let input = io_util::read_to_bytes_fd(ctx.lio(), &ctx.stdin())?;
      let inputs = parse_yaml_inputs(&input, self.slurp, self.stream_input)?;
      if self.exit_status {
        for input in &inputs {
          let outputs = self.plan.execute(input.clone(), &self.args)?;
          last_result = outputs.last().map(is_truthy);
        }
      }
      rendered_output.extend(render_yaml_inputs(
        inputs,
        &self.plan,
        &self.args,
        self.raw_output,
        self.sort_keys,
        color_output,
      )?);
      io_util::write_all(ctx.lio(), &ctx.stdout(), rendered_output)?;
      if self.exit_status && !last_result.unwrap_or(false) {
        return Err(crate::exit_with_status(1));
      }
      return Ok(());
    }

    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    let open_results = io_util::run_all(ctx.lio(), open_receivers);
    let mut files = Vec::with_capacity(open_results.len());
    for file in open_results {
      files.push(file?);
    }

    for batch in files.chunks(LIO_READ_BATCH_SIZE) {
      let inputs = read_file_batch(ctx, batch)?;
      for input in inputs {
        if self.slurp {
          slurped.extend(parse_yaml_stream(&input, self.stream_input)?);
        } else {
          let inputs = parse_yaml_inputs(&input, false, self.stream_input)?;
          if self.exit_status {
            for input in &inputs {
              let outputs = self.plan.execute(input.clone(), &self.args)?;
              last_result = outputs.last().map(is_truthy);
            }
          }
          rendered_output.extend(render_yaml_inputs(
            inputs,
            &self.plan,
            &self.args,
            self.raw_output,
            self.sort_keys,
            color_output,
          )?);
        }
      }
    }

    if self.slurp {
      if self.exit_status {
        let outputs =
          self.plan.execute(Value::Array(slurped.clone()), &self.args)?;
        last_result = outputs.last().map(is_truthy);
      }
      rendered_output.extend(render_yaml_inputs(
        vec![Value::Array(slurped)],
        &self.plan,
        &self.args,
        self.raw_output,
        self.sort_keys,
        color_output,
      )?);
    }

    io_util::write_all(ctx.lio(), &ctx.stdout(), rendered_output)?;

    if self.exit_status && !last_result.unwrap_or(false) {
      return Err(crate::exit_with_status(1));
    }

    Ok(())
  }
}

fn parse_command(args: &[String]) -> io::Result<YqCommand> {
  let mut raw_output = false;
  let mut sort_keys = false;
  let mut color_output = None;
  let mut slurp = false;
  let mut stream_input = false;
  let mut null_input = false;
  let mut exit_status = false;
  let mut arg_bindings = BTreeMap::new();
  let mut filter_text = None;
  let mut files = Vec::new();
  let mut index = 0;

  while index < args.len() {
    let arg = &args[index];
    if arg == "-r" {
      raw_output = true;
      index += 1;
      continue;
    }
    if arg == "-S" {
      sort_keys = true;
      index += 1;
      continue;
    }
    if arg == "-C" {
      color_output = Some(true);
      index += 1;
      continue;
    }
    if arg == "-M" {
      color_output = Some(false);
      index += 1;
      continue;
    }
    if arg == "--slurp" {
      slurp = true;
      index += 1;
      continue;
    }
    if arg == "--stream" {
      stream_input = true;
      index += 1;
      continue;
    }
    if arg == "-n" {
      null_input = true;
      index += 1;
      continue;
    }
    if arg == "-e" {
      exit_status = true;
      index += 1;
      continue;
    }
    if arg == "--arg" {
      if index + 2 >= args.len() {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "yq: --arg requires a name and a value",
        ));
      }
      let name = args[index + 1].clone();
      if !is_identifier(&name) {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("yq: invalid variable name: {name}"),
        ));
      }
      arg_bindings.insert(name, Value::String(args[index + 2].clone()));
      index += 3;
      continue;
    }
    if arg == "--argjson" || arg == "-c" {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("yq: unsupported flag {arg}"),
      ));
    }

    if arg.starts_with('-') && arg != "-" {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("yq: unsupported flag {arg}"),
      ));
    }

    if filter_text.is_none() && looks_like_filter_text(arg) {
      filter_text = Some(arg.clone());
      index += 1;
      continue;
    }

    files.push(arg.clone());
    index += 1;
  }

  let implicit_filter = filter_text.is_none();
  let plan = match filter_text.as_deref() {
    Some(".") | None => QueryPlan::identity(),
    Some(filter) => QueryParser::parse(filter)?,
  };

  Ok(YqCommand {
    plan,
    implicit_filter,
    raw_output,
    sort_keys,
    color_output,
    slurp,
    stream_input,
    null_input,
    exit_status,
    args: arg_bindings,
    files,
  })
}

fn looks_like_filter_text(arg: &str) -> bool {
  arg == "."
    || arg.starts_with('.')
    || arg.starts_with('$')
    || arg.contains('|')
    || arg.contains(',')
    || arg.contains("==")
    || arg.contains("!=")
    || arg == "length"
    || arg == "type"
    || arg == "keys"
    || arg == "keys_unsorted"
    || arg == "values"
    || arg == "empty"
    || arg == "sort"
    || arg == "reverse"
    || arg == "unique"
    || arg == "any"
    || arg == "all"
    || arg == "to_entries"
    || arg == "from_entries"
    || arg == "first"
    || arg == "last"
    || arg == ".."
    || arg.starts_with("has(")
    || arg.starts_with("contains(")
    || arg.starts_with("startswith(")
    || arg.starts_with("endswith(")
    || arg.starts_with("join(")
    || arg.starts_with("unique_by(")
    || arg.starts_with("map(")
    || arg.starts_with("map_values(")
    || arg.starts_with("select(")
}

fn is_identifier(value: &str) -> bool {
  let mut chars = value.chars();
  match chars.next() {
    Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
    _ => return false,
  }
  chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn parse_yaml_stream(input: &[u8], stream: bool) -> io::Result<Vec<Value>> {
  let mut values = Vec::new();
  for document in serde_norway::Deserializer::from_slice(input) {
    let value = Value::deserialize(document).map_err(|err| {
      io::Error::new(io::ErrorKind::InvalidData, format!("yq: {err}"))
    })?;
    if stream {
      values.extend(stream_value(&value));
    } else {
      values.push(value);
    }
  }
  Ok(values)
}

fn parse_yaml_inputs(
  input: &[u8],
  slurp: bool,
  stream: bool,
) -> io::Result<Vec<Value>> {
  let values = parse_yaml_stream(input, stream)?;
  if slurp { Ok(vec![Value::Array(values)]) } else { Ok(values) }
}

fn render_yaml_inputs(
  inputs: Vec<Value>,
  plan: &QueryPlan,
  args: &BTreeMap<String, Value>,
  raw_output: bool,
  sort_keys: bool,
  color_output: bool,
) -> io::Result<Vec<u8>> {
  let mut output = Vec::new();
  for input in inputs {
    let values = plan.execute(input, args)?;
    output.extend(render_yaml_outputs(
      values,
      raw_output,
      sort_keys,
      color_output,
    )?);
  }
  Ok(output)
}

fn render_yaml_outputs(
  values: Vec<Value>,
  raw_output: bool,
  sort_keys: bool,
  color_output: bool,
) -> io::Result<Vec<u8>> {
  let mut output = Vec::new();
  for value in values {
    let value = if sort_keys { sort_value_keys(value) } else { value };
    match (raw_output, &value) {
      (true, Value::String(text)) => output.extend_from_slice(text.as_bytes()),
      _ => {
        if color_output {
          output.extend(render_colored_yaml(&value));
        } else {
          output.extend_from_slice(
            &YamlCodec
              .encode_value(&value, EncodeOptions::default())
              .map_err(|err| io::Error::other(format!("yq: {err}")))?,
          );
        }
      }
    }
    if !output.ends_with(b"\n") {
      output.push(b'\n');
    }
  }
  Ok(output)
}

fn render_colored_yaml(value: &Value) -> Vec<u8> {
  let text = YamlCodec
    .encode_value(value, EncodeOptions::default())
    .expect("yaml render");
  colorize_yaml_text(&String::from_utf8_lossy(&text))
}

fn colorize_yaml_text(text: &str) -> Vec<u8> {
  let mut out = Vec::new();
  for chunk in text.split_inclusive('\n') {
    let (line, newline) =
      chunk.strip_suffix('\n').map_or((chunk, ""), |line| (line, "\n"));
    out.extend(colorize_yaml_line(line));
    out.extend_from_slice(newline.as_bytes());
  }
  out
}

fn colorize_yaml_line(line: &str) -> Vec<u8> {
  let mut out = Vec::new();
  let indent_len = line
    .char_indices()
    .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
    .unwrap_or(line.len());
  let (indent, rest) = line.split_at(indent_len);
  out.extend_from_slice(indent.as_bytes());

  if rest.is_empty() || rest.starts_with('#') {
    out.extend_from_slice(rest.as_bytes());
    return out;
  }

  let rest = if let Some(tail) = rest.strip_prefix("- ") {
    out.extend_from_slice(b"- ");
    tail
  } else {
    rest
  };

  if let Some((key, tail)) = split_yaml_key_value(rest) {
    write_colorized(&mut out, COLOR_KEY, key);
    out.push(b':');
    let tail = tail
      .strip_prefix(':')
      .expect("yaml key/value tail should start with ':'");
    if let Some(value) = tail.strip_prefix(' ') {
      out.push(b' ');
      colorize_yaml_scalar(&mut out, value);
    } else {
      out.extend_from_slice(tail.as_bytes());
    }
    return out;
  }

  colorize_yaml_scalar(&mut out, rest);
  out
}

fn split_yaml_key_value(line: &str) -> Option<(&str, &str)> {
  let colon = line.find(':')?;
  let (key, tail) = line.split_at(colon);
  if key.is_empty() || key.contains("  ") {
    return None;
  }
  Some((key, tail))
}

fn colorize_yaml_scalar(out: &mut Vec<u8>, text: &str) {
  let trimmed = text.trim();
  let color = if trimmed == "null" || trimmed == "~" {
    Some(COLOR_NULL)
  } else if trimmed == "true" || trimmed == "false" {
    Some(COLOR_BOOL)
  } else if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
    Some(COLOR_NUMBER)
  } else if looks_like_quoted_yaml_string(trimmed) || !trimmed.is_empty() {
    Some(COLOR_STRING)
  } else {
    None
  };

  if let Some(color) = color {
    write_colorized(out, color, text);
  } else {
    out.extend_from_slice(text.as_bytes());
  }
}

fn looks_like_quoted_yaml_string(text: &str) -> bool {
  (text.starts_with('"') && text.ends_with('"'))
    || (text.starts_with('\'') && text.ends_with('\''))
}

fn write_colorized(out: &mut Vec<u8>, color: &str, text: &str) {
  out.extend_from_slice(color.as_bytes());
  out.extend_from_slice(text.as_bytes());
  out.extend_from_slice(COLOR_RESET.as_bytes());
}

fn sort_value_keys(value: Value) -> Value {
  match value {
    Value::Array(values) => {
      Value::Array(values.into_iter().map(sort_value_keys).collect())
    }
    Value::Object(object) => {
      let mut sorted = IndexMap::new();
      let mut entries: Vec<_> = object.into_iter().collect();
      entries.sort_by(|(left, _), (right, _)| left.cmp(right));
      for (key, value) in entries {
        sorted.insert(key, sort_value_keys(value));
      }
      Value::Object(sorted)
    }
    value => value,
  }
}

fn stream_value(value: &Value) -> Vec<Value> {
  let mut out = Vec::new();
  let mut path = Vec::new();
  push_stream_value(&mut out, &mut path, value);
  out
}

fn push_stream_value(
  out: &mut Vec<Value>,
  path: &mut Vec<Value>,
  value: &Value,
) {
  match value {
    Value::Array(values) => {
      if values.is_empty() {
        out.push(Value::Array(vec![
          Value::Array(path.clone()),
          Value::Array(Vec::new()),
        ]));
        return;
      }
      for (index, entry) in values.iter().enumerate() {
        path.push(Value::from(index));
        push_stream_value(out, path, entry);
        path.pop();
      }
    }
    Value::Object(object) => {
      if object.is_empty() {
        out.push(Value::Array(vec![
          Value::Array(path.clone()),
          Value::Object(IndexMap::new()),
        ]));
        return;
      }
      for (key, entry) in object {
        path.push(Value::String(key.clone()));
        push_stream_value(out, path, entry);
        path.pop();
      }
    }
    _ => {
      out.push(Value::Array(vec![Value::Array(path.clone()), value.clone()]))
    }
  }
}

fn stdin_is_tty(ctx: &AppContext) -> bool {
  #[cfg(unix)]
  {
    use std::os::fd::AsRawFd;

    let stdin = ctx.stdin();
    unsafe { libc::isatty(stdin.as_raw_fd()) == 1 }
  }

  #[cfg(not(unix))]
  {
    let _ = ctx;
    false
  }
}

fn stdout_is_tty(ctx: &AppContext) -> bool {
  #[cfg(unix)]
  {
    use std::os::fd::AsRawFd;

    let stdout = ctx.stdout();
    unsafe { libc::isatty(stdout.as_raw_fd()) == 1 }
  }

  #[cfg(not(unix))]
  {
    let _ = ctx;
    false
  }
}

fn read_file_batch(
  ctx: &AppContext,
  files: &[lio::api::resource::Resource],
) -> io::Result<Vec<Vec<u8>>> {
  #[derive(Debug)]
  struct PendingRead {
    fd: lio::api::resource::Resource,
    buf: Vec<u8>,
    bytes: Vec<u8>,
    done: bool,
  }

  let mut pending = files
    .iter()
    .cloned()
    .map(|fd| PendingRead {
      fd,
      buf: vec![0u8; LIO_READ_BUF_SIZE],
      bytes: Vec::new(),
      done: false,
    })
    .collect::<Vec<_>>();

  while pending.iter().any(|file| !file.done) {
    let mut read_receivers = Vec::new();
    let mut active_indices = Vec::new();

    for (index, file) in pending.iter_mut().enumerate() {
      if file.done {
        continue;
      }
      read_receivers.push(
        api::read(&file.fd, std::mem::take(&mut file.buf))
          .with_lio(ctx.lio())
          .send(),
      );
      active_indices.push(index);
    }

    for (index, (result, returned_buf)) in active_indices
      .into_iter()
      .zip(io_util::run_all(ctx.lio(), read_receivers))
    {
      let file = &mut pending[index];
      file.buf = returned_buf;

      let n = result? as usize;
      if n == 0 {
        file.done = true;
        continue;
      }

      file.bytes.extend_from_slice(&file.buf[..n]);
    }
  }

  Ok(pending.into_iter().map(|file| file.bytes).collect())
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;

  #[test]
  fn parse_command_collects_flags_and_args() {
    let parsed = YqCommand::parse(&[
      "-n".into(),
      "-e".into(),
      "-S".into(),
      "-C".into(),
      "--arg".into(),
      "who".into(),
      "vt".into(),
      "$who".into(),
    ])
    .unwrap();
    assert!(parsed.null_input);
    assert!(parsed.exit_status);
    assert!(parsed.sort_keys);
    assert_eq!(parsed.color_output, Some(true));
    assert_eq!(parsed.args.get("who"), Some(&Value::from("vt")));
  }

  #[test]
  fn parse_yaml_inputs_supports_multi_document_slurp() {
    let values =
      parse_yaml_inputs(b"---\na: 1\n---\na: 2\n", true, false).unwrap();
    assert_eq!(
      values,
      vec![Value::Array(vec![
        Value::object([("a", Value::from(1_i64))]),
        Value::object([("a", Value::from(2_i64))]),
      ])]
    );
  }

  #[test]
  fn render_yaml_inputs_emits_plain_yaml() {
    let output = render_yaml_inputs(
      vec![Value::object([(
        "meta",
        Value::object([("b", Value::from(2_i64)), ("a", Value::from(1_i64))]),
      )])],
      &QueryParser::parse(".meta").unwrap(),
      &BTreeMap::new(),
      false,
      true,
      false,
    )
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("a: 1"));
    assert!(text.contains("b: 2"));
  }

  #[test]
  fn raw_output_prints_strings_without_yaml_encoding() {
    let output =
      render_yaml_outputs(vec![Value::from("vt")], true, false, false).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "vt\n");
  }

  #[test]
  fn color_output_renders_ansi_sequences() {
    let output = render_yaml_outputs(
      vec![Value::object([("name", Value::from("vt"))])],
      false,
      false,
      true,
    )
    .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("\u{1b}[1;34mname\u{1b}[0m:"));
    assert!(text.contains("\u{1b}[0m"));
    assert!(text.contains("vt"));
  }

  #[test]
  fn stream_input_emits_path_value_tuples() {
    let values = parse_yaml_inputs(b"a:\n  - 1\n", false, true).unwrap();
    assert_eq!(
      values,
      vec![Value::Array(vec![
        Value::Array(vec![Value::from("a"), Value::from(0_i64)]),
        Value::from(1_i64),
      ])]
    );
  }
}
