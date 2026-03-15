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

  /// Add interest for a file descriptor
  /// This is not idempotent.
  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()>;

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
}

/// Represents a readiness event from the poller
#[derive(Debug, Clone, Copy)]
pub struct Event {
  /// User-provided key to identify this event
  pub key: u64,
  /// The interest flags that triggered (what actually happened)
  pub interest: Interest,
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

    Event { key, interest }
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

    // Filter out the internal notification event (key == u64::MAX)
    if event.key == u64::MAX {
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

  /// Run an op by reference using peek (no ownership transfer of buffers).
  /// Used in wait_timeout so the op can be put back in op_map on EAGAIN.
  fn run_op_on_event(op: &crate::backend::op::Op) -> isize {
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
      Op::Timeout { .. } => 0,
      Op::Connect { fd, addr, len, connect_called } => {
        let ret = syscall!(raw connect(fd.as_raw_fd(), *addr as *const libc::sockaddr, *len));
        // EINPROGRESS is returned as-is (caller waits for socket to become writable)
        // EISCONN means success if we already started connecting
        if ret == -(libc::EISCONN as isize) && *connect_called {
          return 0;
        }
        ret
      }
      _ => panic!("run_op_on_event called for non-event op"),
    }
  }

  fn run_op_blocking(op: crate::backend::op::Op) -> isize {
    use crate::backend::op::Op;
    use std::os::fd::AsRawFd;

    match op {
      Op::Send { fd, flags, buf } => {
        syscall!(raw send(fd.as_raw_fd(), buf.ptr as *const _, buf.len, flags))
      }
      Op::Recv { fd, flags, buf } => {
        syscall!(raw recv(fd.as_raw_fd(), buf.ptr as *mut _, buf.len, flags))
      }
      Op::SendTo { fd, flags, buf, addr, addrlen, .. } => {
        syscall!(raw sendto(fd.as_raw_fd(), buf.ptr as *const _, buf.len, flags, addr as *const libc::sockaddr, addrlen))
      }
      Op::RecvFrom { fd, flags, buf, addr, addrlen, .. } => {
        syscall!(raw recvfrom(fd.as_raw_fd(), buf.ptr as *mut _, buf.len, flags, addr as *mut libc::sockaddr, addrlen))
      }
      Op::Accept { fd, addr, len } => {
        syscall!(raw accept(fd.as_raw_fd(), addr as *mut _, len))
      }
      Op::Connect { fd, addr, len, connect_called } => {
        let ret = syscall!(raw connect(fd.as_raw_fd(), addr as *const libc::sockaddr, len));
        // EINPROGRESS is returned as-is (caller waits for socket to become writable)
        // EISCONN means success if we already started connecting
        if ret == -(libc::EISCONN as isize) && connect_called {
          return 0;
        }
        ret
      }
      Op::Bind { fd, addr, addrlen } => {
        syscall!(raw bind(fd.as_raw_fd(), addr as *const libc::sockaddr, addrlen))
      }
      Op::Listen { fd, backlog } => {
        syscall!(raw listen(fd.as_raw_fd(), backlog))
      }
      Op::Shutdown { fd, how } => {
        syscall!(raw shutdown(fd.as_raw_fd(), how))
      }
      Op::Socket { domain, ty, proto } => {
        syscall!(raw socket(domain, ty, proto))
      }
      Op::OpenAt { dir_fd, path, flags, mode } => {
        syscall!(raw openat(dir_fd.as_raw_fd(), path, flags, mode))
      }
      Op::Close { fd } => {
        syscall!(raw close(fd))
      }
      Op::Fsync { fd } => {
        syscall!(raw fsync(fd.as_raw_fd()))
      }
      Op::Truncate { fd, size } => {
        syscall!(raw ftruncate(fd.as_raw_fd(), size as libc::off_t))
      }
      Op::LinkAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        syscall!(raw linkat(old_dir_fd.as_raw_fd(), old_path, new_dir_fd.as_raw_fd(), new_path, 0))
      }
      Op::SymlinkAt { target, linkpath, dir_fd } => {
        syscall!(raw symlinkat(target, dir_fd.as_raw_fd(), linkpath))
      }
      Op::UnlinkAt { dir_fd, path, flags } => {
        syscall!(raw unlinkat(dir_fd.as_raw_fd(), path, flags))
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        syscall!(raw renameat(old_dir_fd.as_raw_fd(), old_path, new_dir_fd.as_raw_fd(), new_path))
      }
      Op::MkdirAt { dir_fd, path, mode } => {
        syscall!(raw mkdirat(dir_fd.as_raw_fd(), path, mode as libc::mode_t))
      }
      #[cfg(target_os = "linux")]
      Op::Splice { fd_in, off_in, fd_out, off_out, len, flags } => {
        let mut off_in_val = off_in;
        let mut off_out_val = off_out;
        let off_in_ptr = if off_in == -1 { std::ptr::null_mut() } else { &mut off_in_val };
        let off_out_ptr = if off_out == -1 { std::ptr::null_mut() } else { &mut off_out_val };
        syscall!(raw splice(fd_in.as_raw_fd(), off_in_ptr, fd_out.as_raw_fd(), off_out_ptr, len as usize, flags as libc::c_uint))
      }
      #[cfg(target_os = "linux")]
      Op::SendFile { out_fd, in_fd, offset, count } => {
        let mut off = offset;
        syscall!(raw sendfile(out_fd.as_raw_fd(), in_fd.as_raw_fd(), &mut off, count))
      }
      #[cfg(all(unix, not(target_os = "linux")))]
      Op::SendFile { out_fd, in_fd, offset, count } => {
        let mut len: libc::off_t = count as libc::off_t;
        let ret = syscall!(raw sendfile(in_fd.as_raw_fd(), out_fd.as_raw_fd(), offset, &mut len, std::ptr::null_mut(), 0));
        if ret == 0 { len as isize } else { ret }
      }
      #[cfg(target_os = "linux")]
      Op::CopyFileRange { fd_in, off_in, fd_out, off_out, len, flags } => {
        let mut off_in_val = off_in;
        let mut off_out_val = off_out;
        syscall!(raw copy_file_range(fd_in.as_raw_fd(), &mut off_in_val, fd_out.as_raw_fd(), &mut off_out_val, len, flags as libc::c_uint))
      }
      #[cfg(target_os = "linux")]
      Op::Tee { fd_in, fd_out, size } => {
        syscall!(raw tee(fd_in.as_raw_fd(), fd_out.as_raw_fd(), size as libc::size_t, 0))
      }
      Op::ReadV { fd, iovecs, iov_count, .. } => {
        syscall!(raw readv(fd.as_raw_fd(), iovecs, iov_count as libc::c_int))
      }
      Op::WriteV { fd, iovecs, iov_count, .. } => {
        syscall!(raw writev(fd.as_raw_fd(), iovecs, iov_count as libc::c_int))
      }
      Op::ReadVAt { fd, iovecs, iov_count, offset, .. } => {
        syscall!(raw preadv(fd.as_raw_fd(), iovecs, iov_count as libc::c_int, offset))
      }
      Op::WriteVAt { fd, iovecs, iov_count, offset, .. } => {
        syscall!(raw pwritev(fd.as_raw_fd(), iovecs, iov_count as libc::c_int, offset))
      }
      Op::Timeout { duration, .. } => {
        std::thread::sleep(duration);
        0
      }
      Op::Nop => 0,
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
    Ok(())
  }

  fn push(&mut self, id: u64, op: crate::backend::op::Op) -> io::Result<()> {
    use crate::backend::op::Op;
    use crate::backend::pollingv2::interest::Interest;
    use std::os::fd::AsRawFd;

    let fd_and_interest = match &op {
      Op::ReadV { .. }
      | Op::WriteV { .. }
      | Op::ReadVAt { .. }
      | Op::WriteVAt { .. } => {
        let result = Poller::run_op_blocking(op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      Op::Send { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::SendTo { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Recv { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::RecvFrom { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Accept { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Connect { .. } => None,
      Op::Bind { .. }
      | Op::Listen { .. }
      | Op::Shutdown { .. }
      | Op::OpenAt { .. }
      | Op::Close { .. }
      | Op::Fsync { .. }
      | Op::Truncate { .. } => {
        let result = Poller::run_op_blocking(op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(target_os = "linux")]
      Op::Tee { fd_in, .. } => {
        Some((fd_in.as_raw_fd(), Interest::READ_AND_WRITE))
      }
      Op::Timeout { .. } => None,
      Op::Nop => {
        let result = Poller::run_op_blocking(op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      Op::Socket { .. } => None,
      Op::LinkAt { .. }
      | Op::SymlinkAt { .. }
      | Op::UnlinkAt { .. }
      | Op::RenameAt { .. }
      | Op::MkdirAt { .. } => {
        let result = Poller::run_op_blocking(op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(target_os = "linux")]
      Op::Splice { .. } | Op::CopyFileRange { .. } => {
        let result = Poller::run_op_blocking(op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
      #[cfg(unix)]
      Op::SendFile { .. } => {
        let result = Poller::run_op_blocking(op);
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
        let result = Poller::run_op_blocking(op);
        let final_result = if result < 0 { result } else { -(errno as isize) };
        self.immediate.push(ImmediateCompletion { id, result: final_result });
        return Ok(());
      }
    } else {
      match &self.op_map[&id] {
        Op::Connect { fd, .. } => {
          let fd = fd.as_raw_fd();
          let result =
            Poller::run_op_blocking(self.op_map.remove(&id).unwrap());
          if result == -(libc::EINPROGRESS as isize) {
            self.fd_map().insert(id, fd);
            self.sys().add(fd, id, Interest::WRITE)?;
          } else {
            self.immediate.push(ImmediateCompletion { id, result });
          }
        }
        Op::Timeout { duration, .. } => {
          let duration_ms = duration.as_millis() as RawFd;
          self.fd_map().insert(id, duration_ms);
          self.sys().add(duration_ms, id, Interest::TIMER)?;
          // op stays in op_map; kqueue fires when duration elapses
        }
        Op::Socket { .. } => {
          let result =
            Poller::run_op_blocking(self.op_map.remove(&id).unwrap());
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

      // Look up fd from our internal map
      let Some(entry_fd) = self.fd_map().get(&operation_id).copied() else {
        panic!("couldn't find fd for operation {}", operation_id);
      };

      // Remove op temporarily; put it back on EAGAIN
      let op = match self.op_map.remove(&operation_id) {
        Some(op) => op,
        None => {
          panic!("couldn't find op for operation {}", operation_id);
        }
      };
      let result = Poller::run_op_on_event(&op);

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
      let was_deleted = self.fd_map().remove(&operation_id).is_some();
      assert!(was_deleted);

      self.completed.push(OpCompleted::new(operation_id, result));
    }

    Ok(self.completed.as_ref())
  }
}

#[cfg(test)]
mod unit_tests {
  use super::*;

  // Run the standard IoBackend test suite
  crate::test_io_backend!(Poller::new());
}
