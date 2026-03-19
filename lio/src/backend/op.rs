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
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RawBuf {
  pub ptr: *mut u8,
  pub len: usize,
}

impl RawBuf {
  /// Creates an empty RawBuf (null pointer, zero length).
  #[inline]
  pub const fn empty() -> Self {
    Self { ptr: std::ptr::null_mut(), len: 0 }
  }

  /// Creates a RawBuf from a pointer and length.
  #[inline]
  pub const fn new(ptr: *mut u8, len: usize) -> Self {
    Self { ptr, len }
  }
}

// SAFETY: The pointed-to data is owned by a TypedOp which is Send + Sync.
// We only use this pointer from the thread that scheduled the operation.
unsafe impl Send for RawBuf {}
// SAFETY: Same as Send - we only access through the owning TypedOp.
unsafe impl Sync for RawBuf {}

/// All I/O operations as pure data.
///
/// Each variant contains only the data needed to execute the operation.
/// Backends match on this enum to create submission entries or execute syscalls.
#[derive(Clone)]
pub enum Op {
  // ═══════════════════════════════════════════════════════════════════════════════
  // Buffer operations - RawBuf (ptr/len) points to TypedOp's buffer
  // ═══════════════════════════════════════════════════════════════════════════════
  Send {
    fd: Resource,
    flags: i32,
    buf: RawBuf,
  },
  Recv {
    fd: Resource,
    flags: i32,
    buf: RawBuf,
  },
  SendTo {
    fd: Resource,
    flags: i32,
    buf: RawBuf,
    addr: *const libc::sockaddr_storage,
    addrlen: libc::socklen_t,
    /// Pointer to msghdr in TypedOp for io_uring sendmsg (null for other backends)
    msghdr: *const libc::msghdr,
  },
  RecvFrom {
    fd: Resource,
    flags: i32,
    buf: RawBuf,
    addr: *mut libc::sockaddr_storage,
    addrlen: *mut libc::socklen_t,
    /// Pointer to msghdr in TypedOp for io_uring recvmsg (null for other backends)
    msghdr: *mut libc::msghdr,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Socket operations
  // ═══════════════════════════════════════════════════════════════════════════════
  Accept {
    fd: Resource,
    addr: *mut libc::sockaddr_storage,
    len: *mut libc::socklen_t,
  },
  /// Streaming accept - yields multiple connections from a single submission.
  ///
  /// On io_uring: Uses IORING_OP_ACCEPT with multishot flag.
  /// On pollingv2: Auto-resubmits after each accept.
  AcceptStream {
    fd: Resource,
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
    /// File creation mode (permissions). Used when O_CREAT is set.
    /// On Unix this is passed to openat() as the 4th argument.
    mode: u32,
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
  UnlinkAt {
    dir_fd: Resource,
    path: *const c_char,
    flags: i32,
  },
  RenameAt {
    old_dir_fd: Resource,
    old_path: *const c_char,
    new_dir_fd: Resource,
    new_path: *const c_char,
  },
  MkdirAt {
    dir_fd: Resource,
    path: *const c_char,
    mode: u32,
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
  /// Splice data between file descriptors via pipe (Linux only).
  #[cfg(target_os = "linux")]
  Splice {
    fd_in: Resource,
    off_in: i64,
    fd_out: Resource,
    off_out: i64,
    len: u32,
    flags: u32,
  },
  /// Send file data to a socket (Unix).
  #[cfg(unix)]
  SendFile {
    out_fd: Resource,
    in_fd: Resource,
    offset: i64,
    count: usize,
  },
  /// Copy data between files server-side (Linux only).
  #[cfg(target_os = "linux")]
  CopyFileRange {
    fd_in: Resource,
    off_in: i64,
    fd_out: Resource,
    off_out: i64,
    len: usize,
    flags: u32,
  },
  Sleep {
    duration: Duration,
    #[cfg(target_os = "linux")]
    timer_fd: Resource,
    #[cfg(target_os = "linux")]
    timespec: *const libc::timespec,
  },
  /// Wraps an inner operation with a timeout deadline.
  ///
  /// On io_uring: Uses IORING_OP_LINK_TIMEOUT for kernel-native timeout handling.
  /// On pollingv2: Uses userspace timeout coordination via TimeManager.
  ///
  /// If the timeout fires first, the inner operation is cancelled.
  /// If the inner operation completes first, the timeout is cancelled.
  #[cfg(unix)]
  Timeout {
    /// The wrapped operation
    inner: Box<Op>,
    /// Timeout duration
    duration: Duration,
    /// Pointer to timespec in the TypedOp (used by io_uring)
    #[cfg(target_os = "linux")]
    timespec: *const libc::timespec,
  },
  Nop,

  /// Watch a file or directory for changes.
  ///
  /// On BSD/macOS: Uses EVFILT_VNODE with kqueue.
  /// On Linux: Uses inotify.
  #[cfg(unix)]
  Watch {
    /// Path to watch (C string, owned by TypedOp)
    path: *const c_char,
    /// Events to watch for (platform-specific flags)
    mask: u32,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Process operations
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Wait for a child process to change state.
  ///
  /// On Linux with io_uring 6.7+: Uses IORING_OP_WAITID.
  /// On other platforms: Uses blocking waitid() syscall.
  #[cfg(unix)]
  Waitid {
    /// What type of ID to wait for (P_PID, P_PGID, P_ALL, P_PIDFD).
    idtype: libc::idtype_t,
    /// The ID value (pid, pgid, or pidfd depending on idtype).
    id: libc::id_t,
    /// Options (WEXITED, WSTOPPED, WCONTINUED, WNOHANG, WNOWAIT).
    options: libc::c_int,
    /// Pointer to siginfo_t storage in the TypedOp.
    infop: *mut libc::siginfo_t,
  },

  /// Spawn a new process.
  ///
  /// Uses posix_spawn() to create a new process.
  #[cfg(unix)]
  Spawn {
    /// Path to executable (C string, owned by TypedOp).
    path: *const c_char,
    /// Argument vector (null-terminated array, owned by TypedOp).
    argv: *const *const c_char,
    /// Environment vector (null-terminated array, owned by TypedOp).
    envp: *const *const c_char,
    /// Pointer to pid_t storage in the TypedOp.
    pid: *mut libc::pid_t,
    /// File actions for stdio redirection (null for inherit).
    file_actions: *const libc::posix_spawn_file_actions_t,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Vectored I/O operations (scatter/gather)
  // ═══════════════════════════════════════════════════════════════════════════════
  ReadV {
    fd: Resource,
    buf: RawBuf,
    iovecs: *const libc::iovec,
    iov_count: usize,
  },
  WriteV {
    fd: Resource,
    buf: RawBuf,
    iovecs: *const libc::iovec,
    iov_count: usize,
  },
  ReadVAt {
    fd: Resource,
    buf: RawBuf,
    iovecs: *const libc::iovec,
    iov_count: usize,
    offset: i64,
  },
  WriteVAt {
    fd: Resource,
    buf: RawBuf,
    iovecs: *const libc::iovec,
    iov_count: usize,
    offset: i64,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // File locking
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Advisory file locking.
  ///
  /// # Platform-specific behavior
  ///
  /// This operation corresponds to `flock()` on Unix with `LOCK_SH`/`LOCK_EX`/`LOCK_UN`,
  /// and `LockFileEx`/`UnlockFileEx` on Windows.
  ///
  /// - **Unix**: Advisory locking (other processes can ignore locks)
  /// - **Windows**: Mandatory locking (OS enforces). Locking fails if file is opened
  ///   only for append; open with `.read(true)` or `.write(true)`.
  ///
  /// Operations: `LOCK_SH` (shared), `LOCK_EX` (exclusive), `LOCK_UN` (unlock).
  /// Can be combined with `LOCK_NB` for non-blocking behavior.
  Flock {
    fd: Resource,
    /// Locking operation: LOCK_SH, LOCK_EX, LOCK_UN (optionally | LOCK_NB)
    operation: i32,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Directory operations
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Read directory entries (getdents64 on Linux, getdirentries on BSD).
  #[cfg(unix)]
  GetDents {
    fd: Resource,
    buf: RawBuf,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Signal handling
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Wait for a signal from the specified signal set.
  ///
  /// On Linux: Uses signalfd.
  /// On BSD/macOS: Uses kqueue EVFILT_SIGNAL.
  #[cfg(unix)]
  Signal {
    /// Pointer to sigset_t in the TypedOp.
    sigset: *const libc::sigset_t,
  },
}

// SAFETY: Op contains raw pointers but they point to data owned by ErasedBuffer
// which is stored alongside Op in StoredOp. The pointers are valid for the
// lifetime of the operation.
unsafe impl Send for Op {}
// SAFETY: Same as Send - pointers are valid for the operation's lifetime.
unsafe impl Sync for Op {}
