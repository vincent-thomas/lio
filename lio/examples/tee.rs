//! A simple `tee` implementation using lio.
//!
//! Reads from stdin and writes to stdout AND specified files.
//!
//! Usage: cargo run --example tee [file...]
//!
//! Options:
//!   -a    Append to files instead of overwriting

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::env;
use std::ffi::CString;

fn main() -> std::io::Result<()> {
  let lio = Lio::new(64)?;
  let args: Vec<String> = env::args().skip(1).collect();

  // Parse -a flag
  let (append, files): (bool, Vec<&str>) =
    if args.first().map(|s| s.as_str()) == Some("-a") {
      (true, args[1..].iter().map(|s| s.as_str()).collect())
    } else {
      (false, args.iter().map(|s| s.as_str()).collect())
    };

  // Open output files
  let mut outputs: Vec<Resource> = Vec::new();
  let cwd = Resource::cwd();

  for path in &files {
    let cpath = CString::new(*path)?;
    let flags = libc::O_WRONLY
      | libc::O_CREAT
      | if append { libc::O_APPEND } else { libc::O_TRUNC };
    let rx = api::openat(&cwd, cpath, flags).with_lio(&lio).send();
    let file = run(&lio, rx)?;
    outputs.push(file);
  }

  // Read from stdin, write to stdout + all files
  let stdin = Resource::stdin();
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];

  loop {
    // Read from stdin
    let rx = api::read(&stdin, buf).with_lio(&lio).send();
    let (result, returned_buf) = run(&lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break; // EOF
    }

    let data = &buf[..n];

    // Write to stdout
    let rx = api::write(&stdout, data.to_vec()).with_lio(&lio).send();
    let (result, _) = run(&lio, rx);
    result?;

    // Write to each file
    for file in &outputs {
      let rx = api::write(file, data.to_vec()).with_lio(&lio).send();
      let (result, _) = run(&lio, rx);
      result?;
    }
  }

  // Files are automatically closed when dropped
  Ok(())
}

/// Run the event loop until the receiver has a result
fn run<T>(lio: &Lio, mut rx: api::io::Receiver<T>) -> T {
  loop {
    lio.try_run().expect("lio.try_run()");
    if let Some(result) = rx.try_recv() {
      return result;
    }
    lio.run().expect("lio.run()");
  }
}
