#![allow(clippy::undocumented_unsafe_blocks)]

//! `lio`-provided [`IoBackend`] impl for `epoll`/`kqueue` (platform-specific).

mod os;

#[cfg(target_os = "linux")]
use os::epoll as sys;

#[cfg(any(
  target_os = "macos",
  target_os = "ios",
  target_os = "tvos",
  target_os = "watchos",
  target_os = "freebsd",
  target_os = "dragonfly",
  target_os = "openbsd",
  target_os = "netbsd"
))]
use os::kqueue as sys;

#[cfg(test)]
pub(crate) mod tests;

use std::{
  io,
  net::SocketAddr,
  os::fd::{AsRawFd, RawFd},
  time::Duration,
};

use bumpalo::Bump;

use crate::backend::{
  IoBackend, OpCompleted,
  impls::pollingv2::interest::Interest,
  op::{
    DirEntryRef, MsgRecv, MsgSend, Op, ReadDirResult,
    file_type_from_dirent_dtype,
  },
};
use crate::slab::{Slab, SlabKey};
mod interest;

/// Trait for OS-specific readiness polling implementations
///
/// This abstraction makes it easy to add new platforms (epoll, IOCP, etc.)
///
/// ## Design for cross-platform compatibility
///
/// - **epoll**: Registers both read/write interest on a single fd
/// - **kqueue**: Registers read and write separately as different filters
/// - This trait accommodates both by accepting Interest flags
///
/// ## Thread Safety
///
/// Implementations of this trait are intentionally `!Send` to ensure they are
/// used only from a single thread. This allows for more efficient interior
/// mutability without synchronization overhead.
pub(crate) trait ReadinessPoll {
  /// The native event type used by this implementation
  type NativeEvent;

  /// Add interest for a file descriptor (one-shot mode).
  /// This is not idempotent.
  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()>;

  /// Remove all interest for a file descriptor
  /// This fails if 'fd' hasn't previously been added.
  fn delete(&self, fd: RawFd) -> io::Result<()>;

  /// Wait for events, filling the provided buffer
  /// Returns the number of events received
  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize>;

  /// Extract the key from a native event
  fn event_key(event: &Self::NativeEvent) -> u64;

  /// Extract the interest from a native event
  fn event_interest(event: &Self::NativeEvent) -> Interest;

  /// Extract fflags from a native event (kqueue VNODE events).
  /// Returns 0 on platforms that don't support fflags.
  fn event_fflags(_event: &Self::NativeEvent) -> u32 {
    0
  }
}

/// Represents a readiness event from the poller
#[derive(Debug, Clone, Copy)]
pub struct Event {
  /// User-provided key to identify this event
  #[allow(dead_code)]
  pub key: u64,
  /// The interest flags that triggered (what actually happened)
  #[allow(dead_code)]
  pub interest: Interest,
  /// For VNODE events (kqueue): the fflags indicating what changed
  #[allow(dead_code)]
  pub fflags: u32,
}

type LoweredMsg = (
  [libc::iovec; crate::buf::MAX_IOV_COUNT],
  libc::msghdr,
  Option<(libc::sockaddr_storage, libc::socklen_t)>,
);

/// A collection of events returned from polling
pub(crate) struct Events {
  events: Vec<<sys::OsPoller as ReadinessPoll>::NativeEvent>,
}

impl Default for Events {
  fn default() -> Self {
    Self::with_capacity(512)
  }
}

impl Events {
  /// Create a new empty events collection with specified capacity
  pub(crate) fn with_capacity(capacity: usize) -> Self {
    Self { events: Vec::with_capacity(capacity) }
  }

  /// Returns the vec of maybe-initialised values. Meant for OS to fill and
  /// then we set correct length.
  fn as_buf(&mut self) -> &mut [<sys::OsPoller as ReadinessPoll>::NativeEvent] {
    assert!(
      self.events.is_empty(),
      "lio logic error: Left over items during Events::as_buf call."
    );
    let spare = self.events.spare_capacity_mut();
    // SAFETY: The OS wait syscall will initialize up to `spare.len()` entries.
    // `set_len()` is called afterward with the actual initialized count.
    unsafe {
      std::slice::from_raw_parts_mut(
        spare
          .as_mut_ptr()
          .cast::<<sys::OsPoller as ReadinessPoll>::NativeEvent>(),
        spare.len(),
      )
    }
  }

  unsafe fn set_len(&mut self, len: usize) {
    assert!(len <= self.events.capacity(), "set_len: len must be <= capacity");
    // SAFETY: The caller guarantees that the first `len` elements have been initialized
    // by the OS's wait() call. We've verified len <= capacity above.
    unsafe { self.events.set_len(len) }
  }

  fn pop(&mut self) -> Option<Event> {
    let native_event = self.events.pop()?;
    let key = sys::OsPoller::event_key(&native_event);
    let interest = sys::OsPoller::event_interest(&native_event);
    let fflags = sys::OsPoller::event_fflags(&native_event);

    Some(Event { key, interest, fflags })
  }
}

/// Polling-based I/O backend for epoll (Linux) and kqueue (BSD/macOS).
pub struct Poller {
  sys: Option<sys::OsPoller>,
  events: Events,
  capacity: usize,
  backlog: Vec<PendingOp>,
  /// Operations that have been registered with the poller and are waiting for readiness
  pending: Slab<PendingOp>,
  /// Immediate completions produced during `flush()` and surfaced on the next `wait()`.
  queued_completed: Vec<OpCompleted>,
  /// Reusable buffer for completed operations
  completed: Vec<OpCompleted>,
}

impl Default for Poller {
  fn default() -> Self {
    Self {
      sys: None,
      events: Events::default(),
      capacity: 0,
      backlog: Vec::new(),
      pending: Slab::new(0),
      queued_completed: Vec::new(),
      completed: Vec::new(),
    }
  }
}

pub struct PendingOp {
  registration_id: u64,
  op: crate::backend::op::Op,
}

// EAGAIN and EWOULDBLOCK are the same on Linux, but may differ on other systems
const EAGAIN_NEG: isize = -(libc::EAGAIN as isize);
const EWOULDBLOCK_NEG: isize = -(libc::EWOULDBLOCK as isize);
impl Poller {
  fn set_fd_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = syscall!(fcntl(fd, libc::F_GETFD))?;
    syscall!(fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    Ok(())
  }

  fn set_fd_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
    syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
    Ok(())
  }

  fn configure_socket_fd(fd: RawFd) -> io::Result<()> {
    Self::set_fd_cloexec(fd)?;
    Self::set_fd_nonblocking(fd)?;
    Ok(())
  }

  #[inline]
  fn dirent_ino(entry: *const libc::dirent) -> u64 {
    #[cfg(any(
      target_os = "linux",
      target_os = "macos",
      target_os = "ios",
      target_os = "tvos",
      target_os = "watchos"
    ))]
    // SAFETY: `entry` is a live `libc::dirent` pointer returned by `readdir`
    // for the duration of the current iteration.
    unsafe {
      (*entry).d_ino
    }

    #[cfg(any(
      target_os = "freebsd",
      target_os = "dragonfly",
      target_os = "openbsd",
      target_os = "netbsd"
    ))]
    // SAFETY: `entry` is a live `libc::dirent` pointer returned by `readdir`
    // for the duration of the current iteration.
    unsafe {
      (*entry).d_fileno
    }
  }

  unsafe fn drop_readdir_state(state: *mut ()) {
    if !state.is_null() {
      // SAFETY: `state` was created by `fdopendir` and is owned by the
      // backend continuation state for this directory stream.
      unsafe {
        libc::closedir(state.cast());
      }
    }
  }

  fn read_dir_entries_readdir(
    fd: RawFd,
    opaque: &mut *mut (),
    opaque_drop: &mut Option<crate::backend::op::OpaqueDropFn>,
    raw: &mut [u8],
    out: &mut [DirEntryRef],
  ) -> io::Result<ReadDirResult> {
    let mut created_here = false;
    let dir = if opaque.is_null() {
      let dup_fd = syscall!(dup(fd))?;

      // SAFETY: `dup_fd` is a valid duplicated directory descriptor.
      let dir = unsafe { libc::fdopendir(dup_fd) };
      if dir.is_null() {
        let err = io::Error::last_os_error();
        let _ = syscall!(close(dup_fd));
        return Err(err);
      }
      *opaque = dir.cast();
      *opaque_drop = Some(Self::drop_readdir_state);
      created_here = true;
      dir
    } else {
      opaque.cast()
    };

    let result = (|| {
      let mut written = 0usize;
      let mut raw_written = 0usize;
      let mut eof = false;
      loop {
        // SAFETY: `dir` is a valid `DIR*` stream owned by this function.
        let pos = unsafe { libc::telldir(dir) };
        if pos < 0 {
          return Err(io::Error::last_os_error());
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: resetting thread-local errno before `readdir`.
        unsafe {
          *libc::__errno_location() = 0;
        }
        #[cfg(any(
          target_os = "macos",
          target_os = "ios",
          target_os = "freebsd",
          target_os = "dragonfly",
          target_os = "openbsd",
          target_os = "netbsd"
        ))]
        // SAFETY: resetting thread-local errno before `readdir`.
        unsafe {
          *libc::__error() = 0;
        }

        // SAFETY: `dir` is a valid `DIR*` stream owned by this function.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
          let err = io::Error::last_os_error();
          if err.raw_os_error().unwrap_or(0) == 0 {
            eof = true;
            break;
          }
          return Err(err);
        }

        // SAFETY: `readdir` returned a valid directory entry whose `d_name`
        // is NUL-terminated for the lifetime of this iteration.
        let name =
          unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_bytes();
        if name == b"." || name == b".." {
          continue;
        }

        if written == out.len() || raw_written + name.len() > raw.len() {
          // SAFETY: `pos` came from `telldir(dir)` on the same directory stream.
          unsafe {
            libc::seekdir(dir, pos);
          }
          break;
        }
        raw[raw_written..raw_written + name.len()].copy_from_slice(name);
        out[written] = DirEntryRef {
          name_offset: raw_written as u32,
          name_len: name.len() as u16,
          // SAFETY: `entry` is valid for this iteration.
          file_type: file_type_from_dirent_dtype(unsafe { (*entry).d_type }),
          ino: Some(Self::dirent_ino(entry)),
        };
        raw_written += name.len();
        written += 1;
      }
      Ok(ReadDirResult { entries: written, raw_written, eof })
    })();

    match result {
      Ok(entries) => {
        if entries.eof {
          let close_result = syscall!(closedir(dir));
          *opaque = std::ptr::null_mut();
          *opaque_drop = None;

          let _ = close_result?;
          Ok(entries)
        } else {
          Ok(entries)
        }
      }
      Err(err) => {
        if !created_here || !dir.is_null() {
          let _ = syscall!(raw closedir(dir));
          *opaque = std::ptr::null_mut();
          *opaque_drop = None;
        }
        Err(err)
      }
    }
  }

  fn read_dir_entries(
    fd: RawFd,
    opaque: &mut *mut (),
    opaque_drop: &mut Option<crate::backend::op::OpaqueDropFn>,
    raw: &mut [u8],
    out: &mut [DirEntryRef],
  ) -> io::Result<ReadDirResult> {
    Self::read_dir_entries_readdir(fd, opaque, opaque_drop, raw, out)
  }

  fn lower_socket_addr(
    addr: &crate::backend::op::SocketAddrBuf,
  ) -> io::Result<(libc::sockaddr_storage, libc::socklen_t)> {
    crate::backend::op::socket_addr_buf_to_storage(addr)
  }

  fn raise_socket_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
  ) -> io::Result<crate::backend::op::SocketAddrBuf> {
    crate::backend::op::socket_addr_buf_from_storage(storage, len)
  }

  fn lower_raw_iovecs(
    iovecs: std::ptr::NonNull<crate::backend::op::RawBuf>,
    iov_count: usize,
  ) -> Option<[libc::iovec; crate::buf::MAX_IOV_COUNT]> {
    // SAFETY: `iovecs` points to `iov_count` valid `RawBuf` entries.
    let raws =
      unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
    let mut native =
      [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT];
    for (dst, src) in native.iter_mut().zip(raws.iter()) {
      dst.iov_base = src.ptr.cast();
      dst.iov_len = src.len;
    }
    Some(native)
  }

  fn lower_recv_msg(msg: &MsgRecv) -> Option<LoweredMsg> {
    // SAFETY: `msg.bufs` points to `msg.buf_count` valid `MsgBufMut` items.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut iovecs =
      [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT];

    for (dst, src) in iovecs.iter_mut().zip(bufs.iter()) {
      dst.iov_base = src.ptr.as_ptr().cast();
      dst.iov_len = src.len;
    }

    let addr = if msg.from.is_some() {
      Some((
        // SAFETY: `sockaddr_storage` is POD and may be zero-initialized.
        unsafe { std::mem::zeroed::<libc::sockaddr_storage>() },
        std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      ))
    } else {
      None
    };

    let hdr = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: std::ptr::null_mut(),
      msg_iovlen: 0,
      msg_control: std::ptr::null_mut(),
      msg_controllen: 0,
      msg_flags: 0,
    };
    Some((iovecs, hdr, addr))
  }

  fn lower_send_msg(msg: &MsgSend) -> Option<LoweredMsg> {
    // SAFETY: `msg.bufs` points to `msg.buf_count` valid `MsgBuf` items.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut iovecs =
      [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT];

    for (dst, src) in iovecs.iter_mut().zip(bufs.iter()) {
      dst.iov_base = src.ptr.as_ptr().cast();
      dst.iov_len = src.len;
    }

    let addr = msg.to.map(crate::backend::op::socket_addr_to_storage);
    let hdr = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: std::ptr::null_mut(),
      msg_iovlen: 0,
      msg_control: std::ptr::null_mut(),
      msg_controllen: 0,
      msg_flags: 0,
    };
    Some((iovecs, hdr, addr))
  }

  fn debug_sendmsg_zero(fd: RawFd, hdr: &libc::msghdr) {
    let mut so_error: i32 = 0;
    let mut so_error_len = std::mem::size_of::<i32>() as libc::socklen_t;
    let so_error_ret = syscall!(raw getsockopt(
      fd,
      libc::SOL_SOCKET,
      libc::SO_ERROR,
      (&mut so_error as *mut i32).cast(),
      &mut so_error_len,
    ));

    let mut peer = unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
    let mut peer_len =
      std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let peer_ret = syscall!(raw getpeername(
      fd,
      (&mut peer as *mut libc::sockaddr_storage).cast(),
      &mut peer_len,
    ));

    let peer = if peer_ret >= 0 {
      Self::raise_socket_addr(&peer, peer_len)
        .and_then(|addr| crate::backend::op::socket_addr_from_buf(&addr))
        .map(|addr| addr.to_string())
        .unwrap_or_else(|err| format!("<decode-error:{err}>"))
    } else {
      format!("<getpeername:{}>", -peer_ret)
    };

    eprintln!(
      "lio-debug sendmsg returned 0 fd={} msg_iovlen={} first_iov_len={} so_error_ret={} so_error={} peer={}",
      fd,
      hdr.msg_iovlen,
      if hdr.msg_iovlen > 0 {
        // SAFETY: msg_iovlen > 0 means msg_iov points to at least one iovec.
        unsafe { (*hdr.msg_iov).iov_len }
      } else {
        0
      },
      so_error_ret,
      so_error,
      peer,
    );
  }

  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn sys(&self) -> &sys::OsPoller {
    self.sys.as_ref().expect("Poller not initialized")
  }

  /// Perform vectored read, choosing the appropriate syscall based on offset and flags
  #[inline]
  fn do_readv(
    fd: RawFd,
    iovecs: *const libc::iovec,
    iov_count: usize,
    offset: i64,
    flags: i32,
  ) -> isize {
    if offset < -1 || iovecs.is_null() {
      return -(libc::EINVAL as isize);
    }

    const RWF_HIPRI: i32 = 0x00000001;
    const RWF_DSYNC: i32 = 0x00000002;
    const RWF_SYNC: i32 = 0x00000004;
    const RWF_APPEND: i32 = 0x00000010;

    const ALL_KNOWN_FLAGS: i32 = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND;

    // Validate inputs
    if flags & !ALL_KNOWN_FLAGS != 0 {
      return -(libc::ENOTSUP as isize);
    }

    // Linux: native preadv2 support
    #[cfg(target_os = "linux")]
    return syscall!(raw preadv2(fd, iovecs, iov_count as i32, offset, flags));

    #[cfg(not(target_os = "linux"))]
    {
      // Non-Linux: Emulate flags (all are no-ops for reads)
      let result = if offset == -1 {
        syscall!(raw readv(fd, iovecs, iov_count as i32)?)
      } else {
        syscall!(raw preadv(fd, iovecs, iov_count as i32, offset)?)
      };

      assert!(result >= 0, "error did not return");
      result
    }
  }

  /// Perform vectored write, choosing the appropriate syscall based on offset and flags
  #[inline]
  fn do_writev(
    fd: RawFd,
    iovecs: *const libc::iovec,
    iov_count: usize,
    offset: i64,
    flags: i32,
  ) -> isize {
    if offset < -1 || iovecs.is_null() {
      return -(libc::EINVAL as isize);
    }
    const RWF_HIPRI: i32 = 0x00000001;
    const RWF_DSYNC: i32 = 0x00000002;
    const RWF_SYNC: i32 = 0x00000004;
    const RWF_APPEND: i32 = 0x00000010;

    const ALL_KNOWN_FLAGS: i32 = RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND;

    // Validate inputs
    if flags & !ALL_KNOWN_FLAGS != 0 {
      return -(libc::ENOTSUP as isize);
    }

    // Linux: native pwritev2 support
    #[cfg(target_os = "linux")]
    return syscall!(raw pwritev2(fd, iovecs, iov_count as i32, offset, flags));

    #[cfg(not(target_os = "linux"))]
    {
      let has_dsync = flags & RWF_DSYNC != 0;
      let has_sync = flags & RWF_SYNC != 0;
      let has_append = flags & RWF_APPEND != 0;

      // Emulate RWF_APPEND: write at end of file
      let write_offset = if has_append {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = syscall!(raw fstat(fd, stat.as_mut_ptr())?);
        assert_eq!(result, 0);
        // SAFETY: successful `fstat` initialized `stat`.
        unsafe { stat.assume_init() }.st_size
      } else {
        offset
      };

      let result = if write_offset == -1 {
        syscall!(raw writev(fd, iovecs, iov_count as i32)?)
      } else {
        syscall!(raw pwritev(fd, iovecs, iov_count as i32, write_offset)?)
      };

      assert!(result >= 0, "error did not return");

      // RWF_SYNC takes precedence over RWF_DSYNC
      if has_sync {
        let _ = syscall!(raw fsync(fd)?);
      } else if has_dsync {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let _ = syscall!(raw fcntl(fd, libc::F_FULLFSYNC)?);
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let _ = syscall!(raw fdatasync(fd)?);
      }
      result
    }
  }

  /// Extract fd and interest from an operation
  fn extract_fd_interest(
    &self,
    op: &crate::backend::op::Op,
  ) -> (RawFd, Interest) {
    match op {
      crate::backend::op::Op::Read { fd, .. } => {
        (fd.as_raw_fd(), Interest::READ)
      }
      crate::backend::op::Op::Write { fd, .. } => {
        (fd.as_raw_fd(), Interest::WRITE)
      }
      crate::backend::op::Op::Recv { fd, .. } => {
        (fd.as_raw_fd(), Interest::READ)
      }
      crate::backend::op::Op::Send { fd, .. } => {
        (fd.as_raw_fd(), Interest::WRITE)
      }
      crate::backend::op::Op::Accept { fd, .. } => {
        (fd.as_raw_fd(), Interest::READ)
      }
      crate::backend::op::Op::Connect { fd, .. } => {
        (fd.as_raw_fd(), Interest::WRITE)
      }
      crate::backend::op::Op::Socket { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Bind { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Listen { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Shutdown { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Fsync { .. } => (0, Interest::NONE),
      crate::backend::op::Op::OpenAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Stat { .. } => (0, Interest::NONE),
      crate::backend::op::Op::ReadDir { .. } => (0, Interest::NONE),
      crate::backend::op::Op::UnlinkAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::RenameAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::MkdirAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::LinkAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::ReadlinkAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::GetCwd { .. } => (0, Interest::NONE),
      #[cfg(unix)]
      crate::backend::op::Op::Spawn { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Nop => (0, Interest::NONE),
    }
  }

  fn should_complete_immediately(op: &crate::backend::op::Op) -> bool {
    let fd = match op {
      Op::Read { fd, .. } | Op::Write { fd, .. } => fd.as_raw_fd(),
      Op::Socket { .. } => return true,
      Op::Bind { .. } => return true,
      Op::Listen { .. } => return true,
      Op::Shutdown { .. } => return true,
      Op::Fsync { .. } => return true,
      Op::OpenAt { .. } => return true,
      Op::Stat { .. } => return true,
      Op::ReadDir { .. } => return true,
      Op::UnlinkAt { .. } => return true,
      Op::RenameAt { .. } => return true,
      Op::MkdirAt { .. } => return true,
      Op::LinkAt { .. } => return true,
      Op::ReadlinkAt { .. } => return true,
      Op::GetCwd { .. } => return true,
      #[cfg(unix)]
      Op::Spawn { .. } => return true,
      _ => return false,
    };

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if syscall!(raw fstat(fd, stat.as_mut_ptr())) < 0 {
      return false;
    }
    // SAFETY: successful `fstat` initialized `stat`.
    let stat = unsafe { stat.assume_init() };

    (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
  }

  /// Validate malformed ops that should fail immediately without readiness registration.
  fn validate_op(op: &crate::backend::op::Op) -> Option<isize> {
    match op {
      Op::Read { iovecs: _, offset, flags, .. } => {
        if *offset < -1 {
          return Some(-(libc::EINVAL as isize));
        }

        const RWF_HIPRI: i32 = 0x00000001;
        const RWF_DSYNC: i32 = 0x00000002;
        const RWF_SYNC: i32 = 0x00000004;
        const RWF_APPEND: i32 = 0x00000010;
        const ALL_KNOWN_FLAGS: i32 =
          RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND;

        if flags & !ALL_KNOWN_FLAGS != 0 {
          return Some(-(libc::ENOTSUP as isize));
        }

        None
      }
      Op::Write { iovecs: _, offset, flags, .. } => {
        if *offset < -1 {
          return Some(-(libc::EINVAL as isize));
        }

        const RWF_HIPRI: i32 = 0x00000001;
        const RWF_DSYNC: i32 = 0x00000002;
        const RWF_SYNC: i32 = 0x00000004;
        const RWF_APPEND: i32 = 0x00000010;
        const ALL_KNOWN_FLAGS: i32 =
          RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND;

        if flags & !ALL_KNOWN_FLAGS != 0 {
          return Some(-(libc::ENOTSUP as isize));
        }

        None
      }
      _ => None,
    }
  }

  /// Try to execute an operation (non-blocking syscall)
  /// Returns positive on success (bytes transferred), negative on error (-errno)
  fn exec_op(op: &crate::backend::op::Op) -> isize {
    use crate::backend::op::Op;

    match op {
      Op::Read { fd, iovecs, iov_count, offset, flags } => {
        let Some(native_iovecs) = Self::lower_raw_iovecs(*iovecs, *iov_count)
        else {
          return -(libc::EINVAL as isize);
        };
        Self::do_readv(
          fd.as_raw_fd(),
          native_iovecs.as_ptr(),
          *iov_count,
          *offset,
          *flags,
        )
      }

      Op::Write { fd, iovecs, iov_count, offset, flags } => {
        let Some(native_iovecs) = Self::lower_raw_iovecs(*iovecs, *iov_count)
        else {
          return -(libc::EINVAL as isize);
        };
        Self::do_writev(
          fd.as_raw_fd(),
          native_iovecs.as_ptr(),
          *iov_count,
          *offset,
          *flags,
        )
      }

      Op::Recv { fd, msg, flags } => {
        let fd = fd.as_raw_fd();
        let Some((mut iovecs, mut hdr, mut addr)) = Self::lower_recv_msg(msg)
        else {
          return -(libc::EINVAL as isize);
        };
        hdr.msg_iov = iovecs.as_mut_ptr();
        hdr.msg_iovlen = msg.buf_count.get() as _;
        if let Some((storage, len)) = addr.as_mut() {
          hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
          hdr.msg_namelen = *len;
        }
        let result = syscall!(raw recvmsg(fd, &mut hdr, *flags));
        if result >= 0
          && let (Some(out), Some((storage, len))) = (msg.from, addr.as_ref())
          && let Ok(addr) = Self::raise_socket_addr(storage, *len)
        {
          unsafe {
            *out.as_ptr() = addr;
          }
        }
        result
      }

      Op::Send { fd, msg, flags } => {
        let fd = fd.as_raw_fd();
        let Some((mut iovecs, mut hdr, mut addr)) = Self::lower_send_msg(msg)
        else {
          return -(libc::EINVAL as isize);
        };
        hdr.msg_iov = iovecs.as_mut_ptr();
        hdr.msg_iovlen = msg.buf_count.get() as _;
        if let Some((storage, len)) = addr.as_mut() {
          hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
          hdr.msg_namelen = *len;
        }
        let result = syscall!(raw sendmsg(fd, &mut hdr, *flags));
        if result == 0 && hdr.msg_iovlen > 0 {
          Self::debug_sendmsg_zero(fd, &hdr);
        }
        result
      }

      Op::Accept { fd, addr } => {
        let fd = fd.as_raw_fd();
        // SAFETY: `sockaddr_storage` is POD and may be zero-initialized.
        let mut storage =
          unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
        let mut len =
          std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let result = syscall!(raw accept(fd, (&mut storage as *mut libc::sockaddr_storage).cast(), &mut len));
        if result >= 0 {
          let accepted_fd = result as RawFd;
          if let Err(err) = Self::configure_socket_fd(accepted_fd) {
            let _ = syscall!(raw close(accepted_fd));
            return -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize);
          }
          match Self::raise_socket_addr(&storage, len) {
            Ok(peer) => {
              // SAFETY: `addr` points to caller-provided writable peer storage.
              unsafe { *addr.as_ptr() = peer }
            }
            Err(err) => {
              let _ = syscall!(raw close(accepted_fd));
              return -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize);
            }
          }
        }
        result
      }

      Op::Connect { fd, .. } => {
        let fd = fd.as_raw_fd();
        let mut err: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        let ret = syscall!(raw getsockopt(
          fd,
          libc::SOL_SOCKET,
          libc::SO_ERROR,
          (&mut err as *mut i32).cast(),
          &mut len,
        ));

        if ret < 0 {
          ret
        } else if err == 0 {
          0
        } else {
          -(err as isize)
        }
      }

      Op::OpenAt { dir_fd, path, flags, mode } => syscall!(raw openat(
        dir_fd.as_raw_fd(),
        path.as_ptr(),
        *flags,
        *mode,
      )),
      Op::Stat { target, out } => {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = match target {
          crate::backend::op::StatTarget::Path {
            dir_fd,
            path,
            follow_symlinks,
          } => {
            let flags =
              if *follow_symlinks { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
            syscall!(raw fstatat(
              dir_fd.as_raw_fd(),
              path.as_ptr(),
              stat.as_mut_ptr(),
              flags,
            ))
          }
          crate::backend::op::StatTarget::Fd { fd } => {
            syscall!(raw fstat(fd.as_raw_fd(), stat.as_mut_ptr()))
          }
        };
        if result >= 0 {
          // SAFETY: successful `fstatat`/`fstat` initialized `stat`.
          let stat = unsafe { stat.assume_init() };
          // SAFETY: `out` points to caller-provided writable result storage.
          unsafe {
            *out.as_ptr() = crate::backend::op::file_stat_from_raw(&stat);
          }
        }
        result
      }
      Op::ReadDir {
        fd,
        raw_buf,
        raw_cap,
        entries,
        entries_cap,
        opaque,
        opaque_drop,
        out,
      } => {
        // SAFETY: `raw_buf` points to `raw_cap` writable bytes.
        let raw =
          unsafe { std::slice::from_raw_parts_mut(raw_buf.as_ptr(), *raw_cap) };
        // SAFETY: `entries` points to `entries_cap` writable `DirEntryRef`s.
        let entries = unsafe {
          std::slice::from_raw_parts_mut(entries.as_ptr(), *entries_cap)
        };
        // SAFETY: `opaque` references backend-owned continuation state.
        let opaque = unsafe { &mut *opaque.as_ptr() };
        // SAFETY: `opaque_drop` references backend-owned continuation state.
        let opaque_drop = unsafe { &mut *opaque_drop.as_ptr() };
        match Self::read_dir_entries(
          fd.as_raw_fd(),
          opaque,
          opaque_drop,
          raw,
          entries,
        ) {
          Ok(result) => {
            // SAFETY: `out` points to caller-provided writable result storage.
            unsafe {
              *out.as_ptr() = result;
            }
            0
          }
          Err(err) => -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize),
        }
      }
      Op::UnlinkAt { dir_fd, path, flags } => syscall!(raw unlinkat(
        dir_fd.as_raw_fd(),
        path.as_ptr(),
        *flags,
      )),
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        syscall!(raw renameat(
          old_dir_fd.as_raw_fd(),
          old_path.as_ptr(),
          new_dir_fd.as_raw_fd(),
          new_path.as_ptr(),
        ))
      }
      Op::MkdirAt { dir_fd, path, mode } => {
        syscall!(raw mkdirat(
          dir_fd.as_raw_fd(),
          path.as_ptr(),
          *mode as libc::mode_t,
        ))
      }
      Op::LinkAt { kind, source_dir_fd, source_path, new_dir_fd, new_path } => {
        match kind {
          crate::backend::op::LinkKind::Hard => syscall!(raw linkat(
            source_dir_fd.as_raw_fd(),
            source_path.as_ptr(),
            new_dir_fd.as_raw_fd(),
            new_path.as_ptr(),
            0,
          )),
          crate::backend::op::LinkKind::Soft => syscall!(raw symlinkat(
            source_path.as_ptr(),
            new_dir_fd.as_raw_fd(),
            new_path.as_ptr(),
          )),
        }
      }
      Op::ReadlinkAt { dir_fd, path, buf, buf_len } => syscall!(raw readlinkat(
        dir_fd.as_raw_fd(),
        path.as_ptr(),
        buf.as_ptr().cast::<libc::c_char>(),
        *buf_len,
      )),
      Op::GetCwd { buf, buf_len } => {
        // SAFETY: `buf` points to a writable caller-provided buffer.
        let result = unsafe {
          libc::getcwd(buf.as_ptr().cast::<libc::c_char>(), *buf_len)
        };
        if result.is_null() {
          -(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EINVAL) as isize)
        } else {
          // SAFETY: successful `getcwd` returns a NUL-terminated string.
          unsafe { libc::strlen(result) as isize }
        }
      }
      #[cfg(unix)]
      Op::Spawn { path, argv, envp } => {
        unsafe extern "C" {
          static mut environ: *mut *mut libc::c_char;
        }
        let mut pid: libc::pid_t = 0;
        let envp = if let Some(envp) = envp {
          envp.as_ptr().cast_const()
        } else {
          // SAFETY: `environ` is the process-global environment vector.
          unsafe { environ as *const *mut libc::c_char }
        };
        // SAFETY: all pointers passed to `posix_spawn` remain valid for the call.
        let result = unsafe {
          libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv.as_ptr().cast_const(),
            envp,
          )
        };
        if result != 0 { -(result as isize) } else { pid as isize }
      }

      Op::Socket { domain, ty, proto } => {
        match crate::backend::op::socket_to_raw(*domain, *ty, *proto) {
          Ok((domain, ty, proto)) => {
            let result = syscall!(raw socket(domain, ty, proto));
            if result < 0 {
              result
            } else {
              let fd = result as RawFd;
              if let Err(err) = Self::configure_socket_fd(fd) {
                let _ = syscall!(raw close(fd));
                -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize)
              } else {
                result
              }
            }
          }
          Err(errno) => -(errno as isize),
        }
      }
      Op::Bind { fd, addr } => {
        let storage = crate::api::ops::std_socketaddr_into_libc(*addr);
        let len = match addr {
          SocketAddr::V4(_) => std::mem::size_of::<libc::sockaddr_in>(),
          SocketAddr::V6(_) => std::mem::size_of::<libc::sockaddr_in6>(),
        } as libc::socklen_t;
        syscall!(raw bind(
          fd.as_raw_fd(),
          (&storage as *const libc::sockaddr_storage).cast(),
          len
        ))
      }
      Op::Listen { fd, backlog } => {
        syscall!(raw listen(fd.as_raw_fd(), *backlog))
      }
      Op::Shutdown { fd, how } => {
        syscall!(raw shutdown(fd.as_raw_fd(), *how))
      }
      Op::Fsync { fd } => {
        syscall!(raw fsync(fd.as_raw_fd()))
      }

      Op::Nop => 0,
    }
  }
}

impl IoBackend for Poller {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.sys = Some(sys::OsPoller::new()?);
    self.events = Events::with_capacity(cap.min(4096));
    self.capacity = cap;
    self.backlog.clear();
    self.backlog.reserve_exact(cap);
    self.pending = Slab::new(cap);
    self.queued_completed = Vec::with_capacity(cap.min(256));
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(
    &mut self,
    id: u64,
    op: crate::backend::op::Op,
    _step_bump: &mut Bump,
  ) {
    assert!(
      self.backlog.len() + self.pending.len() < self.capacity,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.capacity
    );
    self.backlog.push(PendingOp { registration_id: id, op });
  }

  fn flush(&mut self) -> io::Result<()> {
    while let Some(entry) = self.backlog.pop() {
      if matches!(entry.op, Op::Nop) {
        self.queued_completed.push(OpCompleted::new(entry.registration_id, 0));
        continue;
      };

      if let Some(result) = Self::validate_op(&entry.op) {
        self
          .queued_completed
          .push(OpCompleted::new(entry.registration_id, result));
        continue;
      }

      if Self::should_complete_immediately(&entry.op) {
        let result = Self::exec_op(&entry.op);
        self
          .queued_completed
          .push(OpCompleted::new(entry.registration_id, result));
        continue;
      }

      if let Op::Connect { fd, addr } = &entry.op {
        let Ok((storage, len)) = Self::lower_socket_addr(addr) else {
          self.queued_completed.push(OpCompleted::new(
            entry.registration_id,
            -(libc::EINVAL as isize),
          ));
          continue;
        };
        let result = syscall!(raw connect(
          fd.as_raw_fd(),
          (&storage as *const libc::sockaddr_storage).cast(),
          len,
        ));

        match result {
          0 => {
            self
              .queued_completed
              .push(OpCompleted::new(entry.registration_id, 0));
            continue;
          }
          #[allow(unreachable_patterns)]
          EAGAIN_NEG | EWOULDBLOCK_NEG => {}
          #[cfg(unix)]
          x if x == -(libc::EINPROGRESS as isize) => {}
          _ => {
            self
              .queued_completed
              .push(OpCompleted::new(entry.registration_id, result));
            continue;
          }
        }
      }

      let (fd, interest) = self.extract_fd_interest(&entry.op);

      // Operation-local registration failures still belong to the op result
      // contract, so surface them as raw completions instead of bubbling them
      // out as backend infrastructure errors.
      let registration_id = entry.registration_id;
      let Some((pending_key, _entry)) = self.pending.insert_get_mut(entry)
      else {
        let result = -(libc::EIO as isize);
        self.queued_completed.push(OpCompleted::new(registration_id, result));
        continue;
      };

      if let Err(err) = self.sys().add(fd, pending_key.as_u64(), interest) {
        let result = err
          .raw_os_error()
          .map(|code| -(code as isize))
          .unwrap_or(-(libc::EIO as isize));
        self.queued_completed.push(OpCompleted::new(registration_id, result));
        let _ = self.pending.remove(pending_key);
        continue;
      }
    }

    Ok(())
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    if !self.queued_completed.is_empty() {
      completed.append(&mut self.queued_completed);
      return Ok(());
    }

    let sys = self.sys.as_ref().unwrap();

    {
      let event_count = sys.wait(self.events.as_buf(), timeout)?;
      // SAFETY: `wait()` initialized exactly `event_count` events in the buffer.
      unsafe { self.events.set_len(event_count) };
    }

    while let Some(event) = self.events.pop() {
      let key = SlabKey::from_u64(event.key);
      let Some(entry) = self.pending.remove_value(key) else {
        continue; // Cancelled or doesn't exist
      };

      // Execute the operation
      let result = Poller::exec_op(&entry.op);

      match result {
        #[allow(unreachable_patterns)]
        EAGAIN_NEG | EWOULDBLOCK_NEG => {
          // EAGAIN/EWOULDBLOCK - re-register for retry
          let (fd, interest) = self.extract_fd_interest(&entry.op);
          let registration_id = entry.registration_id;
          let (pending_key, _entry) = self
            .pending
            .insert_get_mut(entry)
            .expect("pending retry must fit backend capacity");
          if let Err(err) = sys.add(fd, pending_key.as_u64(), interest) {
            let _ = self.pending.remove(pending_key);
            let result = err
              .raw_os_error()
              .map(|code| -(code as isize))
              .unwrap_or(-(libc::EIO as isize));
            self.completed.push(OpCompleted::new(registration_id, result));
            continue;
          }
        }
        _ => {
          let (fd, interest) = self.extract_fd_interest(&entry.op);
          if !interest.is_none() {
            let _ = sys.delete(fd);
          }
          // Success or real error - add to completions
          self.completed.push(OpCompleted::new(entry.registration_id, result));
        }
      }
    }

    completed.append(&mut self.completed);

    Ok(())
  }
}
