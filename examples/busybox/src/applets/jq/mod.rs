use std::{collections::BTreeMap, ffi::CString, io};

#[cfg(feature = "jq")]
use lio::api;

use crate::{
  app::AppContext, command::Command, query::Value, util::io as io_util,
};

pub(crate) mod ast;
pub(crate) mod parser;
#[cfg(feature = "jq")]
pub(crate) mod render;

use ast::{
  Assignment, AssignmentOp, BinaryOp, Builtin, BuiltinArg, BuiltinCall,
  CompareOp, Comparison, Constructor, ExprTerm, Filter, ObjectEntry, Pipeline,
  Stage, UnaryOp,
};
#[cfg(feature = "jq")]
use render::{
  RenderOptions, Renderer, parse_json_inputs, parse_json_stream, render_inputs,
  stdout_is_tty,
};

pub(crate) use ast::{QueryPlan, is_truthy};
pub(crate) use parser::QueryParser;

#[cfg(feature = "jq")]
const LIO_READ_BATCH_SIZE: usize = 32;
#[cfg(feature = "jq")]
const LIO_READ_BUF_SIZE: usize = 16 * 1024;

#[cfg(feature = "jq")]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JqCommand {
  plan: QueryPlan,
  implicit_filter: bool,
  raw_output: bool,
  compact_output: bool,
  sort_keys: bool,
  color_output: Option<bool>,
  slurp: bool,
  stream_input: bool,
  null_input: bool,
  exit_status: bool,
  args: BTreeMap<String, Value>,
  files: Vec<String>,
}

#[cfg(feature = "jq")]
impl Command for JqCommand {
  fn name() -> &'static str {
    "jq"
  }

  fn summary() -> &'static str {
    "Parse and pretty-print JSON."
  }

  fn usage() -> &'static str {
    "jq [.] [file ...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    QueryParser::parse_command(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    if self.implicit_filter && self.files.is_empty() && stdin_is_tty(ctx) {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}\n\nUsage:\n  {}\n", Self::summary(), Self::usage()),
      ));
    }

    let color_output = self.color_output.unwrap_or_else(|| stdout_is_tty(ctx));
    let renderer = Renderer::new(RenderOptions {
      raw_output: self.raw_output,
      compact_output: self.compact_output,
      sort_keys: self.sort_keys,
      color_output,
    });
    let mut slurped = Vec::new();
    let mut last_result = None;
    let mut rendered_output = Vec::new();
    if self.null_input {
      let outputs = self.plan.execute(Value::Null, &self.args)?;
      if self.exit_status {
        last_result = outputs.last().map(is_truthy);
      }
      rendered_output.extend(render_inputs(
        outputs,
        &QueryPlan::identity(),
        &BTreeMap::new(),
        &renderer,
      )?);
      io_util::write_all(ctx.lio(), &ctx.stdout(), rendered_output)?;
      if self.exit_status && !last_result.unwrap_or(false) {
        return Err(crate::exit_with_status(1));
      }
      return Ok(());
    }

    if self.files.is_empty() {
      let input = io_util::read_to_bytes_fd(ctx.lio(), &ctx.stdin())?;
      let inputs = parse_json_inputs(&input, self.slurp, self.stream_input)?;
      if self.exit_status {
        for input in &inputs {
          let outputs = self.plan.execute(input.clone(), &self.args)?;
          last_result = outputs.last().map(is_truthy);
        }
      }
      rendered_output
        .extend(render_inputs(inputs, &self.plan, &self.args, &renderer)?);
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
          slurped.extend(parse_json_stream(&input, self.stream_input)?);
        } else {
          let inputs = parse_json_inputs(&input, false, self.stream_input)?;
          if self.exit_status {
            for input in &inputs {
              let outputs = self.plan.execute(input.clone(), &self.args)?;
              last_result = outputs.last().map(is_truthy);
            }
          }
          rendered_output
            .extend(render_inputs(inputs, &self.plan, &self.args, &renderer)?);
        }
      }
    }

    if self.slurp {
      if self.exit_status {
        let outputs =
          self.plan.execute(Value::Array(slurped.clone()), &self.args)?;
        last_result = outputs.last().map(is_truthy);
      }
      rendered_output.extend(render_inputs(
        vec![Value::Array(slurped)],
        &self.plan,
        &self.args,
        &renderer,
      )?);
    }

    io_util::write_all(ctx.lio(), &ctx.stdout(), rendered_output)?;

    if self.exit_status && !last_result.unwrap_or(false) {
      return Err(crate::exit_with_status(1));
    }

    Ok(())
  }
}

#[cfg(feature = "jq")]
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

#[cfg(feature = "jq")]
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

#[cfg(feature = "jq")]
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_command_recognizes_output_flags_and_files() {
    let parsed = JqCommand::parse(&[
      "-c".into(),
      "-S".into(),
      "--slurp".into(),
      ".".into(),
      "data.json".into(),
    ])
    .unwrap();
    assert!(parsed.compact_output);
    assert!(parsed.sort_keys);
    assert!(parsed.slurp);
    assert!(!parsed.implicit_filter);
    assert_eq!(parsed.files, vec!["data.json"]);
    assert_eq!(parsed.plan, QueryPlan::identity());
  }

  #[test]
  fn parse_command_collects_arg_bindings() {
    let parsed = JqCommand::parse(&[
      "--arg".into(),
      "who".into(),
      "vt".into(),
      "$who".into(),
    ])
    .unwrap();
    assert_eq!(parsed.args.get("who"), Some(&Value::String("vt".into())));
  }

  #[test]
  fn parse_command_recognizes_stream_flag() {
    let parsed =
      JqCommand::parse(&["--stream".into(), ".".into(), "data.json".into()])
        .unwrap();
    assert!(parsed.stream_input);
    assert_eq!(parsed.files, vec!["data.json"]);
  }

  #[test]
  fn parse_command_tracks_implicit_filter_when_missing() {
    let parsed = JqCommand::parse(&[]).unwrap();
    assert!(parsed.implicit_filter);
    assert_eq!(parsed.plan, QueryPlan::identity());
  }

  #[test]
  fn parse_command_marks_explicit_identity_filter_as_explicit() {
    let parsed = JqCommand::parse(&[".".into()]).unwrap();
    assert!(!parsed.implicit_filter);
    assert_eq!(parsed.plan, QueryPlan::identity());
  }
}
