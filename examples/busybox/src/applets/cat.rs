use std::{ffi::CString, io};

use lio::api;

use crate::{app::AppContext, command::Command, util::io as io_util};

#[derive(Debug, Clone, Default)]
pub struct CatCommand {
  pub number_lines: bool,
  pub squeeze_blank: bool,
  pub show_all: bool,
  pub files: Vec<String>,
}

impl Command for CatCommand {
  fn name() -> &'static str {
    "cat"
  }

  fn aliases() -> &'static [&'static str] {
    &[]
  }

  fn summary() -> &'static str {
    "Concatenate files to stdout."
  }

  fn usage() -> &'static str {
    "cat [-u] [-n] [-s] [-A|--show-all] [file...]"
  }

  fn parse(args: &[String]) -> io::Result<Self> {
    parse_cat_command(args)
  }

  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    if self.files.is_empty() {
      return cat_fd(ctx, &ctx.stdin(), self);
    }

    let mut open_receivers = Vec::with_capacity(self.files.len());
    for path in &self.files {
      let cpath = CString::new(path.as_str())?;
      open_receivers.push(
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY)
          .with_lio(ctx.lio())
          .send(),
      );
    }

    for file in io_util::run_all(ctx.lio(), open_receivers) {
      let file = file?;
      cat_fd(ctx, &file, self)?;
    }

    Ok(())
  }
}

fn parse_cat_command(args: &[String]) -> io::Result<CatCommand> {
  let mut files = args;
  let mut parsed = CatCommand::default();

  while let Some(arg) = files.first() {
    match arg.as_str() {
      "-u" => files = &files[1..],
      "-n" => {
        parsed.number_lines = true;
        files = &files[1..];
      }
      "-s" => {
        parsed.squeeze_blank = true;
        files = &files[1..];
      }
      "-A" | "--show-all" => {
        parsed.show_all = true;
        files = &files[1..];
      }
      flag if flag.starts_with('-') && flag != "-" => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("cat: unsupported flag {flag}"),
        ));
      }
      _ => break,
    }
  }

  parsed.files = files.to_vec();
  Ok(parsed)
}

fn cat_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  args: &CatCommand,
) -> io::Result<()> {
  if !args.number_lines && !args.squeeze_blank && !args.show_all {
    let mut buf = vec![0u8; 8192];
    loop {
      let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;

      let n = result? as usize;
      if n == 0 {
        break;
      }

      io_util::write_all(ctx.lio(), &ctx.stdout(), buf[..n].to_vec())?;
    }
    return Ok(());
  }

  let input = io_util::read_to_string_fd(ctx.lio(), fd)?;
  let stdout = ctx.stdout();
  let mut line_no = 1usize;
  let mut previous_blank = false;

  for line in input.split_inclusive('\n') {
    let is_blank = line.trim_end_matches('\n').is_empty();
    if args.squeeze_blank && is_blank && previous_blank {
      continue;
    }
    previous_blank = is_blank;

    let mut rendered =
      if args.show_all { render_cat_show_all(line) } else { line.to_string() };
    if args.number_lines {
      rendered = format!("{:>6}\t{}", line_no, rendered);
      line_no += 1;
    }
    io_util::write_all(ctx.lio(), &stdout, rendered.into_bytes())?;
  }
  Ok(())
}

fn render_cat_show_all(line: &str) -> String {
  let mut out = String::new();
  for ch in line.chars() {
    match ch {
      '\n' => out.push_str("$\n"),
      '\t' => out.push_str("^I"),
      c if c.is_ascii_control() => {
        out.push('^');
        out.push(((c as u8) + 64) as char);
      }
      c => out.push(c),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::fd::FromRawFd;

  #[test]
  fn parse_cat_command_supports_show_all_and_numbering() {
    let args = vec!["-A".to_string(), "-n".to_string(), "file.txt".to_string()];
    let parsed = parse_cat_command(&args).unwrap();
    assert!(parsed.show_all);
    assert!(parsed.number_lines);
    assert_eq!(parsed.files, args[2..].to_vec());
  }

  #[test]
  fn parse_cat_command_rejects_unknown_flags() {
    let error = parse_cat_command(&["-z".to_string()]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("unsupported flag"));
  }

  #[test]
  fn render_show_all_marks_tabs_and_newlines() {
    assert_eq!(render_cat_show_all("a\tb\n"), "a^Ib$\n");
  }

  #[test]
  fn render_show_all_does_not_duplicate_last_line_without_newline() {
    let ctx = AppContext::new().unwrap();
    let path = CString::new("/tmp/busybox_cat_command_no_newline.txt").unwrap();
    unsafe {
      let fd = libc::open(
        path.as_ptr(),
        libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
        0o644,
      );
      assert!(fd >= 0);
      let payload = b"hello";
      let written = libc::write(fd, payload.as_ptr().cast(), payload.len());
      assert_eq!(written, payload.len() as isize);
      libc::close(fd);

      let file = lio::api::resource::Resource::from_raw_fd(libc::open(
        path.as_ptr(),
        libc::O_RDONLY,
      ));
      let command = CatCommand { show_all: true, ..CatCommand::default() };
      let output = read_rendered_cat_output(&ctx, &file, &command).unwrap();
      assert_eq!(output, "hello");
      libc::unlink(path.as_ptr());
    }
  }

  #[test]
  fn cat_command_reads_file() {
    let ctx = AppContext::new().unwrap();
    let path = CString::new("/tmp/busybox_cat_command_reads_file.txt").unwrap();
    unsafe {
      let fd = libc::open(
        path.as_ptr(),
        libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
        0o644,
      );
      assert!(fd >= 0);
      let payload = b"hello\n";
      let written = libc::write(fd, payload.as_ptr().cast(), payload.len());
      assert_eq!(written, payload.len() as isize);
      libc::close(fd);

      let file = lio::api::resource::Resource::from_raw_fd(libc::open(
        path.as_ptr(),
        libc::O_RDONLY,
      ));
      let text = io_util::read_to_string_fd(ctx.lio(), &file).unwrap();
      assert_eq!(text, "hello\n");
      libc::unlink(path.as_ptr());
    }
  }

  fn read_rendered_cat_output(
    ctx: &AppContext,
    file: &lio::api::resource::Resource,
    command: &CatCommand,
  ) -> io::Result<String> {
    let input = io_util::read_to_string_fd(ctx.lio(), file)?;
    let mut rendered = String::new();
    let mut line_no = 1usize;
    let mut previous_blank = false;

    for line in input.split_inclusive('\n') {
      let is_blank = line.trim_end_matches('\n').is_empty();
      if command.squeeze_blank && is_blank && previous_blank {
        continue;
      }
      previous_blank = is_blank;

      let mut line_rendered = if command.show_all {
        render_cat_show_all(line)
      } else {
        line.to_string()
      };
      if command.number_lines {
        line_rendered = format!("{:>6}\t{}", line_no, line_rendered);
        line_no += 1;
      }
      rendered.push_str(&line_rendered);
    }

    Ok(rendered)
  }
}
