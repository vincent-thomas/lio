use std::{ffi::CString, io};

use lio::{api, api::resource::Resource};

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

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
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec { name: "append", short: &['a'], long: &[], takes_value: false },
      FlagSpec {
        name: "ignore_sigint",
        short: &['i'],
        long: &[],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("tee", SPECS).parse(args)?;
    Ok(Self {
      append: parsed.get_flag_exists("append"),
      ignore_sigint: parsed.get_flag_exists("ignore_sigint"),
      files: parsed.positional().to_vec(),
    })
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
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, flags, 0o666).with_lio(ctx.lio()).send(),
      );
    }
    for result in io_util::run_all(ctx.lio(), open_receivers) {
      outputs.push(result?);
    }
    let stdin = ctx.stdin();
    let stdout = ctx.stdout();
    let mut sinks = Vec::with_capacity(outputs.len() + 1);
    sinks.push(stdout);
    sinks.extend(outputs);
    let mut buf = vec![0u8; 8192];
    loop {
      let rx = api::read(&stdin, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        break;
      }
      buf.truncate(n);

      if sinks.len() == 1 {
        buf = io_util::write_all_reusing_buffer(ctx.lio(), &sinks[0], buf)?;
        buf.resize(8192, 0);
        continue;
      }

      for sink in &sinks[..sinks.len() - 1] {
        io_util::write_all(ctx.lio(), sink, buf.clone())?;
      }
      buf = io_util::write_all_reusing_buffer(
        ctx.lio(),
        sinks.last().expect("missing tee sink"),
        buf,
      )?;
      buf.resize(8192, 0);
    }
    Ok(())
  }
}
