use std::io;
use std::os::fd::{FromRawFd, RawFd};

use crate::api::resource::Resource;
use crate::operation::{Operation, OperationExt};

// Not detach safe.
pub struct Socket {
  domain: libc::c_int,
  ty: libc::c_int,
  proto: libc::c_int,
}

assert_op_max_size!(Socket);

impl Socket {
  pub(crate) fn new(
    domain: libc::c_int,
    ty: libc::c_int,
    proto: libc::c_int,
  ) -> Self {
    Self { domain, ty, proto }
  }

  /// Sets the close-on-exec flag on the socket file descriptor.
  ///
  /// **What it does:**
  /// Sets the FD_CLOEXEC flag, which causes this file descriptor to be automatically
  /// closed when the process executes a new program via exec() family of functions.
  ///
  /// **Why it's needed:**
  /// - Security: Prevents child processes from inheriting sensitive file descriptors
  /// - Resource management: Child processes shouldn't keep parent's sockets open
  /// - Prevents port binding conflicts if child process continues running
  ///
  /// **Platform differences:**
  /// - Linux/BSD: Can use SOCK_CLOEXEC flag atomically during socket creation
  /// - macOS: Must use FIOCLEX ioctl after creation (small race window)
  /// - Some embedded systems: Not available (feature-gated out)
  ///
  /// **Consequences if omitted:**
  /// - Child processes inherit the socket FD and can read/write/close it
  /// - Security risk: sensitive connections leak to untrusted child processes
  /// - Resource leak: socket stays open even if parent closes it (child keeps ref)
  /// - Port remains bound even after parent exits if child is still running
  #[cfg(not(any(
    target_env = "newlib",
    target_os = "solaris",
    target_os = "illumos",
    target_os = "emscripten",
    target_os = "fuchsia",
    target_os = "l4re",
    target_os = "linux",
    target_os = "cygwin",
    target_os = "haiku",
    target_os = "redox",
    target_os = "vxworks",
    target_os = "nto",
  )))]
  fn set_cloexec(fd: RawFd) -> isize {
    syscall!(raw ioctl(fd, libc::FIOCLEX))
  }

  /// Sets up all socket options in sequence.
  ///
  /// **Error handling:**
  /// This function propagates errors using `?` operator. If any setup step fails,
  /// the error is returned immediately. The caller MUST close the socket FD to
  /// prevent resource leaks (see run_blocking implementation).
  fn setup_socket_options(fd: RawFd) -> isize {
    // SIGPIPE handling (BSD/macOS only)
    {
      #[cfg(any(
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        target_vendor = "apple"
      ))]
      {
        let opt: i32 = 1;
        syscall!(raw setsockopt(
          fd,
          libc::SOL_SOCKET,
          libc::SO_NOSIGPIPE,
          &opt as *const i32 as *const libc::c_void,
          std::mem::size_of::<i32>() as libc::socklen_t
        )?);
      }
    }

    // Non-blocking mode (required for kqueue/epoll, not needed on Linux with io_uring)
    #[cfg(not(linux))]
    {
      let mut nonblocking = true as libc::c_int;
      syscall!(raw ioctl(fd, libc::FIONBIO, &mut nonblocking)?)
    };

    // SO_REUSEADDR (allows quick rebind)
    {
      let opt: i32 = 1;
      syscall!(raw setsockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_REUSEADDR,
        &opt as *const i32 as *const libc::c_void,
        std::mem::size_of::<i32>() as libc::socklen_t
      )?);
    };

    0
  }
}

impl OperationExt for Socket {
  type Result = std::io::Result<Resource>;
}

impl Operation for Socket {
  impl_result!(|_this, res: isize| -> io::Result<Resource> {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      // SAFETY: 'res' is valid fd.
      let resource = unsafe { Resource::from_raw_fd(res as RawFd) };
      Ok(resource)
    }
  });
  impl_no_readyness!();

  #[cfg(linux)]
  // const OPCODE: u8 = 45;
  #[cfg(linux)]
  fn create_entry(&self) -> lio_uring::submission::Entry {
    lio_uring::operation::Socket::new(self.domain, self.ty, self.proto).build()
  }

  fn run_blocking(&self) -> isize {
    eprintln!(
      "[Socket::run_blocking] Starting socket creation: domain={}, type={}, proto={}",
      self.domain, self.ty, self.proto
    );

    // Path 1: Platforms with SOCK_CLOEXEC support (atomic CLOEXEC flag)
    #[cfg(any(
      target_os = "android",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "illumos",
      target_os = "hurd",
      target_os = "linux",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "cygwin",
      target_os = "nto",
      target_os = "solaris",
    ))]
    let fd = {
      // Create socket with CLOEXEC set atomically (no race window)
      let fd = syscall!(raw socket(
        self.domain,
        self.ty | libc::SOCK_CLOEXEC,
        self.proto
      )?);

      // CRITICAL ERROR CLEANUP: If setup fails, close FD before returning error
      let result = Self::setup_socket_options(fd as i32);

      if result < 0 {
        unsafe { libc::close(fd as i32) };
        return result;
      };
      fd
    };

    // Path 2: Platforms without SOCK_CLOEXEC (macOS, etc.)
    #[cfg(not(any(
      target_os = "android",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "illumos",
      target_os = "hurd",
      target_os = "linux",
      target_os = "netbsd",
      target_os = "openbsd",
      target_os = "cygwin",
      target_os = "nto",
      target_os = "solaris",
    )))]
    let fd = {
      let fd = syscall!(raw socket(self.domain, self.ty, self.proto)?);

      // Set CLOEXEC if supported on this platform
      #[cfg(not(any(
        target_env = "newlib",
        target_os = "emscripten",
        target_os = "fuchsia",
        target_os = "l4re",
        target_os = "haiku",
        target_os = "redox",
        target_os = "vxworks",
      )))]
      {
        let result = Self::set_cloexec(fd as i32);
        if result < 0 {
          let _ = syscall!(raw close(fd as i32));
          return result;
        }
      }

      let result = Self::setup_socket_options(fd as i32);
      if result < 0 {
        let _ = syscall!(raw close(fd as i32));
        return result;
      }
      fd
    };

    fd
  }
}
