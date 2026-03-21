//! A simple `tail` implementation using lio.
//!
//! Usage: cargo run --example tail [-f] [-n lines] [file...]
//!
//! Default is 10 lines. If no files specified, reads from stdin.
//! Use -f to follow the file (watch for appended data).

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::collections::VecDeque;
use std::env;
use std::ffi::CString;
use std::time::Duration;

fn main() -> std::io::Result<()> {
  let args: Vec<String> = env::args().skip(1).collect();

  // Parse options
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

  let lio = Lio::new(64)?;

  if files.is_empty() {
    tail_fd(&lio, &Resource::stdin(), num_lines, follow)?;
  } else {
    for (i, path) in files.iter().enumerate() {
      if files.len() > 1 {
        if i > 0 {
          println!();
        }
        println!("==> {} <==", path);
      }
      tail_file(&lio, path, num_lines, follow)?;
    }
  }

  Ok(())
}

fn tail_file(
  lio: &Lio,
  path: &str,
  num_lines: usize,
  follow: bool,
) -> std::io::Result<()> {
  let cpath = CString::new(path)?;
  let rx =
    api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send();
  let file = run(lio, rx)?;
  tail_fd(lio, &file, num_lines, follow)
}

fn tail_fd(
  lio: &Lio,
  fd: &Resource,
  num_lines: usize,
  follow: bool,
) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];

  // Keep last N lines in a ring buffer
  let mut lines: VecDeque<Vec<u8>> = VecDeque::with_capacity(num_lines + 1);
  let mut current_line = Vec::new();

  // Read entire input, keeping only last N lines
  loop {
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break; // EOF
    }

    // Process bytes, splitting on newlines
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

  // Handle last line without newline
  if !current_line.is_empty() {
    lines.push_back(current_line);
    if lines.len() > num_lines {
      lines.pop_front();
    }
  }

  // Output the last N lines
  for line in lines {
    let rx = api::write(&stdout, line).with_lio(lio).send();
    run(lio, rx).0?;
  }

  // Follow mode: keep watching for new data
  if follow {
    loop {
      // Sleep briefly before checking for new data
      let rx = api::sleep(Duration::from_millis(100)).with_lio(lio).send();
      run(lio, rx)?;

      // Try to read more
      let rx = api::read(fd, buf).with_lio(lio).send();
      let (result, returned_buf) = run(lio, rx);
      buf = returned_buf;

      let n = result? as usize;
      if n > 0 {
        // Output new data immediately
        let data = buf[..n].to_vec();
        let rx = api::write(&stdout, data).with_lio(lio).send();
        run(lio, rx).0?;
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
