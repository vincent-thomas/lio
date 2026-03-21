//! A simple `cat` implementation using lio.
//!
//! Usage: cargo run --example cat [file...]
//!
//! If no files are specified, reads from stdin.

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::env;
use std::ffi::CString;

fn main() -> std::io::Result<()> {
  let lio = Lio::new(64)?;
  let args: Vec<String> = env::args().skip(1).collect();

  if args.is_empty() {
    // Read from stdin
    cat_fd(&lio, &Resource::stdin())?;
  } else {
    // Read each file
    for path in &args {
      cat_file(&lio, path)?;
    }
  }

  Ok(())
}

fn cat_file(lio: &Lio, path: &str) -> std::io::Result<()> {
  let cpath = CString::new(path)?;
  let rx =
    api::openat(&Resource::cwd(), cpath, libc::O_RDONLY).with_lio(lio).send();
  let file = run(lio, rx)?;
  cat_fd(lio, &file)
}

fn cat_fd(lio: &Lio, fd: &Resource) -> std::io::Result<()> {
  let stdout = Resource::stdout();
  let mut buf = vec![0u8; 8192];

  loop {
    // Read
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break; // EOF
    }

    // Write
    let to_write = buf[..n].to_vec();
    let rx = api::write(&stdout, to_write).with_lio(lio).send();
    let (result, _) = run(lio, rx);
    result?;
  }

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
