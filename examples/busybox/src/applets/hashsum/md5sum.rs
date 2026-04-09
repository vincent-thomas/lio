use std::io;

use crate::{
  app::AppContext,
  applets::hashsum::digest::{Md5State, hex_digest, stream_digest_command},
  command::Command,
};

#[cfg(test)]
use crate::applets::hashsum::digest::md5_digest;

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
    stream_digest_command(
      ctx.lio(),
      &self.files,
      Md5State::new,
      |state, chunk| state.update(chunk),
      |state| hex_digest(&state.finalize()),
    )
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

  #[test]
  fn md5_streaming_matches_one_shot() {
    let mut state = Md5State::new();
    state.update(b"he");
    state.update(b"llo");
    assert_eq!(state.finalize(), md5_digest(b"hello"));
  }
}
