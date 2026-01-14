//! Registered buffers example
//!
//! This example demonstrates:
//! - Registering buffers with the kernel for zero-copy I/O
//! - Using WriteFixed for writes and Read for reads
//! - Performance benefits of registered buffers

use lio_uring::completion::CompletionQueue;
use lio_uring::operation::*;
use lio_uring::submission::SubmissionQueue;
use std::io::{self, IoSlice};
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

  uring_print(&mut sq, "io_uring Registered Buffers Example\n")?;
  uring_print(&mut sq, "====================================\n\n")?;

  // Open a test file
  let path = b"/tmp/lio_uring_fixed_test\0";
  let open_op = Openat {
    dfd: libc::AT_FDCWD,
    path: path.as_ptr().cast(),
    flags: libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    mode: 0o644,
  };

  unsafe { sq.push(&open_op, 1) }?;
  flush(&mut sq, &mut cq, 3)?; // 2 prints + 1 open
  let fd = cq.next()?.result()?;
  // Wrap in ManuallyDrop to prevent automatic close() syscall
  // We'll close it using io_uring Close operation instead
  let file = ManuallyDrop::new(unsafe { OwnedFd::from_raw_fd(fd) });
  uring_print(&mut sq, "Opened file\n")?;

  // Prepare buffers to register
  // These buffers will be pinned in kernel memory
  let mut buffer1 = vec![0u8; 4096];
  let mut buffer2 = vec![0u8; 4096];
  let mut buffer3 = vec![0u8; 4096];

  // Fill buffer1 with some test data
  for (i, byte) in buffer1.iter_mut().enumerate() {
    *byte = (i % 256) as u8;
  }
  buffer2.fill(0xAA);
  buffer3.fill(0xBB);

  uring_print(&mut sq, "\n=== Registering Buffers ===\n")?;
  uring_print(&mut sq, "Registering 3 buffers (4KB each)...\n")?;

  // Register the buffers with io_uring
  // SAFETY: The buffers will remain valid and won't be moved until unregistered
  let buffers = vec![
    IoSlice::new(&buffer1),
    IoSlice::new(&buffer2),
    IoSlice::new(&buffer3),
  ];

  flush(&mut sq, &mut cq, 3)?; // 3 prints
  unsafe { sq.register_buffers(&buffers) }?;

  uring_print(&mut sq, "Buffers registered\n")?;
  uring_print(&mut sq, "Buffers are now pinned in kernel memory\n")?;

  // Now use WriteFixed to write using the registered buffers
  uring_print(&mut sq, "\n=== Using WriteFixed Operations ===\n")?;

  // Write buffer 0 (buffer1) to offset 0
  let write1 = WriteFixed {
    fd: file.as_raw_fd(),
    buf: buffer1.as_ptr().cast(),
    nbytes: buffer1.len() as u32,
    offset: 0,
    buf_index: 0, // Reference registered buffer by index
  };

  // Write buffer 1 (buffer2) to offset 4096
  let write2 = WriteFixed {
    fd: file.as_raw_fd(),
    buf: buffer2.as_ptr().cast(),
    nbytes: buffer2.len() as u32,
    offset: 4096,
    buf_index: 1,
  };

  // Write buffer 2 (buffer3) to offset 8192
  let write3 = WriteFixed {
    fd: file.as_raw_fd(),
    buf: buffer3.as_ptr().cast(),
    nbytes: buffer3.len() as u32,
    offset: 8192,
    buf_index: 2,
  };

  uring_print(&mut sq, "Submitting 3 WriteFixed operations (batch)...\n")?;
  flush(&mut sq, &mut cq, 4)?; // 4 prints

  unsafe { sq.push(&write1, 10) }?;
  unsafe { sq.push(&write2, 11) }?;
  unsafe { sq.push(&write3, 12) }?;
  sq.submit()?;

  // Collect completions
  for _ in 0..3 {
    let completion = cq.next()?;
    completion.result()?;
  }
  uring_print(&mut sq, "WriteFixed operations completed\n")?;

  // Sync to ensure data is on disk
  let fsync = Fsync { fd: file.as_raw_fd(), flags: 0 };
  unsafe { sq.push(&fsync, 20) }?;
  flush(&mut sq, &mut cq, 2)?; // 1 print + 1 fsync

  uring_print(&mut sq, "\n=== Using Read Operations ===\n")?;

  // Clear the buffers to verify read actually works
  buffer1.fill(0);
  buffer2.fill(0);
  buffer3.fill(0);

  // Read back using regular Read (not ReadFixed)
  let read1 = Read {
    fd: file.as_raw_fd(),
    ptr: buffer1.as_mut_ptr().cast(),
    len: buffer1.len() as u32,
    offset: 0,
  };

  let read2 = Read {
    fd: file.as_raw_fd(),
    ptr: buffer2.as_mut_ptr().cast(),
    len: buffer2.len() as u32,
    offset: 4096,
  };

  let read3 = Read {
    fd: file.as_raw_fd(),
    ptr: buffer3.as_mut_ptr().cast(),
    len: buffer3.len() as u32,
    offset: 8192,
  };

  uring_print(&mut sq, "Submitting 3 Read operations (batch)...\n")?;
  flush(&mut sq, &mut cq, 2)?; // 2 prints

  unsafe { sq.push(&read1, 30) }?;
  unsafe { sq.push(&read2, 31) }?;
  unsafe { sq.push(&read3, 32) }?;
  sq.submit()?;

  for _ in 0..3 {
    let completion = cq.next()?;
    completion.result()?;
  }
  uring_print(&mut sq, "Read operations completed\n")?;

  // Verify the data
  uring_print(&mut sq, "\n=== Verifying Data ===\n")?;
  let mut verification_ok = true;

  flush(&mut sq, &mut cq, 2)?; // 2 prints

  // Check buffer1 - should be 0, 1, 2, ..., 255, 0, 1, ...
  for (i, &byte) in buffer1.iter().enumerate() {
    if byte != (i % 256) as u8 {
      uring_print(&mut sq, "Buffer 1 verification failed\n")?;
      verification_ok = false;
      break;
    }
  }
  if verification_ok {
    uring_print(&mut sq, "Buffer 1 verified (pattern data)\n")?;
  }

  // Check buffer2 - should all be 0xAA
  if buffer2.iter().all(|&b| b == 0xAA) {
    uring_print(&mut sq, "Buffer 2 verified (0xAA pattern)\n")?;
  } else {
    uring_print(&mut sq, "Buffer 2 verification failed\n")?;
    verification_ok = false;
  }

  // Check buffer3 - should all be 0xBB
  if buffer3.iter().all(|&b| b == 0xBB) {
    uring_print(&mut sq, "Buffer 3 verified (0xBB pattern)\n")?;
  } else {
    uring_print(&mut sq, "Buffer 3 verification failed\n")?;
    verification_ok = false;
  }

  if verification_ok {
    uring_print(&mut sq, "\nAll data verified successfully!\n")?;
  }

  // Performance comparison: regular vs fixed operations
  uring_print(&mut sq, "\n=== Performance Comparison ===\n")?;
  flush(&mut sq, &mut cq, 5)?; // 5 prints max

  // Unregister buffers first
  sq.unregister_buffers()?;
  uring_print(&mut sq, "Buffers unregistered\n")?;

  // Do regular writes for comparison
  let mut temp_buf = vec![0u8; 4096];
  temp_buf.fill(0xCC);

  for i in 0..10 {
    let write = Write {
      fd: file.as_raw_fd(),
      ptr: temp_buf.as_ptr() as *mut _,
      len: temp_buf.len() as u32,
      offset: (i * 4096) as u64,
    };
    unsafe { sq.push(&write, 100 + i) }?;
  }
  sq.submit()?;
  for _ in 0..10 {
    cq.next()?.result()?;
  }
  uring_print(&mut sq, "10 regular writes completed\n")?;

  // Re-register and do fixed writes
  let buffers = vec![IoSlice::new(&temp_buf)];
  flush(&mut sq, &mut cq, 2)?; // 2 prints
  unsafe { sq.register_buffers(&buffers) }?;

  for i in 0..10 {
    let write = WriteFixed {
      fd: file.as_raw_fd(),
      buf: temp_buf.as_ptr().cast(),
      nbytes: temp_buf.len() as u32,
      offset: (i * 4096) as u64,
      buf_index: 0,
    };
    unsafe { sq.push(&write, 200 + i) }?;
  }
  sq.submit()?;
  for _ in 0..10 {
    cq.next()?.result()?;
  }
  uring_print(&mut sq, "10 fixed writes completed\n")?;

  // Cleanup
  sq.unregister_buffers()?;

  // Close file using io_uring Close operation instead of drop() syscall
  let close_op = Close { fd: file.as_raw_fd() };
  unsafe { sq.push(&close_op, 998) }?;

  let unlink =
    Unlinkat { dfd: libc::AT_FDCWD, path: path.as_ptr().cast(), flags: 0 };
  unsafe { sq.push(&unlink, 999) }?;

  uring_print(&mut sq, "\n=== Summary ===\n")?;
  uring_print(&mut sq, "Registered buffers provide:\n")?;
  uring_print(
    &mut sq,
    "• Zero-copy I/O (data doesn't cross user/kernel boundary)\n",
  )?;
  uring_print(&mut sq, "• Reduced CPU overhead\n")?;
  uring_print(&mut sq, "• Better performance for frequently used buffers\n")?;
  uring_print(
    &mut sq,
    "• Buffers are pinned in physical memory (no page faults)\n",
  )?;

  flush(&mut sq, &mut cq, 8)?; // 1 print + 2 ops (close, unlink) + 6 prints

  Ok(())
}
