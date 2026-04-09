use std::io;

#[cfg(unix)]
use std::os::fd::AsRawFd;

use super::*;
use crate::{
  app::AppContext,
  util::{cwd as cwd_util, io as io_util},
};

impl SearchRuntime {
  pub(super) fn from_context(ctx: &AppContext) -> io::Result<Self> {
    let stdin_is_tty = stdin_is_tty(ctx);
    Ok(Self {
      cwd: cwd_util::current_working_directory(ctx)?,
      stdin: if stdin_is_tty {
        None
      } else {
        Some(io_util::read_to_bytes_fd(ctx.lio(), &ctx.stdin())?)
      },
      stdin_is_tty,
      stdout_is_tty: stdout_is_tty(ctx),
    })
  }
}

pub(super) fn stdin_is_tty(ctx: &AppContext) -> bool {
  #[cfg(unix)]
  {
    is_tty(ctx.stdin().as_raw_fd())
  }

  #[cfg(not(unix))]
  {
    let _ = ctx;
    false
  }
}

pub(super) fn stdout_is_tty(ctx: &AppContext) -> bool {
  #[cfg(unix)]
  {
    is_tty(ctx.stdout().as_raw_fd())
  }

  #[cfg(not(unix))]
  {
    let _ = ctx;
    false
  }
}

#[cfg(unix)]
fn is_tty(fd: std::os::fd::RawFd) -> bool {
  unsafe { libc::isatty(fd) == 1 }
}
