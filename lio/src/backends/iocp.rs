use super::{IoBackend, OpStore};
use crate::op_registration::OpNotification;
use crate::{OperationProgress, op};
use std::io;
use std::ptr;
use std::sync::Arc;
use std::thread;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{
  CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE as INVALID_HANDLE,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::{
  CreateIoCompletionPort, GetQueuedCompletionStatus, INVALID_HANDLE_VALUE,
  PostQueuedCompletionStatus,
};

/// Completion packet data posted to IOCP
#[repr(C)]
struct CompletionData {
  operation_id: u64,
  result: i32,
  error_code: u32,
}

/// Windows I/O Completion Ports backend.
///
/// IOCP is the native asynchronous I/O mechanism on Windows, providing
/// high-performance I/O multiplexing through kernel-managed completion ports.
///
/// Currently uses a thread-pool based implementation where operations run
/// in worker threads and post results to the completion port.
pub struct Iocp {
  /// Handle to the I/O completion port
  completion_port: HANDLE,
}

impl Iocp {
  /// Creates a new IOCP backend.
  ///
  /// # Panics
  ///
  /// Panics if the completion port cannot be created.
  pub fn new() -> Self {
    let completion_port = unsafe {
      // Create a completion port not associated with any file handle yet
      CreateIoCompletionPort(
        INVALID_HANDLE_VALUE,
        0, // No existing port to associate with
        0, // Completion key (unused for initial creation)
        0, // Number of concurrent threads (0 = number of processors)
      )
    };

    if completion_port == 0 {
      panic!("Failed to create I/O completion port");
    }

    Self { completion_port }
  }

  /// Associates a file handle with the completion port.
  ///
  /// This must be called for each file handle before performing I/O operations on it.
  pub fn associate_handle(
    &self,
    handle: HANDLE,
    completion_key: usize,
  ) -> io::Result<()> {
    let result = unsafe {
      CreateIoCompletionPort(handle, self.completion_port, completion_key, 0)
    };

    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
  }
}

impl Drop for Iocp {
  fn drop(&mut self) {
    if self.completion_port != 0 {
      unsafe {
        CloseHandle(self.completion_port);
      }
    }
  }
}

impl IoBackend for Iocp {
  fn submit<T>(&self, op: T, store: &OpStore) -> OperationProgress<T>
  where
    T: op::Operation,
  {
    let operation_id = store.next_id();
    let mut op = Box::new(op);

    // Get the result by running the operation in blocking mode
    // This will be executed in a thread pool
    let result = op.run_blocking();

    // Store the operation before spawning the thread
    store.insert(operation_id, op);

    let completion_port = self.completion_port;

    // Spawn a thread to post the completion
    thread::spawn(move || {
      let (bytes_transferred, error_code) = match result {
        Ok(bytes) => (bytes as u32, 0u32),
        Err(e) => (0u32, e.raw_os_error().unwrap_or(0) as u32),
      };

      // Post completion to the IOCP
      unsafe {
        PostQueuedCompletionStatus(
          completion_port,
          bytes_transferred,
          operation_id as usize,
          ptr::null_mut(),
        );
      }
    });

    OperationProgress::<T>::new_store_tracked(operation_id)
  }

  fn notify(&self) {
    // Post a completion packet to wake up any waiting tick() call
    unsafe {
      PostQueuedCompletionStatus(
        self.completion_port,
        0,               // Number of bytes transferred
        0,               // Completion key
        ptr::null_mut(), // OVERLAPPED pointer
      );
    }
  }

  fn tick(&self, store: &OpStore, can_wait: bool) {
    let timeout = if can_wait { u32::MAX } else { 0 };

    let mut bytes_transferred = 0u32;
    let mut completion_key = 0usize;
    let mut overlapped_ptr = ptr::null_mut();

    let result = unsafe {
      GetQueuedCompletionStatus(
        self.completion_port,
        &mut bytes_transferred,
        &mut completion_key,
        &mut overlapped_ptr,
        timeout,
      )
    };

    // If GetQueuedCompletionStatus returned false and overlapped is null,
    // it means timeout with no completions
    if result == 0 && overlapped_ptr.is_null() {
      return;
    }

    // completion_key contains our operation_id
    let operation_id = completion_key as u64;

    // Skip notification packets (operation_id == 0)
    if operation_id == 0 {
      return;
    }

    // Retrieve and remove the operation from the store
    if let Some(mut entry) = store.get_mut(operation_id) {
      // Convert bytes_transferred to i32 result
      let io_result = if result != 0 {
        Ok(bytes_transferred as i32)
      } else {
        // Operation failed, get the error code
        let error_code =
          unsafe { windows_sys::Win32::Foundation::GetLastError() };
        Err(io::Error::from_raw_os_error(error_code as i32))
      };

      // Mark the operation as done and get the notification
      if let Some(notification) = entry.set_done(io_result) {
        // Handle the notification (wake or callback)
        match notification {
          OpNotification::Wake(waker) => waker.wake(),
          OpNotification::Callback(callback) => callback(),
        }
      }
    }
  }
}
