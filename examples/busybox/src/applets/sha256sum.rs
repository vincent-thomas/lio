use std::io;

use crate::{
  app::AppContext,
  applets::support::{digest_command, hex_digest, sha256_digest},
  command::Command,
};

#[derive(Debug, Clone, Default)]
pub struct Sha256sumCommand {
  pub files: Vec<String>,
}

impl Command for Sha256sumCommand {
  fn name() -> &'static str {
    "sha256sum"
  }
  fn summary() -> &'static str {
    "Compute SHA-256 digests."
  }
  fn usage() -> &'static str {
    "sha256sum [file...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    digest_command(ctx.lio(), &self.files, |data| {
      hex_digest(&sha256_digest(data))
    })
  }
}
