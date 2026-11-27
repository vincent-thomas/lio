use std::cell::RefCell;
use std::marker::PhantomData;
use std::time::Duration;
use std::{
  collections::{HashMap, HashSet},
  os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};
use std::{io, ptr};

use crate::backend::impls::pollingv2::ReadinessPoll;
use crate::backend::impls::pollingv2::interest::Interest;

/// Helper to create a kevent struct that works across BSD variants.
/// FreeBSD has an extra `ext` field that macOS/iOS don't have.
#[inline]
fn make_kevent(
  ident: libc::uintptr_t,
  filter: i16,
  flags: u16,
  fflags: u32,
  data: i64,
  udata: *mut libc::c_void,
) -> libc::kevent {
  // SAFETY: zeroed kevent is valid - all fields are integers/pointers
  let mut kev: libc::kevent = unsafe { std::mem::zeroed() };
  kev.ident = ident;
  kev.filter = filter;
  kev.flags = flags;
  kev.fflags = fflags;
  // On macOS data is isize, on FreeBSD it's i64 (intptr_t)
  #[cfg(target_vendor = "apple")]
  {
    kev.data = data as isize;
  }
  #[cfg(not(target_vendor = "apple"))]
  {
    kev.data = data;
  }
  kev.udata = udata;
  kev
}

/// The filters of one descriptor's registration that are still in the
/// kernel.
///
/// Every descriptor is registered one-shot, so the kernel removes a filter
/// as it reports it. Removing it again would be a system call that finds
/// nothing, and a registration is replaced after every event: this is what
/// tells [`OsPoller::delete`] there is nothing left to remove.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Armed {
  read: bool,
  write: bool,
}

/// Wrapper around a kqueue file descriptor
///
/// This type is intentionally `!Send` to ensure it's only used from a single thread.
/// Interior mutability is provided via `RefCell` for tracking registered fds/timers.
pub struct OsPoller {
  kq_fd: OwnedFd,
  /// The registered descriptors, to match epoll's strict add semantics,
  /// each with the filters the kernel still holds for it.
  registered_fds: RefCell<HashMap<RawFd, Armed>>,
  /// Track registered timer keys separately (timers don't have fds)
  registered_timers: RefCell<HashSet<u64>>,
  /// Registrations that have not reached the kernel. `kevent` takes changes
  /// and gives events in one call, so they go with the next wait: however
  /// many descriptors gained a waiter since the last one, that is one
  /// system call instead of one each.
  pending: RefCell<Vec<libc::kevent>>,
  /// Registrations the kernel refused when a full `pending` was submitted
  /// on its own. They are handed out by the next wait, as it would have
  /// reported them.
  refused: RefCell<Vec<libc::kevent>>,
  notify: notify::Notify,
  /// Marker to make this type `!Send`
  _not_send: PhantomData<*const ()>,
}

impl OsPoller {
  /// Create a new kqueue instance
  pub fn new() -> io::Result<Self> {
    // Create kqueue
    // SAFETY: syscall!(kqueue()) returns a valid file descriptor or error.
    // If successful, we immediately take ownership of it via OwnedFd.
    let kqueue_fd = unsafe { OwnedFd::from_raw_fd(syscall!(kqueue())?) };
    syscall!(fcntl(kqueue_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC))?;

    let kqueue = Self {
      kq_fd: kqueue_fd,
      registered_fds: RefCell::new(HashMap::new()),
      registered_timers: RefCell::new(HashSet::new()),
      pending: RefCell::new(Vec::new()),
      refused: RefCell::new(Vec::new()),
      notify: notify::Notify::new()?,
      _not_send: PhantomData,
    };

    kqueue.notify.register(&kqueue)?;

    Ok(kqueue)
  }

  fn submit_changes(
    &self,
    changelist: &[<Self as ReadinessPoll>::NativeEvent],
  ) -> io::Result<()> {
    let changes = changelist;
    let nchanges = changes.len();

    let mut eventlist: Vec<<Self as ReadinessPoll>::NativeEvent> =
      Vec::with_capacity(nchanges);

    let spare = eventlist.spare_capacity_mut();
    let nevents = spare.len();

    syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      changes.as_ptr(),
      nchanges as libc::c_int,
      spare.as_mut_ptr().cast(),
      nevents as libc::c_int,
      ptr::null(),
    ))?;

    for ev in &eventlist {
      if (ev.flags & libc::EV_ERROR) != 0 {
        let err_code = ev.data as i32;
        if err_code != 0 && err_code != libc::ENOENT && err_code != libc::EPIPE
        {
          return Err(io::Error::from_raw_os_error(err_code));
        }
      }
    }

    Ok(())
  }

  /// Add or modify multiple interests in a single syscall (batched)
  ///
  /// Helper to build kevent structs for fd-based interests.
  fn build_fd_kevents(
    &self,
    changes: &mut [libc::kevent; 2],
    n: &mut usize,
    fd: RawFd,
    key: usize,
    interest: Interest,
    oneshot: bool,
  ) {
    let flags = if oneshot {
      libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT
    } else {
      libc::EV_ADD | libc::EV_ENABLE
    };

    if interest.is_readable() {
      changes[*n] = make_kevent(
        fd as libc::uintptr_t,
        libc::EVFILT_READ,
        flags,
        0,
        0,
        key as *mut libc::c_void,
      );
      *n += 1;
    }

    if interest.is_writable() {
      assert!(*n < 2, "Must have space for WRITE event");
      changes[*n] = make_kevent(
        fd as libc::uintptr_t,
        libc::EVFILT_WRITE,
        flags,
        0,
        0,
        key as *mut libc::c_void,
      );
      *n += 1;
    }
  }

  /// This is more efficient than calling add_interest/modify_interest twice
  /// when both readable and writable interests are needed.
  fn change_interests_batched(
    &self,
    fd: RawFd,
    key: usize,
    interest: Interest,
  ) -> io::Result<()> {
    self.change_interests_inner(fd, key, interest, true)
  }

  fn change_interests_inner(
    &self,
    fd: RawFd,
    key: usize,
    interest: Interest,
    oneshot: bool,
  ) -> io::Result<()> {
    // For kqueue timers, fd contains duration in milliseconds
    if interest.is_timer() {
      let duration_ms = fd as i64;

      let kev = make_kevent(
        key as libc::uintptr_t,
        libc::EVFILT_TIMER,
        libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        0,
        duration_ms,
        key as *mut libc::c_void,
      );

      syscall!(kevent(
        self.kq_fd.as_raw_fd(),
        &kev as *const libc::kevent,
        1,
        ptr::null_mut(),
        0,
        ptr::null(),
      ))?;

      return Ok(());
    }

    // SAFETY: libc::kevent is a C struct that is safe to zero-initialize.
    // All fields are primitive integers/pointers where zero is a valid bit pattern.
    let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
    let mut n = 0;

    self.build_fd_kevents(&mut changes, &mut n, fd, key, interest, oneshot);
    assert!(n <= 2, "n must be 0, 1, or 2, got {}", n);

    if n > 0 {
      let ret = syscall!(kevent(
        self.kq_fd.as_raw_fd(),
        changes.as_ptr(),
        n as i32,
        ptr::null_mut(),
        0,
        ptr::null(),
      ))?;

      assert_eq!(
        ret, 0,
        "kevent with no output events should return 0, got {}",
        ret
      );
    }

    Ok(())
  }

  /// Delete interest for a file descriptor
  fn delete_interest(&self, fd: RawFd, filter: i16) -> io::Result<()> {
    let kev = make_kevent(
      fd as libc::uintptr_t,
      filter,
      libc::EV_DELETE,
      0,
      0,
      ptr::null_mut(),
    );

    let result = syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      &kev as *const libc::kevent,
      1,
      std::ptr::null_mut(),
      0,
      std::ptr::null(),
    ));

    match result {
      Err(err) if !utils::is_not_found_error(&err) => Err(err),
      _ => Ok(()),
    }
  }
}

impl OsPoller {
  fn add_inner(
    &self,
    fd: RawFd,
    key: u64,
    interest: Interest,
  ) -> io::Result<()> {
    // For timers, track by key instead of fd
    if interest.is_timer() {
      if !self.registered_timers.borrow_mut().insert(key) {
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
      }

      match Self::change_interests_batched(self, fd, key as usize, interest) {
        Ok(()) => Ok(()),
        Err(e) => {
          self.registered_timers.borrow_mut().remove(&key);
          Err(e)
        }
      }
    } else {
      // Check if fd is already registered (match epoll's EEXIST behavior)
      {
        let mut registered = self.registered_fds.borrow_mut();
        if registered.contains_key(&fd) {
          return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        let armed =
          Armed { read: interest.is_readable(), write: interest.is_writable() };
        registered.insert(fd, armed);
      }

      // A negative descriptor is no descriptor, and an event cannot name
      // it: it is refused here.
      if fd < 0 {
        self.registered_fds.borrow_mut().remove(&fd);
        return Err(io::Error::from_raw_os_error(libc::EBADF));
      }

      // SAFETY: libc::kevent is a C struct that is safe to zero-initialize.
      let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
      let mut n = 0;
      self.build_fd_kevents(
        &mut changes,
        &mut n,
        fd,
        key as usize,
        interest,
        true,
      );
      let full = {
        let mut pending = self.pending.borrow_mut();
        pending.extend_from_slice(&changes[..n]);
        pending.len() >= Self::MAX_PENDING
      };
      if full {
        self.submit_pending()?;
      }
      Ok(())
    }
  }

  /// The most registrations left for a wait to submit. A wait has to have
  /// room for every one of them to be refused.
  const MAX_PENDING: usize = 128;

  /// Whether `event` is about the descriptor `fd`.
  fn is_for(event: &libc::kevent, fd: RawFd) -> bool {
    matches!(event.filter, libc::EVFILT_READ | libc::EVFILT_WRITE)
      && RawFd::try_from(event.ident) == Ok(fd)
  }

  /// Records that the kernel no longer holds the filter `event` names: it
  /// reported it, which uses a one-shot filter up, or refused it.
  fn disarm(registered: &mut HashMap<RawFd, Armed>, event: &libc::kevent) {
    let Ok(fd) = RawFd::try_from(event.ident) else { return };
    let Some(armed) = registered.get_mut(&fd) else { return };
    match event.filter {
      libc::EVFILT_READ => armed.read = false,
      libc::EVFILT_WRITE => armed.write = false,
      _ => {}
    }
  }

  /// Submits the queued registrations on their own, when too many have
  /// gathered to leave for a wait. A refusal is kept for the next wait.
  fn submit_pending(&self) -> io::Result<()> {
    let mut changes = std::mem::take(&mut *self.pending.borrow_mut());
    // With a receipt asked for, the call reports on each change and takes
    // no event out of the queue.
    for change in &mut changes {
      change.flags |= libc::EV_RECEIPT;
    }
    let mut receipts: Vec<libc::kevent> = Vec::with_capacity(changes.len());
    let count = syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      changes.as_ptr(),
      changes.len() as libc::c_int,
      receipts.spare_capacity_mut().as_mut_ptr().cast(),
      changes.len() as libc::c_int,
      ptr::null(),
    ))?;
    // SAFETY: the kernel wrote `count` receipts, at most the capacity.
    unsafe { receipts.set_len(count as usize) };
    let mut registered = self.registered_fds.borrow_mut();
    let mut refused = self.refused.borrow_mut();
    for mut receipt in receipts {
      if receipt.flags & libc::EV_ERROR != 0 && receipt.data != 0 {
        Self::disarm(&mut registered, &receipt);
        receipt.flags &= !libc::EV_RECEIPT;
        refused.push(receipt);
      }
    }
    changes.clear();
    *self.pending.borrow_mut() = changes;
    Ok(())
  }
}

impl ReadinessPoll for OsPoller {
  type NativeEvent = libc::kevent;

  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    self.add_inner(fd, key, interest)
  }

  fn delete(&self, fd: RawFd) -> io::Result<()> {
    // Check if fd is registered, fail with ENOENT if not
    let Some(armed) = self.registered_fds.borrow_mut().remove(&fd) else {
      return Err(io::Error::from_raw_os_error(libc::ENOENT));
    };

    // A registration still queued never reached the kernel: taking it out
    // of the queue removes it.
    {
      let mut pending = self.pending.borrow_mut();
      let queued = pending.len();
      pending.retain(|change| !Self::is_for(change, fd));
      if pending.len() != queued {
        return Ok(());
      }
    }
    self.refused.borrow_mut().retain(|event| !Self::is_for(event, fd));

    // Only the filters the kernel still holds are removed: one it has
    // reported is gone already. `delete_interest` lets a filter that was not
    // found pass, for a descriptor that was closed while registered.
    if armed.read {
      self.delete_interest(fd, libc::EVFILT_READ)?;
    }
    if armed.write {
      self.delete_interest(fd, libc::EVFILT_WRITE)?;
    }
    Ok(())
  }

  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize> {
    // Convert timeout and create pointer from storage reference
    let timeout_storage = utils::timeout_to_timespec(timeout);
    let timeout_ptr = timeout_storage
      .as_ref()
      .map_or(std::ptr::null(), |ts| ts as *const libc::timespec);

    // What the kernel refused earlier is reported first, and then there is
    // something to report without waiting.
    let carried = {
      let mut refused = self.refused.borrow_mut();
      let carried = refused.len().min(events.len());
      for (slot, event) in events.iter_mut().zip(refused.drain(..carried)) {
        *slot = event;
      }
      carried
    };
    let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let timeout_ptr = if carried > 0 { &raw const zero } else { timeout_ptr };
    let room = &mut events[carried..];

    // The queued registrations go in with the wait. One the kernel refuses
    // comes back among the events, flagged `EV_ERROR`, which needs a slot
    // each: `add` keeps the queue shorter than a wait's buffer, and a wait
    // given less room than that submits them on their own first.
    if self.pending.borrow().len() > room.len() {
      self.submit_pending()?;
    }
    let mut changes = std::mem::take(&mut *self.pending.borrow_mut());
    let ret = syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      changes.as_ptr(),
      changes.len() as libc::c_int,
      room.as_mut_ptr(),
      room.len() as i32,
      timeout_ptr,
    ));
    // Applied or not, the changes are spent: an interrupted call has
    // applied them all, and any other failure is the poller's own.
    changes.clear();
    *self.pending.borrow_mut() = changes;
    let ret = ret?;

    let n = ret as usize;
    assert!(
      n <= room.len(),
      "kevent returned more events ({}) than buffer size ({})",
      n,
      room.len()
    );

    // A reported filter was one-shot, and a refused one never took: either
    // way the kernel does not hold it.
    let mut registered = self.registered_fds.borrow_mut();
    if !registered.is_empty() {
      for event in &room[..n] {
        Self::disarm(&mut registered, event);
      }
    }

    Ok(carried + n)
  }

  fn event_key(event: &Self::NativeEvent) -> u64 {
    event.udata as u64
  }

  fn event_error(event: &Self::NativeEvent) -> Option<i32> {
    (event.flags & libc::EV_ERROR != 0 && event.data != 0)
      .then_some(event.data as i32)
  }

  fn event_interest(event: &Self::NativeEvent) -> Interest {
    // kqueue returns one event per filter, so only one will be set
    match event.filter {
      libc::EVFILT_READ => Interest::READ,
      libc::EVFILT_WRITE => Interest::WRITE,
      libc::EVFILT_TIMER => Interest::TIMER,
      libc::EVFILT_VNODE => Interest::VNODE,
      libc::EVFILT_SIGNAL => Interest::SIGNAL,
      _ => Interest::READ, // Fallback
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  crate::test_readiness_poll_contract!(OsPoller::new().unwrap());
}

mod utils {
  use std::io;
  use std::time::Duration;

  /// Convert Duration to libc::timespec
  ///
  /// Shared between kqueue and epoll implementations
  pub fn duration_to_timespec(duration: Duration) -> libc::timespec {
    libc::timespec {
      tv_sec: duration.as_secs() as libc::time_t,
      tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
  }

  /// Convert Option<Duration> to timespec storage
  ///
  /// Returns the timespec value that must be kept alive for the duration of the syscall.
  /// The caller should create a pointer from a reference to this storage.
  pub fn timeout_to_timespec(
    timeout: Option<Duration>,
  ) -> Option<libc::timespec> {
    timeout.map(duration_to_timespec)
  }

  /// Check if an error is "not found" (ENOENT)
  ///
  /// Used when deleting non-existent fd interests - should not error
  pub fn is_not_found_error(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ENOENT)
  }
}

#[cfg(any(
  target_os = "freebsd",
  target_os = "dragonfly",
  target_vendor = "apple",
))]
mod notify {
  use super::*;
  use crate::backend::impls::pollingv2::os::NOTIFY_KEY;
  use std::io;

  /// A notification pipe.
  ///
  /// This implementation uses `EVFILT_USER` to avoid allocating a pipe.
  #[derive(Debug)]
  pub(super) struct Notify;

  impl Notify {
    /// Creates a new notification pipe.
    pub(super) fn new() -> io::Result<Self> {
      Ok(Self)
    }

    /// Registers this notification pipe in the `Poller`.
    pub(super) fn register(&self, poller: &OsPoller) -> io::Result<()> {
      // Register an EVFILT_USER event.
      // IMPORTANT: ident must match what notify() uses to trigger the event
      let kev = super::make_kevent(
        NOTIFY_KEY as libc::uintptr_t,
        libc::EVFILT_USER,
        libc::EV_ADD | libc::EV_RECEIPT | libc::EV_CLEAR,
        0,
        0,
        NOTIFY_KEY as *mut _,
      );
      poller.submit_changes(std::slice::from_ref(&kev))
    }

    /// Notifies the `Poller`.
    #[cfg(test)]
    #[expect(
      dead_code,
      reason = "Only used by backend-specific tests on some targets"
    )]
    pub(super) fn notify(&self, poller: &OsPoller) -> io::Result<()> {
      // Trigger the EVFILT_USER event.
      let mut kev = super::make_kevent(
        0,
        libc::EVFILT_USER,
        libc::EV_ADD | libc::EV_RECEIPT,
        0,
        0,
        NOTIFY_KEY as *mut _,
      );
      kev.fflags = libc::NOTE_TRIGGER;
      poller.submit_changes(std::slice::from_ref(&kev))?;

      Ok(())
    }
  }
}

#[cfg(not(any(
  target_os = "freebsd",
  target_os = "dragonfly",
  target_vendor = "apple",
)))]
mod notify {
  use super::Poller;
  use crate::{Event, NOTIFY_KEY, PollMode};
  use std::io::{self, prelude::*};
  #[cfg(feature = "tracing")]
  use std::os::unix::io::BorrowedFd;
  use std::os::unix::{
    io::{AsFd, AsRawFd},
    net::UnixStream,
  };

  /// A notification pipe.
  ///
  /// This implementation uses a pipe to send notifications.
  #[derive(Debug)]
  pub(super) struct Notify {
    /// The read end of the pipe.
    read_stream: UnixStream,

    /// The write end of the pipe.
    write_stream: UnixStream,
  }

  impl Notify {
    /// Creates a new notification pipe.
    pub(super) fn new() -> io::Result<Self> {
      let (read_stream, write_stream) = UnixStream::pair()?;
      read_stream.set_nonblocking(true)?;
      write_stream.set_nonblocking(true)?;

      Ok(Self { read_stream, write_stream })
    }

    /// Registers this notification pipe in the `Poller`.
    pub(super) fn register(&self, poller: &Poller) -> io::Result<()> {
      // Register the read end of this pipe.
      unsafe {
        poller.add(
          self.read_stream.as_raw_fd(),
          Event::readable(NOTIFY_KEY),
          PollMode::Oneshot,
        )
      }
    }

    /// Reregister this notification pipe in the `Poller`.
    pub(super) fn reregister(&self, poller: &Poller) -> io::Result<()> {
      // Clear out the notification.
      while (&self.read_stream).read(&mut [0; 64]).is_ok() {}

      // Reregister the read end of this pipe.
      poller.modify(
        self.read_stream.as_fd(),
        Event::readable(NOTIFY_KEY),
        PollMode::Oneshot,
      )
    }

    /// Notifies the `Poller`.
    #[allow(clippy::unused_io_amount)]
    pub(super) fn notify(&self, _poller: &Poller) -> io::Result<()> {
      // Write to the write end of the pipe
      (&self.write_stream).write(&[1])?;

      Ok(())
    }

    /// Deregisters this notification pipe from the `Poller`.
    pub(super) fn deregister(&self, poller: &Poller) -> io::Result<()> {
      // Deregister the read end of the pipe.
      poller.delete(self.read_stream.as_fd())
    }

    /// Whether this raw file descriptor is associated with this pipe.
    #[cfg(feature = "tracing")]
    pub(super) fn has_fd(&self, fd: BorrowedFd<'_>) -> bool {
      self.read_stream.as_raw_fd() == fd.as_raw_fd()
    }
  }
}
