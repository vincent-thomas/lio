//! Basic file I/O example using io_uring.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use lio_uring::{
  LioUring,
  operation::{Close, Fsync, OpenAt, Read, UnlinkAt, Write},
};

fn main() -> io::Result<()> {
  let mut ring = LioUring::new(32)?;

  let path = b"/tmp/lio_uring_test\0";
  let open = OpenAt::new(libc::AT_FDCWD, path.as_ptr().cast())
    .flags(libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC)
    .mode(0o644)
    .build();

  unsafe { ring.push(open, 1)? };
  ring.submit()?;
  let open_cqe = ring.wait()?;
  if open_cqe.result() < 0 {
    return Err(io::Error::from_raw_os_error(-open_cqe.result()));
  }

  let fd = open_cqe.result();
  let file = unsafe { OwnedFd::from_raw_fd(fd) };

  let data = b"Hello from io_uring! This is a test of async I/O.\n";
  let write = Write::new(file.as_raw_fd(), data.as_ptr(), data.len() as u32)
    .offset(0)
    .build();
  unsafe { ring.push(write, 2)? };
  ring.submit()?;
  let write_cqe = ring.wait()?;
  if write_cqe.result() != data.len() as i32 {
    return Err(io::Error::other(format!(
      "short write: expected {}, got {}",
      data.len(),
      write_cqe.result()
    )));
  }

  let mut read_buf = vec![0_u8; data.len()];
  let read = Read::new(
    file.as_raw_fd(),
    read_buf.as_mut_ptr(),
    read_buf.len() as u32,
  )
  .offset(0)
  .build();
  unsafe { ring.push(read, 3)? };
  ring.submit()?;
  let read_cqe = ring.wait()?;
  if read_cqe.result() < 0 {
    return Err(io::Error::from_raw_os_error(-read_cqe.result()));
  }
  let bytes_read = read_cqe.result() as usize;
  assert_eq!(&read_buf[..bytes_read], data);

  let fsync = Fsync::new(file.as_raw_fd()).build();
  unsafe { ring.push(fsync, 4)? };
  ring.submit()?;
  let fsync_cqe = ring.wait()?;
  if fsync_cqe.result() < 0 {
    return Err(io::Error::from_raw_os_error(-fsync_cqe.result()));
  }

  let raw_fd = file.as_raw_fd();
  std::mem::forget(file);

  let close = Close::new(raw_fd).build();
  unsafe { ring.push(close, 5)? };
  let unlink = UnlinkAt::new(libc::AT_FDCWD, path.as_ptr().cast()).build();
  unsafe { ring.push(unlink, 6)? };
  ring.submit()?;

  for _ in 0..2 {
    let cqe = ring.wait()?;
    if cqe.result() < 0 {
      return Err(io::Error::from_raw_os_error(-cqe.result()));
    }
  }

  println!("basic io_uring file I/O succeeded");
  Ok(())
}
