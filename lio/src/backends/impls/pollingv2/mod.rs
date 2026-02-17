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
use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

use crate::backends::pollingv2::interest::Interest;
use crate::backends::{IoBackend, OpCompleted, OpStore};
use crate::operation::Operation;
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
/// ```rust,ignore
/// let mut backend = Poller::default();
/// backend.init(1024)?;
///
/// backend.push(id, &op)?;
/// backend.flush()?;
///
/// let completions = backend.wait(&mut store)?;
/// ```
#[derive(Default)]
pub struct Poller {
  /// The OS-specific polling mechanism (epoll/kqueue)
  sys: Option<sys::OsPoller>,
  /// Map of operation ID to file descriptor (for cleanup)
  fd_map: Option<scc::HashMap<u64, RawFd>>,
  /// Map of operation ID to Op (for completion)
  op_map: std::collections::HashMap<u64, crate::op::Op>,
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
  fn fd_map(&self) -> &scc::HashMap<u64, RawFd> {
    self.fd_map.as_ref().expect("Poller not initialized - call init() first")
  }

  fn run_op_blocking(op: crate::op::Op) -> isize {
    use crate::buf::BufLike;
    use crate::op::Op;
    use std::os::fd::AsRawFd;

    match op {
      Op::Read { fd, buffer } => {
        let fd = fd.as_raw_fd();
        let buf = unsafe { buffer.take::<&mut [u8]>() };
        let ptr = buf.as_mut_ptr();
        let len = buf.len();
        unsafe { libc::read(fd, ptr as *mut _, len) }
      }
      Op::Write { fd, buffer } => {
        let fd = fd.as_raw_fd();
        let buf = unsafe { buffer.take::<&[u8]>() };
        let ptr = buf.as_ptr() as *const libc::c_void;
        let len = buf.len();
        unsafe { libc::write(fd, ptr, len) }
      }
      Op::ReadAt { fd, offset, buffer } => {
        let fd = fd.as_raw_fd();
        let buf = unsafe { buffer.take::<&mut [u8]>() };
        let ptr = buf.as_mut_ptr();
        let len = buf.len();
        unsafe { libc::pread(fd, ptr as *mut _, len, offset) }
      }
      Op::WriteAt { fd, offset, buffer } => {
        let fd = fd.as_raw_fd();
        let buf = unsafe { buffer.take::<&[u8]>() };
        let ptr = buf.as_ptr() as *const libc::c_void;
        let len = buf.len();
        unsafe { libc::pwrite(fd, ptr, len, offset) }
      }
      Op::Send { fd, flags, buffer } => {
        let fd = fd.as_raw_fd();
        let buf = unsafe { buffer.take::<&[u8]>() };
        let ptr = buf.as_ptr() as *const libc::c_void;
        let len = buf.len();
        unsafe { libc::send(fd, ptr, len, flags) }
      }
      Op::Recv { fd, flags, buffer } => {
        let fd = fd.as_raw_fd();
        let buf = unsafe { buffer.take::<&mut [u8]>() };
        let ptr = buf.as_mut_ptr();
        let len = buf.len();
        unsafe { libc::recv(fd, ptr as *mut _, len, flags) }
      }
      Op::Accept { fd, addr, len } => unsafe {
        let fd = fd.as_raw_fd();
        libc::accept(fd, addr as *mut _, len) as isize
      },
      Op::Connect { fd, addr, len, connect_called: _ } => {
        let fd = fd.as_raw_fd();
        unsafe {
          let ret = libc::connect(
            fd,
            std::mem::transmute::<&libc::sockaddr_storage, _>(&addr),
            len,
          );
          if ret < 0 {
            let err = *libc::__error();
            if err == libc::EINPROGRESS || err == libc::EISCONN {
              return if err == libc::EISCONN { 0 } else { -(err as isize) };
            }
          }
          ret as isize
        }
      }
      Op::Bind { fd, addr } => unsafe {
        let fd = fd.as_raw_fd();
        libc::bind(
          fd,
          std::mem::transmute::<&libc::sockaddr_storage, _>(&addr),
          std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
        ) as isize
      },
      Op::Listen { fd, backlog } => unsafe {
        let fd = fd.as_raw_fd();
        libc::listen(fd, backlog) as isize
      },
      Op::Shutdown { fd, how } => unsafe {
        let fd = fd.as_raw_fd();
        libc::shutdown(fd, how) as isize
      },
      Op::Socket { domain, ty, proto } => unsafe {
        libc::socket(domain, ty, proto) as isize
      },
      Op::OpenAt { dir_fd, path, flags } => unsafe {
        libc::openat(dir_fd.as_raw_fd(), path, flags) as isize
      },
      Op::Close { fd } => unsafe { libc::close(fd.as_raw_fd()) as isize },
      Op::Fsync { fd } => unsafe { libc::fsync(fd.as_raw_fd()) as isize },
      Op::Truncate { fd, size } => unsafe {
        libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) as isize
      },
      Op::LinkAt { old_dir_fd, old_path, new_dir_fd, new_path } => unsafe {
        libc::linkat(
          old_dir_fd.as_raw_fd(),
          old_path,
          new_dir_fd.as_raw_fd(),
          new_path,
          0,
        ) as isize
      },
      Op::SymlinkAt { target, linkpath, dir_fd } => unsafe {
        libc::symlinkat(target, dir_fd.as_raw_fd(), linkpath) as isize
      },
      #[cfg(target_os = "linux")]
      Op::Tee { fd_in, fd_out, size } => unsafe {
        libc::tee(
          fd_in.as_raw_fd(),
          fd_out.as_raw_fd(),
          size as libc::size_t,
          0,
        ) as isize
      },
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
    self.fd_map = Some(scc::HashMap::with_capacity(cap));
    self.op_map = std::collections::HashMap::with_capacity(cap);
    self.events = Events::with_capacity(cap.min(4096));
    self.immediate = Vec::with_capacity(64);
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, op: crate::op::Op) -> io::Result<()> {
    use crate::backends::pollingv2::interest::Interest;
    use crate::op::Op;
    use std::os::fd::AsRawFd;

    let fd_and_interest = match &op {
      Op::Read { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Write { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::ReadAt { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::WriteAt { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Send { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Recv { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Accept { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Connect { .. } => None,
      Op::Bind { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Listen { fd, .. } => Some((fd.as_raw_fd(), Interest::READ)),
      Op::Shutdown { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::OpenAt { dir_fd, .. } => Some((dir_fd.as_raw_fd(), Interest::READ)),
      Op::Close { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Fsync { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
      Op::Truncate { fd, .. } => Some((fd.as_raw_fd(), Interest::WRITE)),
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
      Op::LinkAt { .. } | Op::SymlinkAt { .. } => {
        let result = Poller::run_op_blocking(op);
        self.immediate.push(ImmediateCompletion { id, result });
        return Ok(());
      }
    };

    self.op_map.insert(id, op);

    if let Some((fd, interest)) = fd_and_interest {
      self.fd_map().insert_sync(id, fd).unwrap();
      self.sys().add(fd, id, interest)?;
    } else {
      match &self.op_map[&id] {
        Op::Connect { fd, .. } => {
          let fd = fd.as_raw_fd();
          let result =
            Poller::run_op_blocking(self.op_map.remove(&id).unwrap());
          if result == -(libc::EINPROGRESS as isize) {
            self.fd_map().insert_sync(id, fd).unwrap();
            self.sys().add(fd, id, Interest::WRITE)?;
          } else {
            self.immediate.push(ImmediateCompletion { id, result });
          }
        }
        Op::Timeout { .. } => {
          self.immediate.push(ImmediateCompletion { id, result: 0 });
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

    let items_written = self.sys.as_ref().unwrap().wait(events, timeout)?;

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

      // Get the operation from our op_map and run it
      let op = match self.op_map.remove(&operation_id) {
        Some(op) => op,
        None => {
          panic!("couldn't find op for operation {}", operation_id);
        }
      };
      let result = Poller::run_op_blocking(op);

      // Look up fd from our internal map
      let Some(entry_fd) = self.fd_map().get_sync(&operation_id).map(|fd| *fd)
      else {
        panic!("couldn't find fd for operation {}", operation_id);
      };

      // Check for EAGAIN/EINPROGRESS (would block)
      if result < 0 {
        let errno = (-result) as i32;
        if errno == libc::EAGAIN
          || errno == libc::EWOULDBLOCK
          || errno == libc::EINPROGRESS
        {
          // Re-arm for more events
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
      let was_deleted = self.fd_map().remove_sync(&operation_id).is_some();
      assert!(was_deleted);

      self.completed.push(OpCompleted::new(operation_id, result));
    }

    Ok(self.completed.as_ref())
  }
}

#[cfg(test)]
mod unit_tests {
  use super::*;

  #[test]
  fn test_init() {
    let mut backend = Poller::new();
    backend.init(64).unwrap();
  }
}
