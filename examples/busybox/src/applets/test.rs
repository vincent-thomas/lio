use std::{io, path::Path};

use crate::{
  app::AppContext, command::Command, exit_with_status, util::fs as fs_util,
};

#[derive(Debug, Clone, Default)]
pub struct TestCommand {
  pub negate: bool,
  pub operands: Vec<String>,
}

impl Command for TestCommand {
  fn name() -> &'static str {
    "test"
  }

  fn aliases() -> &'static [&'static str] {
    &["["]
  }

  fn summary() -> &'static str {
    "Evaluate conditional expressions."
  }

  fn usage() -> &'static str {
    "test <expression>\n  [ <expression> ]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    let mut operands = args.to_vec();
    if operands.last().is_some_and(|arg| arg == "]") {
      operands.pop();
    }

    let mut negate = false;
    while operands.first().is_some_and(|arg| arg == "!") {
      negate = !negate;
      operands.remove(0);
    }

    Ok(Self { negate, operands })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let result = evaluate_expression(ctx, &self.operands)?;
    if self.negate ^ result { Ok(()) } else { Err(exit_with_status(1)) }
  }
}

fn evaluate_expression(
  ctx: &AppContext,
  operands: &[String],
) -> io::Result<bool> {
  match operands {
    [] => Ok(false),
    [value] => Ok(!value.is_empty()),
    [operator, value] => evaluate_unary(ctx, operator, value),
    [left, operator, right] => evaluate_binary(left, operator, right),
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "test: unsupported expression",
    )),
  }
}

fn evaluate_unary(
  ctx: &AppContext,
  operator: &str,
  value: &str,
) -> io::Result<bool> {
  match operator {
    "-n" => Ok(!value.is_empty()),
    "-z" => Ok(value.is_empty()),
    "-e" => Ok(stat_path(ctx, value, true)?.is_some()),
    "-f" => Ok(stat_path(ctx, value, true)?.is_some_and(|meta| meta.is_file())),
    "-d" => Ok(stat_path(ctx, value, true)?.is_some_and(|meta| meta.is_dir())),
    "-L" | "-h" => {
      Ok(stat_path(ctx, value, false)?.is_some_and(|meta| meta.is_symlink()))
    }
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("test: unsupported unary operator {operator}"),
    )),
  }
}

fn evaluate_binary(
  left: &str,
  operator: &str,
  right: &str,
) -> io::Result<bool> {
  match operator {
    "=" | "==" => Ok(left == right),
    "!=" => Ok(left != right),
    "-eq" => Ok(parse_integer(left)? == parse_integer(right)?),
    "-ne" => Ok(parse_integer(left)? != parse_integer(right)?),
    "-gt" => Ok(parse_integer(left)? > parse_integer(right)?),
    "-ge" => Ok(parse_integer(left)? >= parse_integer(right)?),
    "-lt" => Ok(parse_integer(left)? < parse_integer(right)?),
    "-le" => Ok(parse_integer(left)? <= parse_integer(right)?),
    _ => Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("test: unsupported binary operator {operator}"),
    )),
  }
}

fn parse_integer(value: &str) -> io::Result<i64> {
  value.parse::<i64>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("test: invalid integer '{value}'"),
    )
  })
}

fn stat_path(
  ctx: &AppContext,
  value: &str,
  follow_symlinks: bool,
) -> io::Result<Option<lio::api::FileStat>> {
  fs_util::stat_path(ctx, Path::new(value), follow_symlinks)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, path::PathBuf};

  #[test]
  fn parse_test_command_supports_bracket_suffix() {
    let parsed =
      TestCommand::parse(&["-n".into(), "hello".into(), "]".into()]).unwrap();
    assert_eq!(parsed.operands, vec!["-n", "hello"]);
  }

  #[test]
  fn parse_test_command_collects_leading_negation() {
    let parsed = TestCommand::parse(&["!".into(), "foo".into()]).unwrap();
    assert!(parsed.negate);
    assert_eq!(parsed.operands, vec!["foo"]);
  }

  #[test]
  fn test_evaluates_string_and_numeric_expressions() {
    let ctx = AppContext::new().unwrap();
    assert!(
      evaluate_expression(&ctx, &["a".into(), "=".into(), "a".into()]).unwrap()
    );
    assert!(
      evaluate_expression(&ctx, &["7".into(), "-gt".into(), "3".into()])
        .unwrap()
    );
    assert!(!evaluate_expression(&ctx, &["".into()]).unwrap());
  }

  #[test]
  fn test_evaluates_file_predicates() {
    let ctx = AppContext::new().unwrap();
    let path = std::env::temp_dir()
      .join(format!("busybox-test-command-{}.txt", std::process::id()));
    fs::write(&path, b"hello").unwrap();

    assert!(
      evaluate_expression(&ctx, &["-e".into(), path.display().to_string(),])
        .unwrap()
    );
    assert!(
      evaluate_expression(&ctx, &["-f".into(), path.display().to_string(),])
        .unwrap()
    );
    assert!(
      !evaluate_expression(&ctx, &["-d".into(), path.display().to_string(),])
        .unwrap()
    );

    fs::remove_file(path).unwrap();
  }

  #[test]
  fn test_file_predicates_follow_symlinks() {
    let ctx = AppContext::new().unwrap();
    let file = unique_temp_path("test-symlink-file");
    let file_link = unique_temp_path("test-symlink-file-link");
    let dir = unique_temp_path("test-symlink-dir");
    let dir_link = unique_temp_path("test-symlink-dir-link");
    fs::write(&file, b"hello").unwrap();
    fs::create_dir(&dir).unwrap();
    std::os::unix::fs::symlink(&file, &file_link).unwrap();
    std::os::unix::fs::symlink(&dir, &dir_link).unwrap();

    assert!(
      evaluate_expression(
        &ctx,
        &["-e".into(), file_link.display().to_string()]
      )
      .unwrap()
    );
    assert!(
      evaluate_expression(
        &ctx,
        &["-f".into(), file_link.display().to_string()]
      )
      .unwrap()
    );
    assert!(
      !evaluate_expression(
        &ctx,
        &["-d".into(), file_link.display().to_string()]
      )
      .unwrap()
    );

    assert!(
      evaluate_expression(&ctx, &["-e".into(), dir_link.display().to_string()])
        .unwrap()
    );
    assert!(
      evaluate_expression(&ctx, &["-d".into(), dir_link.display().to_string()])
        .unwrap()
    );
    assert!(
      !evaluate_expression(
        &ctx,
        &["-f".into(), dir_link.display().to_string()]
      )
      .unwrap()
    );

    fs::remove_file(file_link).unwrap();
    fs::remove_file(dir_link).unwrap();
    fs::remove_file(file).unwrap();
    fs::remove_dir(dir).unwrap();
  }

  #[test]
  fn test_file_predicates_treat_broken_symlink_as_missing() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("test-broken-target");
    let link = unique_temp_path("test-broken-link");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    fs::remove_file(&target).unwrap();

    assert!(
      !evaluate_expression(&ctx, &["-e".into(), link.display().to_string()])
        .unwrap()
    );
    assert!(
      !evaluate_expression(&ctx, &["-f".into(), link.display().to_string()])
        .unwrap()
    );
    assert!(
      !evaluate_expression(&ctx, &["-d".into(), link.display().to_string()])
        .unwrap()
    );

    fs::remove_file(link).unwrap();
  }

  #[test]
  fn test_symlink_predicates_detect_symlinks_without_following() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("test-link-target");
    let link = unique_temp_path("test-link");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    assert!(
      evaluate_expression(&ctx, &["-L".into(), link.display().to_string()])
        .unwrap()
    );
    assert!(
      evaluate_expression(&ctx, &["-h".into(), link.display().to_string()])
        .unwrap()
    );
    assert!(
      !evaluate_expression(&ctx, &["-L".into(), target.display().to_string()])
        .unwrap()
    );

    fs::remove_file(link).unwrap();
    fs::remove_file(target).unwrap();
  }

  #[test]
  fn test_symlink_predicates_match_broken_symlink() {
    let ctx = AppContext::new().unwrap();
    let target = unique_temp_path("test-broken-link-target");
    let link = unique_temp_path("test-broken-link-predicate");
    fs::write(&target, b"hello").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    fs::remove_file(&target).unwrap();

    assert!(
      evaluate_expression(&ctx, &["-L".into(), link.display().to_string()])
        .unwrap()
    );
    assert!(
      evaluate_expression(&ctx, &["-h".into(), link.display().to_string()])
        .unwrap()
    );

    fs::remove_file(link).unwrap();
  }

  fn unique_temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
      "busybox-{}-{}-{}",
      name,
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ))
  }
}
