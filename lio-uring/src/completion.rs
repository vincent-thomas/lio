use alloc::sync::Arc;
use core::{cell::Cell, marker::PhantomData, ptr};
use std::io;

use crate::{IoUring, bindings};

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

  /// Get the result as an io::Result
  pub fn result(&self) -> i32 {
    self.res
  }

  /// Get the result as an io::Result
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

/// Completion queue handle - used to retrieve completed operations
///
/// # Safety
/// This struct only touches the completion side of the queue.
/// It must not be used concurrently with other UringCompletion instances.
pub struct CompletionQueue {
  uring: Arc<IoUring>,

  _non_sync: PhantomData<Cell<()>>,
}

impl CompletionQueue {
  pub(crate) fn new(uring: Arc<IoUring>) -> Self {
    Self { uring, _non_sync: PhantomData }
  }
}

impl CompletionQueue {
  /// Wait for and retrieve the next completion
  ///
  /// This blocks until at least one completion is available.
  ///
  /// # Errors
  /// Returns an error if waiting fails.
  pub fn next(&mut self) -> io::Result<Completion> {
    let mut cqe_ptr = ptr::null_mut();
    let ret = unsafe {
      bindings::io_uring_wait_cqe(self.uring.ring.get(), &raw mut cqe_ptr)
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }

    let cqe = unsafe { &*cqe_ptr };
    let completion =
      Completion { user_data: cqe.user_data, res: cqe.res, flags: cqe.flags };

    unsafe { bindings::io_uring_cqe_seen(self.uring.ring.get(), cqe_ptr) };

    Ok(completion)
  }

  /// Try to retrieve the next completion without blocking
  ///
  /// Returns `None` if no completions are available.
  ///
  /// # Errors
  /// Returns an error if peeking fails (not including "no data available").
  pub fn try_next(&mut self) -> io::Result<Option<Completion>> {
    let mut cqe_ptr = ptr::null_mut();
    let ret = unsafe {
      bindings::io_uring_peek_cqe(self.uring.ring.get(), &raw mut cqe_ptr)
    };

    if ret < 0 {
      // EAGAIN means no completions available
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

    unsafe { bindings::io_uring_cqe_seen(self.uring.ring.get(), cqe_ptr) };

    Ok(Some(completion))
  }

  /// Peek at the next completion without removing it from the queue
  ///
  /// Returns `None` if no completions are available.
  pub fn peek(&self) -> io::Result<Option<Completion>> {
    let mut cqe_ptr = ptr::null_mut();
    let ret = unsafe {
      bindings::io_uring_peek_cqe(self.uring.ring.get(), &raw mut cqe_ptr)
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

  /// Get the number of available completions ready to be consumed
  pub fn available(&self) -> usize {
    unsafe { bindings::io_uring_cq_ready(self.uring.ring.get()) as usize }
  }
}
