//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use std::collections::HashMap;
use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::time::Duration;

use lio_uring::{
  Entry, LioUring,
  operation::{
    Accept, Connect, LinkAt, MkDirAt, Nop, OpenAt, Readv, RecvMsg, RenameAt,
    SendMsg, Socket, SymlinkAt, UnlinkAt, Writev,
  },
};

use crate::backend::{
  IoBackend, OpCompleted,
  op::{LinkKind, Op},
};

#[derive(Debug)]
struct BacklogEntry {
  id: u64,
  op: Op,
}

#[derive(Debug)]
struct NativeMsgState {
  iovecs: [libc::iovec; crate::buf::MAX_IOV_COUNT],
  addr: Option<(libc::sockaddr_storage, libc::socklen_t)>,
  hdr: libc::msghdr,
}

impl NativeMsgState {
  fn from_recv(msg: &crate::backend::op::MsgRecv) -> Option<Self> {
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut state = Self {
      iovecs: [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT],
      addr: None,
      hdr: unsafe { mem::zeroed() },
    };

    for (dst, src) in state.iovecs.iter_mut().zip(bufs.iter()) {
      dst.iov_base = src.ptr.as_ptr().cast();
      dst.iov_len = src.len;
    }

    state.hdr.msg_iov = state.iovecs.as_mut_ptr();
    state.hdr.msg_iovlen = bufs.len() as _;

    if msg.from {
      state.addr = Some((
        unsafe { mem::zeroed::<libc::sockaddr_storage>() },
        mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      ));
      if let Some((storage, len)) = state.addr.as_mut() {
        state.hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
        state.hdr.msg_namelen = *len;
      }
    }

    Some(state)
  }

  fn from_send(msg: &crate::backend::op::MsgSend) -> Option<Self> {
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut state = Self {
      iovecs: [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT],
      addr: msg.to.map(crate::backend::op::socket_addr_to_storage),
      hdr: unsafe { mem::zeroed() },
    };

    for (dst, src) in state.iovecs.iter_mut().zip(bufs.iter()) {
      dst.iov_base = src.ptr.as_ptr().cast();
      dst.iov_len = src.len;
    }

    state.hdr.msg_iov = state.iovecs.as_mut_ptr();
    state.hdr.msg_iovlen = bufs.len() as _;

    if let Some((storage, len)) = state.addr.as_mut() {
      state.hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
      state.hdr.msg_namelen = *len;
    }

    Some(state)
  }
}

#[derive(Debug)]
struct PendingEntry {
  op: Op,
  native_msg: Option<NativeMsgState>,
}

#[derive(Default)]
pub struct IoUring {
  ring: Option<LioUring>,
  capacity: usize,
  in_flight: usize,
  backlog: Vec<BacklogEntry>,
  pending: HashMap<u64, PendingEntry>,
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
    if offset < 0 { u64::MAX } else { offset as u64 }
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
      Op::Connect { addr, .. } if addr.is_null() => {
        Some(-(libc::EINVAL as isize))
      }
      Op::OpenAt { path, .. } if path.is_null() => {
        Some(-(libc::EINVAL as isize))
      }
      Op::UnlinkAt { path, .. } if path.is_null() => {
        Some(-(libc::EINVAL as isize))
      }
      Op::RenameAt { old_path, new_path, .. }
        if old_path.is_null() || new_path.is_null() =>
      {
        Some(-(libc::EINVAL as isize))
      }
      Op::MkdirAt { path, .. } if path.is_null() => {
        Some(-(libc::EINVAL as isize))
      }
      Op::LinkAt { source_path, new_path, .. }
        if source_path.is_null() || new_path.is_null() =>
      {
        Some(-(libc::EINVAL as isize))
      }
      Op::ReadlinkAt { path, buf, .. } if path.is_null() || buf.is_null() => {
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

  fn create_entry(op: &Op, native_msg: Option<&NativeMsgState>) -> Entry {
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
      Op::Recv { fd, flags, .. } => {
        let native_msg =
          native_msg.expect("recv native state must be hydrated");
        RecvMsg::new(fd.as_raw_fd(), &native_msg.hdr)
          .flags(*flags as u32)
          .build()
      }
      Op::Send { fd, flags, .. } => {
        let native_msg =
          native_msg.expect("send native state must be hydrated");
        SendMsg::new(fd.as_raw_fd(), &native_msg.hdr)
          .flags(*flags as u32)
          .build()
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
      Op::UnlinkAt { dir_fd, path, flags } => {
        UnlinkAt::new(dir_fd.as_raw_fd(), *path).flags(*flags).build()
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        RenameAt::new(
          old_dir_fd.as_raw_fd(),
          *old_path,
          new_dir_fd.as_raw_fd(),
          *new_path,
        )
        .build()
      }
      Op::MkdirAt { dir_fd, path, mode } => {
        MkDirAt::new(dir_fd.as_raw_fd(), *path)
          .mode(*mode as libc::mode_t)
          .build()
      }
      Op::LinkAt { kind, source_dir_fd, source_path, new_dir_fd, new_path } => {
        match kind {
          LinkKind::Hard => LinkAt::new(
            source_dir_fd.as_raw_fd(),
            *source_path,
            new_dir_fd.as_raw_fd(),
            *new_path,
          )
          .build(),
          LinkKind::Soft => {
            SymlinkAt::new(new_dir_fd.as_raw_fd(), *source_path, *new_path)
              .build()
          }
        }
      }
      Op::Socket { domain, ty, proto } => {
        let (domain, ty, proto) =
          crate::backend::op::socket_to_raw(*domain, *ty, *proto)
            .expect("socket op must be validated before entry creation");
        Socket::new(domain, ty, proto).build()
      }
    }
  }

  fn push_op(
    &mut self,
    op: &Op,
    native_msg: Option<&NativeMsgState>,
    id: u64,
  ) -> io::Result<()> {
    let entry = Self::create_entry(op, native_msg);
    match unsafe { self.ring().push(entry, id) } {
      Ok(()) => Ok(()),
      Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
        self.ring().submit()?;
        let entry = Self::create_entry(op, native_msg);
        unsafe { self.ring().push(entry, id) }
      }
      Err(err) => Err(err),
    }
  }

  fn drain_completion(&mut self, user_data: u64, result: i32) {
    self.completed.push(OpCompleted::new(user_data, result as isize));
  }

  fn execute_immediate(op: &Op) -> Option<io::Result<isize>> {
    match op {
      Op::ReadlinkAt { dir_fd, path, buf, buf_len } => {
        let result = unsafe {
          libc::readlinkat(
            dir_fd.as_raw_fd(),
            *path,
            (*buf).cast::<libc::c_char>(),
            *buf_len,
          )
        };
        Some(if result < 0 {
          Err(io::Error::last_os_error())
        } else {
          Ok(result as isize)
        })
      }
      _ => None,
    }
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

      if let Some(result) = Self::execute_immediate(&entry.op) {
        let result = match result {
          Ok(value) => value,
          Err(err) => -(err.raw_os_error().unwrap_or(libc::EIO) as isize),
        };
        self.queued_completed.push(OpCompleted::new(entry.id, result));
        continue;
      }

      let native_msg = match &entry.op {
        Op::Recv { msg, .. } => NativeMsgState::from_recv(msg),
        Op::Send { msg, .. } => NativeMsgState::from_send(msg),
        _ => None,
      };
      self.push_op(&entry.op, native_msg.as_ref(), entry.id)?;
      self.pending.insert(entry.id, PendingEntry { op: entry.op, native_msg });
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
