//! Linked operations example
//!
//! This example demonstrates:
//! - Using IO_LINK to chain operations
//! - Batch submission for better performance
//! - Non-blocking completion checks

use lio_uring::completion::{CompletionQueue, SqeFlags};
use lio_uring::operation::*;
use lio_uring::submission::SubmissionQueue;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// Helper to queue print messages (doesn't submit immediately)
fn uring_print(sq: &mut SubmissionQueue, msg: &str) -> io::Result<()> {
  let write_op = Write {
    fd: 1, // stdout
    ptr: msg.as_ptr() as *mut _,
    len: msg.len() as u32,
    offset: -1i64 as u64, // append mode for stdout
  };
  unsafe { sq.push(&write_op, 0) }?;
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
  let (mut cq, mut sq) = lio_uring::with_capacity(64)?;

  uring_print(&mut sq, "io_uring Linked Operations Example\n")?;
  uring_print(&mut sq, "===================================\n\n")?;

  // Open a test file
  let path = b"/tmp/lio_uring_linked\0";
  let open_op = Openat {
    dfd: libc::AT_FDCWD,
    path: path.as_ptr().cast(),
    flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    mode: 0o644,
  };

  unsafe { sq.push(&open_op, 1) }?;
  flush(&mut sq, &mut cq, 3)?; // 2 prints + 1 open

  let fd = cq.next()?.result()?;
  let file = ManuallyDrop::new(unsafe { OwnedFd::from_raw_fd(fd) });
  uring_print(&mut sq, "Opened file\n")?;

  // Demonstrate linked operations: write -> fsync
  // The fsync will only execute if the write succeeds
  uring_print(&mut sq, "\nSubmitting linked operations (write -> fsync)...\n")?;

  let data1 = b"First line\n";
  let write1 = Write {
    fd: file.as_raw_fd(),
    ptr: data1.as_ptr() as *mut _,
    len: data1.len() as u32,
    offset: 0,
  };

  let fsync1 = Fsync { fd: file.as_raw_fd(), flags: 0 };

  // Link the operations together
  unsafe { sq.push_with_flags(&write1, 10, SqeFlags::IO_LINK) }?;
  unsafe { sq.push(&fsync1, 11) }?;

  flush(&mut sq, &mut cq, 2)?; // 2 prints

  // Submit both operations at once
  sq.submit()?;
  uring_print(&mut sq, "Submitted linked operations\n")?;

  // Wait for both completions
  let c1 = cq.next()?;
  c1.result()?;
  uring_print(&mut sq, "Write completed\n")?;

  let c2 = cq.next()?;
  c2.result()?;
  uring_print(&mut sq, "Fsync completed\n")?;

  // Batch multiple independent writes
  uring_print(&mut sq, "\nBatching multiple independent writes...\n")?;
  let writes = [b"Line 2\n" as &[u8], b"Line 3\n", b"Line 4\n", b"Line 5\n"];

  let mut offset = data1.len() as u64;
  for (i, data) in writes.iter().enumerate() {
    let write = Write {
      fd: file.as_raw_fd(),
      ptr: data.as_ptr() as *mut _,
      len: data.len() as u32,
      offset,
    };
    unsafe { sq.push(&write, 20 + i as u64) }?;
    offset += data.len() as u64;
  }

  flush(&mut sq, &mut cq, 4)?; // 4 prints

  // Submit all writes at once
  sq.submit()?;
  uring_print(&mut sq, "Submitted batch operations\n")?;

  // Collect completions using try_next (non-blocking)
  let mut completed = 0;
  while let Some(completion) = cq.try_next()? {
    completion.result()?;
    completed += 1;
  }

  // If some haven't completed yet, wait for them
  while completed < writes.len() {
    let completion = cq.next()?;
    completion.result()?;
    completed += 1;
  }
  uring_print(&mut sq, "All batch writes completed\n")?;

  // Read everything back to verify
  uring_print(&mut sq, "\nReading entire file back...\n")?;
  let mut read_buf = vec![0u8; 1024];
  let read = Read {
    fd: file.as_raw_fd(),
    ptr: read_buf.as_mut_ptr().cast(),
    len: read_buf.len() as u32,
    offset: 0,
  };

  unsafe { sq.push(&read, 30) }?;
  flush(&mut sq, &mut cq, 3)?; // 2 prints + 1 read

  let bytes_read = cq.next()?.result()? as usize;
  uring_print(&mut sq, "Read file contents:\n")?;
  // Write the file contents using io_uring
  let write_contents = Write {
    fd: 1,
    ptr: read_buf.as_ptr() as *mut _,
    len: bytes_read as u32,
    offset: -1i64 as u64,
  };
  unsafe { sq.push(&write_contents, 31) }?;

  // Cleanup: close file using io_uring Close operation
  let close_op = Close { fd: file.as_raw_fd() };
  unsafe { sq.push(&close_op, 39) }?;

  let unlink =
    Unlinkat { dfd: libc::AT_FDCWD, path: path.as_ptr().cast(), flags: 0 };
  unsafe { sq.push(&unlink, 40) }?;

  uring_print(&mut sq, "\nAll operations completed successfully!\n")?;

  flush(&mut sq, &mut cq, 5)?; // 1 print + 1 write_contents + 2 ops (close, unlink) + 1 print

  Ok(())
}
