use std::io;

use crate::{
  app::AppContext,
  applets::support::{digest_command, hex_digest, sha3_256_digest},
  command::Command,
};

#[derive(Debug, Clone, Default)]
pub struct Sha3sumCommand {
  pub files: Vec<String>,
}

impl Command for Sha3sumCommand {
  fn name() -> &'static str {
    "sha3sum"
  }
  fn summary() -> &'static str {
    "Compute SHA3-256 digests."
  }
  fn usage() -> &'static str {
    "sha3sum [file...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    digest_command(ctx.lio(), &self.files, |data| {
      hex_digest(&sha3_256_digest(data))
    })
  }
}
