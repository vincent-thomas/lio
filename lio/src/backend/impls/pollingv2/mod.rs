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

use core::slice;
use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use crate::backend::pollingv2::interest::Interest;
use crate::backend::{IoBackend, OpCompleted};
// use crate::operation::Operation;
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
pub trait ReadinessPoll {
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
    // SAFETY: The native event type (libc::kevent or libc::epoll_event) is a C struct
    // that is safe to zero-initialize. All fields are primitive types where zero is valid.
    Self { events: vec![unsafe { std::mem::zeroed() }; capacity] }
  }

  /// Get an iterator over the events
  pub fn iter(&self) -> EventsIter<'_> {
    EventsIter { events: self, index: 0 }
  }

  /// Get the number of events
  pub fn len(&self) -> usize {
    self.events.len()
  }

  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Returns the vec of maybe-initialised values. Meant for OS to fill and
  /// then we set correct length.
  unsafe fn as_raw_buf(
    &mut self,
  ) -> &mut [<sys::OsPoller as ReadinessPoll>::NativeEvent] {
    // SAFETY: We create a slice spanning the full capacity of the vector.
    // The caller must ensure they call set_len() with the actual number of events
    // written by the OS before reading the events. The pointer is valid because
    // it comes from a Vec that owns the allocation.
    unsafe {
      slice::from_raw_parts_mut(
        self.events.as_mut_ptr(),
        self.events.capacity(),
      )
    }
  }

  unsafe fn set_len(&mut self, len: usize) {
    assert!(len <= self.events.capacity(), "set_len: len must be <= capacity");
    // SAFETY: The caller guarantees that the first `len` elements have been initialized
    // by the OS's wait() call. We've verified len <= capacity above.
    unsafe { self.events.set_len(len) }
  }

  fn clear(&mut self) {
    self.events.clear();
  }

  fn get_event(&self, index: usize) -> Event {
    assert!(index < self.events.len(), "get_event: index out of bounds");
    let native_event = &self.events[index];
    let key = sys::OsPoller::event_key(native_event);
    let interest = sys::OsPoller::event_interest(native_event);
    let fflags = sys::OsPoller::event_fflags(native_event);

    Event { key, interest, fflags }
  }
}

/// Iterator over events
pub struct EventsIter<'a> {
  events: &'a Events,
  index: usize,
}

impl<'a> Iterator for EventsIter<'a> {
  type Item = Event;

  fn next(&mut self) -> Option<Self::Item> {
    if self.index >= self.events.len() {
      return None;
    }

    let event = self.events.get_event(self.index);
    self.index += 1;

    // Filter out internal events (notification and wheel timer)
    if sys::OsPoller::is_wheel_timer_key(event.key) || event.key == u64::MAX {
      return self.next();
    }

    Some(event)
  }
}

/// Immediate completion for operations that don't need polling.
struct ImmediateCompletion {
  id: u64,
  result: isize,
}

/// Polling-based I/O backend for epoll (Linux) and kqueue (BSD/macOS).
///
/// This backend uses readiness-based polling to handle I/O operations.
/// It's less efficient than io_uring but works on more platforms.
///
/// # Example
///
/// ```
/// use lio::backend::{IoBackend, op::Op, pollingv2::Poller};
/// use std::time::Duration;
///
/// let mut backend = Poller::default();
/// backend.init(1024).unwrap();
///
/// backend.push(1, Op::Nop).unwrap();
/// backend.flush().unwrap();
///
/// let completions = backend.wait_timeout(Some(Duration::ZERO)).unwrap();
/// ```
#[derive(Default)]
pub struct Poller {
  /// The OS-specific polling mechanism (epoll/kqueue)
  sys: Option<sys::OsPoller>,
  /// Map of operation ID to file descriptor (for cleanup)
  fd_map: Option<HashMap<u64, RawFd>>,
  /// Map of operation ID to Op (for completion)
  op_map: std::collections::HashMap<u64, crate::backend::op::Op>,
  /// Event buffer for polling
  events: Events,
  /// Immediate completions (operations that completed without polling)
  immediate: Vec<ImmediateCompletion>,
  /// Reusing the completed allocation.
  completed: Vec<OpCompleted>,
  /// Timeout timer tracking: op_id -> timer_duration_ms
  /// When an op has an entry here, it has both an fd registration AND a timer.
  #[cfg(unix)]
  timeout_timers: HashMap<u64, i32>,
  /// Watch operation tracking: op_id -> (owned_fd, is_inotify)
  /// Stores the fd we opened/created for watch operations so we can close it on completion.
  #[cfg(unix)]
  watch_fds: HashMap<u64, (RawFd, bool)>,
}

impl Poller {
  /// Create a new uninitialized poller.
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn sys(&self) -> &sys::OsPoller {
    self.sys.as_ref().expect("Poller not initialized - call init() first")
  }

  #[inline]
  fn fd_map(&mut self) -> &mut HashMap<u64, RawFd> {
    self.fd_map.as_mut().expect("Poller not initialized - call init() first")
  }

  /// Execute a syscall for the given operation.
  #[inline]
  fn run_op(op: &crate::backend::op::Op) -> isize {
    use crate::backend::op::Op;
    use std::os::fd::AsRawFd;

    match op {
      Op::Send { fd, flags, buf } => {
        syscall!(raw send(fd.as_raw_fd(), buf.ptr as *const _, buf.len, *flags))
      }
      Op::Recv { fd, flags, buf } => {
        syscall!(raw recv(fd.as_raw_fd(), buf.ptr as *mut _, buf.len, *flags))
      }
      Op::SendTo { fd, flags, buf, addr, addrlen, .. } => {
        syscall!(raw sendto(fd.as_raw_fd(), buf.ptr as *const _, buf.len, *flags, *addr as *const libc::sockaddr, *addrlen))
      }
      Op::RecvFrom { fd, flags, buf, addr, addrlen, .. } => {
        syscall!(raw recvfrom(fd.as_raw_fd(), buf.ptr as *mut _, buf.len, *flags, *addr as *mut libc::sockaddr, *addrlen))
      }
      Op::Accept { fd, addr, len } => {
        syscall!(raw accept(fd.as_raw_fd(), *addr as *mut _, *len))
      }
      Op::AcceptStream { fd } => {
        syscall!(raw accept(fd.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut()))
      }
      Op::Connect { fd, addr, len, connect_called } => {
        let ret = syscall!(raw connect(fd.as_raw_fd(), *addr as *const libc::sockaddr, *len));
        if ret == -(libc::EISCONN as isize) && *connect_called {
          0
        } else {
          ret
        }
      }
      Op::Bind { fd, addr, addrlen } => {
        syscall!(raw bind(fd.as_raw_fd(), *addr as *const libc::sockaddr, *addrlen))
      }
      Op::Listen { fd, backlog } => {
        syscall!(raw listen(fd.as_raw_fd(), *backlog))
      }
      Op::Shutdown { fd, how } => {
        syscall!(raw shutdown(fd.as_raw_fd(), *how))
      }
      Op::Socket { domain, ty, proto } => {
        syscall!(raw socket(*domain, *ty, *proto))
      }
      Op::OpenAt { dir_fd, path, flags, mode } => {
        syscall!(raw openat(dir_fd.as_raw_fd(), *path, *flags, *mode))
      }
      Op::Close { fd } => {
        syscall!(raw close(*fd))
      }
      Op::Fsync { fd } => {
        syscall!(raw fsync(fd.as_raw_fd()))
      }
      Op::Truncate { fd, size } => {
        syscall!(raw ftruncate(fd.as_raw_fd(), *size as libc::off_t))
      }
      Op::LinkAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        syscall!(raw linkat(old_dir_fd.as_raw_fd(), *old_path, new_dir_fd.as_raw_fd(), *new_path, 0))
      }
      Op::SymlinkAt { target, linkpath, dir_fd } => {
        syscall!(raw symlinkat(*target, dir_fd.as_raw_fd(), *linkpath))
      }
      Op::UnlinkAt { dir_fd, path, flags } => {
        syscall!(raw unlinkat(dir_fd.as_raw_fd(), *path, *flags))
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        syscall!(raw renameat(old_dir_fd.as_raw_fd(), *old_path, new_dir_fd.as_raw_fd(), *new_path))
      }
      Op::MkdirAt { dir_fd, path, mode } => {
        syscall!(raw mkdirat(dir_fd.as_raw_fd(), *path, *mode as libc::mode_t))
      }
      Op::ReadV { fd, iovecs, iov_count, .. } => {
        syscall!(raw readv(fd.as_raw_fd(), *iovecs, *iov_count as libc::c_int))
      }
      Op::WriteV { fd, iovecs, iov_count, .. } => {
        syscall!(raw writev(fd.as_raw_fd(), *iovecs, *iov_count as libc::c_int))
      }
      Op::ReadVAt { fd, iovecs, iov_count, offset, .. } => {
        syscall!(raw preadv(fd.as_raw_fd(), *iovecs, *iov_count as libc::c_int, *offset))
      }
      Op::WriteVAt { fd, iovecs, iov_count, offset, .. } => {
        syscall!(raw pwritev(fd.as_raw_fd(), *iovecs, *iov_count as libc::c_int, *offset))
      }
      Op::Sleep { duration, .. } => {
        std::thread::sleep(*duration);
        0
      }
      Op::Nop => 0,
      #[cfg(target_os = "linux")]
      Op::Splice { fd_in, off_in, fd_out, off_out, len, flags } => {
        let mut off_in_val = *off_in;
        let mut off_out_val = *off_out;
        let off_in_ptr =
          if *off_in == -1 { std::ptr::null_mut() } else { &mut off_in_val };
        let off_out_ptr =
          if *off_out == -1 { std::ptr::null_mut() } else { &mut off_out_val };
        syscall!(raw splice(fd_in.as_raw_fd(), off_in_ptr, fd_out.as_raw_fd(), off_out_ptr, *len as usize, *flags as libc::c_uint))
      }
      #[cfg(target_os = "linux")]
      Op::SendFile { out_fd, in_fd, offset, count } => {
        let mut off = *offset;
        syscall!(raw sendfile(out_fd.as_raw_fd(), in_fd.as_raw_fd(), &mut off, *count))
      }
      Op::SendFile { out_fd, in_fd, offset, count } => {
        #[cfg(target_vendor = "apple")]
        {
          let mut len: libc::off_t = *count as libc::off_t;
          let ret = syscall!(raw sendfile(in_fd.as_raw_fd(), out_fd.as_raw_fd(), *offset, &mut len, std::ptr::null_mut(), 0));
          if ret == 0 { len as isize } else { ret }
        }
        #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
        {
          let mut sbytes: libc::off_t = 0;
          let ret = syscall!(raw sendfile(in_fd.as_raw_fd(), out_fd.as_raw_fd(), *offset, *count, std::ptr::null_mut(), &mut sbytes, 0));
          if ret == 0 { sbytes as isize } else { ret }
        }
      }
      #[cfg(target_os = "linux")]
      Op::CopyFileRange { fd_in, off_in, fd_out, off_out, len, flags } => {
        let mut off_in_val = *off_in;
        let mut off_out_val = *off_out;
        syscall!(raw copy_file_range(fd_in.as_raw_fd(), &mut off_in_val, fd_out.as_raw_fd(), &mut off_out_val, *len, *flags as libc::c_uint))
      }
      #[cfg(target_os = "linux")]
      Op::Tee { fd_in, fd_out, size } => {
        syscall!(raw tee(fd_in.as_raw_fd(), fd_out.as_raw_fd(), *size as libc::size_t, 0))
      }
      #[cfg(unix)]
      Op::Timeout { .. } => -(libc::ENOTSUP as isize),
      #[cfg(unix)]
      Op::Watch { .. } => -(libc::ENOTSUP as isize),
      #[cfg(unix)]
      Op::Waitid { idtype, id, options, infop } => {
        syscall!(raw waitid(*idtype, *id, *infop, *options))
      }
      #[cfg(unix)]
      Op::Spawn { path, argv, envp, pid, file_actions } => {
        // SAFETY: All pointers are valid and owned by the Op
        let ret = unsafe {
          libc::posix_spawn(
            *pid,
            *path,
            *file_actions,
            std::ptr::null(),
            *argv as *const *mut _,
            *envp as *const *mut _,
          )
        };
        if ret == 0 { 0 } else { -(ret as isize) }
      }
      #[cfg(unix)]
      Op::Flock { fd, operation } => {
        syscall!(raw flock(fd.as_raw_fd(), *operation))
      }
      #[cfg(unix)]
      Op::GetDents { fd, buf } => {
        #[cfg(target_os = "linux")]
        {
          syscall!(raw syscall(libc::SYS_getdents64, fd.as_raw_fd(), buf.ptr as *mut libc::c_void, buf.len))
        }
        #[cfg(target_os = "macos")]
        {
          unsafe extern "C" {
            fn __getdirentries64(
              fd: libc::c_int,
              buf: *mut libc::c_char,
              nbytes: libc::c_int,
              basep: *mut libc::c_long,
            ) -> libc::c_int;
          }
          let mut basep: libc::c_long = 0;
          // SAFETY: fd is a valid directory fd, buf is a valid buffer
          let ret = unsafe {
            __getdirentries64(
              fd.as_raw_fd(),
              buf.ptr as *mut libc::c_char,
              buf.len as libc::c_int,
              &mut basep,
            )
          };
          if ret < 0 {
            -(std::io::Error::last_os_error()
              .raw_os_error()
              .unwrap_or(libc::EIO) as isize)
          } else {
            ret as isize
          }
        }
        #[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
        {
          let mut basep: libc::off_t = 0;
          syscall!(raw getdirentries(fd.as_raw_fd(), buf.ptr as *mut libc::c_char, buf.len as libc::c_int, &mut basep))
        }
        #[cfg(not(any(
          target_os = "linux",
          target_os = "macos",
          target_os = "freebsd",
          target_os = "dragonfly"
        )))]
        {
          let _ = (fd, buf);
          -(libc::ENOTSUP as isize)
        }
      }
      #[cfg(unix)]
      Op::Signal { .. } => -(libc::ENOTSUP as isize),
    }
  }
}

impl IoBackend for Poller {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.sys = Some(sys::OsPoller::new()?);
    self.fd_map = Some(HashMap::with_capacity(cap));
    self.op_map = std::collections::HashMap::with_capacity(cap);
    self.events = Events::with_capacity(cap.min(4096));
    self.immediate = Vec::with_capacity(64);
    self.completed = Vec::with_capacity(cap.min(256));
    #[cfg(unix)]
    {
      self.timeout_timers = HashMap::with_capacity(64);
      self.watch_fds = HashMap::with_capacity(16);
    }
    Ok(())
  }

  fn push(&mut self, id: u64, op: crate::backend::op::Op) -> io::Result<()> {
    use crate::backend::op::Op;
    use crate::backend::pollingv2::interest::Interest;
    use std::os::fd::AsRawFd;

    // Handle Op::Timeout first since we need to take ownership of inner
    #[cfg(unix)]
    if let Op::Timeout { inner, duration, .. } = op {
      // For timeout-wrapped ops, behavior depends on the inner op type:
      // - Nop: completes immediately, timeout irrelevant
      // - Sleep: race two timers, shorter one wins
      // - I/O ops: register fd + timer, whichever fires first wins
      let (fd, interest) = match inner.as_ref() {
        Op::Nop => {
          // Nop completes instantly - timeout never matters
          self.immediate.push(ImmediateCompletion { id, result: 0 });
          return Ok(());
        }
        Op::Sleep { duration: sleep_duration, .. } => {
          // Race two timers: use the shorter duration
          // If timeout is shorter, return -ECANCELED; otherwise normal sleep result
          let sleep_result = {
            #[cfg(target_os = "linux")]
            {
              -(libc::ETIME as isize)
            }
            #[cfg(any(
              target_os = "macos",
              target_os = "freebsd",
              target_os = "dragonfly"
            ))]
            {
              -(libc::ETIMEDOUT as isize)
            }
            #[cfg(not(any(
              target_os = "linux",
              target_os = "macos",
              target_os = "freebsd",
              target_os = "dragonfly"
            )))]
            {
              0
            }
          };
          let (result, effective_duration) = if duration < *sleep_duration {
            (-(libc::ECANCELED as isize), duration)
          } else {
            (sleep_result, *sleep_duration)
          };
          // Register a single timer for the shorter duration
          let duration_ms = effective_duration.as_millis() as RawFd;
          self.fd_map().insert(id, duration_ms);
          self.sys().add(duration_ms, id, Interest::TIMER)?;
          // Store the result to return when timer fires
          // We'll use a special marker in op_map to indicate pre-determined result
          // Actually, simpler: just store the inner Sleep op and handle specially
          // For now, store result in timeout_timers map (overload: negative = use as result)
          self.timeout_timers.insert(id, result as i32);
          self.op_map.insert(id, *inner);
          return Ok(());
        }
        Op::Recv { fd, .. } | Op::RecvFrom { fd, .. } => {
          (fd.as_raw_fd(), Interest::READ)
        }
        Op::Send { fd, .. } | Op::SendTo { fd, .. } => {
          (fd.as_raw_fd(), Interest::WRITE)
        }
        Op::Accept { fd, .. } => (fd.as_raw_fd(), Interest::READ),
        _ => {
          // Other ops don't support timeout on pollingv2
          let result = -(libc::ENOTSUP as isize);
          self.immediate.push(ImmediateCompletion { id, result });
          return Ok(());
        }
      };

      // Register the fd for I/O readiness
      self.fd_map().insert(id, fd);
      if let Err(e) = self.sys().add(fd, id, interest) {
        self.fd_map().remove(&id);
        let errno = e.raw_os_error().unwrap_or(libc::EIO);
        self
          .immediate
          .push(ImmediateCompletion { id, result: -(errno as isize) });
        return Ok(());
      }

      // Register the timeout timer
      let duration_ms = duration.as_millis() as i32;
      if let Err(e) = self.sys().add(duration_ms as RawFd, id, Interest::TIMER)
      {
        let _ = self.sys().delete(fd);
        self.fd_map().remove(&id);
        let errno = e.raw_os_error().unwrap_or(libc::EIO);
        self
          .immediate
          .push(ImmediateCompletion { id, result: -(errno as isize) });
        return Ok(());
      }

      // Track that this op has a timeout timer
      self.timeout_timers.insert(id, duration_ms);
      // Store the inner op
      self.op_map.insert(id, *inner);
      return Ok(());
    }

    // Handle Op::Watch specially - needs to create/open fds
    #[cfg(unix)]
    if let Op::Watch { path, mask } = op {
      use std::ffi::CStr;

      // Platform-specific implementation
      #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
      ))]
      {
        use crate::api::ops::WatchMask;

        // BSD/macOS: Open the file and register EVFILT_VNODE
        // SAFETY: path is a valid null-terminated C string from the TypedOp
        let path_cstr = unsafe { CStr::from_ptr(path) };
        let fd = match syscall!(open(
          path_cstr.as_ptr(),
          libc::O_RDONLY | libc::O_CLOEXEC
        )) {
          Ok(fd) => fd,
          Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            self
              .immediate
              .push(ImmediateCompletion { id, result: -(errno as isize) });
            return Ok(());
          }
        };

        // Convert mask to kqueue fflags
        let fflags = WatchMask::from_bits(mask).to_kqueue_fflags();

        // Register with kqueue
        if let Err(e) = self.sys().add_vnode(fd, id, fflags) {
          // SAFETY: fd is a valid file descriptor we just opened
          unsafe { libc::close(fd) };
          let errno = e.raw_os_error().unwrap_or(libc::EIO);
          self
            .immediate
            .push(ImmediateCompletion { id, result: -(errno as isize) });
          return Ok(());
        }

        // Track the fd so we can close it on completion
        self.watch_fds.insert(id, (fd, false));
        self.fd_map().insert(id, fd);
        return Ok(());
      }

      #[cfg(target_os = "linux")]
      {
        use crate::api::ops::WatchMask;

        // Linux: Create inotify fd and add watch
        let inotify_fd =
          match syscall!(inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC)) {
            Ok(fd) => fd,
            Err(e) => {
              let errno = e.raw_os_error().unwrap_or(libc::EIO);
              self
                .immediate
                .push(ImmediateCompletion { id, result: -(errno as isize) });
              return Ok(());
            }
          };

        let path_cstr = unsafe { CStr::from_ptr(path) };
        let inotify_mask = WatchMask::from_bits(mask).to_inotify_mask();

        let wd = syscall!(inotify_add_watch(
          inotify_fd,
          path_cstr.as_ptr(),
          inotify_mask
        ));
        if wd.is_err() {
          unsafe { libc::close(inotify_fd) };
          let errno = wd.unwrap_err().raw_os_error().unwrap_or(libc::EIO);
          self
            .immediate
            .push(ImmediateCompletion { id, result: -(errno as isize) });
          return Ok(());
        }

        // Register inotify fd with epoll for READ
        self.fd_map().insert(id, inotify_fd);
        if let Err(e) = self.sys().add(inotify_fd, id, Interest::READ) {
          unsafe { libc::close(inotify_fd) };
          self.fd_map().remove(&id);
          let errno = e.raw_os_error().unwrap_or(libc::EIO);
          self
            .immediate
            .push(ImmediateCompletion { id, result: -(errno as isize) });
          return Ok(());
        }

        // Track the fd so we can close it on completion
        self.watch_fds.insert(id, (inotify_fd, true));
        return Ok(());
      }
    }

    // Handle Op::Signal specially - needs platform-specific async registration
    #[cfg(unix)]
    if let Op::Signal { sigset } = op {
      #[cfg(target_os = "linux")]
      {
        // Linux: Create signalfd
        let sigfd = syscall!(signalfd(
          -1,
          sigset,
          libc::SFD_NONBLOCK | libc::SFD_CLOEXEC
        ));
        match sigfd {
          Ok(fd) => {
            self.fd_map().insert(id, fd);
            if let Err(e) = self.sys().add(fd, id, Interest::READ) {
              unsafe { libc::close(fd) };
              self.fd_map().remove(&id);
              let errno = e.raw_os_error().unwrap_or(libc::EIO);
              self
                .immediate
                .push(ImmediateCompletion { id, result: -(errno as isize) });
              return Ok(());
            }
            // Track the signalfd so we can close it on completion
            self.watch_fds.insert(id, (fd, false)); // false = not inotify
            return Ok(());
          }
          Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            self
              .immediate
              .push(ImmediateCompletion { id, result: -(errno as isize) });
            return Ok(());
          }
        }
      }

      #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
      ))]
      {
        // BSD/macOS: Use EVFILT_SIGNAL via kqueue
        // We need to iterate through the sigset and register each signal
        // For simplicity, we register for common signals
        // The actual signal number is returned in kevent's ident field

        // Try to find which signals are in the set
        for sig in 1..32 {
          // SAFETY: sigset is valid pointer from TypedOp
          if unsafe { libc::sigismember(sigset, sig) } == 1 {
            // Register EVFILT_SIGNAL for this signal
            if let Err(e) = self.sys().add_signal(sig, id) {
              let errno = e.raw_os_error().unwrap_or(libc::EIO);
              self
                .immediate
                .push(ImmediateCompletion { id, result: -(errno as isize) });
              return Ok(());
            }
            // Store signal number as "fd" for tracking (we'll use it in completion)
            self.fd_map().insert(id, sig);
            return Ok(());
          }
        }

        // No signals in set
        self
          .immediate
          .push(ImmediateCompletion { id, result: -(libc::EINVAL as isize) });
        return Ok(());
      }
    }

    let fd_and_interest = match &op {
      Op::ReadV { .. }
      | Op::WriteV { .. }
      | Op::ReadVAt { .. }
      | Op::WriteVAt { .. } => {
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      Op::Send { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::SendTo { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Recv { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::RecvFrom { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Accept { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::AcceptStream { fd } => {
        // Stream operation - use level-triggered mode to drain all pending accepts
        let raw_fd = fd.as_raw_fd();

        // Set socket to non-blocking mode for level-triggered drain loop
        // SAFETY: raw_fd is a valid file descriptor from the Resource
        let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };
        if flags == -1 {
          return Err(io::Error::last_os_error());
        }
        // SAFETY: raw_fd is valid, flags is the current flags we just read
        if unsafe {
          libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK)
        } == -1
        {
          return Err(io::Error::last_os_error());
        }

        self.fd_map().insert(id, raw_fd);
        // Level-triggered: fires on every readiness, no re-arm needed
        if let Err(e) = self.sys().add_level(raw_fd, id, Interest::READ) {
          self.fd_map().remove(&id);
          return Err(e);
        }
        self.op_map.insert(id, op);
        return Ok(());
      }
      Op::Connect { .. } => None,
      Op::Bind { .. }
      | Op::Listen { .. }
      | Op::Shutdown { .. }
      | Op::OpenAt { .. }
      | Op::Close { .. }
      | Op::Fsync { .. }
      | Op::Truncate { .. } => {
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(target_os = "linux")]
      Op::Tee { fd_in, .. } => {
        Some((fd_in.as_raw_fd(), Interest::READ_AND_WRITE))
      }
      Op::Sleep { .. } => None,
      Op::Nop => {
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      Op::Socket { .. } => None,
      Op::LinkAt { .. }
      | Op::SymlinkAt { .. }
      | Op::UnlinkAt { .. }
      | Op::RenameAt { .. }
      | Op::MkdirAt { .. } => {
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(target_os = "linux")]
      Op::Splice { .. } | Op::CopyFileRange { .. } => {
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(unix)]
      Op::SendFile { .. } => {
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(unix)]
      Op::Waitid { .. } => {
        // Waitid is a blocking operation (no fd to poll on)
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(unix)]
      Op::Spawn { .. } => {
        // Spawn is a blocking operation
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(unix)]
      Op::Timeout { .. } => {
        // Should be handled above, but in case we reach here
        unreachable!("Op::Timeout should be handled before this match")
      }
      #[cfg(unix)]
      Op::Watch { .. } => {
        // Should be handled above, but in case we reach here
        unreachable!("Op::Watch should be handled before this match")
      }
      #[cfg(unix)]
      Op::Signal { .. } => {
        // Should be handled above, but in case we reach here
        unreachable!("Op::Signal should be handled before this match")
      }
      #[cfg(unix)]
      Op::Flock { .. } => {
        // Flock is a blocking operation
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(unix)]
      Op::GetDents { .. } => {
        // GetDents is a blocking operation
        let result = Poller::run_op(&op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
    };

    self.op_map.insert(id, op);

    if let Some((fd, interest)) = fd_and_interest {
      self.fd_map().insert(id, fd);
      if let Err(e) = self.sys().add(fd, id, interest) {
        // Registration failed (e.g., EBADF for invalid fd).
        // Return as immediate completion with error instead of propagating.
        self.fd_map().remove(&id);
        let op = self.op_map.remove(&id).unwrap();
        let errno = e.raw_os_error().unwrap_or(libc::EIO);
        // Try the operation anyway - it will fail with a proper error
        let result = Poller::run_op(&op);
        let final_result = if result < 0 { result } else { -(errno as isize) };
        self.immediate.push(ImmediateCompletion { id, result: final_result });
        return Ok(());
      }
    } else if let Some(op) = self.op_map.get(&id) {
      match op {
        Op::Connect { fd, .. } => {
          let fd = fd.as_raw_fd();
          let result = Poller::run_op(op);
          self.op_map.remove(&id);
          if result == -(libc::EINPROGRESS as isize) {
            self.fd_map().insert(id, fd);
            self.sys().add(fd, id, Interest::WRITE)?;
          } else {
            self.immediate.push(ImmediateCompletion { id, result });
          }
        }
        Op::Sleep { duration, .. } => {
          let duration_ms = duration.as_millis() as RawFd;
          self.fd_map().insert(id, duration_ms);
          self.sys().add(duration_ms, id, Interest::TIMER)?;
        }
        Op::Socket { .. } => {
          let result = Poller::run_op(op);
          self.op_map.remove(&id);
          self.immediate.push(ImmediateCompletion { id, result });
        }
        _ => {}
      }
    }

    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    // For epoll/kqueue, operations are registered immediately in push()
    // since each registration is a separate syscall anyway.
    // There's no batching opportunity like with io_uring.
    Ok(0)
  }

  /// Poll for completions with optional timeout
  ///
  /// - `timeout = None`: Block indefinitely
  /// - `timeout = Some(Duration::ZERO)`: Non-blocking poll
  /// - `timeout = Some(duration)`: Wait up to duration
  fn wait_timeout(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    use crate::backend::op::Op;

    self.completed.clear();

    // First, drain any immediate completions
    for imm in self.immediate.drain(..) {
      self.completed.push(OpCompleted::new(imm.id, imm.result));
    }

    // Poll for events
    // Get reference to sys before mutating events to avoid borrow conflict
    self.events.clear();
    // SAFETY: as_raw_buf() provides mutable access to the entire capacity of the events buffer
    let events = unsafe { self.events.as_raw_buf() };

    let items_written = match self.sys.as_ref().unwrap().wait(events, timeout) {
      Ok(n) => n,
      Err(e) => {
        // If we already have completions, return them instead of propagating the error
        if !self.completed.is_empty() {
          return Ok(self.completed.as_ref());
        }
        return Err(e);
      }
    };

    // SAFETY: The OS's wait() call filled items_written events into our buffer
    unsafe { self.events.set_len(items_written) };

    // Collect events first to avoid borrow conflicts
    let events_to_process: Vec<_> = self.events.iter().collect();

    for event in events_to_process {
      let operation_id = event.key;

      // Skip internal notification events
      if operation_id == u64::MAX {
        continue;
      }

      // Check if this is a timeout-wrapped operation
      #[cfg(unix)]
      let has_timeout = self.timeout_timers.contains_key(&operation_id);
      #[cfg(not(unix))]
      let has_timeout = false;

      // Look up fd from our internal map (may not exist if already completed)
      let Some(entry_fd) = self.fd_map().get(&operation_id).copied() else {
        // Op already completed by another event (e.g., timer fired after I/O)
        continue;
      };

      // Handle timeout-wrapped operations specially
      #[cfg(unix)]
      if has_timeout && event.interest.is_timer() {
        // Timer fired - check if this is a Sleep or I/O timeout
        let stored_value =
          self.timeout_timers.remove(&operation_id).unwrap_or(0);

        // For I/O ops, stored_value is duration_ms (positive) - return -ECANCELED
        // For Sleep ops, stored_value is the pre-determined result (negative)
        let result = if stored_value < 0 {
          // Sleep case: use pre-determined result
          stored_value as isize
        } else {
          // I/O case: timer fired first, cancel fd registration
          let _ = self.sys().delete(entry_fd);
          -(libc::ECANCELED as isize)
        };

        self.sys().delete_timer(operation_id)?;
        self.fd_map().remove(&operation_id);
        self.op_map.remove(&operation_id);
        self.completed.push(OpCompleted::new(operation_id, result));
        continue;
      }

      // Handle watch operations specially
      #[cfg(unix)]
      if let Some((watch_fd, is_inotify)) = self.watch_fds.remove(&operation_id)
      {
        let result = if is_inotify {
          // Linux: read from inotify fd to get the event
          #[cfg(target_os = "linux")]
          {
            use crate::api::ops::WatchMask;

            // Read one inotify event
            let mut buf = [0u8; 256];
            let n = unsafe {
              libc::read(watch_fd, buf.as_mut_ptr() as *mut _, buf.len())
            };

            if n < 0 {
              -(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO) as isize)
            } else if n >= std::mem::size_of::<libc::inotify_event>() as isize {
              // Parse the inotify_event to get the mask
              let event =
                unsafe { &*(buf.as_ptr() as *const libc::inotify_event) };
              WatchMask::from_inotify_mask(event.mask).bits() as isize
            } else {
              0 // No event data
            }
          }
          #[cfg(not(target_os = "linux"))]
          {
            0
          }
        } else {
          // BSD/macOS: VNODE event - fflags are in the event
          #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
          ))]
          {
            use crate::api::ops::WatchMask;
            // Convert kqueue fflags to WatchMask
            WatchMask::from_kqueue_fflags(event.fflags).bits() as isize
          }
          #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
          )))]
          {
            0
          }
        };

        // Clean up: close the watch fd and remove from tracking
        // SAFETY: watch_fd is a valid file descriptor from watch_fds
        unsafe { libc::close(watch_fd) };
        // On Linux (inotify), we registered with READ interest, use delete()
        // On BSD/macOS (EVFILT_VNODE), use delete_vnode()
        if is_inotify {
          let _ = self.sys().delete(entry_fd);
        } else {
          #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
          ))]
          {
            let _ = self.sys().delete_vnode(entry_fd);
          }
        }
        self.fd_map().remove(&operation_id);
        self.completed.push(OpCompleted::new(operation_id, result));
        continue;
      }

      // Handle signal events specially
      #[cfg(unix)]
      if event.interest.is_signal() {
        // BSD/macOS: signal number is in event.key (ident from kqueue)
        // We stored the signal number as the "fd" in fd_map
        let sig = entry_fd;

        // Clean up
        #[cfg(any(
          target_os = "macos",
          target_os = "ios",
          target_os = "freebsd",
          target_os = "dragonfly",
          target_os = "openbsd",
          target_os = "netbsd"
        ))]
        {
          let _ = self.sys().delete_signal(sig);
        }
        self.fd_map().remove(&operation_id);
        self.completed.push(OpCompleted::new(operation_id, sig as isize));
        continue;
      }

      // Handle signalfd reads on Linux (stored in watch_fds with is_inotify=false)
      // This is now handled above in the watch_fds section when is_inotify is false
      // and the fd is a signalfd - we need to read from it to get the signal number
      #[cfg(target_os = "linux")]
      if let Some(&(signal_fd, false)) = self.watch_fds.get(&operation_id) {
        // This could be a signalfd - check by reading from it
        if event.interest.is_readable() {
          // Read signalfd_siginfo from the fd
          #[repr(C)]
          struct SignalfdSiginfo {
            ssi_signo: u32,
            // ... other fields we don't need
            _padding: [u8; 124],
          }

          let mut siginfo: SignalfdSiginfo = unsafe { std::mem::zeroed() };
          let n = unsafe {
            libc::read(
              signal_fd,
              &mut siginfo as *mut _ as *mut libc::c_void,
              std::mem::size_of::<SignalfdSiginfo>(),
            )
          };

          let result = if n >= 4 {
            siginfo.ssi_signo as isize
          } else if n < 0 {
            -(std::io::Error::last_os_error()
              .raw_os_error()
              .unwrap_or(libc::EIO) as isize)
          } else {
            -(libc::EIO as isize)
          };

          // Clean up
          self.watch_fds.remove(&operation_id);
          unsafe { libc::close(signal_fd) };
          let _ = self.sys().delete(signal_fd);
          self.fd_map().remove(&operation_id);
          self.completed.push(OpCompleted::new(operation_id, result));
          continue;
        }
      }

      // Handle AcceptStream specially: drain all pending accepts (level-triggered)
      // Use get() to avoid remove/insert overhead - op stays in map unless error
      if let Some(op) = self.op_map.get(&operation_id) {
        if matches!(op, Op::AcceptStream { .. }) {
          loop {
            let result = Poller::run_op(op);
            if result < 0 {
              let errno = (-result) as i32;
              if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
                break; // No more pending - op stays in map
              }
              // Real error - clean up stream
              let _ = self.sys().delete(entry_fd);
              self.fd_map().remove(&operation_id);
              self.op_map.remove(&operation_id);
              self
                .completed
                .push(OpCompleted::new(operation_id, result).with_more(false));
              break;
            }
            self
              .completed
              .push(OpCompleted::new(operation_id, result).with_more(true));
          }
          continue;
        }
      }

      // Remove op for processing - may be put back on EAGAIN
      let op = match self.op_map.remove(&operation_id) {
        Some(op) => op,
        None => continue, // Op already completed (e.g., by timeout)
      };

      let result = Poller::run_op(&op);

      // Check for EAGAIN/EINPROGRESS (would block)
      if result < 0 {
        let errno = (-result) as i32;
        if errno == libc::EAGAIN
          || errno == libc::EWOULDBLOCK
          || errno == libc::EINPROGRESS
        {
          // Put op back and re-arm for more events
          self.op_map.insert(operation_id, op);
          self.sys().modify(entry_fd, operation_id, event.interest)?;
          continue;
        }
      }

      // Operation completed (success or error other than would-block)
      // Clean up - use delete_timer for timer events, delete for fd-based events
      if event.interest.is_timer() {
        self.sys().delete_timer(operation_id)?;
      } else {
        self.sys().delete(entry_fd)?;
      }
      self.fd_map().remove(&operation_id);

      // If this was a timeout-wrapped op, also cancel the timer
      #[cfg(unix)]
      if has_timeout {
        let _ = self.sys().delete_timer(operation_id);
        self.timeout_timers.remove(&operation_id);
      }

      self.completed.push(OpCompleted::new(operation_id, result));
    }

    Ok(self.completed.as_ref())
  }

  fn arm_timer(&mut self, duration: Duration) -> io::Result<()> {
    self.sys().arm_wheel_timer(duration)
  }

  fn disarm_timer(&mut self) -> io::Result<()> {
    self.sys().disarm_wheel_timer()
  }

  fn cancel(&mut self, id: u64) -> io::Result<()> {
    if let Some(fd) = self.fd_map().remove(&id) {
      let _ = self.sys().delete(fd);
    }
    self.op_map.remove(&id);
    Ok(())
  }
}

#[cfg(test)]
crate::test_io_backend!(Poller::new());
