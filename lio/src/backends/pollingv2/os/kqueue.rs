use scc::HashSet;

use crate::backends::pollingv2::notifier::{NOTIFY_KEY, Notifier};
use crate::backends::pollingv2::{Interest, ReadinessPoll, util};

// use dashmap::DashSet;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;
use std::{io, ptr};

/// Special identifier for EVFILT_USER notification events
const NOTIFY_IDENT: usize = NOTIFY_KEY as usize;

/// Wrapper around a kqueue file descriptor
pub struct OsPoller {
  kq_fd: OwnedFd,
  /// Track registered fds to match epoll's strict add/modify semantics
  /// Uses DashSet for lock-free concurrent access
  registered_fds: HashSet<RawFd>,
  /// Track registered timer keys separately (timers don't have fds)
  registered_timers: HashSet<u64>,
  /// Notifier for waking up blocked kevent
  notifier: Notifier,
}

impl OsPoller {
  /// Create a new kqueue instance
  pub fn new() -> io::Result<Self> {
    // Create notifier
    let notifier = Notifier::new()?;

    // Create kqueue
    let kqueue = Self {
      kq_fd: unsafe { OwnedFd::from_raw_fd(syscall!(kqueue())?) },
      registered_fds: HashSet::new(),
      registered_timers: HashSet::new(),
      notifier,
    };

    // Register EVFILT_USER for notifications
    // Using EV_CLEAR for edge-triggered behavior to avoid event storms
    let kev = libc::kevent {
      ident: NOTIFY_IDENT as libc::uintptr_t,
      filter: libc::EVFILT_USER,
      flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
      fflags: 0,
      data: 0,
      udata: NOTIFY_IDENT as *mut libc::c_void,
    };

    syscall!(kevent(
      kqueue.kq_fd.as_raw_fd(),
      &kev as *const libc::kevent,
      1,
      ptr::null_mut(),
      0,
      ptr::null(),
    ))?;

    Ok(kqueue)
  }

  /// Add or modify multiple interests in a single syscall (batched)
  ///
  /// This is more efficient than calling add_interest/modify_interest twice
  /// when both readable and writable interests are needed.
  fn change_interests_batched(
    &self,
    fd: RawFd,
    key: usize,
    interest: Interest,
  ) -> io::Result<()> {
    // For kqueue timers, fd contains duration in milliseconds
    if interest.is_timer() {
      let duration_ms = fd;

      let kev = libc::kevent {
        ident: key as libc::uintptr_t,
        filter: libc::EVFILT_TIMER,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: 0,
        data: duration_ms as isize,
        udata: key as *mut libc::c_void,
      };

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

    let mut changes: [libc::kevent; 2] = unsafe { std::mem::zeroed() };
    let mut n = 0;

    if interest.is_readable() {
      changes[n] = libc::kevent {
        ident: fd as libc::uintptr_t,
        filter: libc::EVFILT_READ,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: 0,
        data: 0,
        udata: key as *mut libc::c_void,
      };
      n += 1;
    }

    if interest.is_writable() {
      assert!(n < 2, "Must have space for WRITE event");
      changes[n] = libc::kevent {
        ident: fd as libc::uintptr_t,
        filter: libc::EVFILT_WRITE,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: 0,
        data: 0,
        udata: key as *mut libc::c_void,
      };
      n += 1;
    }

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
    let kev = libc::kevent {
      ident: fd as libc::uintptr_t,
      filter,
      flags: libc::EV_DELETE,
      fflags: 0,
      data: 0,
      udata: ptr::null_mut(),
    };

    let result = syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      &kev as *const libc::kevent,
      1,
      std::ptr::null_mut(),
      0,
      std::ptr::null(),
    ));

    match result {
      Err(err) if !util::is_not_found_error(&err) => Err(err),
      _ => Ok(()),
    }
  }
}

impl ReadinessPoll for OsPoller {
  type NativeEvent = libc::kevent;

  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    // For timers, track by key instead of fd
    if interest.is_timer() {
      if self.registered_timers.insert_sync(key).is_err() {
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
      }

      match self.change_interests_batched(fd, key as usize, interest) {
        Ok(()) => Ok(()),
        Err(e) => {
          self.registered_timers.remove_sync(&key);
          Err(e)
        }
      }
    } else {
      // Check if fd is already registered (match epoll's EEXIST behavior)
      // DashSet::insert returns true if the value was newly inserted
      if self.registered_fds.insert_sync(fd).is_err() {
        // Value already existed
        return Err(io::Error::from_raw_os_error(libc::EEXIST));
      }

      // Optimization: batch both read and write into a single syscall
      match self.change_interests_batched(fd, key as usize, interest) {
        Ok(()) => {
          // Postcondition: fd must still be in registered_fds
          assert!(
            self.registered_fds.contains_sync(&fd),
            "fd should be in registered_fds after successful add"
          );
          Ok(())
        }
        Err(e) => {
          // Rollback: remove from tracking if syscall failed
          let removed = self.registered_fds.remove_sync(&fd);
          assert!(
            removed.is_some(),
            "fd should have been in registered_fds for rollback"
          );
          Err(e)
        }
      }
    }
  }

  fn modify(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    if interest.is_timer() {
      // For timers, check if key is registered
      if !self.registered_timers.contains_sync(&key) {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      }
      self.change_interests_batched(fd, key as usize, interest)
    } else {
      // Check if fd is registered (match epoll's ENOENT behavior)
      if !self.registered_fds.contains_sync(&fd) {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
      }

      let result = self.change_interests_batched(fd, key as usize, interest);

      // Postcondition: fd should still be registered regardless of success/failure
      assert!(
        self.registered_fds.contains_sync(&fd),
        "fd should still be in registered_fds after modify"
      );

      result
    }
  }

  fn delete(&self, fd: RawFd) -> io::Result<()> {
    // Check if fd is registered, fail with ENOENT if not
    if !self.registered_fds.remove_sync(&fd).is_some() {
      return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }

    // Postcondition: fd should no longer be in registered_fds
    assert!(
      !self.registered_fds.contains_sync(&fd),
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
        if !util::is_not_found_error(&read_err) {
          Err(read_err)
        } else if !util::is_not_found_error(&write_err) {
          Err(write_err)
        } else {
          // Both were ENOENT - this shouldn't happen since fd was in registered_fds
          // but if it does, treat it as success (filters were already deleted)
          Ok(())
        }
      }
    }
  }

  fn delete_timer(&self, key: u64) -> io::Result<()> {
    // Check if timer key is registered, fail with ENOENT if not
    if self.registered_timers.remove_sync(&key).is_none() {
      return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }

    // Delete EVFILT_TIMER using the key as ident
    let kev = libc::kevent {
      ident: key as libc::uintptr_t,
      filter: libc::EVFILT_TIMER,
      flags: libc::EV_DELETE,
      fflags: 0,
      data: 0,
      udata: ptr::null_mut(),
    };

    let result = syscall!(kevent(
      self.kq_fd.as_raw_fd(),
      &kev as *const libc::kevent,
      1,
      std::ptr::null_mut(),
      0,
      std::ptr::null(),
    ));

    match result {
      Err(err) if !util::is_not_found_error(&err) => Err(err),
      _ => Ok(()),
    }
  }

  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize> {
    // Convert timeout and create pointer from storage reference
    let timeout_storage = util::timeout_to_timespec(timeout);
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

  fn notify(&self) -> io::Result<()> {
    // Use kqueue-native EVFILT_USER notification
    let kev = libc::kevent {
      ident: NOTIFY_IDENT as libc::uintptr_t,
      filter: libc::EVFILT_USER,
      flags: 0,
      fflags: libc::NOTE_TRIGGER, // Trigger the user event
      data: 0,
      udata: NOTIFY_IDENT as *mut libc::c_void,
    };

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
      _ => Interest::READ, // Fallback
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  crate::generate_tests!(OsPoller::new().unwrap());
}
