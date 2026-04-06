//! Operation data enum for zero-box I/O operations.
//!
//! This module defines the [`Op`] enum which represents all I/O operations
//! as pure data. Backends match on this enum to execute operations.

use std::ffi::c_char;
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::RawFd;

use crate::api::resource::Resource;

// ═══════════════════════════════════════════════════════════════════════════════
// ErasedBuffer - Type-erased buffer storage
// ═══════════════════════════════════════════════════════════════════════════════

/// Non-owning raw buffer pointer used by all backends.
///
/// The actual buffer is owned by the typed op (e.g., `Send<Vec<u8>>`). This struct
/// holds only a pointer + length so backends can call syscalls without taking
/// ownership, leaving the buffer available for `extract_result`.
#[derive(Debug)]
#[repr(transparent)]
pub struct RawBuf(libc::iovec);

impl RawBuf {
  /// Creates a RawBuf from a pointer and length.
  #[inline]
  pub const fn new(buf: &[u8]) -> Self {
    Self(libc::iovec {
      iov_base: buf.as_ptr().cast_mut().cast(),
      iov_len: buf.len(),
    })
  }

  /// Creates a RawBuf from raw parts.
  ///
  /// # Safety
  ///
  /// `ptr` must be valid for reads or writes of `len` bytes for the duration of
  /// the submitted operation.
  #[inline]
  pub const unsafe fn from_raw_parts(ptr: *mut u8, len: usize) -> Self {
    Self(libc::iovec { iov_base: ptr.cast(), iov_len: len })
  }
}

// SAFETY: The pointed-to data is owned by a TypedOp which is Send + Sync.
// We only use this pointer from the thread that scheduled the operation.
unsafe impl Send for RawBuf {}
// SAFETY: Same as Send - we only access through the owning TypedOp.
unsafe impl Sync for RawBuf {}

// pub struct IoSlice(libc::iovec);

/// All I/O operations as pure data.
///
/// Each variant contains only the data needed to execute the operation.
/// Backends match on this enum to create submission entries or execute syscalls.
#[derive(Clone, Debug)]
pub enum Op {
  // ═══════════════════════════════════════════════════════════════════════════════
  // Buffer operations - RawBuf (ptr/len) points to TypedOp's buffer
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Generic read operation supporting all read variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple read: buf.ptr != null, iovecs == null, offset == -1
  /// - Positioned read (pread): buf.ptr != null, offset >= 0
  /// - Vectored read (readv): iovecs != null, offset == -1
  /// - Vectored positioned (preadv): iovecs != null, offset >= 0
  /// - With flags (preadv2): Any above + flags != 0
  ///
  /// The buffer/iovec pointers are owned by the OpModel and must remain valid.
  Read {
    /// File descriptor to read from
    fd: Resource,
    /// Scatter buffers (null if using single buf)
    iovecs: *mut RawBuf,
    /// Number of iovecs
    iov_count: usize,
    /// File offset (-1 for current position)
    offset: i64,
    /// Read flags (RWF_HIPRI, RWF_NOWAIT, etc. - 0 if none)
    flags: i32,
  },

  /// Generic write operation supporting all write variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple write: buf.ptr != null, iovecs == null, offset == -1
  /// - Positioned write (pwrite): buf.ptr != null, offset >= 0
  /// - Vectored write (writev): iovecs != null, offset == -1
  /// - Vectored positioned (pwritev): iovecs != null, offset >= 0
  /// - With flags (pwritev2): Any above + flags != 0
  ///
  /// The buffer/iovec pointers are owned by the OpModel and must remain valid.
  Write {
    /// File descriptor to write to
    fd: Resource,
    /// Gather buffers (null if using single buf)
    iovecs: *const RawBuf,
    /// Number of iovecs
    iov_count: usize,
    /// File offset (-1 for current position)
    offset: i64,
    /// Write flags (RWF_DSYNC, RWF_SYNC, etc. - 0 if none)
    flags: i32,
  },

  /// Generic receive operation supporting all recv variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple recv: buf.ptr != null, iovecs == null, addr == null, msg == null
  /// - Receive with address (recvfrom): addr != null
  /// - Vectored receive: iovecs != null
  /// - Full control (recvmsg): msg != null (all other fields ignored)
  ///
  /// The buffer/iovec/msg pointers are owned by the OpModel and must remain valid.
  Recv {
    /// Socket to receive from
    fd: Resource,
    /// Full msghdr for recvmsg (null if using simpler variant)
    msg: *mut libc::msghdr,
    /// Send flags (MSG_NOSIGNAL, MSG_MORE, MSG_DONTWAIT, etc.)
    flags: i32,
  },

  /// Generic send operation supporting all send variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple send: buf.ptr != null, iovecs == null, addr == null, msg == null
  /// - Send to address (sendto): addr != null
  /// - Vectored send: iovecs != null
  /// - Full control (sendmsg): msg != null (all other fields ignored)
  ///
  /// The buffer/iovec/msg pointers are owned by the OpModel and must remain valid.
  Send {
    /// Socket to send to
    fd: Resource,
    /// Full msghdr for sendmsg (null if using simpler variant)
    msg: *const libc::msghdr,
    /// Send flags (MSG_NOSIGNAL, MSG_MORE, MSG_DONTWAIT, etc.)
    flags: i32,
  },

  //
  // // ═══════════════════════════════════════════════════════════════════════════════
  // // Socket operations
  // // ═══════════════════════════════════════════════════════════════════════════════
  /// Accept one incoming connection from a listening socket.
  ///
  /// Backends typically execute this with `accept(2)` or an equivalent platform
  /// primitive once the socket becomes readable.
  ///
  /// `addr` and `len` follow the normal `accept` contract:
  /// - if `addr` is non-null, the peer address is written there
  /// - `len` must point to the size of the storage on input
  /// - on success, `*len` is updated to the actual peer address length
  ///
  /// The address storage pointers are owned by the `OpModel` and must remain
  /// valid until the operation completes.
  Accept {
    /// Listening socket to accept from.
    fd: Resource,
    /// Peer address output storage.
    addr: *mut libc::sockaddr_storage,
    /// In/out length pointer for `addr`.
    len: *mut libc::socklen_t,
  },
  /// Initiate a connection on a socket.
  ///
  /// Backends typically call `connect(2)` immediately and either:
  /// - complete the op at once if the connection succeeds or fails immediately
  /// - keep it pending until the socket becomes writable, then check the final
  ///   connection result
  ///
  /// `addr` points to the destination socket address and must remain valid until
  /// the operation completes.
  Connect {
    /// Socket to connect.
    fd: Resource,
    /// Destination socket address.
    addr: *const libc::sockaddr_storage,
    /// Length of `addr` in bytes.
    len: libc::socklen_t,
  },
  Nop,
}

// /// Streaming accept - yields multiple connections from a single submission.
// ///
// /// On io_uring: Uses IORING_OP_ACCEPT with multishot flag.
// /// On pollingv2: Auto-resubmits after each accept.
// AcceptStream {
//   fd: Resource,
// },
// Bind {
//   fd: Resource,
//   addr: *const libc::sockaddr_storage,
//   addrlen: libc::socklen_t,
// },
// Listen {
//   fd: Resource,
//   backlog: i32,
// },
// Shutdown {
//   fd: Resource,
//   how: i32,
// },
// Socket {
//   domain: i32,
//   ty: i32,
//   proto: i32,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // File operations
// // ═══════════════════════════════════════════════════════════════════════════════
// OpenAt {
//   dir_fd: Resource,
//   path: *const c_char,
//   flags: i32,
//   /// File creation mode (permissions). Used when O_CREAT is set.
//   /// On Unix this is passed to openat() as the 4th argument.
//   mode: u32,
// },
// Close {
//   /// Raw file descriptor - we do not hold a Resource here to avoid
//   /// double-close (the op itself performs the close syscall).
//   #[cfg(unix)]
//   fd: RawFd,
//   #[cfg(windows)]
//   handle: std::os::windows::io::RawHandle,
//   /// Whether this is a socket (use closesocket()) or a file handle (use CloseHandle())
//   #[cfg(windows)]
//   is_socket: bool,
// },
// Fsync {
//   fd: Resource,
// },
// Truncate {
//   fd: Resource,
//   size: u64,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Link operations
// // ═══════════════════════════════════════════════════════════════════════════════
// LinkAt {
//   old_dir_fd: Resource,
//   old_path: *const c_char,
//   new_dir_fd: Resource,
//   new_path: *const c_char,
// },
// SymlinkAt {
//   dir_fd: Resource,
//   target: *const c_char,
//   linkpath: *const c_char,
// },
// UnlinkAt {
//   dir_fd: Resource,
//   path: *const c_char,
//   flags: i32,
// },
// RenameAt {
//   old_dir_fd: Resource,
//   old_path: *const c_char,
//   new_dir_fd: Resource,
//   new_path: *const c_char,
// },
// MkdirAt {
//   dir_fd: Resource,
//   path: *const c_char,
//   mode: u32,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Misc
// // ═══════════════════════════════════════════════════════════════════════════════
// #[cfg(target_os = "linux")]
// Tee {
//   fd_in: Resource,
//   fd_out: Resource,
//   size: u32,
// },
// /// Splice data between file descriptors via pipe (Linux only).
// #[cfg(target_os = "linux")]
// Splice {
//   fd_in: Resource,
//   off_in: i64,
//   fd_out: Resource,
//   off_out: i64,
//   len: u32,
//   flags: u32,
// },
// /// Send file data to a socket (Unix).
// #[cfg(unix)]
// SendFile {
//   out_fd: Resource,
//   in_fd: Resource,
//   offset: i64,
//   count: usize,
// },
// /// Copy data between files server-side (Linux only).
// #[cfg(target_os = "linux")]
// CopyFileRange {
//   fd_in: Resource,
//   off_in: i64,
//   fd_out: Resource,
//   off_out: i64,
//   len: usize,
//   flags: u32,
// },
// Sleep {
//   duration: Duration,
//   #[cfg(target_os = "linux")]
//   timer_fd: Resource,
//   #[cfg(target_os = "linux")]
//   timespec: *const libc::timespec,
// },
// /// Wraps an inner operation with a timeout deadline.
// ///
// /// On io_uring: Uses IORING_OP_LINK_TIMEOUT for kernel-native timeout handling.
// /// On pollingv2: Uses userspace timeout coordination via TimeManager.
// ///
// /// If the timeout fires first, the inner operation is cancelled.
// /// If the inner operation completes first, the timeout is cancelled.
// #[cfg(unix)]
// Timeout {
//   /// The wrapped operation
//   inner: Box<Op>,
//   /// Timeout duration
//   duration: Duration,
//   /// Pointer to timespec in the TypedOp (used by io_uring)
//   #[cfg(target_os = "linux")]
//   timespec: *const libc::timespec,
// },
// /// Watch a file or directory for changes.
// ///
// /// On BSD/macOS: Uses EVFILT_VNODE with kqueue.
// /// On Linux: Uses inotify.
// #[cfg(unix)]
// Watch {
//   /// Path to watch (C string, owned by TypedOp)
//   path: *const c_char,
//   /// Events to watch for (platform-specific flags)
//   mask: u32,
// },
// // ═══════════════════════════════════════════════════════════════════════════════
// // Process operations
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Wait for a child process to change state.
// ///
// /// On Linux with io_uring 6.7+: Uses IORING_OP_WAITID.
// /// On other platforms: Uses blocking waitid() syscall.
// #[cfg(unix)]
// Waitid {
//   /// What type of ID to wait for (P_PID, P_PGID, P_ALL, P_PIDFD).
//   idtype: libc::idtype_t,
//   /// The ID value (pid, pgid, or pidfd depending on idtype).
//   id: libc::id_t,
//   /// Options (WEXITED, WSTOPPED, WCONTINUED, WNOHANG, WNOWAIT).
//   options: libc::c_int,
//   /// Pointer to siginfo_t storage in the TypedOp.
//   infop: *mut libc::siginfo_t,
// },

// /// Spawn a new process.
// ///
// /// Uses posix_spawn() to create a new process.
// #[cfg(unix)]
// Spawn {
//   /// Path to executable (C string, owned by TypedOp).
//   path: *const c_char,
//   /// Argument vector (null-terminated array, owned by TypedOp).
//   argv: *const *const c_char,
//   /// Environment vector (null-terminated array, owned by TypedOp).
//   envp: *const *const c_char,
//   /// Pointer to pid_t storage in the TypedOp.
//   pid: *mut libc::pid_t,
//   /// File actions for stdio redirection (null for inherit).
//   file_actions: *const libc::posix_spawn_file_actions_t,
// },

// ═══════════════════════════════════════════════════════════════════════════════
// Vectored I/O operations (scatter/gather)
// ═══════════════════════════════════════════════════════════════════════════════
// ReadVAt {
//   fd: Resource,
//   iovecs: *const libc::iovec,
//   iov_count: usize,
//   offset: i64,
// },
// WriteVAt {
//   fd: Resource,
//   iovecs: *const libc::iovec,
//   iov_count: usize,
//   offset: i64,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // File locking
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Advisory file locking.
// ///
// /// # Platform-specific behavior
// ///
// /// This operation corresponds to `flock()` on Unix with `LOCK_SH`/`LOCK_EX`/`LOCK_UN`,
// /// and `LockFileEx`/`UnlockFileEx` on Windows.
// ///
// /// - **Unix**: Advisory locking (other processes can ignore locks)
// /// - **Windows**: Mandatory locking (OS enforces). Locking fails if file is opened
// ///   only for append; open with `.read(true)` or `.write(true)`.
// ///
// /// Operations: `LOCK_SH` (shared), `LOCK_EX` (exclusive), `LOCK_UN` (unlock).
// /// Can be combined with `LOCK_NB` for non-blocking behavior.
// Flock {
//   fd: Resource,
//   /// Locking operation: LOCK_SH, LOCK_EX, LOCK_UN (optionally | LOCK_NB)
//   operation: i32,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Directory operations
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Read directory entries (getdents64 on Linux, getdirentries on BSD).
// #[cfg(unix)]
// GetDents {
//   fd: Resource,
//   buf: RawBuf,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Signal handling
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Wait for a signal from the specified signal set.
// ///
// /// On Linux: Uses signalfd.
// /// On BSD/macOS: Uses kqueue EVFILT_SIGNAL.
// #[cfg(unix)]
// Signal {
//   /// Pointer to sigset_t in the TypedOp.
//   sigset: *const libc::sigset_t,
// },
