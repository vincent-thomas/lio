use std::{ffi::CString, io};

use lio::api;

use crate::{
  app::AppContext,
  applets::support::{
    DdSpec, DdStats, dd_copy_file_to_file, dd_copy_sequential, parse_dd_count,
    parse_dd_size,
  },
  command::Command,
  util::io as io_util,
};

#[derive(Debug, Clone, Default)]
pub struct DdCommand {
  pub spec: DdSpec,
}

impl Command for DdCommand {
  fn name() -> &'static str {
    "dd"
  }
  fn summary() -> &'static str {
    "Copy and convert data with configurable block size."
  }
  fn usage() -> &'static str {
    "dd [if=<file>] [of=<file>] [bs=<n>] [count=<n>] [skip=<n>] [seek=<n>] [iodepth=<n>]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    let mut spec = DdSpec::default();
    for arg in args {
      if let Some(value) = arg.strip_prefix("if=") {
        spec.input = Some(value.to_string());
      } else if let Some(value) = arg.strip_prefix("of=") {
        spec.output = Some(value.to_string());
      } else if let Some(value) = arg.strip_prefix("bs=") {
        spec.block_size = parse_dd_size(value)?;
      } else if let Some(value) = arg.strip_prefix("count=") {
        spec.count = Some(parse_dd_count(value, "count")?);
      } else if let Some(value) = arg.strip_prefix("skip=") {
        spec.skip = parse_dd_count(value, "skip")?;
      } else if let Some(value) = arg.strip_prefix("seek=") {
        spec.seek = parse_dd_count(value, "seek")?;
      } else if let Some(value) = arg.strip_prefix("iodepth=") {
        spec.iodepth = parse_dd_count(value, "iodepth")?;
      } else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("dd: unsupported operand '{arg}'"),
        ));
      }
    }
    if spec.block_size == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "dd: bs must be greater than zero",
      ));
    }
    if spec.iodepth == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "dd: iodepth must be greater than zero",
      ));
    }
    Ok(Self { spec })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let input = match &self.spec.input {
      Some(path) => {
        let cpath = CString::new(path.as_str())?;
        io_util::run(
          ctx.lio(),
          api::openat(&ctx.cwd(), cpath, libc::O_RDONLY)
            .with_lio(ctx.lio())
            .send(),
        )?
      }
      None => ctx.stdin(),
    };
    let output = match &self.spec.output {
      Some(path) => {
        let cpath = CString::new(path.as_str())?;
        io_util::run(
          ctx.lio(),
          api::openat(
            &ctx.cwd(),
            cpath,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
          )
          .with_lio(ctx.lio())
          .send(),
        )?
      }
      None => ctx.stdout(),
    };
    if self.spec.seek > 0 && self.spec.output.is_none() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "dd: seek requires of=<file>",
      ));
    }
    let mut stats = DdStats::default();
    if self.spec.input.is_some() && self.spec.output.is_some() {
      dd_copy_file_to_file(ctx.lio(), &input, &output, &self.spec, &mut stats)?;
    } else {
      dd_copy_sequential(ctx.lio(), &input, &output, &self.spec, &mut stats)?;
    }
    io_util::write_all(
      ctx.lio(),
      &ctx.stderr(),
      format!(
        "{}+{} records in\n{}+{} records out\n{} bytes copied\n",
        stats.full_in,
        stats.partial_in,
        stats.full_out,
        stats.partial_out,
        stats.bytes_copied
      )
      .into_bytes(),
    )
  }
}
