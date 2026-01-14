//! # lio-uring
//!
//! A safe, ergonomic Rust interface to Linux's io_uring asynchronous I/O framework.
//!
//! ## Overview
//!
//! io_uring is a high-performance asynchronous I/O interface for Linux that provides:
//! - Zero-copy I/O operations
//! - Batched submission and completion
//! - Support for a wide variety of operations (file, network, etc.)
//! - Advanced features like polling, registered buffers, and more
//!
//! This crate provides a thin, safe wrapper around liburing while maintaining
//! high performance and full access to io_uring features.
//!
//! ## Basic Usage
//!
//! ```rust,no_run
//! use lio_uring::{IoUring, operation::Nop};
//!
//! # fn main() -> std::io::Result<()> {
//! // Create an io_uring instance with 128 entries
//! let (mut cq, mut sq) = IoUring::with_capacity(128)?;
//!
//! // Submit a no-op operation
//! let op = Nop;
//! unsafe { sq.push(&op, 1) }?;
//! sq.submit()?;
//!
//! // Wait for completion
//! let completion = cq.next()?;
//! assert_eq!(completion.user_data, 1);
//! assert!(completion.is_ok());
//! # Ok(())
//! # }
//! ```
//!
//! ## Registered Buffers and Fixed Operations
//!
//! For high-performance I/O, you can register buffers with the kernel and use
//! `ReadFixed`/`WriteFixed` operations for zero-copy I/O:
//!
//! ```rust,no_run
//! use lio_uring::{IoUring, operation::WriteFixed};
//! use std::io::IoSlice;
//! # use std::os::fd::AsRawFd;
//!
//! # fn main() -> std::io::Result<()> {
//! # let file = std::fs::File::create("/tmp/test")?;
//! let (mut cq, mut sq) = IoUring::with_capacity(32)?;
//!
//! // Prepare buffers
//! let mut buffer = vec![0u8; 4096];
//! buffer.fill(0x42);
//!
//! // Register buffers with io_uring (pins them in kernel memory)
//! let buffers = vec![IoSlice::new(&buffer)];
//! unsafe { sq.register_buffers(&buffers) }?;
//!
//! // Use WriteFixed operation with buf_index to reference the registered buffer
//! let write_op = WriteFixed {
//!     fd: file.as_raw_fd(),
//!     buf: buffer.as_ptr().cast(),  // Pointer to the actual data
//!     nbytes: buffer.len() as u32,
//!     offset: 0,
//!     buf_index: 0,  // Index of the registered buffer (0-based)
//! };
//!
//! unsafe { sq.push(&write_op, 1) }?;
//! sq.submit()?;
//!
//! let completion = cq.next()?;
//! println!("Wrote {} bytes with zero-copy I/O", completion.result()?);
//!
//! // Unregister when done
//! sq.unregister_buffers()?;
//! # Ok(())
//! # }
//! ```
//!
//! ### How Fixed Operations Work
//!
//! When you register buffers:
//! 1. The kernel pins the buffer pages in physical memory
//! 2. The kernel creates a mapping for fast access
//! 3. Operations using `buf_index` avoid the user/kernel copy
//!
//! **Benefits:**
//! - **Zero-copy I/O**: Data doesn't cross the user/kernel boundary
//! - **No page faults**: Buffers are pinned in physical memory
//! - **Reduced overhead**: Faster than regular read/write operations
//! - **Better performance**: Especially for frequently accessed buffers
//!
//! **Use cases:**
//! - High-frequency I/O operations with the same buffers
//! - Network packet processing with fixed-size buffers
//! - Database buffer pools
//! - Any scenario where you reuse the same buffers repeatedly
//!
//! ## Safety
//!
//! Most operations in io_uring involve raw pointers to user data. This crate marks
//! the `push()` method as `unsafe` because the caller must ensure:
//! - All pointers in operations remain valid until the operation completes
//! - Buffers are not accessed mutably while operations are in flight
//! - File descriptors remain valid until operations complete
//!
//! The submission and completion queues are split to prevent accidental concurrent
//! access. Each can be used from separate threads safely.

extern crate alloc;
extern crate core;

use alloc::sync::Arc;
use core::{cell::UnsafeCell, mem::MaybeUninit};
use std::io::Error as io_Error;

use crate::{completion::CompletionQueue, submission::SubmissionQueue};

pub mod operation;

mod bindings;
pub mod completion;
pub mod submission;

/// Configuration parameters for io_uring initialization
#[derive(Debug, Clone)]
pub struct IoUringParams {
  /// Number of entries in the submission queue
  pub sq_entries: u32,
  /// Additional flags for io_uring setup
  pub flags: u32,
  /// CPU affinity for SQPOLL thread
  pub sq_thread_cpu: u32,
  /// Idle time in milliseconds before SQPOLL thread sleeps
  pub sq_thread_idle: u32,
}

impl Default for IoUringParams {
  fn default() -> Self {
    Self { sq_entries: 128, flags: 0, sq_thread_cpu: 0, sq_thread_idle: 0 }
  }
}

impl IoUringParams {
  /// Enable submission queue polling (kernel thread polls SQ)
  pub fn sqpoll(mut self, idle_ms: u32) -> Self {
    self.flags |= bindings::IORING_SETUP_SQPOLL;
    self.sq_thread_idle = idle_ms;
    self
  }

  /// Enable IO polling (busy-wait for completions, lower latency)
  pub fn iopoll(mut self) -> Self {
    self.flags |= bindings::IORING_SETUP_IOPOLL;
    self
  }

  // /// Attach to existing SQ thread (for SQPOLL)
  // pub fn attach_wq(mut self, fd: i32) -> Self {
  //   self.flags |= bindings::IORING_SETUP_ATTACH_WQ;
  //   // Note: Would need to set wq_fd in params struct
  //   self
  // }
}

pub(crate) struct IoUring {
  ring: UnsafeCell<bindings::io_uring>,
  flags: u32,
}

// SAFETY: IoUring can be safely sent between threads. The split API ensures
// that submission and completion queues are accessed from separate owners.
unsafe impl Send for IoUring {}
unsafe impl Sync for IoUring {}

impl Drop for IoUring {
  fn drop(&mut self) {
    unsafe { bindings::io_uring_queue_exit(self.ring.get()) };
  }
}

/// Create a new io_uring instance with the specified capacity
///
/// The capacity determines the size of the submission queue. The kernel
/// may adjust this value.
///
/// # Errors
/// Returns an error if io_uring initialization fails (e.g., insufficient
/// permissions, kernel doesn't support io_uring, or out of resources).
#[inline]
pub fn with_capacity(
  cap: u32,
) -> Result<(SubmissionQueue, CompletionQueue), io_Error> {
  with_params(IoUringParams { sq_entries: cap, ..Default::default() })
}

/// Create a new io_uring instance with custom parameters
///
/// # Errors
/// Returns an error if io_uring initialization fails.
pub fn with_params(
  params: IoUringParams,
) -> Result<(SubmissionQueue, CompletionQueue), io_Error> {
  let mut ring = MaybeUninit::zeroed();
  let mut raw_params = bindings::io_uring_params {
    sq_entries: params.sq_entries,
    cq_entries: 0,
    flags: params.flags,
    sq_thread_cpu: params.sq_thread_cpu,
    sq_thread_idle: params.sq_thread_idle,
    features: 0,
    wq_fd: 0,
    resv: [0; 3],
    sq_off: unsafe { std::mem::zeroed() },
    cq_off: unsafe { std::mem::zeroed() },
  };

  let ret = unsafe {
    bindings::io_uring_queue_init_params(
      params.sq_entries,
      ring.as_mut_ptr(),
      &raw mut raw_params,
    )
  };

  if ret < 0 {
    return Err(io_Error::from_raw_os_error(-ret));
  }

  let ring_init = unsafe { ring.assume_init() };
  let flags = ring_init.flags;

  let this = IoUring { ring: UnsafeCell::new(ring_init), flags };
  Ok(this.split())
}

impl IoUring {
  fn split(self) -> (SubmissionQueue, CompletionQueue) {
    let uring = Arc::new(self);
    (SubmissionQueue::new(uring.clone()), CompletionQueue::new(uring))
  }
}
