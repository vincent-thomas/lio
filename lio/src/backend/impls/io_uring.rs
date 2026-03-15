//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use lio_uring::{
  Entry, LioUring,
  operation::{
    self, Accept, Bind, Close, Connect, Fsync, Ftruncate, LinkAt, Listen,
    MkDirAt, OpenAt, Readv, Recv, RecvMsg, RenameAt, Send, SendMsg, Shutdown,
    Socket, Splice, SymlinkAt, Tee, Timeout, UnlinkAt, Writev,
  },
};

// Note: All read/write operations use Readv/Writev with a single iovec for
// single-buffer ops, or multiple iovecs for vectored ops. The .offset() method
// is used for positional I/O (preadv/pwritev).

use crate::backend::{
  IoBackend, OpCompleted,
  op::{Op, RawBuf},
};
use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

fn create_io_uring_entry(op: &Op) -> Entry {
  match op {
    Op::Nop => operation::Nop::new().build(),
    Op::Send { fd, flags, buf } => {
      Send::new(fd.as_raw_fd(), buf.ptr, buf.len as u32).flags(*flags).build()
    }
    Op::Recv { fd, flags, buf } => {
      Recv::new(fd.as_raw_fd(), buf.ptr, buf.len as u32).flags(*flags).build()
    }
    Op::SendTo { fd, flags, msghdr, .. } => {
      // msghdr is pre-constructed in TypedOp with iovec and address already set
      SendMsg::new(fd.as_raw_fd(), *msghdr).flags(*flags as u32).build()
    }
    Op::RecvFrom { fd, flags, msghdr, .. } => {
      // msghdr is pre-constructed in TypedOp with iovec and address already set
      RecvMsg::new(fd.as_raw_fd(), *msghdr).flags(*flags as u32).build()
    }
    Op::Accept { fd, addr, len } => {
      // Cast sockaddr_storage* to sockaddr*
      Accept::new(fd.as_raw_fd(), (*addr) as *mut libc::sockaddr, *len).build()
    }
    Op::Connect { fd, addr, len, .. } => {
      Connect::new(fd.as_raw_fd(), (*addr) as *const libc::sockaddr, *len)
        .build()
    }
    Op::Bind { fd, addr, addrlen } => {
      Bind::new(fd.as_raw_fd(), (*addr) as *const libc::sockaddr, *addrlen)
        .build()
    }
    Op::Listen { fd, backlog } => Listen::new(fd.as_raw_fd(), *backlog).build(),
    Op::Shutdown { fd, how } => Shutdown::new(fd.as_raw_fd(), *how).build(),
    Op::Socket { domain, ty, proto } => {
      Socket::new(*domain, *ty, *proto).build()
    }
    Op::OpenAt { dir_fd, path, flags, mode } => {
      OpenAt::new(dir_fd.as_raw_fd(), *path).flags(*flags).mode(*mode).build()
    }
    Op::Close { fd } => Close::new(*fd).build(),
    Op::Fsync { fd } => Fsync::new(fd.as_raw_fd()).build(),
    // TODO: IORING_OP_FTRUNCATE (kernel 6.9+) returns success but doesn't
    // actually truncate in some tests. Needs investigation - might be SQE
    // setup issue or kernel-specific behavior.
    Op::Truncate { fd, size } => Ftruncate::new(fd.as_raw_fd(), *size).build(),
    Op::LinkAt { old_dir_fd, old_path, new_dir_fd, new_path } => LinkAt::new(
      old_dir_fd.as_raw_fd(),
      *old_path,
      new_dir_fd.as_raw_fd(),
      *new_path,
    )
    .build(),
    Op::SymlinkAt { target, linkpath, dir_fd } => {
      SymlinkAt::new(dir_fd.as_raw_fd(), *target, *linkpath).build()
    }
    Op::UnlinkAt { dir_fd, path, flags } => {
      UnlinkAt::new(dir_fd.as_raw_fd(), *path).flags(*flags).build()
    }
    Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
      RenameAt::new(old_dir_fd.as_raw_fd(), *old_path, new_dir_fd.as_raw_fd(), *new_path).build()
    }
    Op::MkdirAt { dir_fd, path, mode } => {
      MkDirAt::new(dir_fd.as_raw_fd(), *path).mode(*mode as libc::mode_t).build()
    }
    #[cfg(target_os = "linux")]
    Op::Tee { fd_in, fd_out, size } => {
      Tee::new(fd_in.as_raw_fd(), fd_out.as_raw_fd(), *size).build()
    }
    Op::Timeout { timespec, .. } => {
      // __kernel_timespec has same layout as libc::timespec
      // timespec is already a pointer to data in the boxed TypedOp
      Timeout::new(*timespec as *const _).build()
    }
    Op::ReadV { fd, iovecs, iov_count, .. } => {
      Readv::new(fd.as_raw_fd(), *iovecs, *iov_count as u32).build()
    }
    Op::WriteV { fd, iovecs, iov_count, .. } => {
      Writev::new(fd.as_raw_fd(), *iovecs, *iov_count as u32).build()
    }
    Op::ReadVAt { fd, iovecs, iov_count, offset, .. } => {
      Readv::new(fd.as_raw_fd(), *iovecs, *iov_count as u32).offset(*offset as u64).build()
    }
    Op::WriteVAt { fd, iovecs, iov_count, offset, .. } => {
      Writev::new(fd.as_raw_fd(), *iovecs, *iov_count as u32).offset(*offset as u64).build()
    }
  }
}

/// io_uring backend for Linux.
///
/// This is the highest-performance backend, using Linux's io_uring interface
/// for truly asynchronous I/O with minimal syscall overhead.
///
/// # Example
///
/// ```rust,ignore
/// let mut backend = IoUring::default();
/// backend.init(1024)?;
///
/// backend.push(id, &op)?;
/// backend.flush()?;
///
/// let completions = backend.wait(&mut store)?;
/// ```
#[derive(Default)]
pub struct IoUring {
  ring: Option<LioUring>,
  /// Reusable buffer for completed operations (avoids allocation per poll/wait).
  completed: Vec<OpCompleted>,
}

impl IoUring {
  /// Create a new uninitialized io_uring backend.
  ///
  /// Call [`init`](Self::init) before using.
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn ring(&mut self) -> &mut LioUring {
    self.ring.as_mut().expect("IoUring not initialized - call init() first")
  }

  /// Poll for completions with optional timeout.
  ///
  /// - `timeout = None`: Block indefinitely
  /// - `timeout = Some(Duration::ZERO)`: Non-blocking poll
  /// - `timeout = Some(duration)`: Wait up to duration
  fn poll_inner(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();

    let ring = self.ring.as_mut().expect("IoUring not initialized");

    match timeout {
      None => {
        // Block indefinitely for first completion
        let first = ring.wait()?;
        self
          .completed
          .push(OpCompleted::new(first.user_data(), first.result() as isize));
      }
      Some(d) if d.is_zero() => {
        // Non-blocking: check if anything is ready
        match ring.try_wait()? {
          Some(op) => {
            self
              .completed
              .push(OpCompleted::new(op.user_data(), op.result() as isize));
          }
          None => return Ok(&[]),
        }
      }
      Some(d) => {
        // Wait with timeout
        match ring.wait_timeout(d)? {
          Some(first) => {
            self.completed.push(OpCompleted::new(
              first.user_data(),
              first.result() as isize,
            ));
          }
          None => return Ok(&[]), // Timeout expired
        }
      }
    }

    // Drain any additional completions (non-blocking)
    let ring = self.ring.as_mut().expect("IoUring not initialized");
    while let Ok(Some(op)) = ring.try_wait() {
      self
        .completed
        .push(OpCompleted::new(op.user_data(), op.result() as isize));
    }

    Ok(&self.completed)
  }
}

impl IoBackend for IoUring {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    let ring = LioUring::new(cap as u32)?;
    self.ring = Some(ring);
    // Pre-allocate completions buffer (reasonable batch size)
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op) -> io::Result<()> {
    let entry = create_io_uring_entry(&op);

    // Push to submission queue without syscall
    // SAFETY: entry is a valid SQE created from op, id is used as user_data
    unsafe { self.ring().push(entry, id) }.map_err(|_| {
      io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
    })?;

    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    // Submit all queued operations with a single syscall
    let submitted = self.ring().submit()?;
    Ok(submitted)
  }

  fn wait_timeout(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.poll_inner(timeout)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Run the standard IoBackend test suite
  crate::test_io_backend!(IoUring::new());
}
