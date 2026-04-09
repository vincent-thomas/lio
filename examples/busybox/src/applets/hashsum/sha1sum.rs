use std::io;

use crate::{
  app::AppContext,
  applets::hashsum::digest::{Sha1State, hex_digest, stream_digest_command},
  command::Command,
};

#[cfg(test)]
use crate::applets::hashsum::digest::sha1_digest;

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
    stream_digest_command(
      ctx.lio(),
      &self.files,
      Sha1State::new,
      |state, chunk| state.update(chunk),
      |state| hex_digest(&state.finalize()),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn sha1_streaming_matches_one_shot() {
    let mut state = Sha1State::new();
    state.update(b"he");
    state.update(b"llo");
    assert_eq!(state.finalize(), sha1_digest(b"hello"));
  }
}
