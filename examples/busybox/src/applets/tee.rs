use std::{ffi::CString, io};

use lio::{api, api::resource::Resource};

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct TeeCommand {
  pub append: bool,
  pub ignore_sigint: bool,
  pub files: Vec<String>,
}

impl Command for TeeCommand {
  fn name() -> &'static str {
    "tee"
  }
  fn summary() -> &'static str {
    "Read from stdin and write to stdout and files."
  }
  fn usage() -> &'static str {
    "tee [-a] [-i] [file...]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    let mut append = false;
    let mut ignore_sigint = false;
    let mut files = Vec::new();
    for arg in args {
      match arg.as_str() {
        "-a" => append = true,
        "-i" => ignore_sigint = true,
        _ => files.push(arg.clone()),
      }
    }
    Ok(Self { append, ignore_sigint, files })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    if self.ignore_sigint {
      #[cfg(unix)]
      unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
      }
    }
    let mut outputs: Vec<Resource> = Vec::new();
    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      let flags = libc::O_WRONLY
        | libc::O_CREAT
        | if self.append { libc::O_APPEND } else { libc::O_TRUNC };
      open_receivers
        .push(api::openat(&ctx.cwd(), cpath, flags).with_lio(ctx.lio()).send());
    }
    for result in io_util::run_all(ctx.lio(), open_receivers) {
      outputs.push(result?);
    }
    let stdin = ctx.stdin();
    let stdout = ctx.stdout();
    let mut buf = vec![0u8; 8192];
    loop {
      let rx = api::read(&stdin, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        break;
      }
      let data = &buf[..n];
      let mut writes = Vec::with_capacity(outputs.len() + 1);
      writes.push((stdout.clone(), data.to_vec()));
      for file in &outputs {
        writes.push((file.clone(), data.to_vec()));
      }
      io_util::write_all_many(ctx.lio(), writes)?;
    }
    Ok(())
  }
}
