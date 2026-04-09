use std::io;

use crate::{
  app::AppContext,
  applets::hashsum::digest::{Sha256State, hex_digest, stream_digest_command},
  command::Command,
};

#[cfg(test)]
use crate::applets::hashsum::digest::sha256_digest;

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
    stream_digest_command(
      ctx.lio(),
      &self.files,
      Sha256State::new,
      |state, chunk| state.update(chunk),
      |state| hex_digest(&state.finalize()),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sha256_streaming_matches_one_shot() {
    let mut state = Sha256State::new();
    state.update(b"he");
    state.update(b"llo");
    assert_eq!(state.finalize(), sha256_digest(b"hello"));
  }
}
