use std::io;

use crate::{
  app::AppContext, applets::support::cksum_crc32, command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct CksumCommand {
  pub files: Vec<String>,
}

impl Command for CksumCommand {
  fn name() -> &'static str {
    "cksum"
  }
  fn summary() -> &'static str {
    "Compute CRC checksum and byte count."
  }
  fn usage() -> &'static str {
    "cksum [file...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    Ok(Self { files: args.to_vec() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    if self.files.is_empty() {
      let data = io_util::read_to_bytes(ctx.lio(), None)?;
      let crc = cksum_crc32(&data);
      return io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        format!("{crc} {}\n", data.len()).into_bytes(),
      );
    }
    for path in &self.files {
      let data = io_util::read_to_bytes(ctx.lio(), Some(path.as_str()))?;
      let crc = cksum_crc32(&data);
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        format!("{crc} {} {path}\n", data.len()).into_bytes(),
      )?;
    }
    Ok(())
  }
}
