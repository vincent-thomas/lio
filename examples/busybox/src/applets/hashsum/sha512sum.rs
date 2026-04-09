use std::io;

use crate::{
  app::AppContext,
  applets::hashsum::digest::{Sha512State, hex_digest, stream_digest_command},
  command::Command,
};

#[cfg(test)]
use crate::applets::hashsum::digest::sha512_digest;

#[derive(Debug, Clone, Default)]
pub struct Sha512sumCommand {
  pub files: Vec<String>,
}

impl Command for Sha512sumCommand {
  fn name() -> &'static str {
    "sha512sum"
  }

  fn summary() -> &'static str {
    "Compute SHA-512 digests."
  }

  fn usage() -> &'static str {
    "sha512sum [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    stream_digest_command(
      ctx.lio(),
      &self.files,
      Sha512State::new,
      |state, chunk| state.update(chunk),
      |state| hex_digest(&state.finalize()),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sha512_streaming_matches_one_shot() {
    let mut state = Sha512State::new();
    state.update(b"he");
    state.update(b"llo");
    assert_eq!(state.finalize(), sha512_digest(b"hello"));
  }
}
