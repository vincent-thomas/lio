use super::super::notifier::{NOTIFY_KEY, Notifier};
use super::super::{Interest, ReadinessPoll};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;
use std::{io, ptr};

/// Wrapper around an epoll file descriptor
pub struct OsPoller {
  epoll_fd: OwnedFd,
  /// Notifier for waking up blocked epoll_wait
  notifier: Notifier,
}

impl OsPoller {
  /// Create a new epoll instance
  pub fn new() -> io::Result<Self> {
    // Create epoll instance with CLOEXEC
    let epoll_fd = unsafe {
      let fd = syscall!(epoll_create1(libc::EPOLL_CLOEXEC))?;
      OwnedFd::from_raw_fd(fd)
    };

    // Create notifier (uses pipe for portability)
    let notifier = Notifier::new()?;

    let epoll = Self { epoll_fd, notifier };

    // Register notifier's read fd for read events with special key
    if let Some(notify_fd) = epoll.notifier.read_fd() {
      let mut event =
        libc::epoll_event { events: libc::EPOLLIN as u32, u64: NOTIFY_KEY };

      syscall!(epoll_ctl(
        epoll.epoll_fd.as_raw_fd(),
        libc::EPOLL_CTL_ADD,
        notify_fd,
        &mut event as *mut libc::epoll_event,
      ))?;
    }

    Ok(epoll)
  }
}

impl ReadinessPoll for OsPoller {
  type NativeEvent = libc::epoll_event;

  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    let mut events = 0u32;

    if interest.is_readable() {
      events |= libc::EPOLLIN as u32;
    }
    if interest.is_writable() {
      events |= libc::EPOLLOUT as u32;
    }

    // Use EPOLLONESHOT for consistency with kqueue's EV_ONESHOT behavior
    events |= libc::EPOLLONESHOT as u32;

    assert!(
      events & libc::EPOLLONESHOT as u32 != 0,
      "EPOLLONESHOT must be set"
    );

    let mut event = libc::epoll_event { events, u64: key as u64 };

    syscall!(epoll_ctl(
      self.epoll_fd.as_raw_fd(),
      libc::EPOLL_CTL_ADD,
      fd,
      &mut event as *mut libc::epoll_event,
    ))?;

    // Postcondition: if successful, fd is now registered
    // (no way to verify without tracking state, rely on kernel)
    Ok(())
  }

  fn modify(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()> {
    let mut events = 0u32;

    if interest.is_readable() {
      events |= libc::EPOLLIN as u32;
    }
    if interest.is_writable() {
      events |= libc::EPOLLOUT as u32;
    }

    // Use EPOLLONESHOT for consistency with kqueue's EV_ONESHOT behavior
    events |= libc::EPOLLONESHOT as u32;

    assert!(
      events & libc::EPOLLONESHOT as u32 != 0,
      "EPOLLONESHOT must be set"
    );

    let mut event = libc::epoll_event { events, u64: key as u64 };

    syscall!(epoll_ctl(
      self.epoll_fd.as_raw_fd(),
      libc::EPOLL_CTL_MOD,
      fd,
      &mut event as *mut libc::epoll_event,
    ))?;

    Ok(())
  }

  fn delete(&self, fd: RawFd) -> io::Result<()> {
    // For EPOLL_CTL_DEL, event pointer can be NULL in Linux 2.6.9+
    match syscall!(epoll_ctl(
      self.epoll_fd.as_raw_fd(),
      libc::EPOLL_CTL_DEL,
      fd,
      ptr::null_mut(),
    )) {
      Ok(_) => Ok(()),
      Err(err) => Err(match err.raw_os_error() {
        Some(libc::EBADF) => io::Error::from_raw_os_error(libc::ENOENT),
        _ => err,
      }),
    }
  }

  fn delete_timer(&self, _key: u64) -> io::Result<()> {
    // epoll doesn't use this - timers on Linux use timerfd which is a regular fd
    // and gets deleted via delete() instead
    Err(io::Error::from_raw_os_error(libc::ENOENT))
  }

  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize> {
    // Convert timeout to milliseconds (-1 for infinite wait)
    let timeout_ms = match timeout {
      Some(d) => {
        let ms = d.as_millis();
        if ms > i32::MAX as u128 { i32::MAX } else { ms as i32 }
      }
      None => -1,
    };

    assert!(timeout_ms >= -1, "timeout_ms must be >= -1, got {}", timeout_ms);

    let ret = syscall!(epoll_wait(
      self.epoll_fd.as_raw_fd(),
      events.as_mut_ptr(),
      events.len() as i32,
      timeout_ms,
    ))?;

    let n = ret as usize;
    assert!(
      n <= events.len(),
      "epoll_wait returned more events ({}) than buffer size ({})",
      n,
      events.len()
    );

    Ok(n)
  }

  fn notify(&self) -> io::Result<()> {
    self.notifier.notify()
  }

  fn event_key(event: &Self::NativeEvent) -> u64 {
    event.u64
  }

  fn event_interest(event: &Self::NativeEvent) -> Interest {
    // epoll can return both read and write in a single event
    let readable = (event.events & libc::EPOLLIN as u32) != 0;
    let writable = (event.events & libc::EPOLLOUT as u32) != 0;

    match (readable, writable) {
      (true, true) => Interest::READ_AND_WRITE,
      (true, false) => Interest::READ,
      (false, true) => Interest::WRITE,
      (false, false) => Interest::READ, // Fallback, shouldn't happen
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  crate::generate_tests!(OsPoller::new().unwrap());
}
