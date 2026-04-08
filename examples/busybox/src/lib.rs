mod app;
pub mod applets;
pub mod command;
mod registry;
mod util;

use std::{env, io, path::Path, process::ExitCode};

use app::AppContext;
use command::Command;
use registry::Registry;

pub struct Busybox {
  ctx: AppContext,
  registry: Registry,
}

#[derive(Debug)]
struct ExitStatusError(u8);

impl std::fmt::Display for ExitStatusError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "process exited with status {}", self.0)
  }
}

impl std::error::Error for ExitStatusError {}

pub(crate) fn exit_with_status(status: u8) -> io::Error {
  io::Error::other(ExitStatusError(status))
}

fn extract_exit_status(err: &io::Error) -> Option<u8> {
  err
    .get_ref()
    .and_then(|inner| inner.downcast_ref::<ExitStatusError>())
    .map(|status| status.0)
}

impl Busybox {
  pub fn new() -> io::Result<Self> {
    Ok(Self { ctx: AppContext::new()?, registry: Registry::new() })
  }

  pub fn call(&self, command: impl Command) -> io::Result<()> {
    command.execute(&self.ctx)
  }

  pub fn run_registered(&self, name: &str, args: &[String]) -> io::Result<()> {
    validate_registered_invocation(name, args)?;
    if let Some(command) = self.registry.find(name) {
      return command.parse_and_execute(&self.ctx, args);
    }

    Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!("unknown applet: {name}"),
    ))
  }
}

pub fn run_from_env() -> io::Result<()> {
  let args: Vec<String> = env::args().collect();
  run_with_args(args)
}

pub fn run_from_env_exit_code() -> ExitCode {
  match run_from_env() {
    Ok(()) => ExitCode::SUCCESS,
    Err(err) => match extract_exit_status(&err) {
      Some(status) => ExitCode::from(status),
      None => {
        eprintln!("{err}");
        ExitCode::FAILURE
      }
    },
  }
}

fn run_with_args(args: Vec<String>) -> io::Result<()> {
  let ctx = AppContext::new()?;
  let registry = Registry::new();
  let argv0 = args
    .first()
    .and_then(|s| Path::new(s).file_name())
    .and_then(|s| s.to_str())
    .unwrap_or("busybox");

  let (applet, rest) = resolve_applet(&registry, argv0, &args);

  if let Some(applet) = applet {
    if rest.iter().any(|arg| arg == "--help" || arg == "-h") {
      if let Some(command) = registry.find(applet) {
        print_command_help(&ctx, command)?;
      } else {
        print_top_level_help(&ctx, &registry, argv0)?;
      }
      return Ok(());
    }

    if let Some(command) = registry.find(applet) {
      validate_registered_invocation(applet, rest)?;
      return command.parse_and_execute(&ctx, rest);
    }

    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!("unknown applet: {applet}"),
    ));
  }

  print_top_level_help(&ctx, &registry, argv0)?;
  Ok(())
}

fn validate_registered_invocation(
  name: &str,
  args: &[String],
) -> io::Result<()> {
  if name == "[" && !args.last().is_some_and(|arg| arg == "]") {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "[: missing closing ']'",
    ));
  }
  Ok(())
}

fn resolve_applet<'a>(
  registry: &'a Registry,
  argv0: &'a str,
  args: &'a [String],
) -> (Option<&'a str>, &'a [String]) {
  if registry.find(argv0).is_some() {
    return (Some(argv0), &args[1..]);
  }

  let Some(applet) = args.get(1).map(String::as_str) else {
    return (None, &[]);
  };

  (Some(applet), &args[2..])
}

fn print_top_level_help(
  ctx: &AppContext,
  registry: &Registry,
  bin: &str,
) -> io::Result<()> {
  let mut output = format!(
    "{bin} - BusyBox-style lio example\n\nUsage:\n  {bin} <applet> [args...]\n  <applet> [args...]\n\nCommands:\n"
  );

  for command in registry.commands() {
    let display_name = format_command_display_name(command);
    output.push_str(&format!("  {:<12} {}\n", display_name, command.summary()));
  }

  output.push_str("\nOptions:\n  -h, --help    Show this help message\n");
  util::io::write_all(ctx.lio(), &ctx.stdout(), output.into_bytes())
}

fn print_command_help(
  ctx: &AppContext,
  command: &command::Registration,
) -> io::Result<()> {
  let output =
    format!("{}\n\nUsage:\n  {}\n", command.summary(), command.usage());
  util::io::write_all(ctx.lio(), &ctx.stdout(), output.into_bytes())
}

fn format_command_display_name(command: &command::Registration) -> String {
  if command.aliases().is_empty() {
    command.name().to_string()
  } else {
    format!("{}, {}", command.name(), command.aliases().join(", "))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::applets::CatCommand;
  use std::fs;

  #[test]
  fn resolves_argv0_registered_command() {
    let registry = Registry::new();
    let args = vec!["cat".to_string(), "file.txt".to_string()];
    let (applet, rest) = resolve_applet(&registry, "cat", &args);
    assert_eq!(applet, Some("cat"));
    assert_eq!(rest, &args[1..]);
  }

  #[test]
  fn resolves_busybox_style_command() {
    let registry = Registry::new();
    let args =
      vec!["busybox".to_string(), "cat".to_string(), "file.txt".to_string()];
    let (applet, rest) = resolve_applet(&registry, "busybox", &args);
    assert_eq!(applet, Some("cat"));
    assert_eq!(rest, &args[2..]);
  }

  #[test]
  fn resolves_unknown_command_from_busybox_invocation() {
    let registry = Registry::new();
    let args = vec!["busybox".to_string(), "missing".to_string()];
    let (applet, rest) = resolve_applet(&registry, "busybox", &args);
    assert_eq!(applet, Some("missing"));
    assert_eq!(rest, &args[2..]);
  }

  #[test]
  fn sdk_exposes_typed_cat_method() {
    let sdk = Busybox::new().unwrap();
    let path = std::env::temp_dir()
      .join(format!("busybox-sdk-cat-{}.txt", std::process::id()));
    fs::write(&path, b"hello\n").unwrap();

    let result = sdk.call(CatCommand {
      files: vec![path.display().to_string()],
      ..CatCommand::default()
    });

    fs::remove_file(&path).unwrap();
    assert!(result.is_ok());
  }

  #[test]
  fn bracket_invocation_requires_closing_delimiter() {
    let error =
      validate_registered_invocation("[", &["-n".into(), "value".into()])
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn top_level_help_formats_aliases_next_to_command_name() {
    let registry = Registry::new();
    let command = registry.find("test").unwrap();
    assert_eq!(format_command_display_name(command), "test, [");
  }
}
