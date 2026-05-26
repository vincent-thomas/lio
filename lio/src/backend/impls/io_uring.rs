//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use std::io;
use std::mem;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use bumpalo::Bump;
use lio_uring::{
  Entry, LioUring,
  operation::{
    Accept, Bind, Connect, Fsync, LinkAt, Listen, MkDirAt, Nop, OpenAt, Readv,
    RecvMsg, RenameAt, SendMsg, Shutdown, Socket, Statx, SymlinkAt, UnlinkAt,
    Writev,
  },
};

use crate::backend::{
  IoBackend, OpCompleted,
  op::{DirEntryRef, LinkKind, Op, ReadDirResult, file_type_from_dirent_dtype},
};
use crate::slab::{Slab, SlabKey};

const STATX_BASIC_STATS: u32 = 0x0000_07ff;
const LINUX_DIRENT64_NAME_OFFSET: usize = 19;
type CurrentDirent<'a> = (u64, u8, usize, &'a [u8]);

#[derive(Debug)]
struct PendingOp {
  registration_id: u64,
  op: Op,
  lowered: LoweredState,
}

#[derive(Debug)]
struct NativeMsgState {
  iovecs: [libc::iovec; crate::buf::MAX_IOV_COUNT],
  iov_count: usize,
  addr: Option<(libc::sockaddr_storage, libc::socklen_t)>,
  from_out: Option<std::ptr::NonNull<crate::backend::op::SocketAddrBuf>>,
  hdr: libc::msghdr,
}

impl NativeMsgState {
  fn from_recv(msg: &crate::backend::op::MsgRecv) -> Option<Self> {
    // SAFETY: `MsgRecv` validation guarantees `bufs` points to `buf_count`
    // live `RawBuf` entries for the duration of lowering.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut state = Self {
      iovecs: [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT],
      iov_count: bufs.len(),
      addr: None,
      from_out: msg.from,
      // SAFETY: `libc::msghdr` is a plain C struct and zero is a valid
      // sentinel initialization before we fill its pointer fields.
      hdr: unsafe { mem::zeroed() },
    };

    for (dst, src) in state.iovecs.iter_mut().zip(bufs.iter()) {
      dst.iov_base = src.ptr.as_ptr().cast();
      dst.iov_len = src.len;
    }

    if msg.from.is_some() {
      state.addr = Some((
        // SAFETY: zeroed `sockaddr_storage` is valid scratch storage before a
        // socket syscall writes an address into it.
        unsafe { mem::zeroed::<libc::sockaddr_storage>() },
        mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      ));
    }

    Some(state)
  }

  fn from_send(msg: &crate::backend::op::MsgSend) -> Option<Self> {
    // SAFETY: `MsgSend` validation guarantees `bufs` points to `buf_count`
    // live `RawBuf` entries for the duration of lowering.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut state = Self {
      iovecs: [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT],
      iov_count: bufs.len(),
      addr: msg.to.map(crate::backend::op::socket_addr_to_storage),
      from_out: None,
      // SAFETY: `libc::msghdr` is a plain C struct and zero is a valid
      // sentinel initialization before we fill its pointer fields.
      hdr: unsafe { mem::zeroed() },
    };

    for (dst, src) in state.iovecs.iter_mut().zip(bufs.iter()) {
      dst.iov_base = src.ptr.as_ptr().cast();
      dst.iov_len = src.len;
    }

    Some(state)
  }

  fn refresh_pointers(&mut self) {
    self.hdr.msg_iov = self.iovecs.as_mut_ptr();
    self.hdr.msg_iovlen = self.iov_count as _;

    if let Some((storage, len)) = self.addr.as_mut() {
      self.hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
      self.hdr.msg_namelen = *len;
    } else {
      self.hdr.msg_name = std::ptr::null_mut();
      self.hdr.msg_namelen = 0;
    }
  }
}

#[derive(Debug)]
struct NativeRwState {
  iovecs: [libc::iovec; crate::buf::MAX_IOV_COUNT],
}

#[derive(Debug)]
struct ReadDirState {
  buf: Vec<u8>,
  filled: usize,
  cursor: usize,
  eof: bool,
}

impl ReadDirState {
  fn with_capacity(cap: usize) -> Self {
    Self { buf: vec![0; cap], filled: 0, cursor: 0, eof: false }
  }

  fn ensure_capacity(&mut self, cap: usize) {
    if self.buf.len() != cap {
      self.buf.resize(cap, 0);
      self.filled = 0;
      self.cursor = 0;
    }
  }
}

impl NativeRwState {
  fn from_raws(
    iovecs: std::ptr::NonNull<crate::backend::op::RawBuf>,
    iov_count: usize,
  ) -> Option<Self> {
    // SAFETY: op validation guarantees `iovecs` references `iov_count`
    // contiguous `RawBuf` entries that stay live through lowering.
    let raws =
      unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
    let mut state = Self {
      iovecs: [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 };
        crate::buf::MAX_IOV_COUNT],
    };
    for (dst, src) in state.iovecs.iter_mut().zip(raws.iter()) {
      dst.iov_base = src.ptr.cast();
      dst.iov_len = src.len;
    }
    Some(state)
  }
}

#[derive(Debug)]
enum NativeSocketKind {
  Accept { out: std::ptr::NonNull<crate::backend::op::SocketAddrBuf> },
  Connect,
}

#[derive(Debug)]
struct NativeSocketState {
  storage: libc::sockaddr_storage,
  len: libc::socklen_t,
  kind: NativeSocketKind,
}

#[derive(Debug)]
struct NativeStatState {
  statx: lio_uring::statx,
  out: std::ptr::NonNull<crate::backend::op::FileStat>,
}

impl NativeStatState {
  fn new(out: std::ptr::NonNull<crate::backend::op::FileStat>) -> Self {
    // SAFETY: `statx` is a POD output struct written by the kernel before
    // being read during finalize.
    Self { statx: unsafe { mem::zeroed() }, out }
  }

  fn finalize(&self) {
    let file_type = match self.statx.stx_mode & libc::S_IFMT as u16 {
      x if x == libc::S_IFREG as u16 => crate::backend::op::FileType::File,
      x if x == libc::S_IFDIR as u16 => crate::backend::op::FileType::Directory,
      x if x == libc::S_IFLNK as u16 => crate::backend::op::FileType::Symlink,
      x if x == libc::S_IFBLK as u16 => {
        crate::backend::op::FileType::BlockDevice
      }
      x if x == libc::S_IFCHR as u16 => {
        crate::backend::op::FileType::CharDevice
      }
      x if x == libc::S_IFIFO as u16 => crate::backend::op::FileType::Fifo,
      x if x == libc::S_IFSOCK as u16 => crate::backend::op::FileType::Socket,
      _ => crate::backend::op::FileType::Unknown,
    };

    // SAFETY: `out` was provided by the caller as writable output storage and
    // remains valid until the operation is finalized.
    unsafe {
      *self.out.as_ptr() = crate::backend::op::FileStat {
        file_type,
        size: self.statx.stx_size,
        permissions: (self.statx.stx_mode as u32) & 0o7777,
        mode: self.statx.stx_mode as u32,
        nlink: self.statx.stx_nlink as u64,
        uid: self.statx.stx_uid,
        gid: self.statx.stx_gid,
      };
    }
  }
}

impl NativeSocketState {
  fn from_accept(
    out: std::ptr::NonNull<crate::backend::op::SocketAddrBuf>,
  ) -> Self {
    Self {
      // SAFETY: zeroed `sockaddr_storage` is valid scratch storage before
      // `accept` writes the peer address into it.
      storage: unsafe { mem::zeroed() },
      len: mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      kind: NativeSocketKind::Accept { out },
    }
  }

  fn from_connect(
    addr: &crate::backend::op::SocketAddrBuf,
  ) -> io::Result<Self> {
    let (storage, len) = crate::backend::op::socket_addr_buf_to_storage(addr)?;
    Ok(Self { storage, len, kind: NativeSocketKind::Connect })
  }
}

#[derive(Debug, Clone, Copy)]
enum LoweredState {
  Plain,
  Rw(NonNull<NativeRwState>),
  Msg(NonNull<NativeMsgState>),
  Socket(NonNull<NativeSocketState>),
  Stat(NonNull<NativeStatState>),
}

impl LoweredState {
  fn lower_in(op: &Op, arena: &Bump) -> io::Result<Self> {
    Ok(match op {
      Op::Read { iovecs, iov_count, .. } => {
        let state = NativeRwState::from_raws(*iovecs, *iov_count)
          .expect("validated read op must have iovecs");
        Self::Rw(NonNull::from(arena.alloc(state)))
      }
      Op::Write { iovecs, iov_count, .. } => {
        let state = NativeRwState::from_raws(*iovecs, *iov_count)
          .expect("validated read/write op must have iovecs");
        Self::Rw(NonNull::from(arena.alloc(state)))
      }
      Op::Recv { msg, .. } => {
        let state = NativeMsgState::from_recv(msg)
          .expect("validated recv op must have native message state");
        Self::Msg(NonNull::from(arena.alloc(state)))
      }
      Op::Send { msg, .. } => {
        let state = NativeMsgState::from_send(msg)
          .expect("validated send op must have native message state");
        Self::Msg(NonNull::from(arena.alloc(state)))
      }
      Op::Accept { addr, .. } => Self::Socket(NonNull::from(
        arena.alloc(NativeSocketState::from_accept(*addr)),
      )),
      Op::Connect { addr, .. } => Self::Socket(NonNull::from(
        arena.alloc(NativeSocketState::from_connect(addr)?),
      )),
      Op::Stat { out, .. } => {
        Self::Stat(NonNull::from(arena.alloc(NativeStatState::new(*out))))
      }
      _ => Self::Plain,
    })
  }

  fn create_entry(self, op: &Op) -> Entry {
    match self {
      Self::Plain => IoUring::create_plain_entry(op),
      Self::Rw(state) => {
        // SAFETY: `state` points into the bump arena backing this lowered op
        // and remains valid until submission finishes.
        IoUring::create_rw_entry(op, unsafe { state.as_ref() })
      }
      Self::Msg(state) => {
        // SAFETY: `state` points into the bump arena backing this lowered op
        // and remains valid until submission finishes.
        IoUring::create_msg_entry(op, unsafe { state.as_ref() })
      }
      Self::Socket(state) => {
        // SAFETY: `state` points into the bump arena backing this lowered op
        // and remains valid until submission finishes.
        IoUring::create_socket_entry(op, unsafe { state.as_ref() })
      }
      Self::Stat(state) => {
        // SAFETY: `state` points into the bump arena backing this lowered op
        // and remains valid until submission finishes.
        IoUring::create_stat_entry(op, unsafe { state.as_ref() })
      }
    }
  }

  fn finalize(self, result: i32) {
    if result < 0 {
      return;
    }
    match self {
      Self::Socket(state) => {
        // SAFETY: `state` points to the still-live lowered socket state for
        // this completion.
        let state = unsafe { state.as_ref() };
        let NativeSocketKind::Accept { out } = state.kind else {
          return;
        };
        if let Ok(addr) = crate::backend::op::socket_addr_buf_from_storage(
          &state.storage,
          state.len,
        ) {
          // SAFETY: `out` is caller-provided writable output storage that
          // remains valid until completion processing.
          unsafe {
            *out.as_ptr() = addr;
          }
        }
      }
      Self::Msg(state) => {
        let state = unsafe { state.as_ref() };
        if let (Some(out), Some((storage, len))) =
          (state.from_out, state.addr.as_ref())
          && let Ok(addr) = crate::backend::op::socket_addr_buf_from_storage(
            storage,
            *len,
          )
        {
          unsafe {
            *out.as_ptr() = addr;
          }
        }
      }
      Self::Stat(state) => {
        // SAFETY: `state` points to the still-live lowered stat state for this
        // completion.
        unsafe { state.as_ref() }.finalize()
      }
      _ => {}
    }
  }

  fn prepare(self) {
    if let Self::Msg(mut state) = self {
      // SAFETY: `state` points into the bump arena backing this lowered op and
      // is uniquely borrowed during preparation.
      unsafe { state.as_mut() }.refresh_pointers();
    }
  }
}

pub struct IoUring {
  ring: Option<LioUring>,
  capacity: usize,
  in_flight: usize,
  needs_submit: bool,
  backlog: Vec<PendingOp>,
  pending: Slab<PendingOp>,
  queued_completed: Vec<OpCompleted>,
  profile: Option<IoUringProfile>,
}

impl Default for IoUring {
  fn default() -> Self {
    Self {
      ring: None,
      capacity: 0,
      in_flight: 0,
      needs_submit: false,
      backlog: Vec::new(),
      pending: Slab::new(0),
      queued_completed: Vec::new(),
      profile: None,
    }
  }
}

#[derive(Debug, Default)]
struct IoUringProfile {
  flush_calls: usize,
  empty_flush_calls: usize,
  backlog_entries: usize,
  validated_ops: usize,
  nop_ops: usize,
  immediate_ops: usize,
  lowered_ops: usize,
  backlog_stage_time: Duration,
  validate_time: Duration,
  immediate_time: Duration,
  lower_time: Duration,
  prepare_time: Duration,
  push_time: Duration,
  pending_insert_time: Duration,
  final_submit_time: Duration,
}

impl Drop for IoUring {
  fn drop(&mut self) {
    let Some(profile) = &self.profile else {
      return;
    };
    eprintln!(
      "io_uring-profile flush_calls={} empty_flush_calls={} backlog_entries={} validated_ops={} nop_ops={} immediate_ops={} lowered_ops={} backlog_stage_ms={:.3} validate_ms={:.3} immediate_ms={:.3} lower_ms={:.3} prepare_ms={:.3} push_ms={:.3} pending_insert_ms={:.3} final_submit_ms={:.3}",
      profile.flush_calls,
      profile.empty_flush_calls,
      profile.backlog_entries,
      profile.validated_ops,
      profile.nop_ops,
      profile.immediate_ops,
      profile.lowered_ops,
      profile.backlog_stage_time.as_secs_f64() * 1000.0,
      profile.validate_time.as_secs_f64() * 1000.0,
      profile.immediate_time.as_secs_f64() * 1000.0,
      profile.lower_time.as_secs_f64() * 1000.0,
      profile.prepare_time.as_secs_f64() * 1000.0,
      profile.push_time.as_secs_f64() * 1000.0,
      profile.pending_insert_time.as_secs_f64() * 1000.0,
      profile.final_submit_time.as_secs_f64() * 1000.0,
    );
  }
}

impl IoUring {
  unsafe fn drop_read_dir_state(state: *mut ()) {
    if !state.is_null() {
      // SAFETY: `state` was allocated with `Box::into_raw` for `ReadDirState`
      // and is owned by the corresponding `ReadDirBuf` opaque slot.
      unsafe {
        drop(Box::from_raw(state.cast::<ReadDirState>()));
      }
    }
  }

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

  fn refill_read_dir_state(
    fd: std::os::fd::RawFd,
    state: &mut ReadDirState,
  ) -> io::Result<()> {
    if state.eof || state.buf.is_empty() {
      return Ok(());
    }

    state.cursor = 0;
    // SAFETY: `state.buf` is a valid writable buffer and `fd` is a live
    // directory file descriptor supplied by the caller.
    let nread = unsafe {
      libc::syscall(
        libc::SYS_getdents64,
        fd,
        state.buf.as_mut_ptr().cast::<libc::c_void>(),
        state.buf.len(),
      )
    };

    if nread < 0 {
      return Err(io::Error::last_os_error());
    }

    state.filled = nread as usize;
    state.eof = state.filled == 0;
    Ok(())
  }

  fn current_dirent(
    state: &ReadDirState,
  ) -> io::Result<Option<CurrentDirent<'_>>> {
    if state.cursor == state.filled {
      return Ok(None);
    }

    let remaining = &state.buf[state.cursor..state.filled];
    if remaining.len() < LINUX_DIRENT64_NAME_OFFSET {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "short linux_dirent64 record",
      ));
    }

    // SAFETY: `remaining.len() >= LINUX_DIRENT64_NAME_OFFSET` above guarantees
    // these unaligned field loads are within bounds of the current dirent.
    let ino =
      unsafe { std::ptr::read_unaligned(remaining.as_ptr().cast::<u64>()) };
    // SAFETY: `remaining` still points at the current dirent and byte 16 is
    // within bounds for a valid `linux_dirent64` header.
    let reclen = unsafe {
      std::ptr::read_unaligned(remaining.as_ptr().add(16).cast::<u16>())
    } as usize;
    let dtype = remaining[18];

    if reclen < LINUX_DIRENT64_NAME_OFFSET || reclen > remaining.len() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid linux_dirent64 reclen",
      ));
    }

    let name_field = &remaining[LINUX_DIRENT64_NAME_OFFSET..reclen];
    // SAFETY: `name_field` is a valid byte slice; `memchr` reads at most its
    // length and returns either null or a pointer inside the slice.
    let name_len = unsafe {
      let nul = libc::memchr(name_field.as_ptr().cast(), 0, name_field.len());
      if nul.is_null() {
        name_field.len()
      } else {
        nul.cast::<u8>().offset_from(name_field.as_ptr()) as usize
      }
    };
    Ok(Some((ino, dtype, reclen, &name_field[..name_len])))
  }

  #[inline]
  fn is_dot_entry(name: &[u8]) -> bool {
    match name.len() {
      1 => name[0] == b'.',
      2 => name[0] == b'.' && name[1] == b'.',
      _ => false,
    }
  }

  fn read_dir_entries(
    fd: std::os::fd::RawFd,
    opaque: &mut *mut (),
    opaque_drop: &mut Option<crate::backend::op::OpaqueDropFn>,
    raw: &mut [u8],
    out: &mut [DirEntryRef],
  ) -> io::Result<ReadDirResult> {
    let had_persistent_state = !opaque.is_null();
    let mut local_state =
      (!had_persistent_state).then(|| ReadDirState::with_capacity(raw.len()));

    let state = if let Some(state) = local_state.as_mut() {
      state
    } else {
      // SAFETY: `opaque` and `opaque_drop` are managed together and point to a
      // previously allocated `ReadDirState`.
      unsafe { &mut *opaque.cast::<ReadDirState>() }
    };
    state.ensure_capacity(raw.len());
    let result = (|| {
      let mut written = 0usize;
      let mut raw_written = 0usize;
      loop {
        if written == out.len() || raw_written == raw.len() {
          break;
        }

        if state.cursor == state.filled {
          Self::refill_read_dir_state(fd, state)?;
          if state.cursor == state.filled {
            break;
          }
        }

        let Some((ino, dtype, reclen, name)) = Self::current_dirent(state)?
        else {
          break;
        };
        let name_len = name.len();

        if Self::is_dot_entry(name) {
          state.cursor += reclen;
          continue;
        }

        if raw_written + name_len > raw.len() {
          break;
        }
        // SAFETY: bounds were checked above and source/destination do not
        // overlap because `name` points into `state.buf`, not `raw`.
        unsafe {
          std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            raw.as_mut_ptr().add(raw_written),
            name_len,
          );
        }
        out[written] = DirEntryRef {
          name_offset: raw_written as u32,
          name_len: name_len as u16,
          file_type: file_type_from_dirent_dtype(dtype),
          ino: Some(ino),
        };
        raw_written += name_len;
        written += 1;
        state.cursor += reclen;
      }
      Ok(ReadDirResult {
        entries: written,
        raw_written,
        eof: state.eof && state.cursor == state.filled,
      })
    })();

    match result {
      Ok(entries) => {
        if entries.eof {
          if had_persistent_state {
            *opaque = std::ptr::null_mut();
            *opaque_drop = None;
            // SAFETY: `opaque` points to a `ReadDirState` allocated with
            // `Box::into_raw`; clearing it here prevents double-drop.
            unsafe {
              drop(Box::from_raw(state));
            }
          }
          Ok(entries)
        } else {
          if let Some(state) = local_state.take() {
            let ptr = Box::into_raw(Box::new(state));
            *opaque = ptr.cast();
            *opaque_drop = Some(Self::drop_read_dir_state);
          }
          Ok(entries)
        }
      }
      Err(err) => {
        if had_persistent_state && !(*opaque).is_null() {
          // SAFETY: `opaque` points to a `ReadDirState` allocated with
          // `Box::into_raw`; clearing it here prevents double-drop.
          unsafe {
            drop(Box::from_raw((*opaque).cast::<ReadDirState>()));
          }
          *opaque = std::ptr::null_mut();
          *opaque_drop = None;
        }
        Err(err)
      }
    }
  }

  fn validate_op(op: &Op) -> Option<isize> {
    match op {
      Op::Read { iovecs: _, offset, flags, .. } => {
        if *offset < -1 {
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
      Op::Write { iovecs: _, offset, flags, .. } => {
        if *offset < -1 {
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
      Op::Connect { addr, .. } => {
        crate::backend::op::socket_addr_buf_to_storage(addr)
          .err()
          .and_then(|err| err.raw_os_error())
          .map(|errno| -(errno as isize))
      }
      Op::Socket { domain, ty, proto } => {
        crate::backend::op::socket_to_raw(*domain, *ty, *proto)
          .err()
          .map(|errno| -(errno as isize))
      }
      _ => None,
    }
  }

  fn create_plain_entry(op: &Op) -> Entry {
    match op {
      Op::Nop => Nop::new().build(),
      Op::OpenAt { dir_fd, path, flags, mode } => {
        OpenAt::new(dir_fd.as_raw_fd(), path.as_ptr())
          .flags(*flags)
          .mode(*mode as libc::mode_t)
          .build()
      }
      Op::UnlinkAt { dir_fd, path, flags } => {
        UnlinkAt::new(dir_fd.as_raw_fd(), path.as_ptr()).flags(*flags).build()
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        RenameAt::new(
          old_dir_fd.as_raw_fd(),
          old_path.as_ptr(),
          new_dir_fd.as_raw_fd(),
          new_path.as_ptr(),
        )
        .build()
      }
      Op::MkdirAt { dir_fd, path, mode } => {
        MkDirAt::new(dir_fd.as_raw_fd(), path.as_ptr())
          .mode(*mode as libc::mode_t)
          .build()
      }
      Op::LinkAt { kind, source_dir_fd, source_path, new_dir_fd, new_path } => {
        match kind {
          LinkKind::Hard => LinkAt::new(
            source_dir_fd.as_raw_fd(),
            source_path.as_ptr(),
            new_dir_fd.as_raw_fd(),
            new_path.as_ptr(),
          )
          .build(),
          LinkKind::Soft => SymlinkAt::new(
            new_dir_fd.as_raw_fd(),
            source_path.as_ptr(),
            new_path.as_ptr(),
          )
          .build(),
        }
      }
      Op::Socket { domain, ty, proto } => {
        let (domain, ty, proto) =
          crate::backend::op::socket_to_raw(*domain, *ty, *proto)
            .expect("socket op must be validated before entry creation");
        Socket::new(domain, ty, proto).build()
      }
      Op::Stat { .. } => {
        unreachable!("lowered stat op must not use plain entry creation")
      }
      Op::ReadlinkAt { .. } | Op::GetCwd { .. } | Op::ReadDir { .. } => {
        unreachable!("immediate-only op must not build an io_uring entry")
      }
      #[cfg(unix)]
      Op::Spawn { .. } => {
        unreachable!("immediate-only op must not build an io_uring entry")
      }
      Op::Bind { fd, addr } => {
        let storage = crate::api::ops::std_socketaddr_into_libc(*addr);
        let len = match addr {
          SocketAddr::V4(_) => std::mem::size_of::<libc::sockaddr_in>(),
          SocketAddr::V6(_) => std::mem::size_of::<libc::sockaddr_in6>(),
        } as libc::socklen_t;
        Bind::new(
          fd.as_raw_fd(),
          (&storage as *const libc::sockaddr_storage).cast(),
          len,
        )
        .build()
      }
      Op::Listen { fd, backlog } => {
        Listen::new(fd.as_raw_fd(), *backlog).build()
      }
      Op::Shutdown { fd, how } => Shutdown::new(fd.as_raw_fd(), *how).build(),
      Op::Fsync { fd } => Fsync::new(fd.as_raw_fd()).build(),
      Op::Read { .. }
      | Op::Write { .. }
      | Op::Recv { .. }
      | Op::Send { .. }
      | Op::Accept { .. }
      | Op::Connect { .. } => {
        unreachable!("lowered op must not use plain entry creation")
      }
    }
  }

  fn create_rw_entry(op: &Op, native_rw: &NativeRwState) -> Entry {
    match op {
      Op::Read { fd, iov_count, offset, flags, .. } => {
        Readv::new(fd.as_raw_fd(), native_rw.iovecs.as_ptr(), *iov_count as u32)
          .offset(Self::io_offset(*offset))
          .rw_flags(*flags)
          .build()
      }
      Op::Write { fd, iov_count, offset, flags, .. } => Writev::new(
        fd.as_raw_fd(),
        native_rw.iovecs.as_ptr(),
        *iov_count as u32,
      )
      .offset(Self::io_offset(*offset))
      .rw_flags(*flags)
      .build(),
      _ => unreachable!("rw entry requires read/write op"),
    }
  }

  fn create_msg_entry(op: &Op, native_msg: &NativeMsgState) -> Entry {
    match op {
      Op::Recv { fd, flags, .. } => RecvMsg::new(
        fd.as_raw_fd(),
        (&native_msg.hdr as *const libc::msghdr).cast_mut(),
      )
      .flags(*flags as u32)
      .build(),
      Op::Send { fd, flags, .. } => {
        SendMsg::new(fd.as_raw_fd(), &native_msg.hdr)
          .flags(*flags as u32)
          .build()
      }
      _ => unreachable!("msg entry requires recv/send op"),
    }
  }

  fn create_socket_entry(op: &Op, native_socket: &NativeSocketState) -> Entry {
    match op {
      Op::Accept { fd, .. } => Accept::new(
        fd.as_raw_fd(),
        (&native_socket.storage as *const libc::sockaddr_storage)
          .cast_mut()
          .cast(),
        (&native_socket.len as *const libc::socklen_t).cast_mut(),
      )
      .build(),
      Op::Connect { fd, .. } => Connect::new(
        fd.as_raw_fd(),
        (&native_socket.storage as *const libc::sockaddr_storage).cast(),
        native_socket.len,
      )
      .build(),
      _ => unreachable!("socket entry requires accept/connect op"),
    }
  }

  fn create_stat_entry(op: &Op, native_stat: &NativeStatState) -> Entry {
    match op {
      Op::Stat { target, .. } => match target {
        crate::backend::op::StatTarget::Path {
          dir_fd,
          path,
          follow_symlinks,
        } => {
          let flags =
            if *follow_symlinks { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
          Statx::new(
            dir_fd.as_raw_fd(),
            path.as_ptr().cast_const(),
            (&native_stat.statx as *const lio_uring::statx).cast_mut(),
          )
          .flags(flags)
          .mask(STATX_BASIC_STATS)
          .build()
        }
        crate::backend::op::StatTarget::Fd { fd } => Statx::new(
          fd.as_raw_fd(),
          c"".as_ptr(),
          (&native_stat.statx as *const lio_uring::statx).cast_mut(),
        )
        .flags(libc::AT_EMPTY_PATH)
        .mask(STATX_BASIC_STATS)
        .build(),
      },
      _ => unreachable!("stat entry requires stat op"),
    }
  }

  fn push_entry(
    &mut self,
    pending: &PendingOp,
    id: u64,
    sq_space_left: &mut usize,
  ) -> io::Result<()> {
    if *sq_space_left == 0 {
      self.ring().submit()?;
      *sq_space_left = self.ring().sq_space_left();
    }
    let io_uring_entry = pending.lowered.create_entry(&pending.op);
    // SAFETY: `io_uring_entry` references buffers owned by `pending`, which
    // stays in the slab until completion processing removes it.
    unsafe { self.ring().push(io_uring_entry, id) }?;
    *sq_space_left = sq_space_left.saturating_sub(1);
    Ok(())
  }

  fn execute_immediate(op: &Op) -> Option<io::Result<isize>> {
    match op {
      Op::ReadlinkAt { dir_fd, path, buf, buf_len } => {
        // SAFETY: pointers come directly from validated op arguments and remain
        // valid for the duration of this synchronous syscall.
        let result = unsafe {
          libc::readlinkat(
            dir_fd.as_raw_fd(),
            path.as_ptr(),
            buf.as_ptr().cast::<libc::c_char>(),
            *buf_len,
          )
        };
        Some(if result < 0 {
          Err(io::Error::last_os_error())
        } else {
          Ok(result as isize)
        })
      }
      Op::GetCwd { buf, buf_len } => {
        // SAFETY: `buf` is caller-provided writable storage of length
        // `buf_len`, valid for this synchronous libc call.
        let result = unsafe {
          libc::getcwd(buf.as_ptr().cast::<libc::c_char>(), *buf_len)
        };
        Some(if result.is_null() {
          Err(io::Error::last_os_error())
        } else {
          // SAFETY: `getcwd` returned a valid NUL-terminated pointer into
          // `buf`, so `strlen` may read until the first terminator.
          Ok(unsafe { libc::strlen(result) as isize })
        })
      }
      Op::ReadDir {
        fd,
        raw_buf,
        raw_cap,
        entries,
        entries_cap,
        opaque,
        opaque_drop,
        out,
      } => {
        // SAFETY: `raw_buf` points to `raw_cap` writable bytes owned by the
        // caller for the duration of this synchronous immediate operation.
        let raw =
          unsafe { std::slice::from_raw_parts_mut(raw_buf.as_ptr(), *raw_cap) };
        // SAFETY: `entries` points to `entries_cap` writable `DirEntryRef`
        // slots owned by the caller for the duration of this call.
        let entries = unsafe {
          std::slice::from_raw_parts_mut(entries.as_ptr(), *entries_cap)
        };
        // SAFETY: `opaque` is an out-parameter pointing to caller-owned state
        // storage used to persist `ReadDirState` across calls.
        let opaque = unsafe { &mut *opaque.as_ptr() };
        // SAFETY: `opaque_drop` is an out-parameter paired with `opaque` and
        // points to caller-owned storage for the drop hook.
        let opaque_drop = unsafe { &mut *opaque_drop.as_ptr() };
        Some(
          match Self::read_dir_entries(
            fd.as_raw_fd(),
            opaque,
            opaque_drop,
            raw,
            entries,
          ) {
            Ok(result) => {
              // SAFETY: `out` points to caller-provided writable output
              // storage for the immediate result.
              unsafe {
                *out.as_ptr() = result;
              }
              Ok(0)
            }
            Err(err) => Err(err),
          },
        )
      }
      #[cfg(unix)]
      Op::Spawn { path, argv, envp } => {
        unsafe extern "C" {
          static mut environ: *mut *mut libc::c_char;
        }
        let mut pid: libc::pid_t = 0;
        let envp = if let Some(envp) = envp {
          envp.as_ptr().cast_const()
        } else {
          // SAFETY: `environ` is the process-global environment pointer
          // provided by libc and is valid to read here.
          unsafe { environ as *const *mut libc::c_char }
        };
        // SAFETY: all pointers are passed directly from validated op inputs and
        // remain valid for this synchronous `posix_spawn` call.
        let result = unsafe {
          libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv.as_ptr().cast_const(),
            envp,
          )
        };
        Some(if result != 0 {
          Err(io::Error::from_raw_os_error(result))
        } else {
          Ok(pid as isize)
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
    self.needs_submit = false;
    self.backlog.clear();
    self.backlog.reserve_exact(cap);
    self.pending = Slab::new(cap);
    self.queued_completed = Vec::with_capacity(cap);
    self.profile =
      std::env::var_os("LIO_PROFILE").map(|_| IoUringProfile::default());
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, step_bump: &mut Bump) {
    assert!(
      self.backlog.len() + self.in_flight < self.capacity,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.capacity
    );
    let lowered = match LoweredState::lower_in(&op, step_bump) {
      Ok(lowered) => lowered,
      Err(err) => {
        self.queued_completed.push(OpCompleted::new(
          id,
          -(err.raw_os_error().unwrap_or(libc::EIO) as isize),
        ));
        return;
      }
    };
    self.backlog.push(PendingOp { registration_id: id, op, lowered });
  }

  fn flush(&mut self) -> io::Result<()> {
    let profiling_enabled = self.profile.is_some();
    let mut backlog_stage_time = Duration::ZERO;
    let mut validate_time = Duration::ZERO;
    let mut immediate_time = Duration::ZERO;
    let lower_time = Duration::ZERO;
    let mut prepare_time = Duration::ZERO;
    let mut push_time = Duration::ZERO;
    let mut pending_insert_time = Duration::ZERO;
    let final_submit_time = Duration::ZERO;
    let mut validated_ops = 0usize;
    let mut nop_ops = 0usize;
    let mut immediate_ops = 0usize;
    let mut lowered_ops = 0usize;
    let backlog_stage_started =
      if profiling_enabled { Some(Instant::now()) } else { None };
    let processing_backlog = std::mem::take(&mut self.backlog);
    if let Some(started) = backlog_stage_started {
      backlog_stage_time += started.elapsed();
    }

    if processing_backlog.is_empty() {
      if let Some(profile) = self.profile.as_mut() {
        profile.flush_calls += 1;
        profile.empty_flush_calls += 1;
        profile.backlog_stage_time += backlog_stage_time;
      }
      return Ok(());
    }

    let mut submitted_any = false;
    let backlog_entries = processing_backlog.len();
    let mut sq_space_left = self.ring().sq_space_left();

    for entry in processing_backlog {
      let validate_started =
        if profiling_enabled { Some(Instant::now()) } else { None };
      if let Some(result) = Self::validate_op(&entry.op) {
        if let Some(started) = validate_started {
          validate_time += started.elapsed();
        }
        validated_ops += 1;
        self
          .queued_completed
          .push(OpCompleted::new(entry.registration_id, result));
        continue;
      }
      if let Some(started) = validate_started {
        validate_time += started.elapsed();
      }

      if matches!(&entry.op, Op::Nop) {
        nop_ops += 1;
        self.queued_completed.push(OpCompleted::new(entry.registration_id, 0));
        continue;
      }

      let immediate_started =
        if profiling_enabled { Some(Instant::now()) } else { None };
      if let Some(result) = Self::execute_immediate(&entry.op) {
        if let Some(started) = immediate_started {
          immediate_time += started.elapsed();
        }
        immediate_ops += 1;
        let result = match result {
          Ok(value) => value,
          Err(err) => -(err.raw_os_error().unwrap_or(libc::EIO) as isize),
        };
        self
          .queued_completed
          .push(OpCompleted::new(entry.registration_id, result));
        continue;
      }
      if let Some(started) = immediate_started {
        immediate_time += started.elapsed();
      }

      let pending_insert_started =
        if profiling_enabled { Some(Instant::now()) } else { None };
      let (pending_key, pending) = self
        .pending
        .insert_get_mut(entry)
        .expect("in-flight pending operations must fit backend capacity");
      if let Some(started) = pending_insert_started {
        pending_insert_time += started.elapsed();
      }

      let mut local_prepare_time = Duration::ZERO;
      let pending_ptr = {
        let prepare_started =
          if profiling_enabled { Some(Instant::now()) } else { None };
        pending.lowered.prepare();
        if let Some(started) = prepare_started {
          local_prepare_time += started.elapsed();
        }
        pending as *const PendingOp
      };
      prepare_time += local_prepare_time;

      // SAFETY: `pending_ptr` points into a slab slot that remains stable
      // until completion removes it from the slab.
      let pending = unsafe { &*pending_ptr };
      let push_started =
        if profiling_enabled { Some(Instant::now()) } else { None };
      self.push_entry(pending, pending_key.as_u64(), &mut sq_space_left)?;
      if let Some(started) = push_started {
        push_time += started.elapsed();
      }

      self.in_flight += 1;
      submitted_any = true;
      lowered_ops += 1;
    }

    if submitted_any {
      self.needs_submit = true;
    }
    if let Some(profile) = self.profile.as_mut() {
      profile.flush_calls += 1;
      profile.backlog_entries += backlog_entries;
      profile.validated_ops += validated_ops;
      profile.nop_ops += nop_ops;
      profile.immediate_ops += immediate_ops;
      profile.lowered_ops += lowered_ops;
      profile.backlog_stage_time += backlog_stage_time;
      profile.validate_time += validate_time;
      profile.immediate_time += immediate_time;
      profile.lower_time += lower_time;
      profile.prepare_time += prepare_time;
      profile.push_time += push_time;
      profile.pending_insert_time += pending_insert_time;
      profile.final_submit_time += final_submit_time;
    }
    Ok(())
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();

    if !self.queued_completed.is_empty() {
      completed.append(&mut self.queued_completed);
      return Ok(());
    }

    let first = match timeout {
      None => {
        if self.needs_submit {
          self.ring().submit_and_wait(1)?;
          self.needs_submit = false;
          Some(self.ring().wait()?)
        } else {
          Some(self.ring().wait()?)
        }
      }
      Some(d) if d.is_zero() => {
        if self.needs_submit {
          self.ring().submit()?;
          self.needs_submit = false;
        }
        self.ring().try_wait()?
      }
      Some(d) => {
        if self.needs_submit {
          self.ring().submit()?;
          self.needs_submit = false;
        }
        self.ring().wait_timeout(d)?
      }
    };

    if let Some(completion) = first {
      self.in_flight = self.in_flight.saturating_sub(1);
      let key = SlabKey::from_u64(completion.user_data());
      let result = completion.result() as isize;
      if let Some(pending) = self.pending.remove_value(key) {
        pending.lowered.finalize(completion.result());
        completed.push(OpCompleted::new(pending.registration_id, result));
      }
      while let Some(completion) = self.ring().try_wait()? {
        self.in_flight = self.in_flight.saturating_sub(1);
        let key = SlabKey::from_u64(completion.user_data());
        let result = completion.result() as isize;
        if let Some(pending) = self.pending.remove_value(key) {
          pending.lowered.finalize(completion.result());
          completed.push(OpCompleted::new(pending.registration_id, result));
        }
      }
    }

    Ok(())
  }
}
