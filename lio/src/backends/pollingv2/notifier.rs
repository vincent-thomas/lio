//! Cross-platform notification mechanism for waking up blocked poll/epoll/kqueue
//!
//! This module provides a portable way to wake up a thread blocked in a poll operation.
//! - On kqueue: Uses EVFILT_USER (native kqueue user events)
//! - On epoll: Uses a pipe (POSIX-compliant, works on Linux and Redox OS)

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Special key used to identify notification events
pub const NOTIFY_KEY: u64 = u64::MAX;

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
pub struct Notifier {
  /// kqueue uses EVFILT_USER, so no fd needed
  _marker: std::marker::PhantomData<()>,
}

#[cfg(not(any(
  target_os = "macos",
  target_os = "ios",
  target_os = "tvos",
  target_os = "watchos",
  target_os = "freebsd",
  target_os = "dragonfly",
  target_os = "openbsd",
  target_os = "netbsd"
)))]
pub struct Notifier {
  /// Read end of the pipe
  read_fd: OwnedFd,
  /// Write end of the pipe
  write_fd: OwnedFd,
}

impl Notifier {
  /// Create a new notifier
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
  pub fn new() -> io::Result<Self> {
    // kqueue doesn't need any fd for notifications
    Ok(Self { _marker: std::marker::PhantomData })
  }

  /// Create a new notifier using a pipe (for epoll-based systems)
  #[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  )))]
  pub fn new() -> io::Result<Self> {
    let mut fds = [0i32; 2];
    syscall!(pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK))?;

    Ok(Self {
      read_fd: unsafe { OwnedFd::from_raw_fd(fds[0]) },
      write_fd: unsafe { OwnedFd::from_raw_fd(fds[1]) },
    })
  }

  /// Get the file descriptor to register with the poller
  /// Returns None for kqueue (uses EVFILT_USER), Some(fd) for epoll (pipe read end)
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
  pub fn read_fd(&self) -> Option<RawFd> {
    None
  }

  /// Get the file descriptor to register with the poller
  #[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  )))]
  pub fn read_fd(&self) -> Option<RawFd> {
    Some(self.read_fd.as_raw_fd())
  }

  /// Trigger a notification
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
  pub fn notify(&self) -> io::Result<()> {
    // For kqueue, the notification is triggered externally via EVFILT_USER
    // This method exists for API compatibility but does nothing
    Ok(())
  }

  /// Trigger a notification by writing to the pipe
  #[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  )))]
  pub fn notify(&self) -> io::Result<()> {
    let byte: u8 = 1;
    let result = syscall!(write(
      self.write_fd.as_raw_fd(),
      &byte as *const u8 as *const libc::c_void,
      1,
    ));

    match result {
      Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
      other => other.map(|_| ()),
    }
  }
}
