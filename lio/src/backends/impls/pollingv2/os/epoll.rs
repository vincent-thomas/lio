use super::super::{Interest, ReadinessPoll};
use super::NOTIFY_KEY;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Duration;
use std::{io, ptr};

/// Wrapper around an epoll file descriptor
pub struct OsPoller {
  /// File descriptor for the epoll instance.
  epoll_fd: OwnedFd,
  // /// Notifier used to wake up epoll.
  notifier: Notifier,

  /// File descriptor for the timerfd that produces timeouts.
  ///
  /// Redox does not support timerfd.
  #[cfg(not(target_os = "redox"))]
  timer_fd: Option<OwnedFd>,
}

impl OsPoller {
  /// Create a new epoll instance
  pub fn new() -> io::Result<Self> {
    // Create epoll instance with CLOEXEC
    let epoll_fd = {
      let fd = syscall!(epoll_create1(libc::EPOLL_CLOEXEC))?;
      unsafe { OwnedFd::from_raw_fd(fd) }
    };

    // // Create notifier (uses pipe for portability)
    let notifier = Notifier::new()?;

    #[cfg(not(target_os = "redox"))]
    let timer_fd = {
      let fd: RawFd =
        syscall!(timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC))?;
      unsafe { OwnedFd::from_raw_fd(fd) }
    };

    let epoll = Self {
      epoll_fd,
      notifier,

      #[cfg(not(target_os = "redox"))]
      timer_fd: Some(timer_fd),
    };

    #[cfg(not(target_os = "redox"))]
    if let Some(ref timer_fd) = epoll.timer_fd {
      epoll.add(timer_fd.as_raw_fd(), NOTIFY_KEY, Interest::NONE)?;
    }

    epoll.add(
      epoll.notifier.as_fd().as_raw_fd(),
      NOTIFY_KEY,
      Interest::READ,
    )?;

    Ok(epoll)
  }
}
impl Drop for OsPoller {
  fn drop(&mut self) {
    #[cfg(not(target_os = "redox"))]
    if let Some(timer_fd) = self.timer_fd.take() {
      let _ = self.delete(timer_fd.as_fd().as_raw_fd());
    }
    let _ = self.delete(self.notifier.as_fd().as_raw_fd());
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
    events |= libc::EPOLLPRI as u32;
    events |= libc::EPOLLHUP as u32;

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
    events |= libc::EPOLLPRI as u32;
    events |= libc::EPOLLHUP as u32;

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

  /// Returns [`libc::EINVAL`] if events.is_empty()
  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize> {
    /// `timespec` value that equals zero.
    #[cfg(not(target_os = "redox"))]
    const TS_ZERO: libc::timespec = unsafe {
      std::mem::transmute([0u8; std::mem::size_of::<libc::timespec>()])
    };
    const ITS_ZERO: libc::itimerspec = unsafe {
      std::mem::transmute([0u8; std::mem::size_of::<libc::itimerspec>()])
    };

    #[cfg(not(target_os = "redox"))]
    if let Some(ref timer_fd) = self.timer_fd {
      // Configure the timeout using timerfd.
      let mut new_val = libc::itimerspec {
        it_interval: TS_ZERO,
        it_value: match timeout {
          None => TS_ZERO,
          Some(t) => {
            let mut ts = TS_ZERO;
            ts.tv_sec = t.as_secs() as _;
            ts.tv_nsec = t.subsec_nanos() as _;
            ts
          }
        },
        ..unsafe { std::mem::zeroed() }
      };

      let mut result = MaybeUninit::<libc::itimerspec>::uninit();
      syscall!(timerfd_settime(
        timer_fd.as_raw_fd(),
        0,
        &mut new_val as *mut _,
        result.as_mut_ptr()
      ))?;

      // Set interest in timerfd.
      self.modify(timer_fd.as_raw_fd(), NOTIFY_KEY, Interest::READ)?;
    }

    #[cfg(not(target_os = "redox"))]
    let timer_fd = &self.timer_fd;
    #[cfg(target_os = "redox")]
    let timer_fd: Option<core::convert::Infallible> = None;

    // Timeout for epoll. In case of overflow, use no timeout.
    let timeout = match (timer_fd, timeout) {
      (_, Some(t)) if t == Duration::from_secs(0) => Some(TS_ZERO),
      (None, Some(t)) => Some(libc::timespec {
        tv_sec: t.as_secs() as i64,
        tv_nsec: t.subsec_nanos() as i64,
      }),
      _ => None,
    };

    // Wait for I/O events.
    let n = syscall!(epoll_pwait2(
      self.epoll_fd.as_raw_fd(),
      events.as_mut_ptr(),
      events.len() as i32,
      timeout.as_ref().map(|v| v as *const _).unwrap_or(ptr::null()),
      ptr::null_mut()
    ))?;

    // Clear the notification (if received) and re-register interest in it.
    self.notifier.clear();
    self.modify(
      self.notifier.as_fd().as_raw_fd(),
      NOTIFY_KEY,
      Interest::READ,
    )?;

    Ok(n as usize)
  }

  fn notify(&self) -> io::Result<()> {
    self.notifier.notify();
    Ok(())
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

enum Notifier {
  /// The primary notifier, using eventfd.
  #[cfg(not(target_os = "redox"))]
  EventFd(OwnedFd),

  /// The fallback notifier, using a pipe.
  Pipe {
    /// The read end of the pipe.
    read_pipe: OwnedFd,

    /// The write end of the pipe.
    write_pipe: OwnedFd,
  },
}

impl AsFd for Notifier {
  fn as_fd(&self) -> BorrowedFd<'_> {
    match self {
      #[cfg(not(target_os = "redox"))]
      Notifier::EventFd(fd) => fd.as_fd(),
      Notifier::Pipe { read_pipe: read, .. } => read.as_fd(),
    }
  }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
  let mut result = MaybeUninit::<[OwnedFd; 2]>::uninit();
  match syscall!(pipe2(result.as_mut_ptr().cast::<_>(), libc::O_CLOEXEC)) {
    Ok(_) => {
      let [read, write] = unsafe { result.assume_init() };
      Ok((read, write))
    }
    Err(_) => {
      use libc::{F_GETFD, F_GETFL, F_SETFD, F_SETFL, FD_CLOEXEC, O_NONBLOCK};
      syscall!(pipe(result.as_mut_ptr().cast::<_>()))?;
      let [read, write] = unsafe { result.assume_init() };

      let flags = syscall!(fcntl(read.as_raw_fd(), F_GETFD))?;
      syscall!(fcntl(read.as_raw_fd(), F_SETFD, flags | FD_CLOEXEC))?;

      let flags = syscall!(fcntl(write.as_raw_fd(), F_GETFD))?;
      syscall!(fcntl(write.as_raw_fd(), F_SETFD, flags | FD_CLOEXEC))?;

      Ok((read, write))
    }
  }
}

impl Notifier {
  fn new() -> io::Result<Self> {
    // Skip eventfd for testing if necessary.
    #[cfg(not(target_os = "redox"))]
    {
      // Try to create an eventfd.
      match syscall!(eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)) {
        Ok(fd) => {
          let owned = unsafe { OwnedFd::from_raw_fd(fd) };
          return Ok(Notifier::EventFd(owned));
        }

        Err(_err) => {}
      }
    }

    let (read, write) = pipe()?;
    let flags = syscall!(fcntl(read.as_raw_fd(), libc::F_GETFL))?;
    syscall!(fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK))?;

    Ok(Notifier::Pipe { read_pipe: read, write_pipe: write })
  }

  pub fn notify(&self) {
    match self {
      #[cfg(not(target_os = "redox"))]
      Self::EventFd(fd) => {
        let buf: [u8; 8] = 1u64.to_ne_bytes();
        let _ = syscall!(write(
          fd.as_raw_fd(),
          buf.as_ptr().cast::<libc::c_void>(),
          buf.len()
        ));
      }

      Self::Pipe { write_pipe, .. } => {
        let buf = [0; 1];
        syscall!(write(
          write_pipe.as_raw_fd(),
          buf.as_ptr().cast::<libc::c_void>(),
          buf.len()
        ))
        .ok();
      }
    }
  }

  /// Clear the notification.
  fn clear(&self) {
    match self {
      #[cfg(not(target_os = "redox"))]
      Self::EventFd(fd) => {
        const SIZE: usize = 8;
        let mut buf = [0u8; SIZE];
        let _ = syscall!(read(
          fd.as_raw_fd(),
          buf.as_mut_ptr().cast::<libc::c_void>(),
          SIZE
        ));
      }

      Self::Pipe { read_pipe, .. } => {
        const SIZE: usize = 1024;
        while syscall!(read(
          read_pipe.as_raw_fd(),
          [0u8; SIZE].as_mut_ptr().cast::<libc::c_void>(),
          SIZE
        ))
        .is_ok()
        {}
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  crate::generate_tests!(OsPoller::new().unwrap());
}
