use std::io;

use crate::{
  app::AppContext,
  applets::support::{digest_command, hex_digest, md5_digest},
  command::Command,
};

#[derive(Debug, Clone, Default)]
pub struct Md5sumCommand {
  pub files: Vec<String>,
}

impl Command for Md5sumCommand {
  fn name() -> &'static str {
    "md5sum"
  }
  fn summary() -> &'static str {
    "Compute MD5 digests."
  }
  fn usage() -> &'static str {
    "md5sum [file...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    digest_command(ctx.lio(), &self.files, |data| hex_digest(&md5_digest(data)))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_md5sum_command_collects_files() {
    let parsed =
      Md5sumCommand::parse(&["a.txt".into(), "b.txt".into()]).unwrap();
    assert_eq!(parsed.files, vec!["a.txt", "b.txt"]);
  }

  #[test]
  fn md5sum_matches_known_vector() {
    assert_eq!(
      hex_digest(&md5_digest(b"hello")),
      "5d41402abc4b2a76b9719d911017c592"
    );
  }
}
