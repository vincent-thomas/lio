//! Simple OS-specific polling implementation
//!
//! This module provides a minimal polling interface for async I/O.
//! Uses kqueue on BSD/macOS, epoll on Linux (future), IOCP on Windows (future).

mod notifier;
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

mod util;

#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::slice;
use std::time::Duration;

use crate::op::Operation;
use crate::op_registration::OpNotification;
use crate::sync::Mutex;
use crate::{OperationProgress, backends::IoBackend, driver::OpStore};

/// Interest flags for event registration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
  None,
  Read,
  Write,
  ReadAndWrite,
  Timer,
}

impl Interest {
  pub const READ: Self = Self::Read;
  pub const WRITE: Self = Self::Write;

  pub fn is_readable(&self) -> bool {
    matches!(self, Self::Read | Self::ReadAndWrite)
  }

  pub fn is_writable(&self) -> bool {
    matches!(self, Self::Write | Self::ReadAndWrite)
  }

  pub fn is_timer(&self) -> bool {
    matches!(self, Self::Timer)
  }
}

/// Trait for OS-specific readiness polling implementations
///
/// This abstraction makes it easy to add new platforms (epoll, IOCP, etc.)
///
/// ## Design for cross-platform compatibility
///
/// - **epoll**: Registers both read/write interest on a single fd
/// - **kqueue**: Registers read and write separately as different filters
/// - This trait accommodates both by accepting Interest flags
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
  ///
  /// For kqueue: returns either READ or WRITE (one event per filter)
  /// For epoll: may return both READ and WRITE in a single event
  fn event_interest(event: &Self::NativeEvent) -> Interest;
}

/// Represents an event interest registration
#[derive(Debug, Clone, Copy)]
pub struct Event {
  /// User-provided key to identify this event
  pub key: u64,
  /// Whether this is a read event
  pub readable: bool,
  /// Whether this is a write event
  pub writable: bool,
  /// Whether this is a timer event
  pub timer: bool,
}

/// A collection of events returned from polling
pub struct Events {
  events: Vec<<sys::OsPoller as ReadinessPoll>::NativeEvent>,
}

// impl AsMut<[<sys::OsPoller as ReadinessPoll>::NativeEvent]> for Events {
//   /// Makes Events ready for input.
//   fn as_mut(&mut self) -> &mut [<sys::OsPoller as ReadinessPoll>::NativeEvent] {
//
//     // &mut self.events
//   }
// }

// SAFETY: Events buffer is protected by Mutex and only used within the same thread
// The raw pointers in NativeEvent (e.g., kevent) are never dereferenced across threads
unsafe impl Send for Events {}
unsafe impl Sync for Events {}

impl Events {
  /// Create a new empty events collection with default capacity
  pub fn new() -> Self {
    Self::with_capacity(512)
  }

  /// Create a new empty events collection with specified capacity
  pub fn with_capacity(capacity: usize) -> Self {
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

  unsafe fn set_len(&mut self, len: usize) {
    assert!(len <= self.events.capacity(), "set_len: len must be <= capacity");
    unsafe { self.events.set_len(len) }
  }

  /// Returns the whole allocated buf
  fn as_buf_mut(
    &mut self,
  ) -> &mut [<sys::OsPoller as ReadinessPoll>::NativeEvent] {
    unsafe {
      slice::from_raw_parts_mut(
        self.events.as_mut_ptr(),
        self.events.capacity(),
      )
    }
  }

  fn clear(&mut self) {
    self.events.clear();
  }

  fn get_event(&self, index: usize) -> Event {
    assert!(index < self.events.len(), "get_event: index out of bounds");
    let native_event = &self.events[index];
    let key = sys::OsPoller::event_key(native_event);
    let interest = sys::OsPoller::event_interest(native_event);

    Event {
      key,
      readable: interest.is_readable(),
      writable: interest.is_writable(),
      timer: interest.is_timer(),
    }
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

/// Main polling structure
pub struct Poller {
  fd_map: Mutex<HashMap<u64, RawFd>>,
  inner: sys::OsPoller,
  events: Mutex<Events>,
}

impl Poller {
  /// Create a new poller
  pub fn new() -> io::Result<Self> {
    Ok(Self {
      inner: sys::OsPoller::new()?,
      fd_map: Mutex::new(HashMap::default()),
      events: Mutex::new(Events::new()),
    })
  }

  /// Add interest for a file descriptor
  pub unsafe fn add(
    &self,
    fd: RawFd,
    key: u64,
    interest: Interest,
  ) -> io::Result<()> {
    assert!(fd >= 0, "Poller::add: fd must be >= 0, got {}", fd);
    assert_ne!(
      interest,
      Interest::None,
      "Poller::add: interest cannot be None"
    );

    self.inner.add(fd, key, interest)
  }

  /// Modify interest for a file descriptor
  pub unsafe fn modify(&self, fd: RawFd, event: Event) -> io::Result<()> {
    if fd < 0 {
      return Err(io::Error::from_raw_os_error(libc::EBADF));
    }

    assert!(
      event.readable || event.writable || event.timer,
      "Poller::modify: at least one of readable/writable/timer must be true"
    );

    let interest = match (event.readable, event.writable) {
      (true, true) => Interest::ReadAndWrite,
      (true, false) => Interest::Read,
      (false, true) => Interest::Write,
      (false, false) => unreachable!("already checked above"),
    };

    self.inner.modify(fd, event.key, interest)
  }

  /// Remove interest for a file descriptor
  pub unsafe fn delete(&self, fd: RawFd) -> io::Result<()> {
    if fd < 0 {
      return Err(io::Error::from_raw_os_error(libc::EBADF));
    }

    self.inner.delete(fd)
  }

  /// Remove a timer by key
  pub unsafe fn delete_timer(&self, key: u64) -> io::Result<()> {
    self.inner.delete_timer(key)
  }

  /// Wait for events with optional timeout
  /// Reuses the internal events buffer to avoid allocations
  fn wait(&self, timeout: Option<Duration>) -> io::Result<()> {
    let mut events = self.events.lock();
    events.clear();

    let n = self.inner.wait(events.as_buf_mut(), timeout)?;
    assert!(
      n <= events.events.capacity(),
      "wait returned more events than buffer capacity"
    );
    unsafe { events.set_len(n) };
    Ok(())
  }

  fn new_polling<T>(&self, op: T, store: &OpStore) -> OperationProgress<T>
  where
    T: Operation,
  {
    let interest = T::INTEREST.expect("op is event but no interest??");
    let id = store.next_id();
    let fd = op.fd().expect("not provided fd");
    assert!(fd >= 0, "new_polling: fd must be valid (>= 0)");

    self.fd_map.lock().insert(id, fd);
    store.insert(id, Box::new(op));
    unsafe { self.add(fd, id, interest) }.expect("registration failed");
    OperationProgress::<T>::new_store_tracked(id)
  }
}

impl IoBackend for Poller {
  fn submit<O>(&self, op: O, store: &OpStore) -> OperationProgress<O>
  where
    O: Operation + Sized,
  {
    if O::INTEREST.is_none() {
      return OperationProgress::FromResult {
        res: Some(op.run_blocking()),
        operation: op,
      };
    };

    if !O::FLAGS.is_connect() {
      return self.new_polling(op, store);
    };

    let result = op.run_blocking();

    if result
      .as_ref()
      .is_err_and(|err| err.raw_os_error() == Some(libc::EINPROGRESS))
    {
      self.new_polling(op, store)
    } else {
      OperationProgress::<O>::new_from_result(op, result)
    }
  }
  fn notify(&self) {
    self.inner.notify().unwrap();
  }
  fn tick(&self, store: &OpStore, can_wait: bool) {
    self
      .wait(if can_wait { None } else { Some(Duration::from_millis(0)) })
      .expect("background thread failed");

    let events = self.events.lock();

    for event in events.iter() {
      let operation_id = event.key as u64;
      assert_ne!(
        operation_id,
        u64::MAX,
        "Poller::tick: internal notification event should be filtered"
      );

      // Look up fd from our internal map
      let entry_fd = *self
        .fd_map
        .lock()
        .get(&operation_id)
        .expect("couldn't find fd for operation");

      let result = store
        .get_mut(operation_id, |entry| entry.run_blocking())
        .expect("couldn't find entry");

      match result {
        // Special-case.
        Err(err)
          if err.kind() == ErrorKind::WouldBlock
            || err.raw_os_error() == Some(libc::EINPROGRESS) =>
        {
          unsafe { self.modify(entry_fd, event) }.expect("fd sure exists");

          continue;
        }
        _ => {
          // Clean up - use delete_timer for timer events, delete for fd-based events
          if event.timer {
            unsafe { self.delete_timer(operation_id) }.unwrap();
          } else {
            unsafe { self.delete(entry_fd) }.unwrap();
          }
          self.fd_map.lock().remove(&operation_id);

          let set_done_result = store
            .get_mut(operation_id, |reg| {
              // if should keep.
              reg.set_done(result)
            })
            .expect("Cannot find matching operation");
          match set_done_result {
            None => {}
            Some(value) => match value {
              OpNotification::Waker(waker) => waker.wake(),
              OpNotification::Callback(callback) => {
                store.get_mut(operation_id, |entry| callback.call(entry));
                assert!(store.remove(operation_id));
              }
            },
          }
        }
      };
    }
  }
}
