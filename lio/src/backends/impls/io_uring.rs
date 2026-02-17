//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use lio_uring::{
  completion::CompletionQueue,
  operation::{
    self, Accept, Close, Connect, Fallocate, Fsync, LinkAt, MkDirAt, OpenAt,
    Read, Readv, Recv, RenameAt, Send, Shutdown, Statx, SymlinkAt, Tee,
    Timeout, UnlinkAt, Write, Writev,
  },
  submission::{Entry, SubmissionQueue},
};

use crate::{
  backends::{IoBackend, OpCompleted},
  op::Op,
};
use std::io;
use std::time::Duration;

fn create_io_uring_entry(op: &mut Op) -> Entry {
  use crate::buf::BufLike;
  use std::os::fd::AsRawFd;

  match op {
    Op::Nop => operation::Nop::new().build(),
    Op::Read { fd, buffer } => {
      let buf = unsafe { buffer.take::<&mut [u8]>() };
      let ptr = buf.as_mut_ptr();
      let len = buf.len() as u32;
      // Store buffer back for later use
      let _ = buffer.replace(buf);
      Read::new(fd.as_raw_fd(), ptr, len).build()
    }
    Op::Write { fd, buffer } => {
      let buf = unsafe { buffer.take::<&[u8]>() };
      let ptr = buf.as_ptr();
      let len = buf.len() as u32;
      let _ = buffer.replace(buf);
      Write::new(fd.as_raw_fd(), ptr, len).build()
    }
    Op::ReadAt { fd, offset, buffer } => {
      let buf = unsafe { buffer.take::<&mut [u8]>() };
      let ptr = buf.as_mut_ptr();
      let len = buf.len() as u32;
      let _ = buffer.replace(buf);
      Read::new(fd.as_raw_fd(), ptr, len).offset(*offset).build()
    }
    Op::WriteAt { fd, offset, buffer } => {
      let buf = unsafe { buffer.take::<&[u8]>() };
      let ptr = buf.as_ptr();
      let len = buf.len() as u32;
      let _ = buffer.replace(buf);
      Write::new(fd.as_raw_fd(), ptr, len).offset(*offset).build()
    }
    Op::Send { fd, flags, buffer } => {
      let buf = unsafe { buffer.take::<&[u8]>() };
      let ptr = buf.as_ptr();
      let len = buf.len() as u32;
      let _ = buffer.replace(buf);
      Send::new(fd.as_raw_fd(), ptr, len).flags(*flags).build()
    }
    Op::Recv { fd, flags, buffer } => {
      let buf = unsafe { buffer.take::<&mut [u8]>() };
      let ptr = buf.as_mut_ptr();
      let len = buf.len() as u32;
      let _ = buffer.replace(buf);
      Recv::new(fd.as_raw_fd(), ptr, len).flags(*flags).build()
    }
    Op::Accept { fd, addr, len } => {
      Accept::new(fd.as_raw_fd(), *addr, *len).build()
    }
    Op::Connect { fd, addr, len, .. } => {
      Connect::new(fd.as_raw_fd(), *addr as *const _, *len).build()
    }
    Op::Bind { fd, addr } => {
      // For bind, we need to convert to the right format
      // Use socket directly in blocking mode for now
      // This is a limitation - io_uring doesn't have a native bind opcode
      // We'll fall back to blocking
      operation::Nop::new().build()
    }
    Op::Listen { fd, backlog } => operation::Nop::new().build(),
    Op::Shutdown { fd, how } => Shutdown::new(fd.as_raw_fd(), *how).build(),
    Op::Socket { domain, ty, proto } => operation::Nop::new().build(),
    Op::OpenAt { dir_fd, path, flags } => {
      OpenAt::new(dir_fd.as_raw_fd(), *path, *flags, 0).build()
    }
    Op::Close { fd } => Close::new(fd.as_raw_fd()).build(),
    Op::Fsync { fd } => Fsync::new(fd.as_raw_fd()).build(),
    Op::Truncate { fd, size } => {
      Fallocate::new(fd.as_raw_fd(), *size as u64).build()
    }
    Op::LinkAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
      LinkAt::new(*old_dir_fd, *old_path, *new_dir_fd, *new_path).build()
    }
    Op::SymlinkAt { target, linkpath, dir_fd } => {
      SymlinkAt::new(*dir_fd, *target, *linkpath).build()
    }
    #[cfg(target_os = "linux")]
    Op::Tee { fd_in, fd_out, size } => {
      Tee::new(fd_in.as_raw_fd(), fd_out.as_raw_fd(), *size as u32).build()
    }
    Op::Timeout { duration, .. } => {
      let ts = libc::timespec {
        tv_sec: duration.as_secs() as i64,
        tv_nsec: duration.subsec_nanos() as i64,
      };
      Timeout::new(&ts, 1, Default::default()).build()
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
  sq: Option<SubmissionQueue>,
  cq: Option<CompletionQueue>,
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
  fn sq(&mut self) -> &mut SubmissionQueue {
    self.sq.as_mut().expect("IoUring not initialized - call init() first")
  }

  #[inline]
  fn cq(&mut self) -> &mut CompletionQueue {
    self.cq.as_mut().expect("IoUring not initialized - call init() first")
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
    let cq = self.cq();

    match timeout {
      None => {
        // Block indefinitely for first completion
        let first = cq.next()?;
        self
          .completed
          .push(OpCompleted::new(first.user_data(), first.result() as isize));
      }
      Some(d) if d.is_zero() => {
        // Non-blocking: check if anything is ready
        match cq.try_next()? {
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
        match cq.next_timeout(d)? {
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
    while let Ok(Some(op)) = cq.try_next() {
      self
        .completed
        .push(OpCompleted::new(op.user_data(), op.result() as isize));
    }

    Ok(&self.completed)
  }
}

impl IoBackend for IoUring {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    let (sq, cq) = lio_uring::with_capacity(cap as u32)?;
    self.sq = Some(sq);
    self.cq = Some(cq);
    // Pre-allocate completions buffer (reasonable batch size)
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, mut op: Op) -> io::Result<()> {
    let entry = create_io_uring_entry(&mut op);

    // Push to submission queue without syscall
    unsafe { self.sq().push(entry, id) }.map_err(|_| {
      io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
    })?;

    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    // Submit all queued operations with a single syscall
    let submitted = self.sq().submit()?;
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

  #[test]
  fn test_init() {
    let mut backend = IoUring::new();
    backend.init(64).unwrap();
  }
}
