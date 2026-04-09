use std::{collections::BTreeMap, io};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use indexmap::IndexMap;

#[cfg(test)]
use crate::query::ValueDecoder;
use crate::{
  app::AppContext,
  query::{EncodeOptions, JsonCodec, Value, ValueEncoder},
};

use super::QueryPlan;

const COLOR_RESET: &str = "\x1b[0m";
const COLOR_NULL: &str = "\x1b[1;30m";
const COLOR_BOOL: &str = "\x1b[0;33m";
const COLOR_NUMBER: &str = "\x1b[0;36m";
const COLOR_STRING: &str = "\x1b[0;32m";
const COLOR_KEY: &str = "\x1b[1;34m";

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RenderOptions {
  pub raw_output: bool,
  pub compact_output: bool,
  pub sort_keys: bool,
  pub color_output: bool,
}

pub(super) struct Renderer {
  options: RenderOptions,
}

impl Renderer {
  pub(super) fn new(options: RenderOptions) -> Self {
    Self { options }
  }

  pub(super) fn render(
    &self,
    plan: &QueryPlan,
    input: Value,
    args: &BTreeMap<String, Value>,
  ) -> io::Result<Vec<u8>> {
    let values = plan.execute(input, args)?;
    let mut output = Vec::new();
    for value in values {
      let value = if self.options.sort_keys {
        sort_json_value_keys(value)
      } else {
        value
      };
      match (self.options.raw_output, &value) {
        (true, Value::String(text)) => {
          output.extend_from_slice(text.as_bytes())
        }
        _ => {
          let encoded = if self.options.color_output {
            render_colored_json(&value, self.options.compact_output)
          } else {
            JsonCodec
              .encode_value(
                &value,
                EncodeOptions { pretty: !self.options.compact_output },
              )
              .map_err(|err| io::Error::other(format!("jq: {err}")))?
          };
          output.extend_from_slice(&encoded)
        }
      }
      output.push(b'\n');
    }
    Ok(output)
  }
}

pub(super) fn parse_json_stream(
  input: &[u8],
  stream: bool,
) -> io::Result<Vec<Value>> {
  let parser = serde_json::Deserializer::from_slice(input).into_iter::<Value>();
  let mut values = Vec::new();
  for value in parser {
    let value = value.map_err(|err| {
      io::Error::new(io::ErrorKind::InvalidData, format!("jq: {err}"))
    })?;
    if stream {
      values.extend(stream_json_value(&value));
    } else {
      values.push(value);
    }
  }
  Ok(values)
}

pub(super) fn parse_json_inputs(
  input: &[u8],
  slurp: bool,
  stream: bool,
) -> io::Result<Vec<Value>> {
  let values = parse_json_stream(input, stream)?;
  if slurp { Ok(vec![Value::Array(values)]) } else { Ok(values) }
}

pub(super) fn render_inputs(
  inputs: Vec<Value>,
  plan: &QueryPlan,
  args: &BTreeMap<String, Value>,
  renderer: &Renderer,
) -> io::Result<Vec<u8>> {
  let mut output = Vec::new();
  for input in inputs {
    output.extend(renderer.render(plan, input, args)?);
  }
  Ok(output)
}

#[cfg(test)]
pub(super) fn render_json(
  input: &[u8],
  plan: &QueryPlan,
  raw_output: bool,
) -> io::Result<Vec<u8>> {
  render_json_with_options(
    input,
    plan,
    &BTreeMap::new(),
    RenderOptions { raw_output, ..RenderOptions::default() },
  )
}

#[cfg(test)]
pub(super) fn render_json_with_options(
  input: &[u8],
  plan: &QueryPlan,
  args: &BTreeMap<String, Value>,
  options: RenderOptions,
) -> io::Result<Vec<u8>> {
  let value = JsonCodec.decode_value(input).map_err(|err| {
    io::Error::new(io::ErrorKind::InvalidData, format!("jq: {err}"))
  })?;
  Renderer::new(options).render(plan, value, args)
}

#[cfg(test)]
pub(super) fn render_json_stream_with_options(
  input: &[u8],
  plan: &QueryPlan,
  args: &BTreeMap<String, Value>,
  slurp: bool,
  options: RenderOptions,
) -> io::Result<Vec<u8>> {
  let mut output = Vec::new();
  let renderer = Renderer::new(options);
  for value in parse_json_inputs(input, slurp, false)? {
    output.extend(renderer.render(plan, value, args)?);
  }
  Ok(output)
}

fn stream_json_value(value: &Value) -> Vec<Value> {
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

pub(super) fn stdout_is_tty(ctx: &AppContext) -> bool {
  #[cfg(unix)]
  {
    let stdout = ctx.stdout();
    unsafe { libc::isatty(stdout.as_raw_fd()) == 1 }
  }

  #[cfg(not(unix))]
  {
    let _ = ctx;
    false
  }
}

pub(super) fn render_colored_json(value: &Value, compact: bool) -> Vec<u8> {
  let mut out = Vec::new();
  write_colored_json_value(&mut out, value, compact, 0);
  out
}

fn write_colored_json_value(
  out: &mut Vec<u8>,
  value: &Value,
  compact: bool,
  indent: usize,
) {
  match value {
    Value::Null => write_colorized(out, COLOR_NULL, "null"),
    Value::Bool(boolean) => {
      write_colorized(out, COLOR_BOOL, if *boolean { "true" } else { "false" })
    }
    Value::Number(number) => {
      write_colorized(out, COLOR_NUMBER, &number.to_string())
    }
    Value::String(text) => {
      let rendered = serde_json::to_string(text).expect("json string encoding");
      write_colorized(out, COLOR_STRING, &rendered);
    }
    Value::Array(values) => {
      out.push(b'[');
      if !values.is_empty() {
        for (index, entry) in values.iter().enumerate() {
          if compact {
            if index > 0 {
              out.push(b',');
            }
          } else {
            out.push(b'\n');
            write_indent(out, indent + 2);
          }
          write_colored_json_value(out, entry, compact, indent + 2);
        }
        if !compact {
          out.push(b'\n');
          write_indent(out, indent);
        }
      }
      out.push(b']');
    }
    Value::Object(object) => {
      out.push(b'{');
      if !object.is_empty() {
        for (index, (key, entry)) in object.iter().enumerate() {
          if compact {
            if index > 0 {
              out.push(b',');
            }
          } else {
            out.push(b'\n');
            write_indent(out, indent + 2);
          }
          let rendered_key =
            serde_json::to_string(key).expect("json string encoding");
          write_colorized(out, COLOR_KEY, &rendered_key);
          if compact {
            out.push(b':');
          } else {
            out.extend_from_slice(b": ");
          }
          write_colored_json_value(out, entry, compact, indent + 2);
        }
        if !compact {
          out.push(b'\n');
          write_indent(out, indent);
        }
      }
      out.push(b'}');
    }
  }
}

fn write_indent(out: &mut Vec<u8>, indent: usize) {
  out.resize(out.len() + indent, b' ');
}

fn write_colorized(out: &mut Vec<u8>, color: &str, text: &str) {
  out.extend_from_slice(color.as_bytes());
  out.extend_from_slice(text.as_bytes());
  out.extend_from_slice(COLOR_RESET.as_bytes());
}

pub(super) fn sort_json_value_keys(value: Value) -> Value {
  match value {
    Value::Array(values) => {
      Value::Array(values.into_iter().map(sort_json_value_keys).collect())
    }
    Value::Object(object) => {
      let mut sorted = IndexMap::new();
      let mut entries: Vec<_> = object.into_iter().collect();
      entries.sort_by(|(left, _), (right, _)| left.cmp(right));
      for (key, value) in entries {
        sorted.insert(key, sort_json_value_keys(value));
      }
      Value::Object(sorted)
    }
    value => value,
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;
  use crate::applets::jq::{ast::QueryPlan, parser::QueryParser};
  use crate::query::Value;

  #[test]
  fn slurp_collects_multiple_json_values_into_array() {
    let output = render_json_stream_with_options(
      b"1\n2\n3\n",
      &QueryPlan::identity(),
      &BTreeMap::new(),
      true,
      RenderOptions { compact_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "[1,2,3]\n");
  }

  #[test]
  fn raw_output_prints_unquoted_strings() {
    let output = render_json(br#""vt""#, &QueryPlan::identity(), true).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "vt\n");
  }

  #[test]
  fn compact_output_renders_single_line_json() {
    let output = render_json_with_options(
      br#"{"b":2,"a":1}"#,
      &QueryPlan::identity(),
      &BTreeMap::new(),
      RenderOptions { compact_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "{\"b\":2,\"a\":1}\n");
  }

  #[test]
  fn sorted_keys_reorders_object_output() {
    let output = render_json_with_options(
      br#"{"b":2,"a":1}"#,
      &QueryPlan::identity(),
      &BTreeMap::new(),
      RenderOptions {
        compact_output: true,
        sort_keys: true,
        ..RenderOptions::default()
      },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "{\"a\":1,\"b\":2}\n");
  }

  #[test]
  fn stream_without_slurp_runs_plan_per_value() {
    let output = render_json_stream_with_options(
      b"{\"n\":1}\n{\"n\":2}\n",
      &QueryParser::parse(".n").unwrap(),
      &BTreeMap::new(),
      false,
      RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "1\n2\n");
  }

  #[test]
  fn stream_input_emits_path_value_tuples() {
    let streamed = parse_json_inputs(br#"{"a":[1]}"#, false, true).unwrap();
    assert_eq!(
      streamed,
      vec![Value::Array(vec![
        Value::Array(vec![Value::from("a"), Value::from(0_i64)]),
        Value::from(1_i64),
      ])]
    );
  }
}
