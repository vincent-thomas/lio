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
  os::fd::{AsRawFd, RawFd},
  time::Duration,
};

use crate::backend::{
  IoBackend, OpCompleted, op::Op, pollingv2::interest::Interest,
};
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
pub(super) trait ReadinessPoll {
  /// The native event type used by this implementation
  type NativeEvent;

  /// Add interest for a file descriptor (one-shot mode).
  /// This is not idempotent.
  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()>;

  /// Add interest for a file descriptor (level-triggered mode).
  /// Unlike `add`, this stays registered and fires on every readiness.
  fn add_level(
    &self,
    fd: RawFd,
    key: u64,
    interest: Interest,
  ) -> io::Result<()>;

  /// Modify existing interest for a file descriptor
  /// This is idempotent, but fails if not added before.
  fn modify(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()>;

  /// Remove all interest for a file descriptor
  /// This fails if 'fd' hasn't previously been added.
  fn delete(&self, fd: RawFd) -> io::Result<()>;

  /// Remove a timer by key (for timers that don't have fds)
  /// This fails if 'key' hasn't previously been added as a timer.
  fn delete_timer(&self, key: u64) -> io::Result<()>;

  /// Wait for events, filling the provided buffer
  /// Returns the number of events received
  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize>;

  /// Wake up a potentially blocking wait call
  fn notify(&self) -> io::Result<()>;

  /// Extract the key from a native event
  fn event_key(event: &Self::NativeEvent) -> u64;

  /// Extract the interest from a native event
  fn event_interest(event: &Self::NativeEvent) -> Interest;

  /// Extract fflags from a native event (kqueue VNODE events).
  /// Returns 0 on platforms that don't support fflags.
  fn event_fflags(_event: &Self::NativeEvent) -> u32 {
    0
  }

  /// Arms the timing wheel's kernel timer to fire after `duration`.
  ///
  /// This is used by Lio to wake up when the earliest timer in the
  /// timing wheel expires. Only one wheel timer can be armed at a time;
  /// calling this again replaces the previous timer.
  fn arm_wheel_timer(&self, duration: Duration) -> io::Result<()>;

  /// Disarms the timing wheel's kernel timer if one is armed.
  ///
  /// This is a no-op if no timer is currently armed.
  fn disarm_wheel_timer(&self) -> io::Result<()>;

  /// Returns true if the given key is the wheel timer key.
  fn is_wheel_timer_key(key: u64) -> bool;
}

/// Represents a readiness event from the poller
#[derive(Debug, Clone, Copy)]
pub struct Event {
  /// User-provided key to identify this event
  pub key: u64,
  /// The interest flags that triggered (what actually happened)
  pub interest: Interest,
  /// For VNODE events (kqueue): the fflags indicating what changed
  pub fflags: u32,
}

/// A collection of events returned from polling
pub struct Events {
  events: Vec<<sys::OsPoller as ReadinessPoll>::NativeEvent>,
}

impl AsMut<[<sys::OsPoller as ReadinessPoll>::NativeEvent]> for Events {
  fn as_mut(&mut self) -> &mut [<sys::OsPoller as ReadinessPoll>::NativeEvent] {
    &mut self.events
  }
}

impl Default for Events {
  fn default() -> Self {
    Self::with_capacity(512)
  }
}

impl Events {
  /// Create a new empty events collection with specified capacity
  pub fn with_capacity(capacity: usize) -> Self {
    Self { events: Vec::with_capacity(capacity) }
  }

  /// Get the number of events
  pub fn len(&self) -> usize {
    self.events.len()
  }

  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
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
#[derive(Default)]
pub struct Poller {
  sys: Option<sys::OsPoller>,
  events: Events,
  capacity: usize,
  backlog: Vec<BacklogEntry>,
  /// Operations that have been registered with the poller and are waiting for readiness
  pending: std::collections::HashMap<u64, BacklogEntry>,
  /// Immediate completions produced during `flush()` and surfaced on the next `wait()`.
  queued_completed: Vec<OpCompleted>,
  /// Reusable buffer for completed operations
  completed: Vec<OpCompleted>,
}

pub struct BacklogEntry {
  id: u64,
  op: crate::backend::op::Op,
}

// EAGAIN and EWOULDBLOCK are the same on Linux, but may differ on other systems
const EAGAIN_NEG: isize = -(libc::EAGAIN as isize);
const EWOULDBLOCK_NEG: isize = -(libc::EWOULDBLOCK as isize);

impl Poller {
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
    if offset < -1 || iovecs == std::ptr::null() {
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
    if offset < -1 || iovecs == std::ptr::null() {
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
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = syscall!(raw fstat(fd, &mut stat)?);
        assert_eq!(result, 0);
        let file_size = stat.st_size as i64;
        file_size
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
      crate::backend::op::Op::Nop => (0, Interest::NONE),
    }
  }

  fn should_complete_immediately(op: &crate::backend::op::Op) -> bool {
    let fd = match op {
      Op::Read { fd, .. } | Op::Write { fd, .. } => fd.as_raw_fd(),
      _ => return false,
    };

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if syscall!(raw fstat(fd, &mut stat)) < 0 {
      return false;
    }

    (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
  }

  /// Validate malformed ops that should fail immediately without readiness registration.
  fn validate_op(op: &crate::backend::op::Op) -> Option<isize> {
    match op {
      Op::Read { iovecs, offset, flags, .. } => {
        if *offset < -1 || iovecs.is_null() {
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
      Op::Write { iovecs, offset, flags, .. } => {
        if *offset < -1 || iovecs.is_null() {
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
      Op::Read { fd, iovecs, iov_count, offset, flags } => Self::do_readv(
        fd.as_raw_fd(),
        iovecs.cast::<libc::iovec>(),
        *iov_count,
        *offset,
        *flags,
      ),

      Op::Write { fd, iovecs, iov_count, offset, flags } => Self::do_writev(
        fd.as_raw_fd(),
        iovecs.cast::<libc::iovec>(),
        *iov_count,
        *offset,
        *flags,
      ),

      Op::Recv { fd, msg, flags } => {
        let fd = fd.as_raw_fd();
        syscall!(raw recvmsg(fd, *msg, *flags))
      }

      Op::Send { fd, msg, flags } => {
        let fd = fd.as_raw_fd();
        syscall!(raw sendmsg(fd, *msg as *mut _, *flags))
      }

      Op::Accept { fd, addr, len } => {
        let fd = fd.as_raw_fd();
        syscall!(raw accept(fd, (*addr).cast(), *len))
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
    self.pending = std::collections::HashMap::with_capacity(cap);
    self.queued_completed = Vec::with_capacity(cap.min(256));
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, op: crate::backend::op::Op) {
    assert!(
      self.backlog.len() + self.pending.len() < self.capacity,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.capacity
    );
    self.backlog.push(BacklogEntry { id, op });
  }

  fn flush(&mut self) -> io::Result<()> {
    while let Some(entry) = self.backlog.pop() {
      if matches!(entry.op, Op::Nop) {
        continue;
      };

      if let Some(result) = Self::validate_op(&entry.op) {
        self.queued_completed.push(OpCompleted::new(entry.id, result));
        continue;
      }

      if Self::should_complete_immediately(&entry.op) {
        let result = Self::exec_op(&entry.op);
        self.queued_completed.push(OpCompleted::new(entry.id, result));
        continue;
      }

      if let Op::Connect { fd, addr, len } = &entry.op {
        let result = syscall!(raw connect(
          fd.as_raw_fd(),
          (*addr).cast(),
          *len,
        ));

        match result {
          0 => {
            self.queued_completed.push(OpCompleted::new(entry.id, 0));
            continue;
          }
          #[allow(unreachable_patterns)]
          EAGAIN_NEG | EWOULDBLOCK_NEG => {}
          #[cfg(unix)]
          x if x == -(libc::EINPROGRESS as isize) => {}
          _ => {
            self.queued_completed.push(OpCompleted::new(entry.id, result));
            continue;
          }
        }
      }

      let (fd, interest) = self.extract_fd_interest(&entry.op);

      // Operation-local registration failures still belong to the op result
      // contract, so surface them as raw completions instead of bubbling them
      // out as backend infrastructure errors.
      if let Err(err) = self.sys().add(fd, entry.id, interest) {
        let result = err
          .raw_os_error()
          .map(|code| -(code as isize))
          .unwrap_or(-(libc::EIO as isize));
        self.queued_completed.push(OpCompleted::new(entry.id, result));
        continue;
      }

      // Move to pending operations
      self.pending.insert(entry.id, entry);
    }

    Ok(())
  }

  fn wait(&mut self, timeout: Option<Duration>) -> io::Result<&[OpCompleted]> {
    self.completed.clear();
    if !self.queued_completed.is_empty() {
      self.completed.append(&mut self.queued_completed);
      return Ok(&self.completed);
    }

    let sys = self.sys.as_ref().unwrap();

    {
      let event_count = sys.wait(self.events.as_buf(), timeout)?;
      unsafe { self.events.set_len(event_count) };
    }

    while let Some(event) = self.events.pop() {
      let Some(entry) = self.pending.remove(&event.key) else {
        continue; // Cancelled or doesn't exist
      };

      // Execute the operation
      let result = Poller::exec_op(&entry.op);

      match result {
        #[allow(unreachable_patterns)]
        EAGAIN_NEG | EWOULDBLOCK_NEG => {
          // EAGAIN/EWOULDBLOCK - re-register for retry
          let (fd, interest) = self.extract_fd_interest(&entry.op);
          sys.add(fd, entry.id, interest)?;
          self.pending.insert(entry.id, entry);
        }
        _ => {
          let (fd, interest) = self.extract_fd_interest(&entry.op);
          if !interest.is_none() {
            let _ = sys.delete(fd);
          }
          // Success or real error - add to completions
          self.completed.push(OpCompleted::new(entry.id, result));
        }
      }
    }

    Ok(&self.completed)
  }
}

#[cfg(test)]
crate::test_io_backend!(Poller::new());
