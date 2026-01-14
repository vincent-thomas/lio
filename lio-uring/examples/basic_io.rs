//! Basic file I/O example using io_uring
//!
//! This example demonstrates:
//! - Opening a file
//! - Writing data
//! - Reading data back
//! - Proper error handling

use lio_uring::completion::{CompletionQueue, SqeFlags};
use lio_uring::operation::*;
use lio_uring::submission::SubmissionQueue;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// Helper to queue print messages (doesn't submit immediately)
fn uring_print(
  sq: &mut SubmissionQueue,
  msg: &str,
  link: bool,
) -> io::Result<()> {
  let write_op = Write {
    fd: 1, // stdout
    ptr: msg.as_ptr() as *mut _,
    len: msg.len() as u32,
    offset: -1i64 as u64, // append mode for stdout
  };
  if link {
    unsafe { sq.push_with_flags(&write_op, 0, SqeFlags::IO_LINK) }?;
  } else {
    unsafe { sq.push(&write_op, 0) }?;
  }
  Ok(())
}

// Flush all queued operations
fn flush(
  sq: &mut SubmissionQueue,
  cq: &mut CompletionQueue,
  count: usize,
) -> io::Result<()> {
  sq.submit()?;
  for _ in 0..count {
    cq.next()?.result()?;
  }
  Ok(())
}

fn main() -> io::Result<()> {
  let (mut cq, mut sq) = lio_uring::with_capacity(32)?;

  uring_print(&mut sq, "io_uring Basic I/O Example\n", true)?;
  uring_print(&mut sq, "==========================\n\n", true)?;
  uring_print(&mut sq, "Created io_uring\n", false)?;

  // Open a temporary file for testing
  let path = b"/tmp/lio_uring_test\0";
  let open_op = Openat {
    dfd: libc::AT_FDCWD,
    path: path.as_ptr().cast(),
    flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    mode: 0o644,
  };

  unsafe { sq.push(&open_op, 1) }?;
  flush(&mut sq, &mut cq, 4)?; // 3 prints + 1 open

  let fd = cq.next()?.result()?;
  uring_print(&mut sq, "Opened file\n", false)?;

  // SAFETY: We just opened this fd and own it exclusively
  // Wrap in ManuallyDrop to prevent automatic close() syscall
  let file = ManuallyDrop::new(unsafe { OwnedFd::from_raw_fd(fd) });

  // Write some data
  let data = b"Hello from io_uring! This is a test of async I/O.\n";
  let write_op = Write {
    fd: file.as_raw_fd(),
    ptr: data.as_ptr() as *mut _,
    len: data.len() as u32,
    offset: 0,
  };

  unsafe { sq.push(&write_op, 2) }?;
  flush(&mut sq, &mut cq, 2)?; // 1 print + 1 write

  uring_print(&mut sq, "Wrote data\n", false)?;

  // Read the data back
  let mut read_buf = vec![0u8; data.len()];
  let read_op = Read {
    fd: file.as_raw_fd(),
    ptr: read_buf.as_mut_ptr().cast(),
    len: read_buf.len() as u32,
    offset: 0,
  };

  unsafe { sq.push(&read_op, 3) }?;
  flush(&mut sq, &mut cq, 2)?; // 1 print + 1 read

  let bytes_read = cq.next()?.result()?;
  uring_print(&mut sq, "Read data\n", true)?;

  // Verify the data
  let read_data = &read_buf[..bytes_read as usize];
  assert_eq!(read_data, data);
  uring_print(&mut sq, "Data verification: SUCCESS\n", false)?;

  // Sync the file to disk
  let fsync_op = Fsync { fd: file.as_raw_fd(), flags: 0 };

  unsafe { sq.push(&fsync_op, 4) }?;
  flush(&mut sq, &mut cq, 3)?; // 2 prints + 1 fsync

  uring_print(&mut sq, "Synced file to disk\n", false)?;

  // Close file using io_uring Close operation instead of drop() syscall
  let close_op = Close { fd: file.as_raw_fd() };
  unsafe { sq.push(&close_op, 5) }?;

  // Clean up: unlink the file
  let unlink_op =
    Unlinkat { dfd: libc::AT_FDCWD, path: path.as_ptr().cast(), flags: 0 };
  unsafe { sq.push(&unlink_op, 6) }?;

  uring_print(&mut sq, "Closed file\n", true)?;
  uring_print(&mut sq, "Removed test file\n", true)?;
  uring_print(&mut sq, "\nAll operations completed successfully!\n", true)?;

  flush(&mut sq, &mut cq, 6)?; // 1 print + 2 ops (close, unlink) + 3 prints

  Ok(())
}
