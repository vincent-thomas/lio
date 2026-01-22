//! Windows I/O Completion Ports (IOCP) backend for lio.
//!
//! This module provides the IOCP backend implementation, which is the native
//! high-performance async I/O mechanism on Windows.
//!
//! # Architecture
//!
//! IOCP is a completion-based model (like io_uring), not readiness-based (like epoll).
//! Operations are submitted with an OVERLAPPED structure and complete asynchronously.
//!
//! # Implementation Strategy
//!
//! 1. Operations implement `start_iocp()` to initiate overlapped I/O
//! 2. Handles are associated with the completion port on first use
//! 3. Completions are reaped via `GetQueuedCompletionStatus`
//! 4. Operations without IOCP support fall back to `run_blocking()` in a thread

use std::collections::{HashMap, HashSet};
use std::io;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
  CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, FALSE,
  ERROR_IO_PENDING, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::IO::{
  CreateIoCompletionPort, GetQueuedCompletionStatus, PostQueuedCompletionStatus,
  OVERLAPPED,
};

use crate::backends::{IoBackend, OpCompleted, OpStore};
use crate::operation::{IocpStartResult, Operation};

/// Marker value for wake-up notifications (not a real operation).
const NOTIFY_KEY: usize = usize::MAX;

/// Result stored for blocking fallback operations.
struct BlockingResult {
  op_id: u64,
  result: isize,
}

/// Windows I/O Completion Ports backend.
///
/// IOCP is the native asynchronous I/O mechanism on Windows, providing
/// high-performance I/O multiplexing through kernel-managed completion ports.
///
/// # Example
///
/// ```rust,ignore
/// let mut backend = Iocp::default();
/// backend.init(1024)?;
///
/// backend.push(id, &op)?;
/// backend.flush()?;
///
/// let completions = backend.wait(&mut store)?;
/// ```
#[derive(Default)]
pub struct Iocp {
  /// Handle to the I/O completion port
  port: Option<HANDLE>,

  /// Handles that have been associated with the completion port.
  /// We track these to avoid double-association.
  associated_handles: HashSet<isize>,

  /// Mapping from OVERLAPPED pointer address to operation ID.
  /// Used to identify which operation completed.
  overlapped_to_id: HashMap<usize, u64>,

  /// Results from blocking fallback operations.
  /// These are drained during poll/wait.
  blocking_results: Vec<BlockingResult>,

  /// Reusable buffer for completed operations (avoids allocation per poll/wait).
  completed: Vec<OpCompleted>,
}

impl Iocp {
  /// Creates a new uninitialized IOCP backend.
  ///
  /// Call [`init`](Self::init) before using.
  pub fn new() -> Self {
    Self::default()
  }

  /// Returns the completion port handle, panicking if not initialized.
  #[inline]
  fn port(&self) -> HANDLE {
    self.port.expect("Iocp not initialized - call init() first")
  }

  /// Associates a handle with the completion port if not already associated.
  fn ensure_associated(&mut self, handle: HANDLE) -> io::Result<()> {
    let handle_val = handle as isize;

    if self.associated_handles.contains(&handle_val) {
      return Ok(());
    }

    // Associate the handle with the completion port.
    // We use 0 as the completion key since we identify operations via OVERLAPPED pointer.
    let result = unsafe {
      CreateIoCompletionPort(
        handle,
        self.port(),
        0, // Completion key (we use OVERLAPPED pointer instead)
        0, // Number of concurrent threads (0 = system default)
      )
    };

    if result == 0 {
      return Err(io::Error::last_os_error());
    }

    self.associated_handles.insert(handle_val);
    Ok(())
  }

  /// Poll for completions with optional timeout.
  ///
  /// - `timeout = None`: Block indefinitely
  /// - `timeout = Some(Duration::ZERO)`: Non-blocking poll
  /// - `timeout = Some(duration)`: Wait up to duration
  fn poll_inner(
    &mut self,
    _store: &mut OpStore,
    timeout_duration: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();

    // First, drain any blocking fallback results
    for result in self.blocking_results.drain(..) {
      self.completed.push(OpCompleted::new(result.op_id, result.result));
    }

    // Calculate timeout for GetQueuedCompletionStatus:
    // - None: INFINITE (u32::MAX)
    // - Some(Duration::ZERO): 0 (non-blocking)
    // - Some(duration): duration in milliseconds
    let timeout = match timeout_duration {
      None => u32::MAX, // INFINITE
      Some(d) => d.as_millis().min(u32::MAX as u128 - 1) as u32,
    };

    // Whether we're in blocking mode (not a non-blocking poll)
    let is_blocking = timeout_duration != Some(Duration::ZERO);

    loop {
      let mut bytes_transferred: u32 = 0;
      let mut completion_key: usize = 0;
      let mut overlapped_ptr: *mut OVERLAPPED = ptr::null_mut();

      let success = unsafe {
        GetQueuedCompletionStatus(
          self.port(),
          &mut bytes_transferred,
          &mut completion_key,
          &mut overlapped_ptr,
          timeout,
        )
      };

      // Check for timeout or error with no completion
      if success == FALSE && overlapped_ptr.is_null() {
        let error = unsafe { GetLastError() };
        if error == WAIT_TIMEOUT {
          // No more completions available
          break;
        }
        // Real error
        return Err(io::Error::from_raw_os_error(error as i32));
      }

      // Skip wake-up notifications
      if completion_key == NOTIFY_KEY {
        // If we were waiting and got woken up, return what we have
        if is_blocking && !self.completed.is_empty() {
          break;
        }
        // Otherwise continue polling (non-blocking) or wait again
        continue;
      }

      // Look up the operation ID from the OVERLAPPED pointer
      let overlapped_addr = overlapped_ptr as usize;
      if let Some(op_id) = self.overlapped_to_id.remove(&overlapped_addr) {
        // Determine the result
        let result = if success != FALSE {
          // Success - bytes_transferred is the result
          bytes_transferred as isize
        } else {
          // Failure - get the error code
          let error = unsafe { GetLastError() };
          -(error as isize)
        };

        self.completed.push(OpCompleted::new(op_id, result));
      }

      // For non-blocking poll, only process one completion then check for more
      // For blocking wait, keep going until we have at least one completion
      if !is_blocking {
        // Switch to non-blocking for subsequent iterations
        break;
      } else if !self.completed.is_empty() {
        // We have at least one completion, try to drain more without blocking
        break;
      }
    }

    // If we're in wait mode and have completions, try to drain more without blocking
    if is_blocking && !self.completed.is_empty() {
      // Do a non-blocking poll to get any additional ready completions
      loop {
        let mut bytes_transferred: u32 = 0;
        let mut completion_key: usize = 0;
        let mut overlapped_ptr: *mut OVERLAPPED = ptr::null_mut();

        let success = unsafe {
          GetQueuedCompletionStatus(
            self.port(),
            &mut bytes_transferred,
            &mut completion_key,
            &mut overlapped_ptr,
            0, // Non-blocking
          )
        };

        if success == FALSE && overlapped_ptr.is_null() {
          // No more completions
          break;
        }

        if completion_key == NOTIFY_KEY {
          continue;
        }

        let overlapped_addr = overlapped_ptr as usize;
        if let Some(op_id) = self.overlapped_to_id.remove(&overlapped_addr) {
          let result = if success != FALSE {
            bytes_transferred as isize
          } else {
            let error = unsafe { GetLastError() };
            -(error as isize)
          };
          self.completed.push(OpCompleted::new(op_id, result));
        }
      }
    }

    Ok(&self.completed)
  }
}

impl Drop for Iocp {
  fn drop(&mut self) {
    if let Some(port) = self.port {
      unsafe {
        CloseHandle(port);
      }
    }
  }
}

impl IoBackend for Iocp {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    // Create the I/O completion port
    let port = unsafe {
      CreateIoCompletionPort(
        INVALID_HANDLE_VALUE,
        0,  // No existing port
        0,  // Completion key (unused for creation)
        0,  // Concurrent threads (0 = system decides)
      )
    };

    if port == 0 {
      return Err(io::Error::last_os_error());
    }

    self.port = Some(port);
    self.associated_handles = HashSet::with_capacity(cap);
    self.overlapped_to_id = HashMap::with_capacity(cap);
    self.blocking_results = Vec::with_capacity(64);
    self.completed = Vec::with_capacity(cap.min(256)); // Reusable completions buffer

    Ok(())
  }

  fn push(&mut self, id: u64, op: &dyn Operation) -> io::Result<()> {
    // Get the handle and associate it with the completion port if needed
    if let Some(handle) = op.handle() {
      self.ensure_associated(handle)?;
    }

    // Get the OVERLAPPED pointer for this operation
    let overlapped_ptr = op.overlapped_ptr();

    // Try to start the operation using IOCP
    match op.start_iocp(overlapped_ptr as *mut OVERLAPPED) {
      IocpStartResult::Pending => {
        // Operation started asynchronously
        // Store the mapping so we can identify it on completion
        self.overlapped_to_id.insert(overlapped_ptr as usize, id);
        Ok(())
      }

      IocpStartResult::Completed(result) => {
        // Operation completed synchronously
        // Post a manual completion so it goes through the normal path
        let success = unsafe {
          PostQueuedCompletionStatus(
            self.port(),
            result.max(0) as u32,
            0, // We'll use the overlapped pointer to identify
            overlapped_ptr as *mut OVERLAPPED,
          )
        };

        if success == FALSE {
          return Err(io::Error::last_os_error());
        }

        self.overlapped_to_id.insert(overlapped_ptr as usize, id);
        Ok(())
      }

      IocpStartResult::Unsupported => {
        // Fall back to blocking execution
        // TODO: In production, this should use a thread pool instead of
        // blocking the caller. For now, we run it synchronously.
        let result = op.run_blocking();
        self.blocking_results.push(BlockingResult { op_id: id, result });
        Ok(())
      }
    }
  }

  fn flush(&mut self) -> io::Result<usize> {
    // IOCP operations are submitted immediately in push(), unlike io_uring
    // which batches submissions. Return 0 to indicate no batched submissions.
    Ok(0)
  }

  fn wait_timeout(
    &mut self,
    store: &mut OpStore,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.poll_inner(store, timeout)
  }
}

/// Wake up a blocked wait() call.
///
/// This is useful for shutting down the event loop or notifying it
/// of new work from another thread.
impl Iocp {
  pub fn notify(&self) -> io::Result<()> {
    let success = unsafe {
      PostQueuedCompletionStatus(
        self.port(),
        0,
        NOTIFY_KEY,
        ptr::null_mut(),
      )
    };

    if success == FALSE {
      Err(io::Error::last_os_error())
    } else {
      Ok(())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_init() {
    let mut backend = Iocp::new();
    backend.init(64).unwrap();
  }

  #[test]
  fn test_notify() {
    let mut backend = Iocp::new();
    backend.init(64).unwrap();
    backend.notify().unwrap();
  }
}
