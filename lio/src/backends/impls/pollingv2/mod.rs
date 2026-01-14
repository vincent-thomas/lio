//! `lio`-provided [`IoDriver`] impl for `epoll`/`kqueue` (platform-specific).

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
use std::sync::Arc;
use std::time::Duration;

use crate::backends::pollingv2::interest::Interest;
use crate::backends::{IoBackend, IoDriver, IoSubmitter, OpCompleted, OpStore};
use crate::{backends::SubmitErr, operation::Operation};
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

// SAFETY: The raw pointers in NativeEvent (e.g., kevent) are never dereferenced across threads
unsafe impl Send for Events {}
// unsafe impl Sync for Events {}

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

struct BlockingCompletion {
  id: u64,
  result: isize,
}

pub struct PollerState {
  sys: sys::OsPoller,
  fd_map: scc::HashMap<u64, RawFd>,
}
pub struct PollerHandle {
  events: Events,
  blocking_sender: crossbeam_channel::Receiver<BlockingCompletion>,
  state: Arc<PollerState>,
}

impl PollerHandle {
  fn tick_inner(
    &mut self,
    store: &OpStore,
    can_wait: bool,
  ) -> io::Result<Vec<OpCompleted>> {
    let state = &self.state;
    let mut completed = Vec::new();

    // First, collect all blocking completions from the channel
    while let Ok(blocking) = self.blocking_sender.try_recv() {
      completed.push(OpCompleted::new(blocking.id, blocking.result));
    }

    self.events.clear();
    let items_written = state.sys.wait(
      // SAFETY: as_raw_buf() provides mutable access to the entire capacity of the events buffer,
      // which the OS will fill with events. The buffer is properly allocated and sized.
      unsafe { self.events.as_raw_buf() },
      if can_wait { None } else { Some(Duration::from_millis(0)) },
    )?;

    // Blocking calls may have run notify() to get values.
    while let Ok(blocking) = self.blocking_sender.try_recv() {
      completed.push(OpCompleted::new(blocking.id, blocking.result));
    }

    // SAFETY: The OS's wait() call filled items_written events into our buffer.
    // set_len is safe because items_written <= capacity (guaranteed by wait implementation).
    unsafe { self.events.set_len(items_written) };

    for event in self.events.iter() {
      let operation_id = event.key;

      // Skip internal notification events - they're just used to wake up the poller
      if operation_id == u64::MAX {
        continue;
      }

      let result = store
        .get_mut(operation_id, |entry| entry.run_blocking())
        .expect("couldn't find entry");

      // Look up fd from our internal map
      let Some(entry_fd) = state.fd_map.get_sync(&operation_id).map(|fd| *fd)
      else {
        panic!("couldn't find fd for operation");
      };

      // Check for EAGAIN/EINPROGRESS (would block)
      if result < 0 {
        let errno = (-result) as i32;
        if errno == libc::EAGAIN
          || errno == libc::EWOULDBLOCK
          || errno == libc::EINPROGRESS
        {
          state.sys.modify(entry_fd, operation_id, event.interest)?;
          continue;
        }
      }

      // Operation completed (success or error other than would-block)
      {
        // Clean up - use delete_timer for timer events, delete for fd-based events
        if event.interest.is_timer() {
          state.sys.delete_timer(operation_id)?;
        } else {
          state.sys.delete(entry_fd)?;
        }
        let was_deleted = state.fd_map.remove_sync(&operation_id).is_some();
        assert!(was_deleted);

        completed.push(OpCompleted::new(operation_id, result));
      };
    }

    Ok(completed)
  }
}

impl IoDriver for PollerHandle {
  fn tick(&mut self, store: &OpStore) -> io::Result<Vec<OpCompleted>> {
    self.tick_inner(store, true)
  }

  fn try_tick(&mut self, store: &OpStore) -> io::Result<Vec<OpCompleted>> {
    self.tick_inner(store, false)
  }
}
pub struct PollerSubmitter {
  blocking_sender: crossbeam_channel::Sender<BlockingCompletion>,
  state: Arc<PollerState>,
}

impl PollerSubmitter {
  fn new_polling(&self, id: u64, op: &dyn Operation) -> Result<(), SubmitErr> {
    let state = &self.state;
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

    state.fd_map.insert_sync(id, op.cap()).unwrap();
    state.sys.add(op.cap(), id, interest).map_err(SubmitErr::Io)?;

    Ok(())
  }
}

impl IoSubmitter for PollerSubmitter {
  fn submit(&mut self, id: u64, op: &dyn Operation) -> Result<(), SubmitErr> {
    let meta = op.meta();

    if meta.is_cap_none() {
      let result = op.run_blocking();
      let _ = self.blocking_sender.send(BlockingCompletion { id, result });
      dbg!(self.notify())?;
      return Ok(());
    };

    if !meta.is_beh_run_before_noti() {
      return self.new_polling(id, op);
    }

    // CONNECT

    let result = op.run_blocking();

    if result == -(libc::EINPROGRESS as isize) {
      self.new_polling(id, op)
    } else {
      let _ = self.blocking_sender.send(BlockingCompletion { id, result });
      dbg!(self.notify())?;
      Ok(())
    }
  }
  fn notify(&mut self) -> Result<(), SubmitErr> {
    Ok(self.state.sys.notify()?)
  }
}

/// Main polling structure
pub struct Poller(());

impl IoBackend for Poller {
  type Driver = PollerHandle;
  type Submitter = PollerSubmitter;
  type State = PollerState;

  fn new_state() -> io::Result<Self::State> {
    Ok(PollerState {
      sys: sys::OsPoller::new()?,
      fd_map: scc::HashMap::default(),
    })
  }

  fn new(
    state: Arc<Self::State>,
  ) -> io::Result<(Self::Submitter, Self::Driver)> {
    let (blocking_sender, blocking_recv) = crossbeam_channel::unbounded();

    Ok((
      PollerSubmitter {
        blocking_sender: blocking_sender.clone(),
        state: state.clone(),
      },
      PollerHandle {
        events: Events::default(),
        blocking_sender: blocking_recv,
        state,
      },
    ))
  }
}

#[cfg(test)]
test_io_driver!(Poller);
