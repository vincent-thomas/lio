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
//! ```rust,ignore
//! use lio_uring::{LioUring, operation::Nop};
//!
//! // Create an io_uring instance with 128 entries
//! let mut ring = LioUring::new(128)?;
//!
//! // Submit a no-op operation
//! let op = Nop::new().build();
//! unsafe { ring.push(op, 1) }?;
//! ring.submit()?;
//!
//! // Wait for completion
//! let completion = ring.wait()?;
//! assert_eq!(completion.user_data(), 1);
//! assert!(completion.is_ok());
//! ```
//!
//! ## Safety
//!
//! Most operations in io_uring involve raw pointers to user data. This crate marks
//! the `push()` method as `unsafe` because the caller must ensure:
//! - All pointers in operations remain valid until the operation completes
//! - Buffers are not accessed mutably while operations are in flight
//! - File descriptors remain valid until operations complete

extern crate alloc;
extern crate core;

use core::mem::MaybeUninit;
use core::ptr;
use std::io::{self, IoSlice};
use std::time::Duration;

pub mod operation;

mod bindings;

/// A completed operation with result and metadata
#[derive(Debug, Clone, Copy)]
pub struct Completion {
  /// User data that was associated with the submission
  user_data: u64,
  /// Operation result (number of bytes transferred, or negative errno)
  res: i32,
  /// Completion flags providing additional context
  pub flags: u32,
}

impl Completion {
  /// Check if the operation succeeded
  pub fn is_ok(&self) -> bool {
    self.res >= 0
  }

  /// Get the result value
  pub fn result(&self) -> i32 {
    self.res
  }

  /// Get the user data
  pub fn user_data(&self) -> u64 {
    self.user_data
  }

  /// Check if more data is available (for multishot operations)
  pub fn has_more(&self) -> bool {
    (self.flags & bindings::IORING_CQE_F_MORE) != 0
  }

  /// Get the buffer ID (for operations using buffer selection)
  pub fn buffer_id(&self) -> Option<u16> {
    if (self.flags & bindings::IORING_CQE_F_BUFFER) != 0 {
      Some((self.flags >> bindings::IORING_CQE_BUFFER_SHIFT) as u16)
    } else {
      None
    }
  }
}

/// Submission Queue Entry flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqeFlags(u8);

impl SqeFlags {
  /// No flags set
  pub const NONE: Self = Self(0);

  /// Use fixed file descriptor (from registered files)
  pub const FIXED_FILE: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_FIXED_FILE_BIT);

  /// Issue operation asynchronously
  pub const ASYNC: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_ASYNC_BIT);

  /// Link next SQE - next operation won't start until this one completes
  pub const IO_LINK: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_IO_LINK_BIT);

  /// Execute after previous operations complete (ordering barrier)
  pub const IO_DRAIN: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_IO_DRAIN_BIT);

  /// Form a hard link - if any linked operation fails, cancel remaining links
  pub const IO_HARDLINK: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_IO_HARDLINK_BIT);

  /// Use registered buffer (from registered buffers)
  pub const BUFFER_SELECT: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_BUFFER_SELECT_BIT);

  /// Don't generate completion event unless operation fails
  pub const CQE_SKIP_SUCCESS: Self =
    Self(1 << bindings::io_uring_sqe_flags_bit_IOSQE_CQE_SKIP_SUCCESS_BIT);

  /// Combine flags using bitwise OR
  pub const fn or(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }

  /// Check if a flag is set
  pub const fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }

  pub fn bits(self) -> u8 {
    self.0
  }
}

impl std::ops::BitOr for SqeFlags {
  type Output = Self;
  fn bitor(self, rhs: Self) -> Self::Output {
    self.or(rhs)
  }
}

/// A submission queue entry ready to be pushed to the ring.
pub struct Entry(pub(crate) bindings::io_uring_sqe);

impl Entry {
  pub(crate) fn from_sqe(sqe: bindings::io_uring_sqe) -> Self {
    Self(sqe)
  }

  pub(crate) fn into_sqe(self) -> bindings::io_uring_sqe {
    self.0
  }
}

/// Configuration parameters for io_uring initialization
#[derive(Debug, Clone)]
pub struct Params {
  /// Number of entries in the submission queue
  pub sq_entries: u32,
  /// Additional flags for io_uring setup
  pub flags: u32,
  /// CPU affinity for SQPOLL thread
  pub sq_thread_cpu: u32,
  /// Idle time in milliseconds before SQPOLL thread sleeps
  pub sq_thread_idle: u32,
}

impl Default for Params {
  fn default() -> Self {
    Self { sq_entries: 128, flags: 0, sq_thread_cpu: 0, sq_thread_idle: 0 }
  }
}

impl Params {
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
}

/// A Linux io_uring instance for high-performance async I/O.
///
/// This struct provides access to both submission and completion operations
/// on a single io_uring instance.
pub struct LioUring {
  ring: bindings::io_uring,
  flags: u32,
}

impl Drop for LioUring {
  fn drop(&mut self) {
    unsafe { bindings::io_uring_queue_exit(&raw mut self.ring) };
  }
}

impl LioUring {
  /// Create a new io_uring instance with the specified capacity.
  ///
  /// The capacity determines the size of the submission queue. The kernel
  /// may adjust this value.
  ///
  /// # Errors
  /// Returns an error if io_uring initialization fails (e.g., insufficient
  /// permissions, kernel doesn't support io_uring, or out of resources).
  pub fn new(capacity: u32) -> io::Result<Self> {
    Self::with_params(Params { sq_entries: capacity, ..Default::default() })
  }

  /// Create a new io_uring instance with custom parameters.
  ///
  /// # Errors
  /// Returns an error if io_uring initialization fails.
  pub fn with_params(params: Params) -> io::Result<Self> {
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
      return Err(io::Error::from_raw_os_error(-ret));
    }

    let ring_init = unsafe { ring.assume_init() };
    let flags = ring_init.flags;

    Ok(Self { ring: ring_init, flags })
  }

  // ==================== Submission methods ====================

  /// Push an operation to the submission queue.
  ///
  /// # Safety
  /// Caller guarantees that the Entry has valid data and that any
  /// pointers within the operation point to valid data that will remain
  /// valid until the operation completes.
  ///
  /// # Errors
  /// Returns an error if the submission queue is full. Call `submit()` to
  /// drain the queue and try again.
  pub unsafe fn push(
    &mut self,
    entry: Entry,
    user_data: u64,
  ) -> io::Result<()> {
    unsafe { self.push_with_flags(entry, user_data, SqeFlags::NONE) }
  }

  /// Push an operation to the submission queue with custom flags.
  ///
  /// # Safety
  /// Same requirements as `push()`.
  ///
  /// # Errors
  /// Returns an error if the submission queue is full.
  pub unsafe fn push_with_flags(
    &mut self,
    entry: Entry,
    user_data: u64,
    flags: SqeFlags,
  ) -> io::Result<()> {
    let sqe = unsafe { bindings::io_uring_get_sqe(&raw mut self.ring) };
    if sqe.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "submission queue is full",
      ));
    }

    unsafe {
      (*sqe) = entry.into_sqe();
      (*sqe).user_data = user_data;
      (*sqe).flags = flags.bits()
    }

    Ok(())
  }

  /// Submit queued operations to the kernel.
  ///
  /// When SQPOLL is enabled, this avoids the syscall if the kernel thread
  /// is already running. Only enters the kernel if needed to wake the thread.
  ///
  /// Returns the number of operations submitted.
  ///
  /// # Errors
  /// Returns an error if submission fails.
  pub fn submit(&mut self) -> io::Result<usize> {
    if !self.is_sqpoll() {
      let ret =
        unsafe { bindings::io_uring_submit_and_wait(&raw mut self.ring, 0) };
      if ret < 0 {
        return Err(io::Error::from_raw_os_error(-ret));
      }
      return Ok(ret as usize);
    }

    // SQPOLL path: update ktail to make entries visible to kernel
    let pending = unsafe {
      let sq_tail = self.ring.sq.sqe_tail;
      let sq_head = self.ring.sq.sqe_head;

      if sq_head != sq_tail {
        self.ring.sq.sqe_head = sq_tail;
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        std::ptr::write_volatile(self.ring.sq.ktail, sq_tail);
      }

      sq_tail.wrapping_sub(*self.ring.sq.khead)
    };

    if pending == 0 {
      return Ok(0);
    }

    let needs_wakeup =
      unsafe { *self.ring.sq.kflags & bindings::IORING_SQ_NEED_WAKEUP != 0 };

    if needs_wakeup {
      let ret = unsafe { bindings::io_uring_submit(&raw mut self.ring) };
      if ret < 0 {
        return Err(io::Error::from_raw_os_error(-ret));
      }
      Ok(ret as usize)
    } else {
      Ok(pending as usize)
    }
  }

  /// Check if SQPOLL mode is enabled.
  pub fn is_sqpoll(&self) -> bool {
    (self.flags & bindings::IORING_SETUP_SQPOLL) != 0
  }

  /// Get the number of free slots in the submission queue.
  pub fn sq_space_left(&self) -> usize {
    unsafe {
      bindings::io_uring_sq_space_left(&self.ring as *const _ as *mut _)
        as usize
    }
  }

  // ==================== Completion methods ====================

  /// Wait for and retrieve the next completion.
  ///
  /// This blocks until at least one completion is available.
  ///
  /// # Errors
  /// Returns an error if waiting fails.
  pub fn wait(&mut self) -> io::Result<Completion> {
    let mut cqe_ptr = ptr::null_mut();
    let ret = unsafe {
      bindings::io_uring_wait_cqe(&raw mut self.ring, &raw mut cqe_ptr)
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }

    let cqe = unsafe { &*cqe_ptr };
    let completion =
      Completion { user_data: cqe.user_data, res: cqe.res, flags: cqe.flags };

    unsafe { bindings::io_uring_cqe_seen(&raw mut self.ring, cqe_ptr) };

    Ok(completion)
  }

  /// Wait for and retrieve the next completion with a timeout.
  ///
  /// Returns `Ok(None)` if the timeout expires with no completions.
  ///
  /// # Errors
  /// Returns an error if waiting fails (other than timeout).
  pub fn wait_timeout(
    &mut self,
    timeout: Duration,
  ) -> io::Result<Option<Completion>> {
    let mut cqe_ptr = ptr::null_mut();
    let mut ts = bindings::__kernel_timespec {
      tv_sec: timeout.as_secs() as i64,
      tv_nsec: timeout.subsec_nanos() as i64,
    };

    let ret = unsafe {
      bindings::io_uring_wait_cqe_timeout(
        &raw mut self.ring,
        &raw mut cqe_ptr,
        &raw mut ts,
      )
    };

    if ret < 0 {
      let errno = -ret;
      if errno == libc::ETIME {
        return Ok(None);
      }
      return Err(io::Error::from_raw_os_error(errno));
    }

    let cqe = unsafe { &*cqe_ptr };
    let completion =
      Completion { user_data: cqe.user_data, res: cqe.res, flags: cqe.flags };

    unsafe { bindings::io_uring_cqe_seen(&raw mut self.ring, cqe_ptr) };

    Ok(Some(completion))
  }

  /// Try to retrieve the next completion without blocking.
  ///
  /// Returns `None` if no completions are available.
  ///
  /// # Errors
  /// Returns an error if peeking fails (not including "no data available").
  pub fn try_wait(&mut self) -> io::Result<Option<Completion>> {
    let mut cqe_ptr = ptr::null_mut();
    let ret = unsafe {
      bindings::io_uring_peek_cqe(&raw mut self.ring, &raw mut cqe_ptr)
    };

    if ret < 0 {
      if -ret == libc::EAGAIN {
        return Ok(None);
      }
      return Err(io::Error::from_raw_os_error(-ret));
    }

    if cqe_ptr.is_null() {
      return Ok(None);
    }

    let cqe = unsafe { &*cqe_ptr };
    let completion =
      Completion { user_data: cqe.user_data, res: cqe.res, flags: cqe.flags };

    unsafe { bindings::io_uring_cqe_seen(&raw mut self.ring, cqe_ptr) };

    Ok(Some(completion))
  }

  /// Peek at the next completion without removing it from the queue.
  ///
  /// Returns `None` if no completions are available.
  pub fn peek(&self) -> io::Result<Option<Completion>> {
    let mut cqe_ptr = ptr::null_mut();
    let ret = unsafe {
      bindings::io_uring_peek_cqe(
        &self.ring as *const _ as *mut _,
        &raw mut cqe_ptr,
      )
    };

    if ret < 0 {
      if -ret == libc::EAGAIN {
        return Ok(None);
      }
      return Err(io::Error::from_raw_os_error(-ret));
    }

    if cqe_ptr.is_null() {
      return Ok(None);
    }

    let cqe = unsafe { &*cqe_ptr };
    Ok(Some(Completion {
      user_data: cqe.user_data,
      res: cqe.res,
      flags: cqe.flags,
    }))
  }

  /// Get the number of available completions ready to be consumed.
  pub fn cq_ready(&self) -> usize {
    unsafe {
      bindings::io_uring_cq_ready(&self.ring as *const _ as *mut _) as usize
    }
  }

  // ==================== Registration methods ====================

  /// Register fixed buffers for zero-copy I/O.
  ///
  /// Pre-registers buffers with the kernel to enable zero-copy operations.
  /// Registered buffers can be used with `ReadFixed` and `WriteFixed` operations.
  ///
  /// # Safety
  /// The buffers must remain valid and not be modified or moved until they are
  /// unregistered or the io_uring instance is dropped.
  ///
  /// # Errors
  /// Returns an error if registration fails (e.g., insufficient resources).
  pub unsafe fn register_buffers(
    &mut self,
    buffers: &[IoSlice<'_>],
  ) -> io::Result<()> {
    let iovecs: Vec<libc::iovec> = buffers
      .iter()
      .map(|buf| libc::iovec {
        iov_base: buf.as_ptr() as *mut _,
        iov_len: buf.len(),
      })
      .collect();

    let ret = unsafe {
      bindings::io_uring_register_buffers(
        &raw mut self.ring,
        iovecs.as_ptr().cast(),
        iovecs.len() as u32,
      )
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Unregister previously registered buffers.
  pub fn unregister_buffers(&mut self) -> io::Result<()> {
    let ret =
      unsafe { bindings::io_uring_unregister_buffers(&raw mut self.ring) };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Register fixed file descriptors.
  ///
  /// Pre-registers file descriptors with the kernel. Registered files can be
  /// referenced by index instead of fd, avoiding fd lookup overhead.
  ///
  /// # Errors
  /// Returns an error if registration fails.
  pub fn register_files(&mut self, fds: &[i32]) -> io::Result<()> {
    let ret = unsafe {
      bindings::io_uring_register_files(
        &raw mut self.ring,
        fds.as_ptr(),
        fds.len() as u32,
      )
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Update registered files at specific indices.
  ///
  /// Replace file descriptors at the given indices. Use -1 to remove a file.
  pub fn register_files_update(
    &mut self,
    offset: u32,
    fds: &[i32],
  ) -> io::Result<()> {
    let ret = unsafe {
      bindings::io_uring_register_files_update(
        &raw mut self.ring,
        offset,
        fds.as_ptr(),
        fds.len() as u32,
      )
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Unregister all previously registered files.
  pub fn unregister_files(&mut self) -> io::Result<()> {
    let ret =
      unsafe { bindings::io_uring_unregister_files(&raw mut self.ring) };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }
}
