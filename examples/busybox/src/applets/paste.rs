use std::{ffi::CString, io};

use lio::api;

use crate::{
  app::AppContext,
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

const OUTPUT_FLUSH_THRESHOLD: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct PasteCommand {
  pub serial: bool,
  pub delimiter: String,
  pub files: Vec<String>,
}

impl Command for PasteCommand {
  fn name() -> &'static str {
    "paste"
  }

  fn summary() -> &'static str {
    "Merge lines of files."
  }

  fn usage() -> &'static str {
    "paste [-s] [-d <delims>] <file...>"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec { name: "serial", short: &['s'], long: &[], takes_value: false },
      FlagSpec {
        name: "delimiter",
        short: &['d'],
        long: &[],
        takes_value: true,
      },
    ];
    let parsed =
      FlagParser::new("paste", SPECS).parse(args).map_err(|err| {
        if err.kind() == io::ErrorKind::InvalidInput
          && err.to_string().contains("missing value for '-d'")
        {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "paste: missing delimiter",
          )
        } else {
          err
        }
      })?;
    let files = parsed.positional().to_vec();
    if files.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "paste: missing file operand",
      ));
    }
    Ok(Self {
      serial: parsed.get_flag_exists("serial"),
      delimiter: parsed.get_flag_value("delimiter").unwrap_or("\t").to_string(),
      files,
    })
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let stdout = ctx.stdout();
    let mut out = Vec::new();
    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    let mut readers = Vec::with_capacity(self.files.len());
    for file in io_util::run_all(ctx.lio(), open_receivers) {
      readers.push(LineReader::new(file?));
    }

    if self.serial {
      for reader in &mut readers {
        let mut line = String::new();
        let mut idx = 0usize;
        while let Some(part) = reader.next_line(ctx)? {
          if idx > 0 {
            line.push(delim_at(&self.delimiter, idx - 1));
          }
          line.push_str(&part);
          idx += 1;
        }
        line.push('\n');
        out.extend_from_slice(line.as_bytes());
        if out.len() >= OUTPUT_FLUSH_THRESHOLD {
          io_util::write_all(ctx.lio(), &stdout, std::mem::take(&mut out))?;
        }
      }
    } else {
      loop {
        let mut line = String::new();
        let mut any = false;
        for (col, reader) in readers.iter_mut().enumerate() {
          if col > 0 {
            line.push(delim_at(&self.delimiter, col - 1));
          }
          if let Some(part) = reader.next_line(ctx)? {
            line.push_str(&part);
            any = true;
          }
        }
        if !any {
          break;
        }
        line.push('\n');
        out.extend_from_slice(line.as_bytes());
        if out.len() >= OUTPUT_FLUSH_THRESHOLD {
          io_util::write_all(ctx.lio(), &stdout, std::mem::take(&mut out))?;
        }
      }
    }

    if !out.is_empty() {
      io_util::write_all(ctx.lio(), &stdout, out)?;
    }

    Ok(())
  }
}

fn delim_at(delims: &str, index: usize) -> char {
  delims.chars().nth(index % delims.chars().count().max(1)).unwrap_or('\t')
}

struct LineReader {
  fd: lio::api::resource::Resource,
  buf: Vec<u8>,
  pending: Vec<u8>,
  eof: bool,
}

impl LineReader {
  fn new(fd: lio::api::resource::Resource) -> Self {
    Self { fd, buf: vec![0u8; 8192], pending: Vec::new(), eof: false }
  }

  fn next_line(&mut self, ctx: &AppContext) -> io::Result<Option<String>> {
    loop {
      if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
        let line = decode_line(&self.pending[..pos])?;
        self.pending.drain(..=pos);
        return Ok(Some(line));
      }

      if self.eof {
        if self.pending.is_empty() {
          return Ok(None);
        }
        let line = decode_line(&self.pending)?;
        self.pending.clear();
        return Ok(Some(line));
      }

      let rx = api::read(&self.fd, std::mem::take(&mut self.buf))
        .with_lio(ctx.lio())
        .send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      self.buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        self.eof = true;
      } else {
        self.pending.extend_from_slice(&self.buf[..n]);
      }
    }
  }
}

fn decode_line(bytes: &[u8]) -> io::Result<String> {
  String::from_utf8(bytes.to_vec())
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}
