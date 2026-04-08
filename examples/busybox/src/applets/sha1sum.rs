use std::io;

use crate::{
  app::AppContext,
  applets::support::{digest_command, hex_digest, sha1_digest},
  command::Command,
};

#[derive(Debug, Clone, Default)]
pub struct Sha1sumCommand {
  pub files: Vec<String>,
}

impl Command for Sha1sumCommand {
  fn name() -> &'static str {
    "sha1sum"
  }
  fn summary() -> &'static str {
    "Compute SHA-1 digests."
  }
  fn usage() -> &'static str {
    "sha1sum [file...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    digest_command(ctx.lio(), &self.files, |data| {
      hex_digest(&sha1_digest(data))
    })
  }
}
