//! Linked operations example using io_uring.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use lio_uring::{
  LioUring, SqeFlags,
  operation::{Close, Fsync, OpenAt, Read, UnlinkAt, Write},
};

fn main() -> io::Result<()> {
  let mut ring = LioUring::new(64)?;

  let path = b"/tmp/lio_uring_linked\0";
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

  let data1 = b"First line\n";
  let write1 = Write::new(file.as_raw_fd(), data1.as_ptr(), data1.len() as u32)
    .offset(0)
    .build();
  let fsync1 = Fsync::new(file.as_raw_fd()).build();

  unsafe { ring.push_with_flags(write1, 10, SqeFlags::IO_LINK)? };
  unsafe { ring.push(fsync1, 11)? };
  ring.submit()?;

  let c1 = ring.wait()?;
  if c1.result() < 0 {
    return Err(io::Error::from_raw_os_error(-c1.result()));
  }
  let c2 = ring.wait()?;
  if c2.result() < 0 {
    return Err(io::Error::from_raw_os_error(-c2.result()));
  }

  let writes = [b"Line 2\n" as &[u8], b"Line 3\n", b"Line 4\n", b"Line 5\n"];
  let mut offset = data1.len() as u64;
  for (i, data) in writes.iter().enumerate() {
    let write = Write::new(file.as_raw_fd(), data.as_ptr(), data.len() as u32)
      .offset(offset)
      .build();
    unsafe { ring.push(write, 20 + i as u64)? };
    offset += data.len() as u64;
  }
  ring.submit()?;

  for _ in 0..writes.len() {
    let cqe = ring.wait()?;
    if cqe.result() < 0 {
      return Err(io::Error::from_raw_os_error(-cqe.result()));
    }
  }

  let mut read_buf = vec![0_u8; 1024];
  let read = Read::new(
    file.as_raw_fd(),
    read_buf.as_mut_ptr(),
    read_buf.len() as u32,
  )
  .offset(0)
  .build();
  unsafe { ring.push(read, 30)? };
  ring.submit()?;
  let read_cqe = ring.wait()?;
  if read_cqe.result() < 0 {
    return Err(io::Error::from_raw_os_error(-read_cqe.result()));
  }
  let bytes_read = read_cqe.result() as usize;

  let expected = b"First line\nLine 2\nLine 3\nLine 4\nLine 5\n";
  assert_eq!(&read_buf[..bytes_read], expected);

  let raw_fd = file.as_raw_fd();
  std::mem::forget(file);

  let close = Close::new(raw_fd).build();
  unsafe { ring.push(close, 39)? };
  let unlink = UnlinkAt::new(libc::AT_FDCWD, path.as_ptr().cast()).build();
  unsafe { ring.push(unlink, 40)? };
  ring.submit()?;

  for _ in 0..2 {
    let cqe = ring.wait()?;
    if cqe.result() < 0 {
      return Err(io::Error::from_raw_os_error(-cqe.result()));
    }
  }

  println!("linked io_uring operations succeeded");
  Ok(())
}
