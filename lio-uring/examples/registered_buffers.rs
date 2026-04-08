//! Registered buffers example using the current `LioUring` API.

use lio_uring::{LioUring, operation::*};
use std::fs;
use std::io::{self, IoSlice};
use std::os::fd::AsRawFd;

fn main() -> io::Result<()> {
  let path = "/tmp/lio_uring_fixed_test";
  let mut ring = LioUring::new(16)?;
  let file = fs::OpenOptions::new()
    .create(true)
    .truncate(true)
    .read(true)
    .write(true)
    .open(path)?;

  let mut pattern = vec![0u8; 4096];
  for (i, byte) in pattern.iter_mut().enumerate() {
    *byte = (i % 256) as u8;
  }

  let buffers = [IoSlice::new(&pattern)];
  unsafe { ring.register_buffers(&buffers)? };

  let write = WriteFixed::new(
    file.as_raw_fd(),
    pattern.as_ptr(),
    pattern.len() as u32,
    0,
  );
  unsafe { ring.push(write.build(), 1)? };
  ring.submit()?;
  let completion = ring.wait()?;
  assert!(completion.is_ok());
  assert_eq!(completion.result(), pattern.len() as i32);

  let fsync = Fsync::new(file.as_raw_fd());
  unsafe { ring.push(fsync.build(), 2)? };
  ring.submit()?;
  let completion = ring.wait()?;
  assert!(completion.is_ok());

  let mut read_back = vec![0u8; pattern.len()];
  let read =
    Read::new(file.as_raw_fd(), read_back.as_mut_ptr(), read_back.len() as u32);
  let read = read.offset(0);
  unsafe { ring.push(read.build(), 3)? };
  ring.submit()?;
  let completion = ring.wait()?;
  assert!(completion.is_ok());
  assert_eq!(completion.result(), read_back.len() as i32);
  assert_eq!(read_back, pattern);

  ring.unregister_buffers()?;
  fs::remove_file(path)?;
  println!("registered_buffers: wrote and read back {} bytes", read_back.len());
  Ok(())
}
