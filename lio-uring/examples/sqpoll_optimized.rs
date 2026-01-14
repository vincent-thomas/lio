//! Example demonstrating SQPOLL mode optimizations
//!
//! SQPOLL (Submission Queue Polling) allows a kernel thread to poll the
//! submission queue, eliminating syscalls for most submissions.
//!
//! Key optimizations:
//! 1. Check if SQPOLL thread needs waking (IORING_SQ_NEED_WAKEUP)
//! 2. Only call io_uring_enter() when thread is sleeping
//! 3. Use memory fences instead of syscalls when thread is active

use lio_uring::{
  IoUringParams,
  completion::{CompletionQueue, SqeFlags},
  operation::*,
  submission::SubmissionQueue,
};
use std::io;

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
    unsafe { sq.push_with_flags(&write_op, 999, SqeFlags::IO_LINK) }?;
  } else {
    unsafe { sq.push(&write_op, 999) }?;
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

fn main() -> std::io::Result<()> {
  // Create io_uring with SQPOLL enabled
  // idle_ms = 1000 means the kernel thread sleeps after 1 second of inactivity
  let params = IoUringParams::default().sqpoll(1000);

  let (mut cq, mut sq) = match lio_uring::with_params(params) {
    Ok(rings) => rings,
    Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
      // Note: Can't print error here without io_uring initialized
      // Just exit gracefully
      return Ok(());
    }
    Err(e) => return Err(e),
  };

  uring_print(&mut sq, "SQPOLL enabled\n", false)?;
  flush(&mut sq, &mut cq, 1)?;

  // Submit operations using the optimized path
  for _ in 0..10 {
    uring_print(&mut sq, "Write operation\n", false)?;
  }

  // This will:
  // - Check if SQPOLL thread needs waking
  // - If yes: call io_uring_enter() to wake it (1 syscall)
  // - If no: just ensure memory visibility (0 syscalls!)
  sq.submit()?;

  // Collect completions from NOPs
  for _ in 0..10 {
    let completion = cq.next()?;
    assert!(completion.is_ok());
  }

  // Now queue all prints together and flush at the end
  // Use IO_LINK to ensure they execute in order
  uring_print(&mut sq, "Submitted operations\n", true)?;
  uring_print(&mut sq, "Completed all operations\n", true)?;
  uring_print(&mut sq, "\nPerformance tip:\n", true)?;
  uring_print(
    &mut sq,
    "With SQPOLL, batch operations and use submit_sqpoll_optimized()\n",
    true,
  )?;
  uring_print(
    &mut sq,
    "to minimize syscalls. The kernel thread will pick them up!\n",
    false,
  )?; // Last one is not linked

  flush(&mut sq, &mut cq, 5)?; // 5 final prints

  Ok(())
}
