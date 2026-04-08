use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct CpCommand {
  pub source: String,
  pub dest: String,
}

impl Command for CpCommand {
  fn name() -> &'static str {
    "cp"
  }
  fn summary() -> &'static str {
    "Copy files."
  }
  fn usage() -> &'static str {
    "cp <source> <dest>"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    if args.len() != 2 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cp: expected source and destination",
      ));
    }
    Ok(Self { source: args[0].clone(), dest: args[1].clone() })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let src_cpath = CString::new(self.source.as_str())?;
    let dst_cpath = CString::new(self.dest.as_str())?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
    let mut opened = io_util::run_all(
      ctx.lio(),
      vec![
        api::openat(&ctx.cwd(), src_cpath, libc::O_RDONLY)
          .with_lio(ctx.lio())
          .send(),
        api::openat(&ctx.cwd(), dst_cpath, flags).with_lio(ctx.lio()).send(),
      ],
    )
    .into_iter();
    let src = opened.next().expect("missing source open result")?;
    let dst = opened.next().expect("missing dest open result")?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
      let rx = api::read(&src, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        break;
      }
      io_util::write_all(ctx.lio(), &dst, buf[..n].to_vec())?;
    }
    Ok(())
  }
}
