use std::io;

use crate::{
  app::AppContext,
  applets::support::{cksum_crc32_finalize, cksum_crc32_update},
  command::Command,
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
      let (crc, len) = cksum_fd(ctx, &lio::api::resource::Resource::stdin())?;
      return io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        format!("{crc} {len}\n").into_bytes(),
      );
    }
    for path in &self.files {
      let file = io_util::run(
        ctx.lio(),
        lio::api::openat(
          &ctx.cwd(),
          std::ffi::CString::new(path.as_str())?,
          libc::O_RDONLY,
          0,
        )
        .with_lio(ctx.lio())
        .send(),
      )?;
      let (crc, len) = cksum_fd(ctx, &file)?;
      io_util::write_all(
        ctx.lio(),
        &ctx.stdout(),
        format!("{crc} {len} {path}\n").into_bytes(),
      )?;
    }
    Ok(())
  }
}

fn cksum_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
) -> io::Result<(u32, usize)> {
  let mut crc = 0u32;
  let mut len = 0usize;
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = lio::api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    crc = cksum_crc32_update(crc, &buf[..n]);
    len += n;
  }

  Ok((cksum_crc32_finalize(crc, len as u64), len))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn streaming_cksum_matches_whole_buffer() {
    let chunks = [b"hel".as_slice(), b"lo world".as_slice()];
    let mut crc = 0u32;
    let mut len = 0usize;
    for chunk in chunks {
      crc = cksum_crc32_update(crc, chunk);
      len += chunk.len();
    }
    assert_eq!(
      cksum_crc32_finalize(crc, len as u64),
      crate::applets::support::cksum_crc32(b"hello world")
    );
  }
}
