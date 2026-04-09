use std::io;

use crate::{
  app::AppContext,
  applets::hashsum::digest::{
    Sha3_256State, hex_digest, stream_digest_command,
  },
  command::Command,
};

#[cfg(test)]
use crate::applets::hashsum::digest::sha3_256_digest;

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
    stream_digest_command(
      ctx.lio(),
      &self.files,
      Sha3_256State::new,
      |state, chunk| state.update(chunk),
      |state| hex_digest(&state.finalize()),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sha3_streaming_matches_one_shot() {
    let mut state = Sha3_256State::new();
    state.update(b"he");
    state.update(b"llo");
    assert_eq!(state.finalize(), sha3_256_digest(b"hello"));
  }
}
