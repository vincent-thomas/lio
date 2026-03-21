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

/// Registered operation entry - all state needed for completion in one place.
enum Entry {
  /// Standard async I/O - wait for readiness, run op
  Async { fd: RawFd, interest: Interest, op: crate::backend::op::Op },
  /// Level-triggered stream - drain loop on readiness
  Stream { fd: RawFd, op: crate::backend::op::Op },
  /// Timer - complete when it fires
  Timer { op: crate::backend::op::Op },
  /// Timeout wrapper - fd + timer; timer fires = cancel fd
  #[cfg(unix)]
  Timeout { fd: RawFd, timer_result: i32, op: crate::backend::op::Op },
  /// Watch - owned fd to close on completion
  #[cfg(unix)]
  Watch { entry_fd: RawFd, watch_fd: RawFd, is_inotify: bool },
  /// Signal - complete with signal number
  #[cfg(unix)]
  Signal { sig: RawFd },
}

/// Polling-based I/O backend for epoll (Linux) and kqueue (BSD/macOS).
#[derive(Default)]
pub struct Poller {
  sys: Option<sys::OsPoller>,
  entries: HashMap<u64, Entry>,
  events: Events,
  immediate: Vec<(u64, isize)>,
  completed: Vec<OpCompleted>,
}

impl Poller {
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn sys(&self) -> &sys::OsPoller {
    self.sys.as_ref().expect("Poller not initialized")
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
      #[cfg(all(unix, not(target_os = "linux")))]
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
        #[cfg(not(any(
          target_vendor = "apple",
          target_os = "freebsd",
          target_os = "dragonfly"
        )))]
        {
          // sendfile not supported on this platform
          let _ = (out_fd, in_fd, offset, count);
          -(libc::ENOTSUP as isize)
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
          unsafe extern "C" {
            fn getdirentries(
              fd: libc::c_int,
              buf: *mut libc::c_char,
              nbytes: libc::c_int,
              basep: *mut libc::c_long,
            ) -> libc::c_int;
          }
          let mut basep: libc::c_long = 0;
          let ret = unsafe {
            getdirentries(
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

impl Poller {
  fn push_entry(&mut self, id: u64, entry: Entry) -> io::Result<()> {
    use Entry::*;
    match &entry {
      Async { fd, interest, .. } => {
        self.sys().add(*fd, id, *interest)?;
      }
      Stream { fd, .. } => {
        // Set non-blocking for drain loop
        // SAFETY: fcntl with F_GETFL is safe on any valid fd
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFL) };
        if flags != -1 {
          // SAFETY: fcntl with F_SETFL is safe on any valid fd
          unsafe { libc::fcntl(*fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
        self.sys().add_level(*fd, id, Interest::READ)?;
      }
      Timer { .. } => unreachable!("Timer handled separately"),
      #[cfg(unix)]
      Timeout { fd, .. } => {
        self.sys().add(*fd, id, Interest::READ)?; // fd interest
        self.sys().add(id as RawFd, id, Interest::TIMER)?; // timer
      }
      #[cfg(unix)]
      Watch { entry_fd, is_inotify, .. } => {
        if *is_inotify {
          self.sys().add(*entry_fd, id, Interest::READ)?;
        } else {
          #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
          ))]
          self.sys().add_vnode(*entry_fd, id, 0)?;
        }
      }
      #[cfg(unix)]
      Signal { sig } => {
        #[cfg(target_os = "linux")]
        self.sys().add(*sig, id, Interest::READ)?;
        #[cfg(any(
          target_os = "macos",
          target_os = "ios",
          target_os = "freebsd",
          target_os = "dragonfly",
          target_os = "openbsd",
          target_os = "netbsd"
        ))]
        self.sys().add_signal(*sig, id)?;
      }
    }
    self.entries.insert(id, entry);
    Ok(())
  }

  fn handle_event(&mut self, id: u64, event: &Event) -> io::Result<()> {
    use Entry::*;

    let Some(entry) = self.entries.get(&id) else { return Ok(()) };

    match entry {
      // Timeout: timer fired = cancel, fd ready = complete normally
      #[cfg(unix)]
      Timeout { fd, timer_result, op } if event.interest.is_timer() => {
        let result = *timer_result as isize;
        let fd = *fd;
        self.entries.remove(&id);
        let _ = self.sys().delete(fd);
        self.sys().delete_timer(id)?;
        self.completed.push(OpCompleted::new(id, result));
      }

      // Watch: read result and cleanup
      #[cfg(unix)]
      Watch { entry_fd, watch_fd, is_inotify } => {
        let (entry_fd, watch_fd, is_inotify) =
          (*entry_fd, *watch_fd, *is_inotify);
        self.entries.remove(&id);
        let result =
          if is_inotify { read_inotify(watch_fd) } else { read_vnode(event) };
        // SAFETY: watch_fd is a valid fd we opened for watching
        unsafe { libc::close(watch_fd) };
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
        self.completed.push(OpCompleted::new(id, result));
      }

      // Signal: complete with signal number
      #[cfg(unix)]
      Signal { sig } => {
        let sig = *sig;
        self.entries.remove(&id);
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
        #[cfg(target_os = "linux")]
        {
          let _ = self.sys().delete(sig);
        }
        self.completed.push(OpCompleted::new(id, sig as isize));
      }

      // Stream: drain loop until EAGAIN
      Stream { fd, op } => {
        let fd = *fd;
        loop {
          let result = Poller::run_op(op);
          if result < 0 {
            let errno = (-result) as i32;
            if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
              break;
            }
            self.entries.remove(&id);
            let _ = self.sys().delete(fd);
            self.completed.push(OpCompleted::new(id, result).with_more(false));
            return Ok(());
          }
          self.completed.push(OpCompleted::new(id, result).with_more(true));
        }
      }

      // Async: run op, re-arm on EAGAIN
      Async { fd, interest, op } => {
        let (fd, interest) = (*fd, *interest);
        let result = Poller::run_op(op);
        if result < 0 {
          let errno = (-result) as i32;
          if errno == libc::EAGAIN
            || errno == libc::EWOULDBLOCK
            || errno == libc::EINPROGRESS
          {
            self.sys().modify(fd, id, interest)?;
            return Ok(());
          }
        }
        self.entries.remove(&id);
        let _ = self.sys().delete(fd);
        self.completed.push(OpCompleted::new(id, result));
      }

      // Timer: just complete
      Timer { op } => {
        let result = Poller::run_op(op);
        self.entries.remove(&id);
        self.sys().delete_timer(id)?;
        self.completed.push(OpCompleted::new(id, result));
      }

      // Timeout (fd ready, not timer): run op normally
      #[cfg(unix)]
      Timeout { fd, op, .. } => {
        let fd = *fd;
        let result = Poller::run_op(op);
        if result < 0 {
          let errno = (-result) as i32;
          if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
            return Ok(()); // Still waiting
          }
        }
        self.entries.remove(&id);
        let _ = self.sys().delete(fd);
        let _ = self.sys().delete_timer(id);
        self.completed.push(OpCompleted::new(id, result));
      }
    }
    Ok(())
  }

  #[cfg(unix)]
  fn push_timeout(
    &mut self,
    id: u64,
    inner: &crate::backend::op::Op,
    duration: Duration,
  ) -> io::Result<()> {
    use crate::backend::op::Op;
    use std::os::fd::AsRawFd;

    match inner {
      // Sleep: use shorter of timeout vs sleep duration
      Op::Sleep { duration: sleep_dur, .. } => {
        let ms = duration.min(*sleep_dur).as_millis() as RawFd;
        self.sys().add(ms, id, Interest::TIMER)?;
        let result = if duration < *sleep_dur {
          -(libc::ECANCELED as isize)
        } else {
          #[cfg(target_os = "linux")]
          {
            -(libc::ETIME as isize)
          }
          #[cfg(not(target_os = "linux"))]
          {
            -(libc::ETIMEDOUT as isize)
          }
        };
        self.entries.insert(
          id,
          Entry::Timeout {
            fd: 0,
            timer_result: result as i32,
            op: inner.clone(),
          },
        );
      }

      // Async I/O: register fd + timer
      Op::Send { fd, .. } | Op::SendTo { fd, .. } => {
        let fd = fd.as_raw_fd();
        self.sys().add(fd, id, Interest::WRITE)?;
        let ms = duration.as_millis() as RawFd;
        if let Err(e) = self.sys().add(ms, id, Interest::TIMER) {
          let _ = self.sys().delete(fd);
          return Err(e);
        }
        self.entries.insert(
          id,
          Entry::Timeout {
            fd,
            timer_result: -libc::ECANCELED,
            op: inner.clone(),
          },
        );
      }
      Op::Recv { fd, .. } | Op::RecvFrom { fd, .. } | Op::Accept { fd, .. } => {
        let fd = fd.as_raw_fd();
        self.sys().add(fd, id, Interest::READ)?;
        let ms = duration.as_millis() as RawFd;
        if let Err(e) = self.sys().add(ms, id, Interest::TIMER) {
          let _ = self.sys().delete(fd);
          return Err(e);
        }
        self.entries.insert(
          id,
          Entry::Timeout {
            fd,
            timer_result: -libc::ECANCELED,
            op: inner.clone(),
          },
        );
      }

      // Everything else: run immediately (timeout doesn't apply to sync ops)
      _ => {
        self.immediate.push((id, Poller::run_op(inner)));
      }
    }
    Ok(())
  }

  #[cfg(unix)]
  fn push_watch(
    &mut self,
    id: u64,
    path: *const libc::c_char,
    mask: u32,
  ) -> io::Result<()> {
    use crate::api::ops::WatchMask;
    use std::ffi::CStr;

    // SAFETY: path is a valid C string pointer from the caller
    let path_cstr = unsafe { CStr::from_ptr(path) };

    #[cfg(any(
      target_os = "macos",
      target_os = "ios",
      target_os = "freebsd",
      target_os = "dragonfly",
      target_os = "openbsd",
      target_os = "netbsd"
    ))]
    {
      let fd = match syscall!(open(
        path_cstr.as_ptr(),
        libc::O_RDONLY | libc::O_CLOEXEC
      )) {
        Ok(fd) => fd,
        Err(e) => {
          self
            .immediate
            .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
          return Ok(());
        }
      };
      let fflags = WatchMask::from_bits(mask).to_kqueue_fflags();
      if let Err(e) = self.sys().add_vnode(fd, id, fflags) {
        // SAFETY: fd is valid, just opened above
        unsafe { libc::close(fd) };
        self
          .immediate
          .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
        return Ok(());
      }
      self.entries.insert(
        id,
        Entry::Watch { entry_fd: fd, watch_fd: fd, is_inotify: false },
      );
    }

    #[cfg(target_os = "linux")]
    {
      let fd =
        match syscall!(inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC)) {
          Ok(fd) => fd,
          Err(e) => {
            self
              .immediate
              .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
            return Ok(());
          }
        };
      let inotify_mask = WatchMask::from_bits(mask).to_inotify_mask();
      if let Err(e) =
        syscall!(inotify_add_watch(fd, path_cstr.as_ptr(), inotify_mask))
      {
        unsafe { libc::close(fd) };
        self
          .immediate
          .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
        return Ok(());
      }
      if let Err(e) = self.sys().add(fd, id, Interest::READ) {
        unsafe { libc::close(fd) };
        self
          .immediate
          .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
        return Ok(());
      }
      self.entries.insert(
        id,
        Entry::Watch { entry_fd: fd, watch_fd: fd, is_inotify: true },
      );
    }
    Ok(())
  }

  #[cfg(unix)]
  fn push_signal(
    &mut self,
    id: u64,
    sigset: *const libc::sigset_t,
  ) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
      let fd = match syscall!(signalfd(
        -1,
        sigset,
        libc::SFD_NONBLOCK | libc::SFD_CLOEXEC
      )) {
        Ok(fd) => fd,
        Err(e) => {
          self
            .immediate
            .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
          return Ok(());
        }
      };
      if let Err(e) = self.sys().add(fd, id, Interest::READ) {
        unsafe { libc::close(fd) };
        self
          .immediate
          .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
        return Ok(());
      }
      self.entries.insert(id, Entry::Signal { sig: fd });
      return Ok(());
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
      for sig in 1..32 {
        // SAFETY: sigset is a valid pointer from the caller, sig is in valid range
        if unsafe { libc::sigismember(sigset, sig) } == 1 {
          if let Err(e) = self.sys().add_signal(sig, id) {
            self
              .immediate
              .push((id, -(e.raw_os_error().unwrap_or(libc::EIO) as isize)));
            return Ok(());
          }
          self.entries.insert(id, Entry::Signal { sig });
          return Ok(());
        }
      }
      self.immediate.push((id, -(libc::EINVAL as isize)));
      Ok(())
    }
  }
}

#[cfg(target_os = "linux")]
fn read_inotify(fd: RawFd) -> isize {
  use crate::api::ops::WatchMask;
  let mut buf = [0u8; 256];
  let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
  if n < 0 {
    -(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
      as isize)
  } else if n >= std::mem::size_of::<libc::inotify_event>() as isize {
    let ev = unsafe { &*(buf.as_ptr() as *const libc::inotify_event) };
    WatchMask::from_inotify_mask(ev.mask).bits() as isize
  } else {
    0
  }
}

#[cfg(not(target_os = "linux"))]
fn read_inotify(_fd: RawFd) -> isize {
  0
}

#[cfg(any(
  target_os = "macos",
  target_os = "ios",
  target_os = "freebsd",
  target_os = "dragonfly",
  target_os = "openbsd",
  target_os = "netbsd"
))]
fn read_vnode(event: &Event) -> isize {
  use crate::api::ops::WatchMask;
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
fn read_vnode(_event: &Event) -> isize {
  0
}

impl IoBackend for Poller {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.sys = Some(sys::OsPoller::new()?);
    self.entries = HashMap::with_capacity(cap);
    self.events = Events::with_capacity(cap.min(4096));
    self.immediate = Vec::with_capacity(64);
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, op: crate::backend::op::Op) -> io::Result<()> {
    use crate::backend::op::Op;
    use std::os::fd::AsRawFd;

    match &op {
      // Timer
      Op::Sleep { duration, .. } => {
        let ms = duration.as_millis() as RawFd;
        self.sys().add(ms, id, Interest::TIMER)?;
        self.entries.insert(id, Entry::Timer { op });
      }

      // Async I/O (write): try first, register if would block
      Op::Send { fd, .. } | Op::SendTo { fd, .. } => {
        let result = Poller::run_op(&op);
        if result == -(libc::EAGAIN as isize)
          || result == -(libc::EWOULDBLOCK as isize)
        {
          self.push_entry(
            id,
            Entry::Async { fd: fd.as_raw_fd(), interest: Interest::WRITE, op },
          )?;
        } else {
          self.immediate.push((id, result));
        }
      }

      // Async I/O (read): try first, register if would block
      Op::Recv { fd, .. } | Op::RecvFrom { fd, .. } => {
        let result = Poller::run_op(&op);
        if result == -(libc::EAGAIN as isize)
          || result == -(libc::EWOULDBLOCK as isize)
        {
          self.push_entry(
            id,
            Entry::Async { fd: fd.as_raw_fd(), interest: Interest::READ, op },
          )?;
        } else {
          self.immediate.push((id, result));
        }
      }

      // Accept: register for read (wait for incoming connection)
      Op::Accept { fd, .. } => {
        self.push_entry(
          id,
          Entry::Async { fd: fd.as_raw_fd(), interest: Interest::READ, op },
        )?;
      }

      // Stream (level-triggered)
      Op::AcceptStream { fd } => {
        self.push_entry(id, Entry::Stream { fd: fd.as_raw_fd(), op })?;
      }

      // Connect: run immediately, register if EINPROGRESS
      Op::Connect { fd, .. } => {
        let result = Poller::run_op(&op);
        if result == -(libc::EINPROGRESS as isize) {
          self.push_entry(
            id,
            Entry::Async { fd: fd.as_raw_fd(), interest: Interest::WRITE, op },
          )?;
        } else {
          self.immediate.push((id, result));
        }
      }

      // Timeout wrapper
      #[cfg(unix)]
      Op::Timeout { inner, duration, .. } => {
        return self.push_timeout(id, inner, *duration);
      }

      // Watch
      #[cfg(unix)]
      Op::Watch { path, mask } => {
        return self.push_watch(id, *path, *mask);
      }

      // Signal
      #[cfg(unix)]
      Op::Signal { sigset } => {
        return self.push_signal(id, *sigset);
      }

      // Everything else: run immediately
      _ => {
        self.immediate.push((id, Poller::run_op(&op)));
      }
    }
    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    Ok(0)
  }

  fn wait_timeout(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();

    // Drain immediate completions
    for (id, result) in self.immediate.drain(..) {
      self.completed.push(OpCompleted::new(id, result));
    }

    // Poll for events
    self.events.clear();
    let n = {
      // SAFETY: events buffer is valid and owned by self
      let events = unsafe { self.events.as_raw_buf() };
      match self.sys.as_ref().unwrap().wait(events, timeout) {
        Ok(n) => n,
        Err(_) if !self.completed.is_empty() => return Ok(&self.completed),
        Err(e) => return Err(e),
      }
    };
    // SAFETY: n is the count returned by wait(), within buffer capacity
    unsafe { self.events.set_len(n) };

    // Process events
    let events: Vec<_> = self.events.iter().collect();
    for event in events {
      self.handle_event(event.key, &event)?;
    }

    Ok(&self.completed)
  }

  fn arm_timer(&mut self, duration: Duration) -> io::Result<()> {
    self.sys().arm_wheel_timer(duration)
  }

  fn disarm_timer(&mut self) -> io::Result<()> {
    self.sys().disarm_wheel_timer()
  }

  fn cancel(&mut self, id: u64) -> io::Result<()> {
    if let Some(entry) = self.entries.remove(&id) {
      match entry {
        Entry::Async { fd, .. } | Entry::Stream { fd, .. } => {
          let _ = self.sys().delete(fd);
        }
        Entry::Timer { .. } => {
          let _ = self.sys().delete_timer(id);
        }
        #[cfg(unix)]
        Entry::Timeout { fd, .. } => {
          let _ = self.sys().delete(fd);
          let _ = self.sys().delete_timer(id);
        }
        #[cfg(unix)]
        Entry::Watch { entry_fd, watch_fd, is_inotify } => {
          // SAFETY: watch_fd is a valid fd we opened for watching
          unsafe { libc::close(watch_fd) };
          if is_inotify {
            let _ = self.sys().delete(entry_fd);
          }
        }
        #[cfg(unix)]
        Entry::Signal { sig } => {
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
          #[cfg(target_os = "linux")]
          {
            let _ = self.sys().delete(sig);
          }
        }
      }
    }
    Ok(())
  }
}

#[cfg(test)]
crate::test_io_backend!(Poller::new());
