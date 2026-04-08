//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use std::io;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::time::Duration;

use lio_uring::{
  operation::{
    Accept, Connect, Nop, OpenAt, Readv, RecvMsg, SendMsg, Socket, Writev,
  },
  Entry, LioUring,
};

use crate::backend::{op::Op, IoBackend, OpCompleted};

#[derive(Debug)]
struct BacklogEntry {
  id: u64,
  op: Op,
}

#[derive(Default)]
pub struct IoUring {
  ring: Option<LioUring>,
  capacity: usize,
  in_flight: usize,
  backlog: Vec<BacklogEntry>,
  pending: HashMap<u64, Op>,
  queued_completed: Vec<OpCompleted>,
  completed: Vec<OpCompleted>,
}

impl IoUring {
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn ring(&mut self) -> &mut LioUring {
    self.ring.as_mut().expect("IoUring not initialized")
  }

  #[inline]
  fn io_offset(offset: i64) -> u64 {
    if offset < 0 {
      u64::MAX
    } else {
      offset as u64
    }
  }

  fn validate_op(op: &Op) -> Option<isize> {
    match op {
      Op::Read { iovecs, offset, flags, .. } => {
        if *offset < -1 || iovecs.is_null() {
          return Some(-(libc::EINVAL as isize));
        }

        const RWF_HIPRI: i32 = 0x00000001;
        const RWF_DSYNC: i32 = 0x00000002;
        const RWF_SYNC: i32 = 0x00000004;
        const RWF_APPEND: i32 = 0x00000010;
        const ALL_KNOWN_FLAGS: i32 =
          RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND;

        if flags & !ALL_KNOWN_FLAGS != 0 {
          return Some(-(libc::ENOTSUP as isize));
        }

        None
      }
      Op::Write { iovecs, offset, flags, .. } => {
        if *offset < -1 || iovecs.is_null() {
          return Some(-(libc::EINVAL as isize));
        }

        const RWF_HIPRI: i32 = 0x00000001;
        const RWF_DSYNC: i32 = 0x00000002;
        const RWF_SYNC: i32 = 0x00000004;
        const RWF_APPEND: i32 = 0x00000010;
        const ALL_KNOWN_FLAGS: i32 =
          RWF_HIPRI | RWF_DSYNC | RWF_SYNC | RWF_APPEND;

        if flags & !ALL_KNOWN_FLAGS != 0 {
          return Some(-(libc::ENOTSUP as isize));
        }

        None
      }
      Op::Recv { msg, .. } if msg.is_null() => Some(-(libc::EINVAL as isize)),
      Op::Send { msg, .. } if msg.is_null() => Some(-(libc::EINVAL as isize)),
      Op::Connect { addr, .. } if addr.is_null() => {
        Some(-(libc::EINVAL as isize))
      }
      Op::OpenAt { path, .. } if path.is_null() => {
        Some(-(libc::EINVAL as isize))
      }
      Op::Socket { domain, ty, proto } => {
        crate::backend::op::socket_to_raw(*domain, *ty, *proto)
          .err()
          .map(|errno| -(errno as isize))
      }
      _ => None,
    }
  }

  fn create_entry(op: &Op) -> Entry {
    match op {
      Op::Nop => Nop::new().build(),
      Op::Read { fd, iovecs, iov_count, offset, flags } => Readv::new(
        fd.as_raw_fd(),
        iovecs.cast::<libc::iovec>(),
        *iov_count as u32,
      )
      .offset(Self::io_offset(*offset))
      .rw_flags(*flags)
      .build(),
      Op::Write { fd, iovecs, iov_count, offset, flags } => Writev::new(
        fd.as_raw_fd(),
        iovecs.cast::<libc::iovec>(),
        *iov_count as u32,
      )
      .offset(Self::io_offset(*offset))
      .rw_flags(*flags)
      .build(),
      Op::Recv { fd, msg, flags } => {
        RecvMsg::new(fd.as_raw_fd(), *msg).flags(*flags as u32).build()
      }
      Op::Send { fd, msg, flags } => {
        SendMsg::new(fd.as_raw_fd(), *msg).flags(*flags as u32).build()
      }
      Op::Accept { fd, addr, len } => {
        Accept::new(fd.as_raw_fd(), (*addr).cast(), *len).build()
      }
      Op::Connect { fd, addr, len } => {
        Connect::new(fd.as_raw_fd(), (*addr).cast(), *len).build()
      }
      Op::OpenAt { dir_fd, path, flags, mode } => {
        OpenAt::new(dir_fd.as_raw_fd(), *path)
          .flags(*flags)
          .mode(*mode as libc::mode_t)
          .build()
      }
      Op::Socket { domain, ty, proto } => {
        let (domain, ty, proto) =
          crate::backend::op::socket_to_raw(*domain, *ty, *proto)
            .expect("socket op must be validated before entry creation");
        Socket::new(domain, ty, proto).build()
      }
    }
  }

  fn push_op(&mut self, op: &Op, id: u64) -> io::Result<()> {
    let entry = Self::create_entry(op);
    match unsafe { self.ring().push(entry, id) } {
      Ok(()) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
        self.ring().submit()?;
        let entry = Self::create_entry(op);
        unsafe { self.ring().push(entry, id) }
      }
      Err(err) => Err(err),
    }
  }

  fn drain_completion(&mut self, user_data: u64, result: i32) {
    self.completed.push(OpCompleted::new(user_data, result as isize));
  }
}

impl IoBackend for IoUring {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.ring = Some(LioUring::new(cap as u32)?);
    self.capacity = cap;
    self.in_flight = 0;
    self.backlog.clear();
    self.backlog.reserve_exact(cap);
    self.pending = HashMap::with_capacity(cap);
    self.queued_completed = Vec::with_capacity(cap.min(256));
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op) {
    assert!(
      self.backlog.len() + self.in_flight < self.capacity,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.capacity
    );
    self.backlog.push(BacklogEntry { id, op });
  }

  fn flush(&mut self) -> io::Result<()> {
    let backlog = std::mem::take(&mut self.backlog);

    for entry in backlog {
      if let Some(result) = Self::validate_op(&entry.op) {
        self.queued_completed.push(OpCompleted::new(entry.id, result));
        continue;
      }

      if matches!(entry.op, Op::Nop) {
        self.queued_completed.push(OpCompleted::new(entry.id, 0));
        continue;
      }

      self.push_op(&entry.op, entry.id)?;
      self.pending.insert(entry.id, entry.op);
      self.in_flight += 1;
    }

    self.ring().submit()?;
    Ok(())
  }

  fn wait(&mut self, timeout: Option<Duration>) -> io::Result<&[OpCompleted]> {
    self.completed.clear();

    if !self.queued_completed.is_empty() {
      self.completed.append(&mut self.queued_completed);
      return Ok(&self.completed);
    }

    let first = match timeout {
      None => Some(self.ring().wait()?),
      Some(d) if d.is_zero() => self.ring().try_wait()?,
      Some(d) => self.ring().wait_timeout(d)?,
    };

    if let Some(completion) = first {
      self.in_flight = self.in_flight.saturating_sub(1);
      self.pending.remove(&completion.user_data());
      self.drain_completion(completion.user_data(), completion.result());
      while let Some(completion) = self.ring().try_wait()? {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.pending.remove(&completion.user_data());
        self.drain_completion(completion.user_data(), completion.result());
      }
    }

    Ok(&self.completed)
  }
}

#[cfg(test)]
crate::test_io_backend!(IoUring::new());
