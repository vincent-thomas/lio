//! A simple `cp` implementation using lio.
//!
//! Usage: cargo run --example cp <source> <dest>

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::env;
use std::ffi::CString;

fn main() -> std::io::Result<()> {
  let args: Vec<String> = env::args().collect();
  if args.len() != 3 {
    eprintln!("Usage: {} <source> <dest>", args[0]);
    std::process::exit(1);
  }

  let src_path = &args[1];
  let dst_path = &args[2];

  let lio = Lio::new(64)?;
  let cwd = Resource::cwd();

  // Open source file for reading
  let src_cpath = CString::new(src_path.as_str())?;
  let rx = api::openat(&cwd, src_cpath, libc::O_RDONLY).with_lio(&lio).send();
  let src = run(&lio, rx)?;

  // Open/create destination file for writing
  let dst_cpath = CString::new(dst_path.as_str())?;
  let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
  let rx = api::openat(&cwd, dst_cpath, flags).with_lio(&lio).send();
  let dst = run(&lio, rx)?;

  // Copy data
  let mut buf = vec![0u8; 64 * 1024]; // 64KB buffer
  let mut total: usize = 0;

  loop {
    // Read from source
    let rx = api::read(&src, buf).with_lio(&lio).send();
    let (result, returned_buf) = run(&lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break; // EOF
    }

    // Write to destination
    let to_write = buf[..n].to_vec();
    let rx = api::write(&dst, to_write).with_lio(&lio).send();
    let (result, _) = run(&lio, rx);
    result?;

    total += n;
  }

  println!("Copied {} bytes from {} to {}", total, src_path, dst_path);
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
