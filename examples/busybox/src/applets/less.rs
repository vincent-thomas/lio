use std::io;

use crate::{app::AppContext, command::Command, util::io as io_util};

use super::more::{PagerKind, page_text};

#[derive(Debug, Clone, Default)]
pub struct LessCommand {
  pub files: Vec<String>,
}

impl Command for LessCommand {
  fn name() -> &'static str {
    "less"
  }

  fn summary() -> &'static str {
    "View text with forward and backward paging."
  }

  fn usage() -> &'static str {
    "less [file...]  (space/enter forward, b/k backward, g/G start/end, q quit)"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let content = if self.files.is_empty() {
      io_util::read_to_string_fd(ctx.lio(), &ctx.stdin())?
    } else {
      self.files.iter().try_fold(String::new(), |mut acc, path| {
        acc.push_str(&io_util::read_to_string(ctx.lio(), Some(path))?);
        Ok::<_, io::Error>(acc)
      })?
    };
    let label = self.files.first().map(String::as_str);
    page_text(ctx, &content, PagerKind::Less, label)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_less_accepts_optional_files() {
    let parsed = LessCommand::parse(&["a".into(), "b".into()]).unwrap();
    assert_eq!(parsed.files, vec!["a", "b"]);
  }
}
