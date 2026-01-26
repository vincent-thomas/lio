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

// ═══════════════════════════════════════════════════════════════════════════════
// OpResult - Type-safe result extraction
// ═══════════════════════════════════════════════════════════════════════════════

use std::io;
use std::net::SocketAddr;

/// Type-safe result enum for I/O operations.
///
/// Each variant corresponds to an [`Op`] variant and contains the typed result
/// of that operation. This provides compile-time type safety for result extraction,
/// eliminating the need for unsafe pointer casting.
pub enum OpResult {
  /// Result of a Read operation: (io::Result<bytes_read>, buffer)
  Read(io::Result<i32>, ErasedBuffer),

  /// Result of a Write operation: (io::Result<bytes_written>, buffer)
  Write(io::Result<i32>, ErasedBuffer),

  /// Result of a ReadAt operation: (io::Result<bytes_read>, buffer)
  ReadAt(io::Result<i32>, ErasedBuffer),

  /// Result of a WriteAt operation: (io::Result<bytes_written>, buffer)
  WriteAt(io::Result<i32>, ErasedBuffer),

  /// Result of a Send operation: (io::Result<bytes_sent>, buffer)
  Send(io::Result<i32>, ErasedBuffer),

  /// Result of a Recv operation: (io::Result<bytes_received>, buffer)
  Recv(io::Result<i32>, ErasedBuffer),

  /// Result of an Accept operation: io::Result<(new_socket, peer_address)>
  Accept(io::Result<(Resource, SocketAddr)>),

  /// Result of a Connect operation: io::Result<()>
  Connect(io::Result<()>),

  /// Result of a Bind operation: io::Result<()>
  Bind(io::Result<()>),

  /// Result of a Listen operation: io::Result<()>
  Listen(io::Result<()>),

  /// Result of a Shutdown operation: io::Result<()>
  Shutdown(io::Result<()>),

  /// Result of a Socket operation: io::Result<new_socket>
  Socket(io::Result<Resource>),

  /// Result of an OpenAt operation: io::Result<file_descriptor>
  OpenAt(io::Result<Resource>),

  /// Result of a Close operation: io::Result<()>
  Close(io::Result<()>),

  /// Result of a Fsync operation: io::Result<()>
  Fsync(io::Result<()>),

  /// Result of a Truncate operation: io::Result<()>
  Truncate(io::Result<()>),

  /// Result of a LinkAt operation: io::Result<()>
  LinkAt(io::Result<()>),

  /// Result of a SymlinkAt operation: io::Result<()>
  SymlinkAt(io::Result<()>),

  /// Result of a Tee operation: io::Result<bytes_transferred>
  #[cfg(target_os = "linux")]
  Tee(io::Result<i32>),

  /// Result of a Timeout operation: io::Result<()>
  Timeout(io::Result<()>),

  /// Result of a Nop operation: io::Result<()>
  Nop(io::Result<()>),
}

impl OpResult {
  /// Extracts the buffer from buffer-returning operations.
  ///
  /// Returns `Some(buffer)` for Read, Write, ReadAt, WriteAt, Send, and Recv operations.
  /// Returns `None` for all other operation types.
  pub fn into_buffer(self) -> Option<ErasedBuffer> {
    match self {
      OpResult::Read(_, buf)
      | OpResult::Write(_, buf)
      | OpResult::ReadAt(_, buf)
      | OpResult::WriteAt(_, buf)
      | OpResult::Send(_, buf)
      | OpResult::Recv(_, buf) => Some(buf),
      _ => None,
    }
  }

  /// Extracts the io::Result from any operation, discarding buffers if present.
  pub fn into_io_result(self) -> io::Result<()> {
    match self {
      OpResult::Read(res, _)
      | OpResult::Write(res, _)
      | OpResult::ReadAt(res, _)
      | OpResult::WriteAt(res, _)
      | OpResult::Send(res, _)
      | OpResult::Recv(res, _) => res.map(|_| ()),
      OpResult::Accept(res) => res.map(|_| ()),
      OpResult::Connect(res)
      | OpResult::Bind(res)
      | OpResult::Listen(res)
      | OpResult::Shutdown(res)
      | OpResult::Close(res)
      | OpResult::Fsync(res)
      | OpResult::Truncate(res)
      | OpResult::LinkAt(res)
      | OpResult::SymlinkAt(res)
      | OpResult::Timeout(res)
      | OpResult::Nop(res) => res,
      OpResult::Socket(res) | OpResult::OpenAt(res) => res.map(|_| ()),
      #[cfg(target_os = "linux")]
      OpResult::Tee(res) => res.map(|_| ()),
    }
  }
}

/// All I/O operations as pure data.
///
/// Each variant contains only the data needed to execute the operation.
/// Backends match on this enum to create submission entries or execute syscalls.
pub enum Op {
  // ═══════════════════════════════════════════════════════════════════════════
  // Buffer operations - buffer field owns the data, ptr/len point into it
  // ═══════════════════════════════════════════════════════════════════════════
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

  // ═══════════════════════════════════════════════════════════════════════════
  // Socket operations
  // ═══════════════════════════════════════════════════════════════════════════
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

  // ═══════════════════════════════════════════════════════════════════════════
  // File operations
  // ═══════════════════════════════════════════════════════════════════════════
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

  // ═══════════════════════════════════════════════════════════════════════════
  // Link operations
  // ═══════════════════════════════════════════════════════════════════════════
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

  // ═══════════════════════════════════════════════════════════════════════════
  // Misc
  // ═══════════════════════════════════════════════════════════════════════════
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

// impl Op {
//   /// Returns the operation metadata (capability flags).
//   #[cfg(unix)]
//   pub fn meta(&self) -> OpMeta {
//     match self {
//       // No capability (immediate or io_uring handles it)
//       Op::Read { .. }
//       | Op::Write { .. }
//       | Op::ReadAt { .. }
//       | Op::WriteAt { .. }
//       | Op::Socket { .. }
//       | Op::OpenAt { .. }
//       | Op::Close { .. }
//       | Op::Fsync { .. }
//       | Op::Truncate { .. }
//       | Op::Bind { .. }
//       | Op::Listen { .. }
//       | Op::Shutdown { .. }
//       | Op::LinkAt { .. }
//       | Op::SymlinkAt { .. } => OpMeta::CAP_NONE,
//
//       #[cfg(target_os = "linux")]
//       Op::Tee { .. } => OpMeta::CAP_NONE,
//
//       Op::Nop => OpMeta::CAP_NONE,
//
//       // FD + read readiness
//       Op::Accept { .. } | Op::Recv { .. } => OpMeta::CAP_FD | OpMeta::FD_READ,
//
//       // FD + write readiness
//       Op::Send { .. } => OpMeta::CAP_FD | OpMeta::FD_WRITE,
//
//       // FD + write readiness + needs run before notification
//       Op::Connect { .. } => {
//         OpMeta::CAP_FD | OpMeta::FD_WRITE | OpMeta::BEH_NEEDS_RUN
//       }
//
//       // Timer
//       #[cfg(target_os = "linux")]
//       Op::Timeout { .. } => OpMeta::CAP_FD | OpMeta::FD_READ,
//
//       #[cfg(kqueue)]
//       Op::Timeout { .. } => OpMeta::CAP_TIMER,
//     }
//   }
//
//   /// Returns the file descriptor or timer ID for polling.
//   ///
//   /// # Panics
//   /// Panics if called on an operation with `CAP_NONE`.
//   #[cfg(unix)]
//   pub fn cap(&self) -> i32 {
//     match self {
//       Op::Accept { fd, .. }
//       | Op::Connect { fd, .. }
//       | Op::Send { fd, .. }
//       | Op::Recv { fd, .. } => *fd,
//
//       #[cfg(target_os = "linux")]
//       Op::Timeout { timer_fd, .. } => *timer_fd,
//
//       #[cfg(kqueue)]
//       Op::Timeout { duration, .. } => duration.as_millis() as i32,
//
//       _ => panic!("Op::cap called on operation with no capability"),
//     }
//   }
// }
