//! A simple `head` implementation using lio.
//!
//! Usage: cargo run --example head [-n lines] [file...]
//!
//! Default is 10 lines. If no files specified, reads from stdin.

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::env;
use std::ffi::CString;

fn main() -> std::io::Result<()> {
  let args: Vec<String> = env::args().skip(1).collect();

  // Parse -n option
  let (num_lines, files) = if args.first().map(|s| s.as_str()) == Some("-n") {
    let n = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    (n, args[2..].to_vec())
  } else {
    (10, args)
  };

  let lio = Lio::new(64)?;

  if files.is_empty() {
    head_fd(&lio, &Resource::stdin(), num_lines)?;
  } else {
    for (i, path) in files.iter().enumerate() {
      if files.len() > 1 {
        if i > 0 {
          println!();
        }
        println!("==> {} <==", path);
      }
      head_file(&lio, path, num_lines)?;
    }
  }

  Ok(())
}

fn head_file(lio: &Lio, path: &str, num_lines: usize) -> std::io::Result<()> {
  let cpath = CString::new(path)?;
  let rx =
    api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send();
  let file = run(lio, rx)?;
  head_fd(lio, &file, num_lines)
}

fn head_fd(lio: &Lio, fd: &Resource, num_lines: usize) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];
  let mut lines_printed = 0;
  let mut pending = Vec::new(); // Leftover bytes from previous read

  'outer: loop {
    // Read
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      // EOF - print any remaining data
      if !pending.is_empty() {
        let rx = api::write(&stdout, std::mem::take(&mut pending))
          .with_lio(lio)
          .send();
        run(lio, rx).0?;
      }
      break;
    }

    // Process data looking for newlines
    pending.extend_from_slice(&buf[..n]);

    while let Some(newline_pos) = pending.iter().position(|&b| b == b'\n') {
      let line_end = newline_pos + 1;
      let line = pending[..line_end].to_vec();
      pending = pending[line_end..].to_vec();

      // Write line
      let rx = api::write(&stdout, line).with_lio(lio).send();
      run(lio, rx).0?;

      lines_printed += 1;
      if lines_printed >= num_lines {
        break 'outer;
      }
    }
  }

  Ok(())
}

fn run<T>(lio: &Lio, mut rx: api::io::Receiver<T>) -> T {
  loop {
    lio.try_run().expect("lio.try_run()");
    if let Some(result) = rx.try_recv() {
      return result;
    }
    lio.run().expect("lio.run()");
  }
}
