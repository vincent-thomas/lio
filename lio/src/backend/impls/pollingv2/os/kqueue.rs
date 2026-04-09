use std::cell::RefCell;
use std::marker::PhantomData;
use std::time::Duration;
use std::{
  collections::HashSet,
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

/// Wrapper around a kqueue file descriptor
///
/// This type is intentionally `!Send` to ensure it's only used from a single thread.
/// Interior mutability is provided via `RefCell` for tracking registered fds/timers.
pub struct OsPoller {
  kq_fd: OwnedFd,
  /// Track registered fds to match epoll's strict add/modify semantics
  registered_fds: RefCell<HashSet<RawFd>>,
  /// Track registered timer keys separately (timers don't have fds)
  registered_timers: RefCell<HashSet<u64>>,
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
      registered_fds: RefCell::new(HashSet::new()),
      registered_timers: RefCell::new(HashSet::new()),
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
      if !self.registered_fds.borrow_mut().insert(fd) {
        // Value already existed
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
      }

      // Optimization: batch both read and write into a single syscall
      match Self::change_interests_batched(self, fd, key as usize, interest) {
        Ok(()) => {
          // Postcondition: fd must still be in registered_fds
          assert!(
            self.registered_fds.borrow().contains(&fd),
            "fd should be in registered_fds after successful add"
          );
          Ok(())
        }
        Err(e) => {
          // Rollback: remove from tracking if syscall failed
          let removed = self.registered_fds.borrow_mut().remove(&fd);
          assert!(
            removed,
            "fd should have been in registered_fds for rollback"
          );
          Err(e)
        }
      }
    }
  }

  #[cfg(test)]
  fn modify_interest(
    &self,
    fd: RawFd,
    key: u64,
    interest: Interest,
  ) -> io::Result<()> {
    if interest.is_timer() {
      if !self.registered_timers.borrow().contains(&key) {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      }
      self.change_interests_batched(fd, key as usize, interest)
    } else {
      if !self.registered_fds.borrow().contains(&fd) {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      }

      let result = self.change_interests_batched(fd, key as usize, interest);

      assert!(
        self.registered_fds.borrow().contains(&fd),
        "fd should still be in registered_fds after modify"
      );

      result
    }
  }
}

impl ReadinessPoll for OsPoller {
  type NativeEvent = libc::kevent;

  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    self.add_inner(fd, key, interest)
  }

  #[cfg(test)]
  fn modify(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    self.modify_interest(fd, key, interest)
  }

  fn delete(&self, fd: RawFd) -> io::Result<()> {
    // Check if fd is registered, fail with ENOENT if not
    if !self.registered_fds.borrow_mut().remove(&fd) {
      return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }

    // Postcondition: fd should no longer be in registered_fds
    assert!(
      !self.registered_fds.borrow().contains(&fd),
      "fd should not be in registered_fds after remove"
    );

    // Delete both read and write interests
    let read_result = self.delete_interest(fd, libc::EVFILT_READ);
    let write_result = self.delete_interest(fd, libc::EVFILT_WRITE);

    // If either deletion succeeded, we're good
    // If both failed, check if either had a non-ENOENT error
    match (read_result, write_result) {
      (Ok(()), _) | (_, Ok(())) => Ok(()),
      (Err(read_err), Err(write_err)) => {
        // Both failed - check if either is a real error (not ENOENT)
        if !utils::is_not_found_error(&read_err) {
          Err(read_err)
        } else if !utils::is_not_found_error(&write_err) {
          Err(write_err)
        } else {
          // Both were ENOENT - this shouldn't happen since fd was in registered_fds
          // but if it does, treat it as success (filters were already deleted)
          Ok(())
        }
      }
    }
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

    let ret = syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      std::ptr::null(),
      0,
      events.as_mut_ptr(),
      events.len() as i32,
      timeout_ptr,
    ))?;

    let n = ret as usize;
    assert!(
      n <= events.len(),
      "kevent returned more events ({}) than buffer size ({})",
      n,
      events.len()
    );

    Ok(n)
  }

  #[cfg(test)]
  fn notify(&self) -> io::Result<()> {
    // Use kqueue-native EVFILT_USER notification

    use crate::backend::impls::pollingv2::os::NOTIFY_KEY;
    let mut kev = make_kevent(
      NOTIFY_KEY as libc::uintptr_t,
      libc::EVFILT_USER,
      0,
      0,
      0,
      NOTIFY_KEY as *mut libc::c_void,
    );
    kev.fflags = libc::NOTE_TRIGGER; // Trigger the user event

    syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      &kev as *const libc::kevent,
      1,
      ptr::null_mut(),
      0,
      ptr::null(),
    ))?;

    Ok(())
  }

  fn event_key(event: &Self::NativeEvent) -> u64 {
    event.udata as u64
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

  fn event_fflags(event: &Self::NativeEvent) -> u32 {
    event.fflags
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

  #[cfg(test)]
  mod tests {
    use super::*;

    #[test]
    fn test_duration_to_timespec() {
      let dur = Duration::from_millis(1500);
      let ts = duration_to_timespec(dur);
      assert_eq!(ts.tv_sec, 1);
      assert_eq!(ts.tv_nsec, 500_000_000);
    }

    #[test]
    fn test_timeout_to_timespec_some() {
      let dur = Duration::from_millis(2500);
      let timeout = Some(dur);
      let storage = timeout_to_timespec(timeout);

      // Verify storage contains the correct value
      assert!(storage.is_some());
      let ts = storage.unwrap();
      assert_eq!(ts.tv_sec, 2);
      assert_eq!(ts.tv_nsec, 500_000_000);

      // Verify that creating a pointer from storage is valid
      // This mimics what the caller should do
      let ptr = &ts as *const libc::timespec;
      assert!(!ptr.is_null());

      // Verify the pointer points to valid data
      // SAFETY: `ptr` was derived from `&ts`, so it is non-null, aligned, and
      // points to the same initialized `timespec` for the duration of this check.
      unsafe {
        assert_eq!((*ptr).tv_sec, 2);
        assert_eq!((*ptr).tv_nsec, 500_000_000);
      }
    }

    #[test]
    fn test_timeout_to_timespec_none() {
      let storage = timeout_to_timespec(None);
      assert!(storage.is_none());
    }

    #[test]
    fn test_is_not_found_error() {
      let err = io::Error::from_raw_os_error(libc::ENOENT);
      assert!(is_not_found_error(&err));

      let err = io::Error::from_raw_os_error(libc::EINVAL);
      assert!(!is_not_found_error(&err));
    }
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
