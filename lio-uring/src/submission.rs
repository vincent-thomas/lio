use alloc::sync::Arc;
use core::{cell::Cell, marker::PhantomData};
use std::io;

use crate::{IoUring, bindings, completion::SqeFlags};

pub struct Entry(pub(crate) bindings::io_uring_sqe);

impl Entry {
  pub(crate) fn from_sqe(sqe: bindings::io_uring_sqe) -> Self {
    Self(sqe)
  }

  pub(crate) fn into_sqe(self) -> bindings::io_uring_sqe {
    self.0
  }
}

/// Submission queue handle - used to submit new operations
///
/// # Safety
/// This struct only touches the submission side of the queue.
/// It must not be used concurrently with other UringSubmission instances.
pub struct SubmissionQueue {
  uring: Arc<IoUring>,

  _non_sync: PhantomData<Cell<()>>,
}

impl SubmissionQueue {
  pub(crate) fn new(uring: Arc<IoUring>) -> Self {
    Self { uring, _non_sync: PhantomData }
  }
}

impl SubmissionQueue {
  /// Register fixed buffers for zero-copy I/O
  ///
  /// Pre-registers buffers with the kernel to enable zero-copy operations.
  /// Registered buffers can be used with `ReadFixed` and `WriteFixed` operations
  /// for improved performance.
  ///
  /// # Safety
  /// The buffers must remain valid and not be modified or moved until they are
  /// unregistered or the io_uring instance is dropped.
  ///
  /// # Errors
  /// Returns an error if registration fails (e.g., insufficient resources).
  pub unsafe fn register_buffers(
    &mut self,
    buffers: &[io::IoSlice<'_>],
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
        self.uring.ring.get(),
        iovecs.as_ptr().cast(),
        iovecs.len() as u32,
      )
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Unregister previously registered buffers
  pub fn unregister_buffers(&mut self) -> io::Result<()> {
    let ret =
      unsafe { bindings::io_uring_unregister_buffers(self.uring.ring.get()) };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Register fixed file descriptors
  ///
  /// Pre-registers file descriptors with the kernel. Registered files can be
  /// referenced by index instead of fd, avoiding fd lookup overhead.
  ///
  /// # Errors
  /// Returns an error if registration fails.
  pub fn register_files(&mut self, fds: &[i32]) -> io::Result<()> {
    let ret = unsafe {
      bindings::io_uring_register_files(
        self.uring.ring.get(),
        fds.as_ptr(),
        fds.len() as u32,
      )
    };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Update registered files at specific indices
  ///
  /// Replace file descriptors at the given indices. Use -1 to remove a file.
  pub fn register_files_update(
    &mut self,
    offset: u32,
    fds: &[i32],
  ) -> io::Result<()> {
    let ret = unsafe {
      bindings::io_uring_register_files_update(
        self.uring.ring.get(),
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

  /// Unregister all previously registered files
  pub fn unregister_files(&mut self) -> io::Result<()> {
    let ret =
      unsafe { bindings::io_uring_unregister_files(self.uring.ring.get()) };

    if ret < 0 {
      return Err(io::Error::from_raw_os_error(-ret));
    }
    Ok(())
  }

  /// Push an operation to the submission queue
  ///
  /// # Safety
  /// Same safety requirements as `push_with_flags()`.
  ///
  /// # Errors
  /// Returns an error if the submission queue is full. Call `submit()` to
  /// drain the queue and try again.
  pub unsafe fn push(
    &mut self,
    operation: Entry,
    user_data: u64,
  ) -> io::Result<()> {
    unsafe { self.push_with_flags(operation, user_data, SqeFlags::NONE) }
  }

  /// Push an operation to the submission queue with custom flags
  ///
  /// # Safety
  /// Caller guarantees that UringOperation has valid data and that any
  /// pointers within the operation point to valid data that will remain
  /// valid until the operation completes.
  ///
  /// # Errors
  /// Returns an error if the submission queue is full.
  pub unsafe fn push_with_flags(
    &mut self,
    operation: Entry,
    user_data: u64,
    flags: SqeFlags,
  ) -> io::Result<()> {
    let sqe = unsafe { bindings::io_uring_get_sqe(self.uring.ring.get()) };
    if sqe.is_null() {
      return Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "submission queue is full",
      ));
    }

    unsafe {
      (*sqe) = operation.into_sqe();
      (*sqe).user_data = user_data;
      (*sqe).flags = flags.bits()
    }

    Ok(())
  }

  /// Check if SQPOLL mode is enabled
  pub fn is_sqpoll(&self) -> bool {
    (self.uring.flags & bindings::IORING_SETUP_SQPOLL) != 0
  }

  /// Submit operations with SQPOLL optimization
  ///
  /// When SQPOLL is enabled, this avoids the syscall if the kernel thread
  /// is already running. Only enters the kernel if needed to wake the thread.
  ///
  /// Returns the number of operations that were made visible to the kernel.
  ///
  /// # Errors
  /// Returns an error if submission fails.
  pub fn submit(&mut self) -> io::Result<usize> {
    if !self.is_sqpoll() {
      // Fall back to normal submit if SQPOLL is not enabled
      let ret = unsafe {
        // wait_nr cannot be anything else than 0.
        bindings::io_uring_submit_and_wait(self.uring.ring.get(), 0)
      };
      if ret < 0 {
        return Err(io::Error::from_raw_os_error(-ret));
      }
      return Ok(ret as usize);
    }

    // Update ktail to make entries visible to kernel (flush the SQ)
    let pending = unsafe {
      let ring = self.uring.ring.get();
      let sq_tail = (*ring).sq.sqe_tail;
      let sq_head = (*ring).sq.sqe_head;

      if sq_head != sq_tail {
        // Update our local head
        (*ring).sq.sqe_head = sq_tail;

        // Use atomic store with release ordering for SQPOLL
        // This makes the SQEs visible to the kernel thread
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        std::ptr::write_volatile((*ring).sq.ktail, sq_tail);
      }

      // Return number of pending entries
      sq_tail.wrapping_sub(*(*ring).sq.khead)
    };

    if pending == 0 {
      return Ok(0);
    }

    // Check if SQPOLL thread needs waking
    let needs_wakeup = unsafe {
      let ring = self.uring.ring.get();
      *(*ring).sq.kflags & bindings::IORING_SQ_NEED_WAKEUP != 0
    };

    if needs_wakeup {
      // SQPOLL thread is sleeping, need to wake it with a syscall
      let ret = unsafe { bindings::io_uring_submit(self.uring.ring.get()) };
      if ret < 0 {
        return Err(io::Error::from_raw_os_error(-ret));
      }
      Ok(ret as usize)
    } else {
      // SQPOLL thread is active, no syscall needed!
      Ok(pending as usize)
    }
  }

  /// Get the number of free slots in the submission queue
  pub fn capacity(&self) -> usize {
    unsafe { bindings::io_uring_sq_space_left(self.uring.ring.get()) as usize }
  }
}
