use std::{io, process::Command as ProcessCommand};

use crate::{
  app::AppContext,
  applets::support::{XargsSeparator, build_xargs_groups, read_yes_from_tty},
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct XargsCommand {
  pub batch_size: Option<usize>,
  pub separator: Option<XargsSeparator>,
  pub trace: bool,
  pub confirm: bool,
  pub no_run_if_empty: bool,
  pub exact_size: bool,
  pub max_lines: Option<usize>,
  pub replace_token: Option<String>,
  pub command: String,
  pub initial_args: Vec<String>,
}

impl Command for XargsCommand {
  fn name() -> &'static str {
    "xargs"
  }
  fn summary() -> &'static str {
    "Build and execute command lines from stdin."
  }
  fn usage() -> &'static str {
    "xargs [-0] [-r] [-t] [-p] [-x] [-n N] [-L N] [-I repl] [-d delim] <command> [initial-args...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    let mut batch_size = None;
    let mut separator = XargsSeparator::Whitespace;
    let mut trace = false;
    let mut confirm = false;
    let mut no_run_if_empty = false;
    let mut exact_size = false;
    let mut max_lines = None;
    let mut replace_token = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
      match arg.as_str() {
        "-n" => {
          let value = args.get(index + 1).and_then(|s| s.parse::<usize>().ok());
          let Some(parsed) = value.filter(|value| *value > 0) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "xargs: invalid -n value",
            ));
          };
          batch_size = Some(parsed);
          index += 2;
        }
        "-L" => {
          let value = args.get(index + 1).and_then(|s| s.parse::<usize>().ok());
          let Some(parsed) = value.filter(|value| *value > 0) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "xargs: invalid -L value",
            ));
          };
          max_lines = Some(parsed);
          index += 2;
        }
        "-I" => {
          let Some(token) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "xargs: missing replacement token",
            ));
          };
          replace_token = Some(token.clone());
          index += 2;
        }
        "-d" => {
          let Some(delim) = args.get(index + 1) else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "xargs: missing delimiter",
            ));
          };
          let Some(ch) = delim.chars().next() else {
            return Err(io::Error::new(
              io::ErrorKind::InvalidInput,
              "xargs: delimiter must not be empty",
            ));
          };
          separator = XargsSeparator::Delim(ch);
          index += 2;
        }
        "-0" => {
          separator = XargsSeparator::Nul;
          index += 1;
        }
        "-r" => {
          no_run_if_empty = true;
          index += 1;
        }
        "-t" => {
          trace = true;
          index += 1;
        }
        "-p" => {
          confirm = true;
          trace = true;
          index += 1;
        }
        "-x" => {
          exact_size = true;
          index += 1;
        }
        _ => break,
      }
    }
    let Some(command) = args.get(index) else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "xargs: missing command",
      ));
    };
    Ok(Self {
      batch_size,
      separator: Some(separator),
      trace,
      confirm,
      no_run_if_empty,
      exact_size,
      max_lines,
      replace_token,
      command: command.clone(),
      initial_args: args[index + 1..].to_vec(),
    })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = io_util::read_to_string(ctx.lio(), None)?;
    let command_groups = build_xargs_groups(
      &input,
      self.separator.expect("xargs separator should be parsed"),
      self.batch_size,
      self.max_lines,
      self.replace_token.as_deref(),
      self.exact_size,
    )?;
    if command_groups.is_empty() {
      return Ok(());
    }
    for chunk in command_groups {
      let effective_args: Vec<String> =
        if let Some(token) = self.replace_token.as_deref() {
          let replacement = chunk.join(" ");
          let mut args = Vec::new();
          for arg in &self.initial_args {
            args.push(arg.replace(token, &replacement));
          }
          if args.is_empty() {
            args.push(replacement);
          }
          args
        } else {
          let mut args = self.initial_args.clone();
          args.extend(chunk);
          args
        };
      if self.trace {
        let mut rendered = self.command.clone();
        for arg in &effective_args {
          rendered.push(' ');
          rendered.push_str(arg);
        }
        rendered.push('\n');
        io_util::write_all(ctx.lio(), &ctx.stderr(), rendered.into_bytes())?;
      }
      if self.confirm {
        io_util::write_all(ctx.lio(), &ctx.stderr(), b"? ".to_vec())?;
        let accepted = read_yes_from_tty(ctx.lio())?
          .is_some_and(|ch| ch == 'y' || ch == 'Y');
        if !accepted {
          continue;
        }
      }
      let status =
        ProcessCommand::new(&self.command).args(&effective_args).status()?;
      if !status.success() {
        return Err(io::Error::other(format!(
          "xargs child exited with status {status}"
        )));
      }
    }
    Ok(())
  }
}
