//! Operation data enum for zero-box I/O operations.
//!
//! This module defines the [`Op`] enum which represents all I/O operations
//! as pure data. Backends match on this enum to execute operations.

use std::ffi::c_char;
use std::ptr::NonNull;
use std::time::Duration;

use crate::api::resource::Resource;

// ═══════════════════════════════════════════════════════════════════════════════
// ErasedBuffer - Type-erased buffer storage
// ═══════════════════════════════════════════════════════════════════════════════

/// Type-erased buffer storage with proper cleanup.
///
/// This allows storing buffers of any type without boxing the entire operation.
/// The buffer is boxed once, but we avoid the `Box<dyn Operation>` indirection.
pub struct ErasedBuffer {
  ptr: Option<NonNull<()>>,
  drop_fn: unsafe fn(*mut ()),
}

impl ErasedBuffer {
  /// Creates a new erased buffer from a value.
  pub fn new<T: Send + 'static>(value: T) -> Self {
    let boxed = Box::new(value);
    Self {
      ptr: Some(NonNull::new(Box::into_raw(boxed).cast()).unwrap()),
      drop_fn: Self::drop_erased::<T>,
    }
  }

  /// Used in [Self::new].
  unsafe fn drop_erased<T>(ptr: *mut ()) {
    drop(unsafe { Box::from_raw(ptr as *mut T) });
  }

  /// Takes the buffer out, consuming self.
  ///
  /// # Safety
  /// Caller must ensure T matches the original type, if not this is **just as unsafe as
  /// std::mem::transmute**.
  pub unsafe fn take<T>(mut self) -> T {
    let ptr = self.ptr.take().unwrap().as_ptr().cast();

    // SAFETY: Caller guarantees type matches
    *unsafe { Box::from_raw(ptr) }
  }
}

impl Drop for ErasedBuffer {
  fn drop(&mut self) {
    if let Some(ptr) = self.ptr.take() {
      // SAFETY: drop_fn matches the original type
      unsafe { (self.drop_fn)(ptr.as_ptr()) }
    };
  }
}

// SAFETY: The buffer contents are Send (enforced by new())
unsafe impl Send for ErasedBuffer {}
// SAFETY: We only access through &mut self or by consuming
unsafe impl Sync for ErasedBuffer {}

/// All I/O operations as pure data.
///
/// Each variant contains only the data needed to execute the operation.
/// Backends match on this enum to create submission entries or execute syscalls.
pub enum Op {
  // ═══════════════════════════════════════════════════════════════════════════════
  // Buffer operations - buffer field owns the data, ptr/len point into it
  // ═══════════════════════════════════════════════════════════════════════════════
  Read {
    fd: Resource,
    buffer: ErasedBuffer,
  },
  Write {
    fd: Resource,
    buffer: ErasedBuffer,
  },
  ReadAt {
    fd: Resource,
    offset: i64,
    buffer: ErasedBuffer,
  },
  WriteAt {
    fd: Resource,
    offset: i64,
    buffer: ErasedBuffer,
  },
  Send {
    fd: Resource,
    flags: i32,
    buffer: ErasedBuffer,
  },
  Recv {
    fd: Resource,
    flags: i32,
    buffer: ErasedBuffer,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Socket operations
  // ═══════════════════════════════════════════════════════════════════════════════
  Accept {
    fd: Resource,
    addr: *mut libc::sockaddr_storage,
    len: *mut libc::socklen_t,
  },
  Connect {
    fd: Resource,
    addr: libc::sockaddr_storage,
    len: libc::socklen_t,
    /// Tracks whether connect() has been called (for EISCONN handling)
    connect_called: bool,
  },
  Bind {
    fd: Resource,
    addr: libc::sockaddr_storage,
  },
  Listen {
    fd: Resource,
    backlog: i32,
  },
  Shutdown {
    fd: Resource,
    how: i32,
  },
  Socket {
    domain: i32,
    ty: i32,
    proto: i32,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // File operations
  // ═══════════════════════════════════════════════════════════════════════════════
  OpenAt {
    dir_fd: Resource,
    path: *const c_char,
    flags: i32,
  },
  Close {
    fd: Resource,
  },
  Fsync {
    fd: Resource,
  },
  Truncate {
    fd: Resource,
    size: u64,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Link operations
  // ═══════════════════════════════════════════════════════════════════════════════
  LinkAt {
    old_dir_fd: Resource,
    old_path: *const c_char,
    new_dir_fd: Resource,
    new_path: *const c_char,
  },
  SymlinkAt {
    dir_fd: Resource,
    target: *const c_char,
    linkpath: *const c_char,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Misc
  // ═══════════════════════════════════════════════════════════════════════════════
  #[cfg(target_os = "linux")]
  Tee {
    fd_in: Resource,
    fd_out: Resource,
    size: u32,
  },
  Timeout {
    duration: Duration,
    #[cfg(target_os = "linux")]
    timer_fd: Resource,
    #[cfg(target_os = "linux")]
    timespec: libc::timespec,
  },
  Nop,
}

// SAFETY: Op contains raw pointers but they point to data owned by ErasedBuffer
// which is stored alongside Op in StoredOp. The pointers are valid for the
// lifetime of the operation.
unsafe impl Send for Op {}
unsafe impl Sync for Op {}
