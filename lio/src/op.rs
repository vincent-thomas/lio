//! Operation data enum for zero-box I/O operations.
//!
//! This module defines the [`Op`] enum which represents all I/O operations
//! as pure data. Backends match on this enum to execute operations.

use std::ffi::c_char;
use std::ptr::NonNull;
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::RawFd;

use crate::api::resource::Resource;

// ═══════════════════════════════════════════════════════════════════════════════
// ErasedBuffer - Type-erased buffer storage
// ═══════════════════════════════════════════════════════════════════════════════

/// Non-owning raw buffer pointer for use in [`OpBuf`] by the polling backend.
///
/// The actual buffer is owned by the typed op (e.g., `Send<Vec<u8>>`). This struct
/// holds only a pointer + length so the backend can call the syscall without taking
/// ownership, leaving the buffer available for `extract_result`.
#[derive(Clone, Copy)]
pub struct RawBuf {
  pub ptr: *mut u8,
  pub len: usize,
}
// SAFETY: The pointed-to data is owned by a TypedOp which is Send + Sync.
// We only use this pointer from the thread that scheduled the operation.
unsafe impl Send for RawBuf {}
// SAFETY: Same as Send - we only access through the owning TypedOp.
unsafe impl Sync for RawBuf {}

/// Non-owning raw iovec array pointer for scatter-gather I/O operations.
///
/// Points to an array of `libc::iovec` structs owned by the typed op
/// (e.g., `Readv` or `Writev`). The backend uses this to call `readv`/`writev`
/// without taking ownership of the buffer array.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub struct RawIovecBuf {
  pub ptr: *const libc::iovec,
  pub len: u32,
}
// SAFETY: The pointed-to iovec array is owned by a TypedOp which is Send + Sync.
// We only use this pointer from the thread that scheduled the operation.
#[cfg(unix)]
unsafe impl Send for RawIovecBuf {}
// SAFETY: Same as Send - we only access through the owning TypedOp.
#[cfg(unix)]
unsafe impl Sync for RawIovecBuf {}

/// Type-erased buffer storage with proper cleanup.
///
/// This allows storing buffers of any type without boxing the entire operation.
/// The buffer is boxed once, but we avoid the `Box<dyn Operation>` indirection.
pub struct OpBuf {
  ptr: Option<NonNull<()>>,
  drop_fn: unsafe fn(*mut ()),
}

impl OpBuf {
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
    // SAFETY: ptr was created from Box::into_raw in new(), and T matches the original type.
    drop(unsafe { Box::from_raw(ptr as *mut T) });
  }

  /// Peeks at the contained value by copying it (without consuming/dropping).
  ///
  /// # Safety
  /// Caller must ensure T matches the original type and T: Copy.
  pub unsafe fn peek<T: Copy>(&self) -> T {
    let ptr = self
      .ptr
      .as_ref()
      .expect("ErasedBuffer already taken")
      .as_ptr()
      .cast::<T>();
    // SAFETY: Caller guarantees type matches and T is Copy
    unsafe { *ptr }
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

impl Drop for OpBuf {
  fn drop(&mut self) {
    if let Some(ptr) = self.ptr.take() {
      // SAFETY: drop_fn matches the original type
      unsafe { (self.drop_fn)(ptr.as_ptr()) }
    };
  }
}

// SAFETY: The buffer contents are Send (enforced by new())
unsafe impl Send for OpBuf {}
// SAFETY: We only access through &mut self or by consuming
unsafe impl Sync for OpBuf {}

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
    buffer: OpBuf,
  },
  Write {
    fd: Resource,
    buffer: OpBuf,
  },
  ReadAt {
    fd: Resource,
    offset: i64,
    buffer: OpBuf,
  },
  WriteAt {
    fd: Resource,
    offset: i64,
    buffer: OpBuf,
  },
  Send {
    fd: Resource,
    flags: i32,
    buffer: OpBuf,
  },
  Recv {
    fd: Resource,
    flags: i32,
    buffer: OpBuf,
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
    addr: *const libc::sockaddr_storage,
    len: libc::socklen_t,
    /// Tracks whether connect() has been called (for EISCONN handling)
    connect_called: bool,
  },
  Bind {
    fd: Resource,
    addr: *const libc::sockaddr_storage,
    addrlen: libc::socklen_t,
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
    /// Raw file descriptor - we do not hold a Resource here to avoid
    /// double-close (the op itself performs the close syscall).
    #[cfg(unix)]
    fd: RawFd,
    #[cfg(windows)]
    handle: std::os::windows::io::RawHandle,
    /// Whether this is a socket (use closesocket()) or a file handle (use CloseHandle())
    #[cfg(windows)]
    is_socket: bool,
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
  // Scatter-gather operations
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Scatter read: reads data from `fd` into multiple buffers (`readv(2)`).
  ///
  /// The `buffer` field contains a [`RawIovecBuf`] pointing to an array of
  /// `libc::iovec` structs owned by the typed op.
  #[cfg(unix)]
  Readv {
    fd: Resource,
    buffer: OpBuf,
  },
  /// Gather write: writes data from multiple buffers to `fd` (`writev(2)`).
  ///
  /// The `buffer` field contains a [`RawIovecBuf`] pointing to an array of
  /// `libc::iovec` structs owned by the typed op.
  #[cfg(unix)]
  Writev {
    fd: Resource,
    buffer: OpBuf,
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
    timespec: *const libc::timespec,
  },
  Nop,
}

// SAFETY: Op contains raw pointers but they point to data owned by ErasedBuffer
// which is stored alongside Op in StoredOp. The pointers are valid for the
// lifetime of the operation.
unsafe impl Send for Op {}
// SAFETY: Same as Send - pointers are valid for the operation's lifetime.
unsafe impl Sync for Op {}
