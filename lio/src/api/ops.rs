//! Typed I/O operations.
//!
//! Each type here is one operation as an [`OpModel`]: polling it starts the
//! operation and produces its result. They are made by the functions in
//! [`api`](crate::api), and are opaque.
//!
//! Run one with [`Io`](crate::api::Io), or take it out with
//! [`Io::into_inner`](crate::api::Io::into_inner) and poll it from a model
//! of your own.
//!
//! # How an operation is written
//!
//! An operation is two things, next to each other in this file:
//!
//! - its type, which holds what it was asked to do until it is polled, and
//! - its method on [`Cx`], which moves the memory the operation uses into
//!   the driver, says what to submit from it, and says how the result is
//!   read back out of it.
//!
//! The type's `poll` calls the method. Nothing else can: the methods are
//! private to lio, so the result an operation produces is always the result
//! of what it submitted.

use std::ffi::{CString, OsString};
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::Duration;

use crate::api::Pid;
use crate::api::op::{Action, Completion, CompletionFlags};
use crate::api::resource::Resource;
use crate::backend::op::{
  FileMode, FileStat, MsgBuf, MsgBufMut, MsgRecv, MsgSend, Op, OpenFlags,
  RawBuf, RawBufMut, ReadDirBuf, ReadFlags, RecvFlags, SendFlags, ShutdownHow,
  SockDomain, SockProto, SockType, SocketAddrBuf, SocketAddrFamily, SpawnSpec,
  StatTarget, UnlinkKind, WriteFlags, is_valid_file_offset,
  socket_addr_from_buf,
};
use crate::model::{Cx, OpModel, Step, Ticket};
use crate::registration::Regions;
use crate::{
  BufResult, Initialized, IoBufMutVec, IoBufVec, IoSlicesMut, ready,
};

pub use crate::backend::op::LinkKind;

// ============================================================================
// Shared pieces
// ============================================================================

fn path_to_os_string(path: &CString) -> OsString {
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(path.as_bytes().to_vec())
  }
  #[cfg(not(unix))]
  {
    OsString::from(path.to_string_lossy().as_ref())
  }
}

fn os_string_from_bytes(bytes: &[u8]) -> OsString {
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
  }
  #[cfg(not(unix))]
  {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
  }
}

/// What an operation's memory holds until the driver uses it, once.
fn held<T>(value: Option<T>) -> T {
  let Some(value) = value else {
    unreachable!("an operation's memory was used twice");
  };
  value
}

pub(crate) fn nul_path_error() -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    "path contains an interior NUL byte",
  )
}

fn offset_out_of_range_error() -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    "file offset exceeds the largest offset the OS supports (i64::MAX)",
  )
}

/// Submits an operation that owns everything it names.
fn owned(op: &mut Option<Op>) -> Action<'_> {
  action_io!(held(op.take()))
}

impl Cx<'_> {
  /// Submits `op`, which must not point at memory, and reports whether it
  /// succeeded.
  #[inline]
  fn status(&mut self, op: Op) -> Ticket<io::Result<()>> {
    self.submit(Some(op), owned, |_, done| done.status())
  }

  /// Submits `op`, which must not point at memory, and takes ownership of
  /// the resource it creates.
  ///
  /// # Safety
  ///
  /// A successful `op` must report a new descriptor or handle that nothing
  /// else owns.
  #[inline]
  unsafe fn created(&mut self, op: Op) -> Ticket<io::Result<Resource>> {
    self.submit(Some(op), owned, |_, done| {
      // SAFETY: the caller promised that a successful completion of this
      // operation carries a new owned resource.
      unsafe { Resource::from_completion(&done) }
    })
  }

  /// Goes through the driver without doing anything, then produces what
  /// `out` returns: for a request that is known to be invalid, so that its
  /// error still arrives the way a result does.
  #[inline]
  fn refused<Out: 'static>(
    &mut self,
    out: impl FnOnce() -> Out + 'static,
  ) -> Ticket<Out> {
    fn finish<F: FnOnce() -> Out, Out>(
      out: &mut Option<F>,
      _: Completion,
    ) -> Out {
      held(out.take())()
    }
    self.submit(Some(out), |_| action_io!(Op::Nop), finish)
  }
}

/// Lends the writable regions of `buf` to the backend, as its `R`.
#[inline]
fn lend_writable<B: IoBufMutVec, R>(
  buf: &mut B,
  regions: &mut Regions<R>,
  mut region: impl FnMut(NonNull<u8>, usize) -> R,
) {
  let mut writable = IoSlicesMut::new();
  buf.bufs_mut(&mut writable);
  regions.lend(writable.into_iter().map(|lent| {
    let len = lent.len();
    region(NonNull::from(lent).cast(), len)
  }));
}

/// Gives `buf` back with the bytes the backend wrote marked initialized.
/// `parts` is a lent region's pointer and length.
#[inline]
fn filled<B: IoBufMutVec, R>(
  mut buf: B,
  regions: &Regions<R>,
  done: &Completion,
  parts: impl Fn(&R) -> (*mut u8, usize),
) -> BufResult<usize, B> {
  let result = done.count().inspect(|&written| {
    let mut remaining = written;
    for (i, region) in regions.lent().iter().enumerate() {
      let (ptr, cap) = parts(region);
      // The backend filled the lent regions in order, so the first
      // `min(remaining, cap)` bytes of region `i` are initialized.
      buf.commit(i, Initialized::new(ptr, cap, remaining.min(cap)));
      remaining = remaining.saturating_sub(cap);
    }
  });
  (result, buf)
}

// ============================================================================
// Nop, Sleep, Interval
// ============================================================================

leaf!(
  /// Does nothing, through the driver.
  [] pub struct Nop() -> io::Result<()> = cx.nop
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn nop(&mut self) -> Ticket<io::Result<()>> {
    self.submit((), |_| action_io!(Op::Nop), |_, done| done.status())
  }
}

leaf!(
  /// Completes after a duration.
  [] pub struct Sleep(duration: Duration) -> io::Result<()> = cx.sleep
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn sleep(&mut self, duration: Duration) -> Ticket<io::Result<()>> {
    self.submit(
      duration,
      |duration| Action::Sleep(*duration),
      |_, done| {
        assert!(done.flags().contains(CompletionFlags::TIMER));
        if done.result() == 0 { Ok(()) } else { Err(done.os_error()) }
      },
    )
  }
}

/// Yields every time a period has passed, until it is dropped.
pub struct Interval {
  period: Duration,
  sleep: Sleep,
}

impl Interval {
  pub(crate) fn new(period: Duration) -> Self {
    Self { period, sleep: Sleep::new(period) }
  }

  pub fn period(&self) -> Duration {
    self.period
  }
}

impl OpModel for Interval {
  type Item = io::Result<()>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let tick = ready!(self.sleep.poll(cx));
    // The next period starts when this item has been delivered.
    self.sleep = Sleep::new(self.period);
    Step::Yield(tick)
  }
}

// ============================================================================
// Sockets
// ============================================================================

leaf!(
  /// Creates a socket.
  [] pub struct Socket(domain: SockDomain, ty: SockType, proto: SockProto)
    -> io::Result<Resource> = cx.socket
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn socket(
    &mut self,
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
  ) -> Ticket<io::Result<Resource>> {
    // SAFETY: a successful socket completion carries a new owned socket.
    unsafe { self.created(Op::Socket { domain, ty, proto }) }
  }
}

leaf!(
  /// Accepts a connection and its peer address.
  [] pub struct Accept(listener: Resource)
    -> io::Result<(Resource, SocketAddr)> = cx.accept
);

/// The address of a UNIX-domain socket: a filesystem path, or unnamed.
///
/// A client that connects without binding first is unnamed, so that is what
/// [`accept_unix`](crate::api::accept_unix) usually reports for the peer.
/// Linux abstract-namespace addresses are reported as unnamed too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnixAddr {
  path: Option<PathBuf>,
}

impl UnixAddr {
  /// The filesystem path, or `None` if the address is unnamed.
  pub fn as_pathname(&self) -> Option<&Path> {
    self.path.as_deref()
  }

  /// Whether the address is unnamed.
  pub fn is_unnamed(&self) -> bool {
    self.path.is_none()
  }

  pub(crate) fn from_buf(buf: &SocketAddrBuf) -> io::Result<Self> {
    match buf.family {
      SocketAddrFamily::Unix => {
        let len = (buf.unix_path_len as usize).min(buf.unix_path.len());
        let bytes = &buf.unix_path[..len];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let path =
          (end > 0).then(|| PathBuf::from(os_string_from_bytes(&bytes[..end])));
        Ok(Self { path })
      }
      SocketAddrFamily::Unspecified => Ok(Self { path: None }),
      SocketAddrFamily::Ipv4 | SocketAddrFamily::Ipv6 => Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "not a UNIX socket address; use `accept` for IP listeners",
      )),
    }
  }
}

leaf!(
  /// Accepts a connection on a Unix socket.
  [] pub struct AcceptUnix(listener: Resource)
    -> io::Result<(Resource, UnixAddr)> = cx.accept_unix
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn accept(
    &mut self,
    listener: Resource,
  ) -> Ticket<io::Result<(Resource, SocketAddr)>> {
    self.accept_from(listener, |peer| {
      socket_addr_from_buf(peer).map_err(|_| {
        io::Error::new(
          io::ErrorKind::Unsupported,
          "not an IP socket address; use `accept_unix` for UNIX listeners",
        )
      })
    })
  }

  #[inline]
  pub(crate) fn accept_unix(
    &mut self,
    listener: Resource,
  ) -> Ticket<io::Result<(Resource, UnixAddr)>> {
    self.accept_from(listener, UnixAddr::from_buf)
  }

  /// Accepts a connection, reading the peer's address with `peer`.
  fn accept_from<A: 'static>(
    &mut self,
    listener: Resource,
    peer: fn(&SocketAddrBuf) -> io::Result<A>,
  ) -> Ticket<io::Result<(Resource, A)>> {
    self.submit(
      (listener, SocketAddrBuf::unspecified(), peer),
      |(listener, addr, _)| {
        action_io!(Op::Accept {
          fd: listener.clone(),
          addr: NonNull::from(addr),
        })
      },
      |(_, addr, peer), done| {
        // SAFETY: a successful accept completion carries a new owned socket.
        let socket = unsafe { Resource::from_completion(&done) }?;
        peer(addr).map(|peer| (socket, peer))
      },
    )
  }
}

leaf!(
  /// Connects a socket.
  [] pub struct Connect(socket: Resource, addr: SocketAddrBuf)
    -> io::Result<()> = cx.connect
);

leaf!(
  /// Binds a socket to an address.
  [] pub struct Bind(socket: Resource, addr: SocketAddrBuf)
    -> io::Result<()> = cx.bind
);

leaf!(
  /// Marks a socket as listening.
  [] pub struct Listen(socket: Resource, backlog: i32)
    -> io::Result<()> = cx.listen
);

leaf!(
  /// Shuts down part of a connection.
  [] pub struct Shutdown(socket: Resource, how: ShutdownHow)
    -> io::Result<()> = cx.shutdown
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn connect(
    &mut self,
    socket: Resource,
    addr: SocketAddrBuf,
  ) -> Ticket<io::Result<()>> {
    self.status(Op::Connect { fd: socket, addr })
  }

  #[inline]
  pub(crate) fn bind(
    &mut self,
    socket: Resource,
    addr: SocketAddrBuf,
  ) -> Ticket<io::Result<()>> {
    self.status(Op::Bind { fd: socket, addr })
  }

  #[inline]
  pub(crate) fn listen(
    &mut self,
    socket: Resource,
    backlog: i32,
  ) -> Ticket<io::Result<()>> {
    self.status(Op::Listen { fd: socket, backlog })
  }

  #[inline]
  pub(crate) fn shutdown(
    &mut self,
    socket: Resource,
    how: ShutdownHow,
  ) -> Ticket<io::Result<()>> {
    self.status(Op::Shutdown { fd: socket, how })
  }
}

leaf!(
  /// Receives from a socket into an owned buffer.
  [B: IoBufMutVec] pub struct Recv<B>(
    socket: Resource,
    buf: B,
    flags: RecvFlags,
  ) -> BufResult<usize, B> = cx.recv
);

leaf!(
  /// Receives a datagram and the address it came from.
  [B: IoBufMutVec] pub struct RecvFrom<B>(
    socket: Resource,
    buf: B,
    flags: RecvFlags,
  ) -> (io::Result<usize>, B, Option<SocketAddr>) = cx.recv_from
);

/// The memory of a receive, besides the regions it lends: the buffer, and
/// where the sender's address goes.
struct Receiving<B> {
  socket: Resource,
  buf: Option<B>,
  flags: RecvFlags,
  from: Option<SocketAddrBuf>,
}

impl<B: IoBufMutVec> Receiving<B> {
  fn new(socket: Resource, buf: B, flags: RecvFlags, with_from: bool) -> Self {
    Self {
      socket,
      buf: Some(buf),
      flags,
      from: with_from.then(SocketAddrBuf::unspecified),
    }
  }

  fn recv<'a>(&'a mut self, regions: &'a mut Regions<MsgBufMut>) -> Action<'a> {
    lend_writable(held(self.buf.as_mut()), regions, |ptr, len| {
      // SAFETY: the region is this operation's buffer, which stays valid in
      // place until the result is read.
      unsafe { MsgBufMut::from_raw_parts(ptr, len) }
    });
    let regions = regions.lent_or(|| {
      // SAFETY: a zero-length region is never read or written.
      unsafe { MsgBufMut::from_raw_parts(NonNull::dangling(), 0) }
    });
    let msg = match self.from.as_mut() {
      Some(from) => MsgRecv::with_from(regions, NonNull::from(from)),
      None => MsgRecv::new(regions),
    };
    action_io!(Op::Recv { fd: self.socket.clone(), msg, flags: self.flags })
  }

  fn received(
    &mut self,
    regions: &Regions<MsgBufMut>,
    done: &Completion,
  ) -> BufResult<usize, B> {
    filled(held(self.buf.take()), regions, done, |region| {
      (region.ptr.as_ptr(), region.len)
    })
  }
}

impl Cx<'_> {
  #[inline]
  pub(crate) fn recv<B: IoBufMutVec>(
    &mut self,
    socket: Resource,
    buf: B,
    flags: RecvFlags,
  ) -> Ticket<BufResult<usize, B>> {
    self.submit_lending(
      Receiving::new(socket, buf, flags, false),
      Receiving::recv,
      |recv, regions, done| recv.received(regions, &done),
    )
  }

  #[inline]
  pub(crate) fn recv_from<B: IoBufMutVec>(
    &mut self,
    socket: Resource,
    buf: B,
    flags: RecvFlags,
  ) -> Ticket<(io::Result<usize>, B, Option<SocketAddr>)> {
    self.submit_lending(
      Receiving::new(socket, buf, flags, true),
      Receiving::recv,
      |recv, regions, done| {
        let (result, buf) = recv.received(regions, &done);
        let from = result
          .as_ref()
          .ok()
          .and(recv.from.as_ref())
          .and_then(|from| socket_addr_from_buf(from).ok());
        (result, buf, from)
      },
    )
  }
}

leaf!(
  /// Sends an owned buffer on a socket.
  [B: IoBufVec + std::marker::Send + Sync + 'static] pub struct Send<B>(
    socket: Resource,
    buf: B,
    to: Option<SocketAddr>,
    flags: SendFlags,
  ) -> BufResult<usize, B> = cx.send
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn send<B: IoBufVec + 'static>(
    &mut self,
    socket: Resource,
    buf: B,
    to: Option<SocketAddr>,
    flags: SendFlags,
  ) -> Ticket<BufResult<usize, B>> {
    self.submit_lending(
      (socket, Some(buf), to, flags),
      |(socket, buf, to, flags), regions: &mut Regions<MsgBuf>| {
        let buf = held(buf.as_ref());
        regions
          .lend((0..buf.buf_count()).map(|i| MsgBuf::from_slice(buf.buf(i))));
        let regions = regions.lent_or(|| {
          // SAFETY: a zero-length region is never read.
          unsafe { MsgBuf::from_raw_parts(NonNull::dangling(), 0) }
        });
        let msg = MsgSend::new(regions, *to);
        action_io!(Op::Send { fd: socket.clone(), msg, flags: *flags })
      },
      |(_, buf, ..), _, done| (done.count(), held(buf.take())),
    )
  }
}

// ============================================================================
// Files
// ============================================================================

leaf!(
  /// Reads into an owned buffer. Produces the byte count and the buffer.
  [B: IoBufMutVec] pub struct Read<B>(
    file: Resource,
    buf: B,
    offset: Option<u64>,
  ) -> BufResult<usize, B> = cx.read
);

impl Cx<'_> {
  /// Reads at `offset`, or from the current position when `None`.
  ///
  /// An offset past [`MAX_FILE_OFFSET`] submits no read and fails with
  /// `InvalidInput`.
  ///
  /// [`MAX_FILE_OFFSET`]: crate::backend::op::MAX_FILE_OFFSET
  #[inline]
  pub(crate) fn read<B: IoBufMutVec>(
    &mut self,
    file: Resource,
    buf: B,
    offset: Option<u64>,
  ) -> Ticket<BufResult<usize, B>> {
    if !is_valid_file_offset(offset) {
      return self.refused(move || (Err(offset_out_of_range_error()), buf));
    }
    self.submit_lending(
      (file, Some(buf), offset),
      |(file, buf, offset), regions: &mut Regions<RawBufMut<'static>>| {
        lend_writable(held(buf.as_mut()), regions, |ptr, len| {
          // SAFETY: the region is this operation's buffer, which stays
          // valid in place until the result is read.
          unsafe { RawBufMut::from_raw_parts(ptr.as_ptr(), len) }
        });
        action_io!(Op::Read {
          fd: file.clone(),
          iovecs: regions.as_ptr().cast(),
          iov_count: regions.count(),
          offset: *offset,
          flags: ReadFlags::empty(),
        })
      },
      |(_, buf, _), regions, done| {
        filled(held(buf.take()), regions, &done, |region| {
          (region.ptr, region.len)
        })
      },
    )
  }
}

leaf!(
  /// Writes an owned buffer. Produces the byte count and the buffer.
  [B: IoBufVec + std::marker::Send + Sync + 'static] pub struct Write<B>(
    file: Resource,
    buf: B,
    offset: Option<u64>,
  ) -> BufResult<usize, B> = cx.write
);

impl Cx<'_> {
  /// Writes at `offset`, or at the current position when `None`.
  ///
  /// An offset past [`MAX_FILE_OFFSET`] submits no write and fails with
  /// `InvalidInput`.
  ///
  /// [`MAX_FILE_OFFSET`]: crate::backend::op::MAX_FILE_OFFSET
  #[inline]
  pub(crate) fn write<B: IoBufVec + 'static>(
    &mut self,
    file: Resource,
    buf: B,
    offset: Option<u64>,
  ) -> Ticket<BufResult<usize, B>> {
    if !is_valid_file_offset(offset) {
      return self.refused(move || (Err(offset_out_of_range_error()), buf));
    }
    self.submit_lending(
      (file, Some(buf), offset),
      |(file, buf, offset), regions: &mut Regions<RawBuf<'static>>| {
        let buf = held(buf.as_ref());
        regions.lend((0..buf.buf_count()).map(|i| {
          let bytes = buf.buf(i);
          // SAFETY: the bytes are this operation's buffer, which stays
          // valid in place until the result is read.
          unsafe {
            RawBuf::from_raw_parts(bytes.as_ptr().cast_mut(), bytes.len())
          }
        }));
        action_io!(Op::Write {
          fd: file.clone(),
          iovecs: regions.as_ptr().cast(),
          iov_count: regions.count(),
          offset: *offset,
          flags: WriteFlags::empty(),
        })
      },
      |(_, buf, _), _, done| (done.count(), held(buf.take())),
    )
  }
}

leaf!(
  /// Flushes a file to disk.
  [] pub struct Fsync(file: Resource) -> io::Result<()> = cx.fsync
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn fsync(&mut self, file: Resource) -> Ticket<io::Result<()>> {
    self.status(Op::Fsync { fd: file })
  }
}

leaf!(
  /// Opens a file relative to a directory.
  [] pub struct OpenAt(
    dir: Resource,
    path: CString,
    flags: OpenFlags,
    mode: FileMode,
  ) -> io::Result<Resource> = cx.openat
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn openat(
    &mut self,
    dir: Resource,
    path: CString,
    flags: OpenFlags,
    mode: FileMode,
  ) -> Ticket<io::Result<Resource>> {
    let path = path_to_os_string(&path);
    // SAFETY: a successful open completion carries a new owned descriptor.
    unsafe { self.created(Op::OpenAt { dir_fd: dir, path, flags, mode }) }
  }
}

/// What a [`Stat`] looks at.
pub(crate) enum StatOf {
  Path {
    dir: Resource,
    path: CString,
    follow_symlinks: bool,
  },
  File(Resource),
  /// A path that could not be a path: fails with [`nul_path_error`].
  #[cfg_attr(not(feature = "high"), allow(dead_code))]
  InvalidPath,
}

leaf!(
  /// Reads a file's metadata.
  [] pub struct Stat(of: StatOf) -> io::Result<FileStat> = cx.stat
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn stat(&mut self, of: StatOf) -> Ticket<io::Result<FileStat>> {
    let target = match of {
      StatOf::Path { dir, path, follow_symlinks } => StatTarget::Path {
        dir_fd: dir,
        path: path_to_os_string(&path),
        follow_symlinks,
      },
      StatOf::File(fd) => StatTarget::Fd { fd },
      StatOf::InvalidPath => return self.refused(|| Err(nul_path_error())),
    };
    self.submit(
      (Some(target), FileStat::zeroed()),
      |(target, out)| {
        action_io!(Op::Stat {
          target: held(target.take()),
          out: NonNull::from(out),
        })
      },
      |(_, out), done| done.status().map(|()| *out),
    )
  }
}

leaf!(
  /// Reads directory entries into a buffer.
  [] pub struct ReadDir(dir: Resource, buf: ReadDirBuf)
    -> io::Result<ReadDirBuf> = cx.read_dir
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn read_dir(
    &mut self,
    dir: Resource,
    buf: ReadDirBuf,
  ) -> Ticket<io::Result<ReadDirBuf>> {
    self.submit(
      (dir, buf),
      |(dir, buf)| {
        action_io!(Op::ReadDir {
          fd: dir.clone(),
          raw_buf: NonNull::from(buf.raw.as_mut_slice()).cast(),
          raw_cap: buf.raw.len(),
          entries: NonNull::from(buf.entries.as_mut_slice()).cast(),
          entries_cap: buf.entries.len(),
          opaque: NonNull::from(&mut buf.opaque),
          opaque_drop: NonNull::from(&mut buf.opaque_drop),
          out: NonNull::from(&mut buf.result),
        })
      },
      |(_, buf), done| done.status().map(|()| std::mem::take(buf)),
    )
  }
}

leaf!(
  /// Removes a file or an empty directory.
  [] pub struct UnlinkAt(dir: Resource, path: CString, kind: UnlinkKind)
    -> io::Result<()> = cx.unlinkat
);

leaf!(
  /// Renames a file.
  [] pub struct RenameAt(
    old_dir: Resource,
    old_path: CString,
    new_dir: Resource,
    new_path: CString,
  ) -> io::Result<()> = cx.renameat
);

leaf!(
  /// Creates a directory.
  [] pub struct MkdirAt(dir: Resource, path: CString, mode: FileMode)
    -> io::Result<()> = cx.mkdirat
);

leaf!(
  /// Creates a hard or symbolic link.
  [] pub struct LinkAt(
    source_dir: Resource,
    source_path: CString,
    new_dir: Resource,
    new_path: CString,
    kind: LinkKind,
  ) -> io::Result<()> = cx.linkat
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn unlinkat(
    &mut self,
    dir: Resource,
    path: CString,
    kind: UnlinkKind,
  ) -> Ticket<io::Result<()>> {
    let path = path_to_os_string(&path);
    self.status(Op::UnlinkAt { dir_fd: dir, path, kind })
  }

  #[inline]
  pub(crate) fn renameat(
    &mut self,
    old_dir: Resource,
    old_path: CString,
    new_dir: Resource,
    new_path: CString,
  ) -> Ticket<io::Result<()>> {
    self.status(Op::RenameAt {
      old_dir_fd: old_dir,
      old_path: path_to_os_string(&old_path),
      new_dir_fd: new_dir,
      new_path: path_to_os_string(&new_path),
    })
  }

  #[inline]
  pub(crate) fn mkdirat(
    &mut self,
    dir: Resource,
    path: CString,
    mode: FileMode,
  ) -> Ticket<io::Result<()>> {
    let path = path_to_os_string(&path);
    self.status(Op::MkdirAt { dir_fd: dir, path, mode })
  }

  #[inline]
  pub(crate) fn linkat(
    &mut self,
    source_dir: Resource,
    source_path: CString,
    new_dir: Resource,
    new_path: CString,
    kind: LinkKind,
  ) -> Ticket<io::Result<()>> {
    self.status(Op::LinkAt {
      kind,
      source_dir_fd: source_dir,
      source_path: path_to_os_string(&source_path),
      new_dir_fd: new_dir,
      new_path: path_to_os_string(&new_path),
    })
  }
}

leaf!(
  /// Reads the target of a symbolic link into an owned buffer.
  [B: IoBufMutVec] pub struct ReadlinkAt<B>(dir: Resource, path: CString, buf: B)
    -> BufResult<usize, B> = cx.readlinkat
);

impl Cx<'_> {
  /// # Panics
  ///
  /// When the operation is submitted, if `buf` has no writable region.
  #[inline]
  pub(crate) fn readlinkat<B: IoBufMutVec>(
    &mut self,
    dir: Resource,
    path: CString,
    buf: B,
  ) -> Ticket<BufResult<usize, B>> {
    let path = path_to_os_string(&path);
    self.submit(
      (dir, Some(path), Some(buf), NonNull::<u8>::dangling(), 0usize),
      |(dir, path, buf, lent, lent_len)| {
        let mut regions = IoSlicesMut::new();
        held(buf.as_mut()).bufs_mut(&mut regions);
        let Some(region) = regions.into_iter().next() else {
          panic!("readlink needs a buffer with at least one region");
        };
        *lent_len = region.len();
        *lent = NonNull::from(region).cast();
        action_io!(Op::ReadlinkAt {
          dir_fd: dir.clone(),
          path: held(path.take()),
          buf: *lent,
          buf_len: *lent_len,
        })
      },
      |(_, _, buf, lent, lent_len), done| {
        let mut buf = held(buf.take());
        let result = done.count().inspect(|&len| {
          let len = len.min(*lent_len);
          // The backend wrote the first `len` bytes of the region it was
          // lent.
          buf.commit(0, Initialized::new(lent.as_ptr(), *lent_len, len));
        });
        (result, buf)
      },
    )
  }
}

leaf!(
  /// Reads the current working directory into an owned buffer.
  [B: IoBufMutVec] pub struct GetCwd<B>(buf: B) -> BufResult<usize, B> = cx.getcwd
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn getcwd<B: IoBufMutVec>(
    &mut self,
    buf: B,
  ) -> Ticket<BufResult<usize, B>> {
    self.submit(
      (Some(buf), OsString::new()),
      |(_, cwd)| action_io!(Op::GetCwd { out: NonNull::from(cwd) }),
      |(buf, cwd), done| {
        let mut buf = held(buf.take());
        let result = done.status().and_then(|()| {
          let bytes = cwd.as_encoded_bytes();
          let mut regions = IoSlicesMut::new();
          buf.bufs_mut(&mut regions);
          let first = regions.into_iter().next();
          match first {
            Some(region) if bytes.len() <= region.len() => {
              let ptr = region.as_mut_ptr().cast::<u8>();
              let region_len = region.len();
              for (dst, src) in region.iter_mut().zip(bytes) {
                dst.write(*src);
              }
              buf.commit(0, Initialized::new(ptr, region_len, bytes.len()));
              Ok(bytes.len())
            }
            _ => Err(io::Error::from_raw_os_error(
              crate::backend::native::BUFFER_TOO_SMALL,
            )),
          }
        });
        (result, buf)
      },
    )
  }
}

// ============================================================================
// Processes
// ============================================================================

leaf!(
  /// Starts a process.
  [] pub struct Spawn(
    program: CString,
    args: Vec<CString>,
    env: Option<Vec<CString>>,
  ) -> io::Result<Pid> = cx.spawn
);

impl Cx<'_> {
  #[inline]
  pub(crate) fn spawn(
    &mut self,
    program: CString,
    args: Vec<CString>,
    env: Option<Vec<CString>>,
  ) -> Ticket<io::Result<Pid>> {
    let spec = SpawnSpec {
      program: path_to_os_string(&program),
      args: args.iter().map(path_to_os_string).collect(),
      env: env.map(|vars| vars.iter().map(path_to_os_string).collect()),
    };
    self.submit(Some(Op::Spawn { spec }), owned, |_, done| {
      done.status().map(|()| Pid::from_raw(done.result() as i64))
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Lio;
  use crate::api::io::Io;
  use crate::backend::op::socket_addr_into_buf;
  use crate::backend::testing::{Checked, POISON, ScriptedBackend};
  use crate::model;
  use std::cell::RefCell;
  use std::rc::Rc;

  const EIO: isize = -5;

  type Item<M> = <M as OpModel>::Item;

  /// Runs the operation `make` builds twice, on its own through [`Io`] and
  /// polled as a model, and finishes each with `result`. Returns what each
  /// run submitted and produced, which must not depend on how it ran.
  fn run<M>(make: impl Fn() -> M, result: isize) -> Vec<(Op, Item<M>)>
  where
    M: OpModel + 'static,
    Item<M>: 'static,
  {
    let backend = ScriptedBackend::new();
    let lio = Lio::new_with_backend(Checked::new(backend.clone()), 1).unwrap();
    let items = Rc::new(RefCell::new(Vec::new()));
    let mut submitted = Vec::new();
    for polled in [false, true] {
      let sink = Rc::clone(&items);
      if polled {
        let _task =
          model::spawn(&lio, make(), move |item| sink.borrow_mut().push(item));
      } else {
        Io::from_op(make())
          .with_lio(&lio)
          .when_done(move |item| sink.borrow_mut().push(item));
      }
      lio.try_run().unwrap();
      let in_flight = backend.in_flight();
      assert_eq!(in_flight.len(), 1, "one operation, one submission");
      submitted.push(backend.op(in_flight[0]).unwrap());
      backend.complete(in_flight[0], result);
      lio.try_run().unwrap();
      assert!(!lio.is_full(), "the slot is free again");
    }
    let items = std::mem::take(&mut *items.borrow_mut());
    assert_eq!(items.len(), 2, "each run produced one item");
    submitted.into_iter().zip(items).collect()
  }

  fn file() -> Resource {
    Resource::stdin()
  }

  fn path(name: &str) -> CString {
    CString::new(name).unwrap()
  }

  fn addr() -> SocketAddr {
    "127.0.0.1:4000".parse().unwrap()
  }

  /// An operation that only reports success: `make` must submit what
  /// `submits` accepts, and turn a result into `Ok(())` or its OS error.
  fn reports_status<M>(make: impl Fn() -> M, submits: impl Fn(&Op) -> bool)
  where
    M: OpModel<Item = io::Result<()>> + 'static,
  {
    for (op, item) in run(&make, 0) {
      assert!(submits(&op), "unexpected submission: {op:?}");
      item.unwrap();
    }
    for (_, item) in run(&make, EIO) {
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }
  }

  #[test]
  fn operations_that_report_status_submit_their_op() {
    reports_status(Nop::new, |op| matches!(op, Op::Nop));
    reports_status(
      || Connect::new(file(), socket_addr_into_buf(addr())),
      |op| matches!(op, Op::Connect { addr: to, .. } if socket_addr_from_buf(to).ok() == Some(addr())),
    );
    reports_status(
      || Bind::new(file(), socket_addr_into_buf(addr())),
      |op| matches!(op, Op::Bind { addr: at, .. } if socket_addr_from_buf(at).ok() == Some(addr())),
    );
    reports_status(
      || Listen::new(file(), 7),
      |op| matches!(op, Op::Listen { backlog: 7, .. }),
    );
    reports_status(
      || Shutdown::new(file(), ShutdownHow::Write),
      |op| matches!(op, Op::Shutdown { how: ShutdownHow::Write, .. }),
    );
    reports_status(|| Fsync::new(file()), |op| matches!(op, Op::Fsync { .. }));
    reports_status(
      || UnlinkAt::new(file(), path("gone"), UnlinkKind::File),
      |op| matches!(op, Op::UnlinkAt { path, kind: UnlinkKind::File, .. } if path == "gone"),
    );
    reports_status(
      || RenameAt::new(file(), path("old"), file(), path("new")),
      |op| matches!(op, Op::RenameAt { old_path, new_path, .. } if old_path == "old" && new_path == "new"),
    );
    reports_status(
      || MkdirAt::new(file(), path("dir"), FileMode::empty()),
      |op| matches!(op, Op::MkdirAt { path, .. } if path == "dir"),
    );
    reports_status(
      || LinkAt::new(file(), path("from"), file(), path("to"), LinkKind::Soft),
      |op| matches!(op, Op::LinkAt { kind: LinkKind::Soft, source_path, new_path, .. } if source_path == "from" && new_path == "to"),
    );
  }

  #[test]
  fn socket_and_openat_own_the_resource_they_create() {
    let socket =
      || Socket::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP);
    for (op, item) in run(socket, 9) {
      assert!(matches!(op, Op::Socket { .. }));
      item.unwrap();
    }
    for (_, item) in run(socket, EIO) {
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }

    let open =
      || OpenAt::new(file(), path("f"), OpenFlags::READ, FileMode::empty());
    for (op, item) in run(open, 9) {
      assert!(matches!(op, Op::OpenAt { path, .. } if path == "f"));
      item.unwrap();
    }
    for (_, item) in run(open, EIO) {
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }
  }

  #[test]
  fn read_returns_the_buffer_with_only_the_written_bytes() {
    let read = || Read::new(file(), Vec::<u8>::with_capacity(8), Some(40));
    for (op, (result, buf)) in run(read, 3) {
      assert!(matches!(op, Op::Read { iov_count: 1, offset: Some(40), .. }));
      assert_eq!(result.unwrap(), 3);
      assert_eq!(buf, [POISON; 3]);
    }
    for (_, (result, buf)) in run(read, EIO) {
      assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
      assert!(buf.is_empty());
    }
  }

  #[test]
  fn read_fills_several_regions_in_order() {
    let read = || {
      let bufs = (Vec::<u8>::with_capacity(4), Vec::<u8>::with_capacity(4));
      Read::new(file(), bufs, None)
    };
    for (op, (result, (first, second))) in run(read, 6) {
      assert!(matches!(op, Op::Read { iov_count: 2, offset: None, .. }));
      assert_eq!(result.unwrap(), 6);
      assert_eq!((first.len(), second.len()), (4, 2));
    }
  }

  #[test]
  fn an_offset_the_os_cannot_take_submits_nothing_and_fails() {
    let past = Some(u64::MAX);
    for (op, (result, buf)) in
      run(|| Read::new(file(), Vec::<u8>::with_capacity(8), past), 0)
    {
      assert!(matches!(op, Op::Nop), "no read was submitted");
      assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
      assert!(buf.is_empty());
    }
    for (op, (result, buf)) in run(|| Write::new(file(), vec![1u8; 4], past), 0)
    {
      assert!(matches!(op, Op::Nop), "no write was submitted");
      assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
      assert_eq!(buf, [1; 4]);
    }
  }

  #[test]
  fn write_returns_the_count_and_the_buffer() {
    let write = || Write::new(file(), vec![1u8; 4], Some(2));
    for (op, (result, buf)) in run(write, 3) {
      assert!(matches!(op, Op::Write { iov_count: 1, offset: Some(2), .. }));
      assert_eq!(result.unwrap(), 3);
      assert_eq!(buf, [1; 4]);
    }
    for (_, (result, buf)) in run(write, EIO) {
      assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
      assert_eq!(buf, [1; 4]);
    }
    let two = || Write::new(file(), (vec![1u8; 2], vec![2u8; 2]), None);
    for (op, (result, _)) in run(two, 4) {
      assert!(matches!(op, Op::Write { iov_count: 2, .. }));
      assert_eq!(result.unwrap(), 4);
    }
  }

  #[test]
  fn recv_returns_the_buffer_with_only_the_received_bytes() {
    let recv =
      || Recv::new(file(), Vec::<u8>::with_capacity(8), RecvFlags::PEEK);
    for (op, (result, buf)) in run(recv, 3) {
      let Op::Recv { msg, flags, .. } = op else {
        panic!("recv must submit a recv");
      };
      assert_eq!(msg.buf_count.get(), 1);
      assert_eq!(flags, RecvFlags::PEEK);
      assert_eq!(result.unwrap(), 3);
      assert_eq!(buf, [POISON; 3]);
    }
    for (_, (result, buf)) in run(recv, EIO) {
      assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
      assert!(buf.is_empty());
    }
  }

  #[test]
  fn recv_from_reports_no_sender_when_none_was_written() {
    let recv =
      || RecvFrom::new(file(), Vec::<u8>::with_capacity(8), RecvFlags::empty());
    for (op, (result, buf, from)) in run(recv, 2) {
      assert!(matches!(op, Op::Recv { .. }));
      assert_eq!(result.unwrap(), 2);
      assert_eq!(buf.len(), 2);
      assert_eq!(from, None, "the scripted backend writes no address");
    }
    for (_, (result, _, from)) in run(recv, EIO) {
      assert!(result.is_err());
      assert_eq!(from, None);
    }
  }

  #[test]
  fn send_returns_the_count_and_the_buffer() {
    let send =
      || Send::new(file(), b"hello".to_vec(), Some(addr()), SendFlags::OOB);
    for (op, (result, buf)) in run(send, 5) {
      let Op::Send { msg, flags, .. } = op else {
        panic!("send must submit a send");
      };
      assert_eq!(msg.buf_count.get(), 1);
      assert_eq!(flags, SendFlags::OOB);
      assert_eq!(result.unwrap(), 5);
      assert_eq!(buf, b"hello");
    }
    for (_, (result, buf)) in run(send, EIO) {
      assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
      assert_eq!(buf, b"hello");
    }
  }

  #[test]
  fn empty_buffer_collections_submit_one_empty_region() {
    let send =
      || Send::new(file(), Vec::<Vec<u8>>::new(), None, SendFlags::empty());
    for (op, _) in run(send, 0) {
      let Op::Send { msg, .. } = op else { panic!("send must submit a send") };
      assert_eq!(msg.buf_count.get(), 1);
    }
    let recv = || Recv::new(file(), Vec::<Vec<u8>>::new(), RecvFlags::empty());
    for (op, (result, bufs)) in run(recv, 0) {
      let Op::Recv { msg, .. } = op else { panic!("recv must submit a recv") };
      assert_eq!(msg.buf_count.get(), 1);
      assert_eq!(result.unwrap(), 0);
      assert!(bufs.is_empty());
    }
  }

  #[test]
  fn accept_reads_the_peer_address_for_its_family() {
    // The scripted backend writes no address, which is what an unnamed UNIX
    // peer looks like and what no IP peer does.
    for (op, item) in run(|| AcceptUnix::new(file()), 9) {
      assert!(matches!(op, Op::Accept { .. }));
      let (_socket, peer) = item.unwrap();
      assert!(peer.is_unnamed());
    }
    for (op, item) in run(|| Accept::new(file()), 9) {
      assert!(matches!(op, Op::Accept { .. }));
      assert_eq!(item.unwrap_err().kind(), io::ErrorKind::Unsupported);
    }
    for (_, item) in run(|| Accept::new(file()), EIO) {
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }
  }

  #[test]
  fn unix_addresses_are_read_from_their_buffer() {
    let mut buf = SocketAddrBuf::unspecified();
    assert!(UnixAddr::from_buf(&buf).unwrap().is_unnamed());

    buf.family = SocketAddrFamily::Unix;
    buf.unix_path[..4].copy_from_slice(b"sock");
    buf.unix_path_len = 4;
    let named = UnixAddr::from_buf(&buf).unwrap();
    assert_eq!(named.as_pathname(), Some(Path::new("sock")));
    assert!(!named.is_unnamed());

    buf.unix_path_len = 0;
    assert!(UnixAddr::from_buf(&buf).unwrap().is_unnamed());

    let ip = socket_addr_into_buf(addr());
    let err = UnixAddr::from_buf(&ip).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
  }

  #[test]
  fn stat_looks_at_a_path_or_a_file() {
    let by_path = || {
      Stat::new(StatOf::Path {
        dir: file(),
        path: path("f"),
        follow_symlinks: true,
      })
    };
    for (op, item) in run(by_path, 0) {
      assert!(matches!(
        op,
        Op::Stat { target: StatTarget::Path { path, follow_symlinks: true, .. }, .. } if path == "f"
      ));
      item.unwrap();
    }
    for (op, item) in run(|| Stat::new(StatOf::File(file())), EIO) {
      assert!(matches!(op, Op::Stat { target: StatTarget::Fd { .. }, .. }));
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }
    for (op, item) in run(|| Stat::new(StatOf::InvalidPath), 0) {
      assert!(matches!(op, Op::Nop), "no stat was submitted");
      assert_eq!(item.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
  }

  #[test]
  fn read_dir_returns_its_buffer() {
    let read_dir = || ReadDir::new(file(), ReadDirBuf::default());
    for (op, item) in run(read_dir, 0) {
      assert!(matches!(op, Op::ReadDir { .. }));
      item.unwrap();
    }
    for (_, item) in run(read_dir, EIO) {
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }
  }

  #[test]
  fn readlinkat_returns_the_buffer_with_only_the_written_bytes() {
    let readlink =
      || ReadlinkAt::new(file(), path("link"), Vec::<u8>::with_capacity(8));
    for (op, (result, buf)) in run(readlink, 3) {
      assert!(
        matches!(op, Op::ReadlinkAt { path, buf_len: 8, .. } if path == "link")
      );
      assert_eq!(result.unwrap(), 3);
      assert_eq!(buf, [POISON; 3]);
    }
    for (_, (result, buf)) in run(readlink, EIO) {
      assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
      assert!(buf.is_empty());
    }
  }

  #[test]
  fn getcwd_copies_the_directory_into_the_buffer() {
    let getcwd = || GetCwd::new(Vec::<u8>::with_capacity(8));
    // The scripted backend writes no directory: an empty one fits.
    for (op, (result, buf)) in run(getcwd, 0) {
      assert!(matches!(op, Op::GetCwd { .. }));
      assert_eq!(result.unwrap(), 0);
      assert!(buf.is_empty());
    }
    for (_, (result, _)) in run(getcwd, EIO) {
      assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
    }
  }

  #[test]
  fn spawn_reports_the_process_it_started() {
    let spawn =
      || Spawn::new(path("/bin/true"), vec![path("true")], Some(vec![]));
    for (op, item) in run(spawn, 1234) {
      let Op::Spawn { spec } = op else { panic!("spawn must submit a spawn") };
      assert_eq!(spec.program, "/bin/true");
      assert_eq!(spec.args, ["true"]);
      assert_eq!(spec.env, Some(vec![]));
      assert_eq!(item.unwrap().as_raw(), 1234);
    }
    for (_, item) in run(spawn, EIO) {
      assert_eq!(item.unwrap_err().raw_os_error(), Some(5));
    }
  }

  #[test]
  fn sleep_and_interval_wait_on_the_driver_s_clock() {
    let backend = ScriptedBackend::new();
    let lio = Lio::new_with_backend(Checked::new(backend.clone()), 2).unwrap();
    crate::time::pause(&lio);
    let ticks = Rc::new(RefCell::new(Vec::new()));

    for polled in [false, true] {
      let sink = Rc::clone(&ticks);
      let sleep = Sleep::new(Duration::from_millis(5));
      if polled {
        let _task =
          model::spawn(&lio, sleep, move |tick| sink.borrow_mut().push(tick));
      } else {
        Io::from_op(sleep)
          .with_lio(&lio)
          .when_done(move |tick| sink.borrow_mut().push(tick));
      }
    }
    lio.try_run().unwrap();
    assert!(ticks.borrow().is_empty(), "no time has passed");
    assert!(backend.pushed().is_empty(), "a sleep is not backend work");
    lio.advance_time_by_ticks(10);
    lio.try_run().unwrap();
    assert_eq!(ticks.borrow().len(), 2);

    let sink = Rc::clone(&ticks);
    let interval = Interval::new(Duration::from_millis(5));
    assert_eq!(interval.period(), Duration::from_millis(5));
    let task =
      model::spawn(&lio, interval, move |tick| sink.borrow_mut().push(tick));
    for _ in 0..3 {
      lio.advance_time_by_ticks(10);
      lio.try_run().unwrap();
    }
    assert_eq!(ticks.borrow().len(), 5, "one tick per period");
    task.cancel();
    lio.advance_time_by_ticks(10);
    lio.try_run().unwrap();
    assert_eq!(ticks.borrow().len(), 5, "a cancelled interval stops");
    assert!(ticks.borrow().iter().all(|tick| tick.is_ok()));
    assert!(!lio.is_full());
  }
}
