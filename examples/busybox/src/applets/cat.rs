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

#[derive(Debug, Clone, Default)]
pub struct CatCommand {
  pub number_lines: bool,
  pub squeeze_blank: bool,
  pub show_all: bool,
  pub show_ends: bool,
  pub show_tabs: bool,
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
    "cat [-u] [-n] [-s] [-A|--show-all] [-E] [-T] [file...]"
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
        api::openat(&ctx.cwd(), cpath, libc::O_RDONLY, 0)
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
  const FLAGS: &[FlagSpec<'static>] = &[
    FlagSpec {
      name: "ignored_u",
      short: &['u'],
      long: &[],
      takes_value: false,
    },
    FlagSpec {
      name: "number_lines",
      short: &['n'],
      long: &[],
      takes_value: false,
    },
    FlagSpec {
      name: "squeeze_blank",
      short: &['s'],
      long: &[],
      takes_value: false,
    },
    FlagSpec {
      name: "show_all",
      short: &['A'],
      long: &["show-all"],
      takes_value: false,
    },
    FlagSpec {
      name: "show_ends",
      short: &['E'],
      long: &[],
      takes_value: false,
    },
    FlagSpec {
      name: "show_tabs",
      short: &['T'],
      long: &[],
      takes_value: false,
    },
  ];

  let parsed = FlagParser::new("cat", FLAGS).parse(args).map_err(|err| {
    if err.kind() == io::ErrorKind::InvalidInput {
      io::Error::new(
        err.kind(),
        err.to_string().replace("unrecognized option", "unsupported flag"),
      )
    } else {
      err
    }
  })?;
  Ok(CatCommand {
    number_lines: parsed.get_flag_exists("number_lines"),
    squeeze_blank: parsed.get_flag_exists("squeeze_blank"),
    show_all: parsed.get_flag_exists("show_all"),
    show_ends: parsed.get_flag_exists("show_ends"),
    show_tabs: parsed.get_flag_exists("show_tabs"),
    files: parsed.positional().to_vec(),
  })
}

fn cat_fd(
  ctx: &AppContext,
  fd: &lio::api::resource::Resource,
  args: &CatCommand,
) -> io::Result<()> {
  if !args.number_lines
    && !args.squeeze_blank
    && !args.show_all
    && !args.show_ends
    && !args.show_tabs
  {
    let mut buf = vec![0u8; 8192];
    loop {
      let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;

      let n = result? as usize;
      if n == 0 {
        break;
      }

      buf.truncate(n);
      buf = io_util::write_all_reusing_buffer(ctx.lio(), &ctx.stdout(), buf)?;
      buf.resize(8192, 0);
    }
    return Ok(());
  }

  let stdout = ctx.stdout();
  let mut buf = vec![0u8; 8192];
  let mut pending = Vec::new();
  let mut line_no = 1usize;
  let mut previous_blank = false;

  loop {
    let rx = api::read(fd, buf).with_lio(ctx.lio()).send();
    let (result, returned_buf) = io_util::run(ctx.lio(), rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      if !pending.is_empty() {
        flush_cat_line(
          ctx,
          &stdout,
          &mut pending,
          args,
          &mut line_no,
          &mut previous_blank,
        )?;
      }
      break;
    }

    for &byte in &buf[..n] {
      pending.push(byte);
      if byte == b'\n' {
        flush_cat_line(
          ctx,
          &stdout,
          &mut pending,
          args,
          &mut line_no,
          &mut previous_blank,
        )?;
      }
    }
  }
  Ok(())
}

fn flush_cat_line(
  ctx: &AppContext,
  stdout: &lio::api::resource::Resource,
  line: &mut Vec<u8>,
  args: &CatCommand,
  line_no: &mut usize,
  previous_blank: &mut bool,
) -> io::Result<()> {
  let is_blank =
    line.last() == Some(&b'\n') && line.len() == 1 || line.is_empty();
  if args.squeeze_blank && is_blank && *previous_blank {
    line.clear();
    return Ok(());
  }
  *previous_blank = is_blank;

  let mut rendered = render_cat_bytes(line, args);
  if args.number_lines {
    let mut numbered = format!("{:>6}\t", *line_no).into_bytes();
    numbered.append(&mut rendered);
    rendered = numbered;
    *line_no += 1;
  }
  line.clear();
  io_util::write_all(ctx.lio(), stdout, rendered)
}

#[cfg(test)]
fn render_cat_line(line: &str, args: &CatCommand) -> String {
  String::from_utf8(render_cat_bytes(line.as_bytes(), args))
    .expect("cat renders valid utf-8")
}

fn render_cat_bytes(line: &[u8], args: &CatCommand) -> Vec<u8> {
  let mut out = Vec::with_capacity(line.len() + 8);
  for &byte in line {
    match byte {
      b'\n' if args.show_all || args.show_ends => out.extend_from_slice(b"$\n"),
      b'\n' => out.push(b'\n'),
      b'\t' if args.show_all || args.show_tabs => out.extend_from_slice(b"^I"),
      0..=31 | 127 if args.show_all => {
        out.push(b'^');
        out.push(if byte == 127 { b'?' } else { byte + 64 });
      }
      _ => out.push(byte),
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
  fn parse_cat_command_supports_combined_short_aliases() {
    let parsed =
      parse_cat_command(&["-nET".into(), "file.txt".into()]).unwrap();
    assert!(parsed.number_lines);
    assert!(parsed.show_ends);
    assert!(parsed.show_tabs);
    assert_eq!(parsed.files, vec!["file.txt"]);
  }

  #[test]
  fn parse_cat_command_supports_show_ends_and_tabs() {
    let parsed =
      parse_cat_command(&["-E".into(), "-T".into(), "file.txt".into()])
        .unwrap();
    assert!(parsed.show_ends);
    assert!(parsed.show_tabs);
    assert_eq!(parsed.files, vec!["file.txt"]);
  }

  #[test]
  fn parse_cat_command_rejects_unknown_flags() {
    let error = parse_cat_command(&["-z".to_string()]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("unsupported flag"));
  }

  #[test]
  fn render_show_all_marks_tabs_and_newlines() {
    assert_eq!(
      render_cat_line(
        "a\tb\n",
        &CatCommand { show_all: true, ..Default::default() }
      ),
      "a^Ib$\n"
    );
  }

  #[test]
  fn render_show_ends_and_tabs_work_independently() {
    assert_eq!(
      render_cat_line(
        "a\tb\n",
        &CatCommand { show_ends: true, ..Default::default() }
      ),
      "a\tb$\n"
    );
    assert_eq!(
      render_cat_line(
        "a\tb\n",
        &CatCommand { show_tabs: true, ..Default::default() }
      ),
      "a^Ib\n"
    );
  }

  #[test]
  fn render_show_all_marks_del_as_caret_question() {
    assert_eq!(
      render_cat_bytes(
        &[127],
        &CatCommand { show_all: true, ..Default::default() }
      ),
      b"^?"
    );
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

      let mut line_rendered = render_cat_line(line, command);
      if command.number_lines {
        line_rendered = format!("{:>6}\t{}", line_no, line_rendered);
        line_no += 1;
      }
      rendered.push_str(&line_rendered);
    }

    Ok(rendered)
  }
}
