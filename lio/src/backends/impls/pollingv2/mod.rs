//! Simple OS-specific polling implementation
//!
//! This module provides a minimal polling interface for async I/O.
//! Uses kqueue on BSD/macOS, epoll on Linux (future), IOCP on Windows (future).

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

use crate::backends::{IoDriver, IoHandler, IoSubmitter, OpCompleted};
use crate::{backends::SubmitErr, op::Operation, store::OpStore};

/// Interest/Event flags for I/O readiness
///
/// This type is used for both:
/// - Registering interest (what you want to be notified about)
/// - Receiving events (what actually happened)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
  bits: u8,
}

impl Interest {
  pub const NONE: Self = Self { bits: 0 };
  pub const READ: Self = Self { bits: 1 << 0 };
  pub const WRITE: Self = Self { bits: 1 << 1 };
  pub const TIMER: Self = Self { bits: 1 << 2 };
  pub const READ_AND_WRITE: Self =
    Self { bits: Self::READ.bits | Self::WRITE.bits };

  pub const fn is_readable(self) -> bool {
    self.bits & Self::READ.bits != 0
  }

  pub const fn is_writable(self) -> bool {
    self.bits & Self::WRITE.bits != 0
  }

  pub const fn is_timer(self) -> bool {
    self.bits & Self::TIMER.bits != 0
  }

  pub const fn is_none(self) -> bool {
    self.bits == 0
  }

  /// Combine interests using bitwise OR
  pub const fn or(self, other: Self) -> Self {
    Self { bits: self.bits | other.bits }
  }

  /// Check if this interest contains all bits from another
  pub const fn contains(self, other: Self) -> bool {
    (self.bits & other.bits) == other.bits
  }
}

impl std::ops::BitOr for Interest {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    self.or(rhs)
  }
}

impl std::ops::BitOrAssign for Interest {
  fn bitor_assign(&mut self, rhs: Self) {
    *self = self.or(rhs);
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

// The raw pointers in NativeEvent (e.g., kevent) are never dereferenced across threads
unsafe impl Send for Events {}
unsafe impl Sync for Events {}

impl Default for Events {
  fn default() -> Self {
    Self::with_capacity(512)
  }
}

impl Events {
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

  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Returns the vec of maybe-initialised values. Meant for OS to fill and
  /// then we set correct length.
  unsafe fn as_raw_buf(
    &mut self,
  ) -> &mut [<sys::OsPoller as ReadinessPoll>::NativeEvent] {
    unsafe {
      slice::from_raw_parts_mut(
        self.events.as_mut_ptr(),
        self.events.capacity(),
      )
    }
  }

  unsafe fn set_len(&mut self, len: usize) {
    assert!(len <= self.events.capacity(), "set_len: len must be <= capacity");
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

//
// impl Poller {
//   // /// Create a new poller
//   // pub fn new() -> io::Result<Self> {
//   //   Ok(Self { inner: sys::OsPoller::new()? })
//   // }
//
//   /// Add interest for a file descriptor
//   pub unsafe fn add(
//     &self,
//     fd: RawFd,
//     key: u64,
//     interest: Interest,
//   ) -> io::Result<()> {
//     assert!(fd >= 0, "Poller::add: fd must be >= 0, got {}", fd);
//     assert!(!interest.is_none(), "Poller::add: interest cannot be None");
//
//     self.inner.add(fd, key, interest)
//   }
//
//   /// Modify interest for a file descriptor
//   pub unsafe fn modify(
//     &self,
//     fd: RawFd,
//     key: u64,
//     interest: Interest,
//   ) -> io::Result<()> {
//     if fd < 0 {
//       return Err(io::Error::from_raw_os_error(libc::EBADF));
//     }
//
//     assert!(!interest.is_none(), "Poller::modify: interest cannot be None");
//
//     self.inner.modify(fd, key, interest)
//   }
//
//   /// Remove interest for a file descriptor
//   pub unsafe fn delete(&self, fd: RawFd) -> io::Result<()> {
//     if fd < 0 {
//       return Err(io::Error::from_raw_os_error(libc::EBADF));
//     }
//
//     self.inner.delete(fd)
//   }
//
//   /// Remove a timer by key
//   pub unsafe fn delete_timer(&self, key: u64) -> io::Result<()> {
//     self.inner.delete_timer(key)
//   }
// }

struct BlockingCompletion {
  id: u64,
  result: isize,
}

pub struct PollerState {
  sys: sys::OsPoller,
}
pub struct PollerHandle {
  events: Events,
  fd_map: HashMap<u64, RawFd>,
  blocking_sender: crossbeam_channel::Receiver<BlockingCompletion>,
}

impl PollerHandle {
  fn tick_inner(
    &mut self,
    state: &PollerState,
    store: &OpStore,
    can_wait: bool,
  ) -> io::Result<Vec<OpCompleted>> {
    let mut completed = Vec::new();

    // First, collect all blocking completions from the channel
    while let Ok(blocking) = self.blocking_sender.try_recv() {
      completed.push(OpCompleted::new(blocking.id, blocking.result));
    }

    self.events.clear();
    let items_written = state.sys.wait(
      unsafe { self.events.as_raw_buf() },
      if can_wait { None } else { Some(Duration::from_millis(0)) },
    )?;

    unsafe { self.events.set_len(items_written) };

    for event in self.events.iter() {
      let operation_id = event.key;

      // Skip internal notification events - they're just used to wake up the poller
      if operation_id == u64::MAX {
        continue;
      }

      // Look up fd from our internal map
      let entry_fd = *self
        .fd_map
        .get(&operation_id)
        .expect("couldn't find fd for operation");

      let result = store
        .get_mut(operation_id, |entry| entry.run_blocking())
        .expect("couldn't find entry");

      // Check for EAGAIN/EINPROGRESS (would block)
      if result < 0 {
        let errno = (-result) as i32;
        if errno == libc::EAGAIN
          || errno == libc::EWOULDBLOCK
          || errno == libc::EINPROGRESS
        {
          dbg!(state.sys.modify(entry_fd, operation_id, event.interest))?;
          continue;
        }
      }

      // Operation completed (success or error other than would-block)
      {
        // Clean up - use delete_timer for timer events, delete for fd-based events
        if event.interest.is_timer() {
          dbg!(state.sys.delete_timer(operation_id))?;
        } else {
          dbg!(state.sys.delete(entry_fd))?;
        }
        self.fd_map.remove(&operation_id);

        completed.push(OpCompleted::new(operation_id, result));

        // let exists = store.get_mut(operation_id, |reg| reg.set_done(result));
        // assert!(exists.is_some(), "Cannot find matching operation");
        // assert!(store.remove(operation_id));
      };
    }

    Ok(completed)
  }
}

impl IoHandler for PollerHandle {
  fn tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    self.tick_inner(into_shared(state), store, true)
  }

  fn try_tick(
    &mut self,
    state: *const (),
    store: &OpStore,
  ) -> io::Result<Vec<OpCompleted>> {
    self.tick_inner(into_shared(state), store, false)
  }
}
pub struct PollerSubmitter {
  blocking_sender: crossbeam_channel::Sender<BlockingCompletion>,
}

impl PollerSubmitter {
  fn new_polling(
    &self,
    state: &PollerState,
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr> {
    let meta = op.meta();
    let interest = if meta.is_cap_fd() {
      if meta.is_fd_readable() && meta.is_fd_writable() {
        Interest::READ_AND_WRITE
      } else if meta.is_fd_readable() {
        Interest::READ
      } else if meta.is_fd_writable() {
        Interest::WRITE
      } else {
        panic!()
      }
    } else if meta.is_cap_timer() {
      Interest::TIMER
    } else {
      panic!()
    };

    state.sys.add(op.cap(), id, interest).map_err(SubmitErr::Io)?;
    Ok(())
  }
}

fn into_shared<'a>(ptr: *const ()) -> &'a PollerState {
  unsafe { &*(ptr as *const _) }
}

impl IoSubmitter for PollerSubmitter {
  fn submit(
    &mut self,
    state: *const (),
    id: u64,
    op: &dyn Operation,
  ) -> Result<(), SubmitErr> {
    let meta = op.meta();
    if meta.is_cap_none() {
      let result = op.run_blocking();
      let _ = self.blocking_sender.send(BlockingCompletion { id, result });
      self.notify(state)?;
      return Ok(());
    };

    if !meta.is_beh_run_before_noti() {
      return self.new_polling(into_shared(state), id, op);
    }

    // CONNECT

    let result = op.run_blocking();

    if result == libc::EINPROGRESS as isize {
      self.new_polling(into_shared(state), id, op)
    } else {
      let result = op.run_blocking();
      let _ = self.blocking_sender.send(BlockingCompletion { id, result });
      self.notify(state)?;
      Ok(())
    }
  }
  fn notify(&mut self, state: *const ()) -> Result<(), SubmitErr> {
    Ok(into_shared(state).sys.notify()?)
  }
}

/// Main polling structure
pub struct Poller(());

impl IoDriver for Poller {
  type Handler = PollerHandle;
  type Submitter = PollerSubmitter;

  fn new_state() -> io::Result<*const ()> {
    let state = PollerState { sys: sys::OsPoller::new()? };
    Ok(Box::into_raw(Box::new(state)) as *const _)
  }
  fn drop_state(state: *const ()) {
    let _ = unsafe { Box::from_raw(state as *mut PollerState) };
  }
  fn new(_ptr: *const ()) -> io::Result<(Self::Submitter, Self::Handler)> {
    let (blocking_sender, blocking_recv) = crossbeam_channel::unbounded();
    Ok((
      PollerSubmitter { blocking_sender },
      PollerHandle {
        events: Events::default(),
        fd_map: HashMap::new(),
        blocking_sender: blocking_recv,
      },
    ))
  }
}

#[cfg(test)]
test_io_driver!(Poller);
