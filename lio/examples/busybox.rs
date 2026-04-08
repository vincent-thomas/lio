//! A BusyBox-style single binary example using lio.
//!
//! This example dispatches to common BusyBox-style applets based on either:
//! - the executable name (`argv[0]`), or
//! - the first positional argument when invoked as `busybox <applet> ...`
//!
//! Examples:
//! - `cargo run --example busybox -- cp src.txt dst.txt`
//! - `cargo run --example busybox -- cat dst.txt`
//! - `cargo run --example busybox -- head -n 5 dst.txt`
//! - `cargo run --example busybox -- tee -a out.txt`
//! - `cargo run --example busybox -- tail -n 20 dst.txt`
//! - `cargo run --example busybox -- wc dst.txt`
//! - `cargo run --example busybox -- touch created.txt`
//! - `cargo run --example busybox -- truncate -s 1024 dst.txt`
//! - `cargo run --example busybox -- mv old.txt new.txt`
//! - `cargo run --example busybox -- yes hello`
//! - `cargo run --example busybox -- sleep 0.5`
//! - `cargo run --example busybox -- clear`
//! - `cargo run --example busybox -- echo hello world`
//! - `cargo run --example busybox -- pwd`
//! - `cargo run --example busybox -- paste a.txt b.txt`
//! - `cargo run --example busybox -- uniq input.txt`
//! - `cargo run --example busybox -- cut -d : -f 1 passwd.txt`
//! - `cargo run --example busybox -- nl file.txt`
//! - `cargo run --example busybox -- tac file.txt`
//! - `ln -s target/debug/examples/busybox cp && ./cp src.txt dst.txt`

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::collections::VecDeque;
use std::env;
use std::ffi::CString;
use std::io;
use std::path::Path;
use std::time::Duration;

fn main() -> std::io::Result<()> {
  let args: Vec<String> = env::args().collect();
  let lio = Lio::new(64)?;
  if args.iter().any(|arg| arg == "--help" || arg == "-h") {
    print_help(&lio, args.first().map(String::as_str).unwrap_or("busybox"))?;
    return Ok(());
  }
  let argv0 = args
    .first()
    .and_then(|s| Path::new(s).file_name())
    .and_then(|s| s.to_str())
    .unwrap_or("busybox");

  let (applet, rest) = match argv0 {
    "cp" | "cat" | "head" => (argv0, &args[1..]),
    _ => {
      let Some(applet) = args.get(1).map(|s| s.as_str()) else {
        print_usage(args.first().map(String::as_str).unwrap_or("busybox"));
        std::process::exit(1);
      };
      (applet, &args[2..])
    }
  };

  match applet {
    "cp" => cp(&lio, rest),
    "cat" => cat(&lio, rest),
    "head" => head(&lio, rest),
    "tee" => tee(&lio, rest),
    "tail" => tail(&lio, rest),
    "wc" => wc(&lio, rest),
    "yes" => yes(&lio, rest),
    "sleep" => sleep_cmd(&lio, rest),
    "clear" => clear(&lio, rest),
    "echo" => echo(&lio, rest),
    "pwd" => pwd(&lio, rest),
    "paste" => paste(&lio, rest),
    "uniq" => uniq(&lio, rest),
    "cut" => cut(&lio, rest),
    "nl" => nl(&lio, rest),
    "tac" => tac(&lio, rest),
    _ => {
      eprintln!("unknown applet: {applet}");
      print_usage(args.first().map(String::as_str).unwrap_or("busybox"));
      std::process::exit(1);
    }
  }
}

fn print_usage(bin: &str) {
  eprintln!(
    "Usage: {bin} <cp|cat|head|tee|tail|wc|yes|sleep|clear|echo|pwd|paste|uniq|cut|nl|tac> ..."
  );
}

fn print_help(lio: &Lio, bin: &str) -> std::io::Result<()> {
  write_all(
    lio,
    &Resource::stdout(),
    format!(
      "\
{bin} - BusyBox-style lio example

Usage:
  {bin} <applet> [args...]
  <applet> [args...]

Supported applets:
  cp <source> <dest>
  cat [file...]
  head [-n N] [file...]
  tee [-a] [file...]
  tail [-f] [-n N] [file...]
  wc [file...]
  yes [text...]
  sleep <seconds>
  clear
  echo [text...]
  pwd
  paste <file...>
  uniq [file]
  cut -d <delim> -f <field> [file]
  nl [file]
  tac [file]

Options:
  -h, --help    Show this help message

Examples:
  {bin} cp src.txt dst.txt
  {bin} cat file.txt
  {bin} head -n 5 file.txt
  {bin} tee -a out.txt
  {bin} sleep 0.5
  {bin} paste a.txt b.txt
"
    )
    .into_bytes(),
  )
}

fn cp(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.len() != 2 {
    eprintln!("Usage: cp <source> <dest>");
    std::process::exit(1);
  }

  let src_path = &args[0];
  let dst_path = &args[1];
  let cwd = Resource::cwd();

  let src_cpath = CString::new(src_path.as_str())?;
  let dst_cpath = CString::new(dst_path.as_str())?;
  let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
  let mut opened = run_all(
    lio,
    vec![
      api::openat(&cwd, src_cpath, libc::O_RDONLY).with_lio(lio).send(),
      api::openat(&cwd, dst_cpath, flags).with_lio(lio).send(),
    ],
  )
  .into_iter();
  let src = opened.next().expect("missing source open result")?;
  let dst = opened.next().expect("missing dest open result")?;

  let mut buf = vec![0u8; 64 * 1024];

  loop {
    let rx = api::read(&src, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    write_all(lio, &dst, buf[..n].to_vec())?;
  }

  Ok(())
}

fn cat(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.is_empty() {
    return cat_fd(lio, &Resource::stdin());
  }

  let mut open_receivers = Vec::with_capacity(args.len());
  for path in args {
    let cpath = CString::new(path.as_str())?;
    open_receivers.push(
      api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send(),
    );
  }

  for file in run_all(lio, open_receivers) {
    let file = file?;
    cat_fd(lio, &file)?;
  }

  Ok(())
}

fn cat_fd(lio: &Lio, fd: &Resource) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    write_all(lio, &stdout, buf[..n].to_vec())?;
  }

  Ok(())
}

fn head(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  let (num_lines, files) = if args.first().map(|s| s.as_str()) == Some("-n") {
    let n = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    (n, &args[2..])
  } else {
    (10usize, args)
  };

  if files.is_empty() {
    return head_fd(lio, &Resource::stdin(), num_lines);
  }

  let mut open_receivers = Vec::with_capacity(files.len());
  for path in files {
    let cpath = CString::new(path.as_str())?;
    open_receivers.push(
      api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send(),
    );
  }

  for (i, (path, file)) in files.iter().zip(run_all(lio, open_receivers)).enumerate() {
    if files.len() > 1 {
      if i > 0 {
        println!();
      }
      println!("==> {} <==", path);
    }
    let file = file?;
    head_fd(lio, &file, num_lines)?;
  }

  Ok(())
}

fn head_fd(lio: &Lio, fd: &Resource, num_lines: usize) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];
  let mut lines_printed = 0usize;
  let mut pending = Vec::new();

  'outer: loop {
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      if !pending.is_empty() && lines_printed < num_lines {
        write_all(lio, &stdout, std::mem::take(&mut pending))?;
      }
      break;
    }

    pending.extend_from_slice(&buf[..n]);

    while let Some(newline_pos) = pending.iter().position(|&b| b == b'\n') {
      let line_end = newline_pos + 1;
      let line = pending[..line_end].to_vec();
      pending = pending[line_end..].to_vec();

      write_all(lio, &stdout, line)?;

      lines_printed += 1;
      if lines_printed >= num_lines {
        break 'outer;
      }
    }
  }

  Ok(())
}

fn tee(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  let (append, files): (bool, Vec<&str>) =
    if args.first().map(|s| s.as_str()) == Some("-a") {
      (true, args[1..].iter().map(|s| s.as_str()).collect())
    } else {
      (false, args.iter().map(|s| s.as_str()).collect())
    };

  let mut outputs: Vec<Resource> = Vec::new();
  let cwd = Resource::cwd();

  let mut open_receivers = Vec::with_capacity(files.len());
  for path in &files {
    let cpath = CString::new(*path)?;
    let flags = libc::O_WRONLY
      | libc::O_CREAT
      | if append { libc::O_APPEND } else { libc::O_TRUNC };
    open_receivers.push(api::openat(&cwd, cpath, flags).with_lio(lio).send());
  }
  for result in run_all(lio, open_receivers) {
    outputs.push(result?);
  }

  let stdin = Resource::stdin();
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];

  loop {
    let rx = api::read(&stdin, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
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
    write_all_many(lio, writes)?;
  }

  Ok(())
}

fn tail(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  let mut follow = false;
  let mut num_lines = 10usize;
  let mut files = Vec::new();
  let mut i = 0;

  while i < args.len() {
    match args[i].as_str() {
      "-f" => follow = true,
      "-n" => {
        i += 1;
        num_lines = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(10);
      }
      arg => files.push(arg.to_string()),
    }
    i += 1;
  }

  if files.is_empty() {
    tail_fd(lio, &Resource::stdin(), num_lines, follow)
  } else {
    for (i, path) in files.iter().enumerate() {
      if files.len() > 1 {
        if i > 0 {
          println!();
        }
        println!("==> {} <==", path);
      }
      let cpath = CString::new(path.as_str())?;
      let rx = api::openat(&Resource::cwd(), cpath, libc::O_RDONLY)
        .with_lio(lio)
        .send();
      let file = run(lio, rx)?;
      tail_fd(lio, &file, num_lines, follow)?;
    }
    Ok(())
  }
}

fn tail_fd(
  lio: &Lio,
  fd: &Resource,
  num_lines: usize,
  follow: bool,
) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];
  let mut lines: VecDeque<Vec<u8>> = VecDeque::with_capacity(num_lines + 1);
  let mut current_line = Vec::new();

  loop {
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    for &byte in &buf[..n] {
      current_line.push(byte);
      if byte == b'\n' {
        lines.push_back(std::mem::take(&mut current_line));
        if lines.len() > num_lines {
          lines.pop_front();
        }
      }
    }
  }

  if !current_line.is_empty() {
    lines.push_back(current_line);
    if lines.len() > num_lines {
      lines.pop_front();
    }
  }

  for line in lines {
    write_all(lio, &stdout, line)?;
  }

  if follow {
    loop {
      let rx = api::sleep(Duration::from_millis(100)).with_lio(lio).send();
      run(lio, rx)?;

      let rx = api::read(fd, buf).with_lio(lio).send();
      let (result, returned_buf) = run(lio, rx);
      buf = returned_buf;

      let n = result? as usize;
      if n > 0 {
        write_all(lio, &stdout, buf[..n].to_vec())?;
      }
    }
  }

  Ok(())
}

fn wc(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.is_empty() {
    let (lines, words, bytes) = wc_fd(lio, &Resource::stdin())?;
    println!("{:>8} {:>8} {:>8}", lines, words, bytes);
    return Ok(());
  }

  let mut total = (0usize, 0usize, 0usize);

  for path in args {
    let cpath = CString::new(path.as_str())?;
    let rx =
      api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send();
    let file = run(lio, rx)?;
    let counts = wc_fd(lio, &file)?;
    total.0 += counts.0;
    total.1 += counts.1;
    total.2 += counts.2;
    println!("{:>8} {:>8} {:>8} {}", counts.0, counts.1, counts.2, path);
  }

  if args.len() > 1 {
    println!("{:>8} {:>8} {:>8} total", total.0, total.1, total.2);
  }

  Ok(())
}

fn wc_fd(lio: &Lio, fd: &Resource) -> std::io::Result<(usize, usize, usize)> {
  let mut buf = vec![0u8; 8192];
  let mut lines = 0usize;
  let mut words = 0usize;
  let mut bytes = 0usize;
  let mut in_word = false;

  loop {
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }

    bytes += n;
    for &byte in &buf[..n] {
      if byte == b'\n' {
        lines += 1;
      }
      if byte.is_ascii_whitespace() {
        in_word = false;
      } else if !in_word {
        words += 1;
        in_word = true;
      }
    }
  }

  Ok((lines, words, bytes))
}

fn yes(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = if args.is_empty() {
    b"y\n".to_vec()
  } else {
    format!("{}\n", args.join(" ")).into_bytes()
  };

  loop {
    let (result, returned_buf) = write_once(lio, &stdout, buf);
    result?;
    buf = returned_buf;
  }
}

fn sleep_cmd(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  let seconds =
    args.first().and_then(|s| s.parse::<f64>().ok()).unwrap_or_else(|| {
      eprintln!("Usage: sleep <seconds>");
      std::process::exit(1);
    });

  let rx = api::sleep(Duration::from_secs_f64(seconds)).with_lio(lio).send();
  run(lio, rx)?;
  Ok(())
}

fn clear(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if !args.is_empty() {
    eprintln!("Usage: clear");
    std::process::exit(1);
  }

  let stdout = Resource::stdout();
  write_all(lio, &stdout, b"\x1b[2J\x1b[H".to_vec())?;
  Ok(())
}

fn echo(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut line = args.join(" ").into_bytes();
  line.push(b'\n');
  write_all(lio, &stdout, line)?;
  Ok(())
}

fn pwd(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if !args.is_empty() {
    eprintln!("Usage: pwd");
    std::process::exit(1);
  }

  let stdout = Resource::stdout();
  let mut cwd = std::env::current_dir()?.display().to_string().into_bytes();
  cwd.push(b'\n');
  write_all(lio, &stdout, cwd)?;
  Ok(())
}

fn paste(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.is_empty() {
    eprintln!("Usage: paste <file>...");
    std::process::exit(1);
  }

  let stdout = Resource::stdout();
  let mut open_receivers = Vec::with_capacity(args.len());
  for path in args {
    let cpath = CString::new(path.as_str())?;
    open_receivers.push(
      api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send(),
    );
  }

  let mut columns = Vec::with_capacity(args.len());
  for file in run_all(lio, open_receivers) {
    columns.push(read_to_string_fd(lio, &file?)?);
  }

  let line_sets: Vec<Vec<&str>> =
    columns.iter().map(|s| s.lines().collect()).collect();
  let max_lines = line_sets.iter().map(Vec::len).max().unwrap_or(0);

  for row in 0..max_lines {
    let mut line = String::new();
    for (col, lines) in line_sets.iter().enumerate() {
      if col > 0 {
        line.push('\t');
      }
      if let Some(part) = lines.get(row) {
        line.push_str(part);
      }
    }
    line.push('\n');
    write_all(lio, &stdout, line.into_bytes())?;
  }

  Ok(())
}

fn uniq(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.len() > 1 {
    eprintln!("Usage: uniq [file]");
    std::process::exit(1);
  }

  let stdout = Resource::stdout();
  let input = read_to_string(lio, args.first().map(String::as_str))?;
  let mut previous: Option<&str> = None;
  for line in input.split_inclusive('\n') {
    if previous != Some(line) {
      write_all(lio, &stdout, line.as_bytes().to_vec())?;
      previous = Some(line);
    }
  }

  if !input.is_empty() && !input.ends_with('\n') {
    let last = input.lines().last().unwrap_or("");
    if previous != Some(last) {
      let mut out = last.as_bytes().to_vec();
      out.push(b'\n');
      write_all(lio, &stdout, out)?;
    }
  }

  Ok(())
}

fn cut(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.len() < 4 || args[0] != "-d" || args[2] != "-f" {
    eprintln!("Usage: cut -d <delim> -f <field> [file]");
    std::process::exit(1);
  }

  let delim = args[1].chars().next().unwrap_or('\t');
  let field = args[3].parse::<usize>().map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid field")
  })?;
  if field == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "field is 1-based",
    ));
  }

  let input = read_to_string(lio, args.get(4).map(String::as_str))?;
  let stdout = Resource::stdout();
  for raw_line in input.lines() {
    let selected = raw_line.split(delim).nth(field - 1).unwrap_or("");
    let mut out = selected.as_bytes().to_vec();
    out.push(b'\n');
    write_all(lio, &stdout, out)?;
  }

  Ok(())
}

fn nl(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.len() > 1 {
    eprintln!("Usage: nl [file]");
    std::process::exit(1);
  }

  let input = read_to_string(lio, args.first().map(String::as_str))?;
  let stdout = Resource::stdout();
  for (i, line) in input.lines().enumerate() {
    let out = format!("{:>6}\t{}\n", i + 1, line).into_bytes();
    write_all(lio, &stdout, out)?;
  }

  Ok(())
}

fn tac(lio: &Lio, args: &[String]) -> std::io::Result<()> {
  if args.len() > 1 {
    eprintln!("Usage: tac [file]");
    std::process::exit(1);
  }

  let input = read_to_string(lio, args.first().map(String::as_str))?;
  let stdout = Resource::stdout();
  let mut lines: Vec<&str> = input.lines().collect();
  lines.reverse();
  for line in lines {
    let mut out = line.as_bytes().to_vec();
    out.push(b'\n');
    write_all(lio, &stdout, out)?;
  }

  Ok(())
}

fn read_to_string(lio: &Lio, path: Option<&str>) -> std::io::Result<String> {
  match path {
    Some(path) => {
      let cpath = CString::new(path)?;
      let fd = run_all(
        lio,
        vec![
          api::openat(&Resource::cwd(), cpath, libc::O_RDONLY)
            .with_lio(lio)
            .send(),
        ],
      )
      .into_iter()
      .next()
      .expect("missing open result")?;
      read_to_string_fd(lio, &fd)
    }
    None => {
      let stdin = Resource::stdin();
      read_to_string_fd(lio, &stdin)
    }
  }
}

fn read_to_string_fd(lio: &Lio, input: &Resource) -> std::io::Result<String> {
  let mut data = Vec::new();
  let mut buf = vec![0u8; 8192];
  loop {
    let rx = api::read(input, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }
    data.extend_from_slice(&buf[..n]);
  }

  String::from_utf8(data)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn run_all<T>(lio: &Lio, mut rxs: Vec<api::io::Receiver<T>>) -> Vec<T> {
  let mut results: Vec<Option<T>> = Vec::with_capacity(rxs.len());
  results.resize_with(rxs.len(), || None);
  let mut remaining = rxs.len();

  while remaining > 0 {
    for (idx, rx) in rxs.iter_mut().enumerate() {
      if results[idx].is_some() {
        continue;
      }
      if let Some(result) = rx.try_recv() {
        results[idx] = Some(result);
        remaining -= 1;
      }
    }

    if remaining == 0 {
      break;
    }

    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }

  results.into_iter().map(|item| item.expect("missing result")).collect()
}

fn write_once(
  lio: &Lio,
  fd: &Resource,
  buf: Vec<u8>,
) -> (std::io::Result<i32>, Vec<u8>) {
  let rx = api::write(fd, buf).with_lio(lio).send();
  run(lio, rx)
}

fn write_all(lio: &Lio, fd: &Resource, buf: Vec<u8>) -> std::io::Result<()> {
  let mut written = 0usize;
  while written < buf.len() {
    let (result, _) = write_once(lio, fd, buf[written..].to_vec());
    let n = result? as usize;
    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "write returned zero before completing output",
      ));
    }
    written += n;
  }
  Ok(())
}

fn write_all_many(
  lio: &Lio,
  bufs: Vec<(Resource, Vec<u8>)>,
) -> std::io::Result<()> {
  let mut pending: Vec<(Resource, Vec<u8>, usize)> =
    bufs.into_iter().map(|(fd, buf)| (fd, buf, 0usize)).collect();

  while pending.iter().any(|(_, buf, written)| *written < buf.len()) {
    let mut rxs = Vec::new();
    let mut active = Vec::new();

    for (idx, (fd, buf, written)) in pending.iter().enumerate() {
      if *written >= buf.len() {
        continue;
      }
      rxs.push(api::write(fd, buf[*written..].to_vec()).with_lio(lio).send());
      active.push(idx);
    }

    for (idx, (result, _)) in active.into_iter().zip(run_all(lio, rxs)) {
      let n = result? as usize;
      if n == 0 {
        return Err(io::Error::new(
          io::ErrorKind::WriteZero,
          "write returned zero before completing output",
        ));
      }
      pending[idx].2 += n;
    }
  }

  Ok(())
}

fn run<T>(lio: &Lio, mut rx: api::io::Receiver<T>) -> T {
  loop {
    if let Some(result) = rx.try_recv() {
      return result;
    }
    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }
}
