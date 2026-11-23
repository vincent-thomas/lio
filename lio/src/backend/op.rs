//! Operation data enum for zero-box I/O operations.
//!
//! This module defines the [`Op`] enum which represents all I/O operations
//! as pure data. Backends match on this enum to execute operations.

use crate::api::resource::Resource;
use std::ffi::OsString;
use std::io;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockDomain(u8);

impl SockDomain {
  pub const IPV4: Self = Self(1);
  pub const IPV6: Self = Self(2);
  pub const UNIX: Self = Self(3);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockType(u8);

impl SockType {
  pub const STREAM: Self = Self(1);
  pub const DGRAM: Self = Self(2);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockProto(u8);

impl SockProto {
  pub const DEFAULT: Self = Self(0);
  pub const TCP: Self = Self(1);
  pub const UDP: Self = Self(2);
}

/// Defines a portable flag set: a newtype over private bits with named flags
/// that combine with `|`. Backends translate the flags to their native
/// encoding.
macro_rules! portable_flags {
  (
    $(#[$meta:meta])*
    pub struct $name:ident;
    $(
      $(#[$flag_meta:meta])*
      const $flag:ident = $bits:expr;
    )*
  ) => {
    $(#[$meta])*
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct $name(u32);

    impl $name {
      $(
        $(#[$flag_meta])*
        pub const $flag: Self = Self($bits);
      )*

      /// No flags; combine with others.
      pub const fn empty() -> Self {
        Self(0)
      }

      /// Whether no flag is set.
      pub const fn is_empty(self) -> bool {
        self.0 == 0
      }

      /// Whether every flag in `other` is set.
      pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
      }
    }

    impl std::ops::BitOr for $name {
      type Output = Self;

      fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
      }
    }

    impl std::ops::BitOrAssign for $name {
      fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
      }
    }
  };
}

/// The largest file offset a positioned [`Op::Read`] or [`Op::Write`] may
/// carry.
///
/// Kernel file offsets are signed (`off_t` on Unix, `LARGE_INTEGER` on
/// Windows), so a larger offset cannot be honoured. Op models never submit
/// one; a backend that receives one anyway completes the op with `EINVAL`
/// rather than truncating or wrapping it.
pub const MAX_FILE_OFFSET: u64 = i64::MAX as u64;

/// Whether a read or write `offset` is one a backend can honour: the current
/// position (`None`) or an absolute offset of at most [`MAX_FILE_OFFSET`].
pub const fn is_valid_file_offset(offset: Option<u64>) -> bool {
  match offset {
    Some(offset) => offset <= MAX_FILE_OFFSET,
    None => true,
  }
}

portable_flags! {
  /// How [`Op::Read`] reads. No read flags are portable yet, so this is
  /// always [`empty`](Self::empty).
  pub struct ReadFlags;
}

portable_flags! {
  /// How [`Op::Write`] writes. Combine flags with `|`.
  ///
  /// A backend that cannot honour a flag completes the write with
  /// `ENOTSUP` rather than ignoring it.
  pub struct WriteFlags;
  /// Make the written data durable before completing, as if followed by
  /// `fdatasync` (`RWF_DSYNC`).
  const DSYNC = 1 << 0;
  /// Make the written data and its metadata durable before completing, as
  /// if followed by `fsync` (`RWF_SYNC`).
  const SYNC = 1 << 1;
  /// Write at the end of the file, ignoring the offset (`RWF_APPEND`).
  const APPEND = 1 << 2;
}

portable_flags! {
  /// How [`Op::Recv`] receives. Combine flags with `|`.
  ///
  /// A backend that cannot honour a flag completes the receive with
  /// `ENOTSUP` rather than ignoring it.
  pub struct RecvFlags;
  /// Return the data without removing it from the receive queue
  /// (`MSG_PEEK`).
  const PEEK = 1 << 0;
  /// Receive out-of-band data (`MSG_OOB`).
  const OOB = 1 << 1;
}

portable_flags! {
  /// How [`Op::Send`] sends. Combine flags with `|`.
  ///
  /// Sending on a connection the peer has closed fails with `EPIPE`; no
  /// backend raises `SIGPIPE` (`MSG_NOSIGNAL` on Unix). A backend that cannot
  /// honour a flag completes the send with `ENOTSUP` rather than ignoring it.
  pub struct SendFlags;
  /// Send out-of-band data (`MSG_OOB`).
  const OOB = 1 << 0;
  /// Send only to directly connected hosts, bypassing routing
  /// (`MSG_DONTROUTE`).
  const DONTROUTE = 1 << 1;
}

portable_flags! {
  /// How [`Op::OpenAt`] opens a path. Combine flags with `|`.
  ///
  /// At least one of [`READ`](Self::READ), [`WRITE`](Self::WRITE) or
  /// [`APPEND`](Self::APPEND) is required; backends reject an open without an
  /// access mode as invalid input. Backends translate these to their native
  /// flags, and never let the opened resource leak into child processes
  /// (`O_CLOEXEC` on Unix).
  pub struct OpenFlags;
  /// Open for reading.
  const READ = 1 << 0;
  /// Open for writing.
  const WRITE = 1 << 1;
  /// Open for writing, with every write appended to the end (implies
  /// writing).
  const APPEND = 1 << 2;
  /// Create the file if it does not exist.
  const CREATE = 1 << 3;
  /// Truncate an existing file to zero length (requires writing).
  const TRUNCATE = 1 << 4;
  /// With [`CREATE`](Self::CREATE): fail if the file already exists.
  const EXCLUSIVE = 1 << 5;
  /// Fail unless the path is a directory.
  const DIRECTORY = 1 << 6;
}

impl OpenFlags {
  /// Whether the open reads.
  pub const fn reads(self) -> bool {
    self.contains(Self::READ)
  }

  /// Whether the open writes (`WRITE` or `APPEND`).
  pub const fn writes(self) -> bool {
    self.contains(Self::WRITE) || self.contains(Self::APPEND)
  }
}

portable_flags! {
  /// Permissions for a file or directory that [`Op::OpenAt`] or
  /// [`Op::MkdirAt`] creates. Combine flags with `|`, or write them in the
  /// familiar octal form with [`from_octal`](Self::from_octal).
  ///
  /// Backends translate these to their native permissions. Unix applies
  /// them as the `mode` argument, less the process umask. Windows has no
  /// permission bits: it ignores the mode.
  pub struct FileMode;
  /// The owner may read.
  const OWNER_READ = 0o400;
  /// The owner may write.
  const OWNER_WRITE = 0o200;
  /// The owner may execute (search, for a directory).
  const OWNER_EXECUTE = 0o100;
  /// The group may read.
  const GROUP_READ = 0o040;
  /// The group may write.
  const GROUP_WRITE = 0o020;
  /// The group may execute (search, for a directory).
  const GROUP_EXECUTE = 0o010;
  /// Others may read.
  const OTHER_READ = 0o004;
  /// Others may write.
  const OTHER_WRITE = 0o002;
  /// Others may execute (search, for a directory).
  const OTHER_EXECUTE = 0o001;
  /// Execute as the file's owner.
  const SET_USER_ID = 0o4000;
  /// Execute as the file's group; in a directory, new entries inherit its
  /// group.
  const SET_GROUP_ID = 0o2000;
  /// In a directory, only an entry's owner may remove or rename it.
  const STICKY = 0o1000;
}

impl FileMode {
  /// Every permission bit.
  const ALL: u32 = 0o7777;

  /// The mode written in POSIX octal notation, such as `0o644`.
  ///
  /// Returns `None` if `octal` sets anything but the twelve permission bits
  /// (`0o7777`), such as file-type bits.
  pub const fn from_octal(octal: u32) -> Option<Self> {
    if octal & !Self::ALL == 0 { Some(Self(octal)) } else { None }
  }

  /// The mode in POSIX octal notation, such as `0o644`.
  pub const fn octal(self) -> u32 {
    self.0
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlinkKind {
  File,
  Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownHow {
  Read,
  Write,
  Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
  Hard,
  Soft,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
  pub program: OsString,
  pub args: Vec<OsString>,
  pub env: Option<Vec<OsString>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketAddrFamily {
  Unspecified,
  Ipv4,
  Ipv6,
  Unix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketAddrBuf {
  pub family: SocketAddrFamily,
  pub port_be: u16,
  pub ip: [u8; 16],
  pub flowinfo: u32,
  pub scope_id: u32,
  pub unix_path_len: u16,
  pub unix_path: [u8; 108],
}

impl SocketAddrBuf {
  pub const fn unspecified() -> Self {
    Self {
      family: SocketAddrFamily::Unspecified,
      port_be: 0,
      ip: [0; 16],
      flowinfo: 0,
      scope_id: 0,
      unix_path_len: 0,
      unix_path: [0; 108],
    }
  }
}

pub fn unix_socket_addr_buf(path: &[u8]) -> io::Result<SocketAddrBuf> {
  if path.len() >= 108 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "Unix socket path exceeds 107 bytes",
    ));
  }
  let mut buf = SocketAddrBuf::unspecified();
  buf.family = SocketAddrFamily::Unix;
  buf.unix_path_len = path.len() as u16;
  buf.unix_path[..path.len()].copy_from_slice(path);
  Ok(buf)
}

pub fn socket_addr_into_buf(addr: SocketAddr) -> SocketAddrBuf {
  match addr {
    SocketAddr::V4(addr) => {
      let mut ip = [0; 16];
      ip[..4].copy_from_slice(&addr.ip().octets());
      SocketAddrBuf {
        family: SocketAddrFamily::Ipv4,
        port_be: addr.port().to_be(),
        ip,
        flowinfo: 0,
        scope_id: 0,
        unix_path_len: 0,
        unix_path: [0; 108],
      }
    }
    SocketAddr::V6(addr) => SocketAddrBuf {
      family: SocketAddrFamily::Ipv6,
      port_be: addr.port().to_be(),
      ip: addr.ip().octets(),
      flowinfo: addr.flowinfo(),
      scope_id: addr.scope_id(),
      unix_path_len: 0,
      unix_path: [0; 108],
    },
  }
}

pub fn socket_addr_from_buf(buf: &SocketAddrBuf) -> io::Result<SocketAddr> {
  match buf.family {
    SocketAddrFamily::Ipv4 => Ok(SocketAddr::V4(std::net::SocketAddrV4::new(
      std::net::Ipv4Addr::from([buf.ip[0], buf.ip[1], buf.ip[2], buf.ip[3]]),
      u16::from_be(buf.port_be),
    ))),
    SocketAddrFamily::Ipv6 => Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
      std::net::Ipv6Addr::from(buf.ip),
      u16::from_be(buf.port_be),
      buf.flowinfo,
      buf.scope_id,
    ))),
    SocketAddrFamily::Unspecified | SocketAddrFamily::Unix => Err(
      io::Error::new(io::ErrorKind::Unsupported, "not an IP socket address"),
    ),
  }
}

/// Returns whether `domain`, `ty` and `proto` describe a socket lio supports.
///
/// Unix sockets only accept [`SockProto::DEFAULT`], streams only TCP and
/// datagrams only UDP. Backends reject other combinations with `EINVAL`.
pub fn is_supported_socket(
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
) -> bool {
  match (domain, ty, proto) {
    (_, _, SockProto::DEFAULT) => true,
    (SockDomain::UNIX, _, _) => false,
    (_, SockType::STREAM, SockProto::TCP) => true,
    (_, SockType::DGRAM, SockProto::UDP) => true,
    _ => false,
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ErasedBuffer - Type-erased buffer storage
// ═══════════════════════════════════════════════════════════════════════════════

/// Non-owning span of initialized bytes a backend may read.
///
/// A write submits this. The bytes stay owned by the op model. This holds
/// only a pointer and a length so a backend can lower it without taking
/// ownership.
///
/// A read does not use this type. A shared `&[u8]` is not writable, so it
/// cannot be a read destination. Use [`RawBufMut`] for bytes the backend
/// fills.
#[derive(Clone, Copy, Debug)]
pub struct RawBuf<'a> {
  pub(crate) ptr: *mut u8,
  pub(crate) len: usize,
  _lt: PhantomData<&'a [u8]>,
}

impl<'a> RawBuf<'a> {
  /// A descriptor of `buf`. Valid for as long as `buf` is.
  #[inline]
  pub const fn new(buf: &'a [u8]) -> Self {
    Self { ptr: buf.as_ptr().cast_mut(), len: buf.len(), _lt: PhantomData }
  }

  /// A descriptor of initialized `buf`. The backend may read those bytes.
  #[inline]
  pub fn from_mut(buf: &'a mut [u8]) -> Self {
    Self { ptr: buf.as_mut_ptr(), len: buf.len(), _lt: PhantomData }
  }

  /// An empty descriptor. Nothing is read or written through it.
  #[inline]
  pub const fn empty() -> Self {
    Self { ptr: NonNull::<u8>::dangling().as_ptr(), len: 0, _lt: PhantomData }
  }

  /// The address of the bytes.
  #[inline]
  pub const fn as_ptr(self) -> *mut u8 {
    self.ptr
  }

  /// The number of bytes.
  #[inline]
  pub const fn len(self) -> usize {
    self.len
  }

  /// Whether the descriptor covers no bytes.
  #[inline]
  pub const fn is_empty(self) -> bool {
    self.len == 0
  }

  /// Creates a descriptor that outlives the borrow of its bytes.
  ///
  /// # Safety
  ///
  /// `ptr` must be valid for reads of `len` initialized bytes for as long as
  /// the descriptor is used, including after this call returns. A null `ptr`
  /// is only valid when `len` is 0.
  #[inline]
  pub(crate) const unsafe fn from_raw_parts(
    ptr: *mut u8,
    len: usize,
  ) -> RawBuf<'static> {
    RawBuf { ptr, len, _lt: PhantomData }
  }
}

// SAFETY: the descriptor is a pointer and a length. Sending it does not
// move the bytes. The owner of those bytes is `Send`.
unsafe impl Send for RawBuf<'_> {}
// SAFETY: sharing the descriptor does not alias the bytes by itself.
unsafe impl Sync for RawBuf<'_> {}

/// Non-owning span of bytes a backend may write.
///
/// A read submits this. The memory may be uninitialized. A shared `&[u8]`
/// cannot construct it, so a read cannot target read-only memory.
///
/// The descriptor is not `Copy` or `Clone`. Copying it would let two
/// operations write the same bytes. It is `Send` and not `Sync`, like
/// `&mut [u8]`: the owner may move it to another thread, and a shared
/// reference may not.
#[derive(Debug)]
pub struct RawBufMut<'a> {
  pub(crate) ptr: *mut u8,
  pub(crate) len: usize,
  _lt: PhantomData<&'a mut [u8]>,
}

impl<'a> RawBufMut<'a> {
  /// A descriptor of `buf`. The backend may overwrite those bytes.
  #[inline]
  pub fn from_mut(buf: &'a mut [u8]) -> Self {
    Self { ptr: buf.as_mut_ptr(), len: buf.len(), _lt: PhantomData }
  }

  /// A descriptor of an uninitialized region a read may fill.
  #[inline]
  pub fn from_uninit(buf: &'a mut [MaybeUninit<u8>]) -> Self {
    Self { ptr: buf.as_mut_ptr().cast(), len: buf.len(), _lt: PhantomData }
  }

  /// An empty descriptor. Nothing is written through it.
  #[inline]
  pub const fn empty() -> Self {
    Self { ptr: NonNull::<u8>::dangling().as_ptr(), len: 0, _lt: PhantomData }
  }

  /// The address of the bytes.
  #[inline]
  pub const fn as_ptr(&self) -> *mut u8 {
    self.ptr
  }

  /// The number of bytes.
  #[inline]
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Whether the descriptor covers no bytes.
  #[inline]
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// Creates a descriptor that outlives the borrow of its bytes.
  ///
  /// # Safety
  ///
  /// `ptr` must be valid for writes of `len` bytes for as long as the
  /// descriptor is used, including after this call returns. A null `ptr` is
  /// only valid when `len` is 0.
  #[inline]
  pub(crate) const unsafe fn from_raw_parts(
    ptr: *mut u8,
    len: usize,
  ) -> RawBufMut<'static> {
    RawBufMut { ptr, len, _lt: PhantomData }
  }
}

// SAFETY: `RawBufMut` is a unique borrow of the bytes, like `&mut [u8]`.
// Moving that borrow to another thread does not create an alias. A shared
// reference must not cross threads, so this type is not `Sync`.
unsafe impl Send for RawBufMut<'_> {}

/// Pointer and length shared by [`RawBuf`] and [`RawBufMut`].
pub(crate) trait ByteSpan {
  fn bytes_ptr(&self) -> *mut u8;
  fn bytes_len(&self) -> usize;
}

impl ByteSpan for RawBuf<'_> {
  fn bytes_ptr(&self) -> *mut u8 {
    self.ptr
  }
  fn bytes_len(&self) -> usize {
    self.len
  }
}

impl ByteSpan for RawBufMut<'_> {
  fn bytes_ptr(&self) -> *mut u8 {
    self.ptr
  }
  fn bytes_len(&self) -> usize {
    self.len
  }
}

#[derive(Clone, Copy, Debug)]
pub struct MsgBuf {
  pub(crate) ptr: NonNull<u8>,
  pub(crate) len: usize,
}

impl MsgBuf {
  #[inline]
  pub fn from_slice(buf: &[u8]) -> Self {
    Self { ptr: NonNull::from(buf).cast(), len: buf.len() }
  }

  #[inline]
  pub const fn as_ptr(self) -> *const u8 {
    self.ptr.as_ptr()
  }

  #[inline]
  pub const fn len(self) -> usize {
    self.len
  }

  #[inline]
  pub const fn is_empty(self) -> bool {
    self.len == 0
  }

  /// # Safety
  /// `ptr` must be non-null and valid for reads of `len` bytes for as long as
  /// the descriptor is used.
  #[inline]
  pub(crate) const unsafe fn from_raw_parts(
    ptr: NonNull<u8>,
    len: usize,
  ) -> Self {
    Self { ptr, len }
  }
}

// SAFETY: `MsgBuf` is a plain pointer/length pair describing external memory;
// sending or sharing it does not change aliasing guarantees by itself.
unsafe impl Send for MsgBuf {}
// SAFETY: `MsgBuf` carries no interior mutability and does not own the memory.
unsafe impl Sync for MsgBuf {}

#[derive(Clone, Copy, Debug)]
pub struct MsgBufMut {
  pub(crate) ptr: NonNull<u8>,
  pub(crate) len: usize,
}

impl MsgBufMut {
  #[inline]
  pub fn from_slice(buf: &mut [u8]) -> Self {
    Self { ptr: NonNull::from(&mut *buf).cast(), len: buf.len() }
  }

  #[inline]
  pub const fn as_ptr(self) -> *mut u8 {
    self.ptr.as_ptr()
  }

  #[inline]
  pub const fn len(self) -> usize {
    self.len
  }

  #[inline]
  pub const fn is_empty(self) -> bool {
    self.len == 0
  }

  #[inline]
  pub(crate) const fn empty() -> Self {
    Self { ptr: NonNull::dangling(), len: 0 }
  }

  /// # Safety
  /// `ptr` must be non-null and valid for writes of `len` bytes for as long as
  /// the descriptor is used.
  #[inline]
  pub(crate) const unsafe fn from_raw_parts(
    ptr: NonNull<u8>,
    len: usize,
  ) -> Self {
    Self { ptr, len }
  }
}

// SAFETY: `MsgBufMut` is just a pointer/length pair. Callers are responsible
// for ensuring exclusive access to the pointed-to memory for the operation lifetime.
unsafe impl Send for MsgBufMut {}
// SAFETY: sharing the descriptor value does not itself permit mutation without
// dereferencing the raw pointer.
unsafe impl Sync for MsgBufMut {}

#[derive(Clone, Copy, Debug)]
pub struct MsgSend {
  pub(crate) bufs: NonNull<MsgBuf>,
  pub(crate) buf_count: NonZeroUsize,
  pub to: Option<SocketAddr>,
}

impl MsgSend {
  /// # Panics
  /// If `bufs` is empty.
  #[inline]
  pub fn new(bufs: &[MsgBuf], to: Option<SocketAddr>) -> Self {
    let Some(buf_count) = NonZeroUsize::new(bufs.len()) else {
      panic!("MsgSend must contain at least one buffer");
    };
    let bufs = NonNull::from(bufs).cast();
    Self { bufs, buf_count, to }
  }
}

// SAFETY: `MsgSend` is metadata over caller-owned buffers and socket address data.
unsafe impl Send for MsgSend {}
// SAFETY: it contains no interior mutability beyond raw pointers.
unsafe impl Sync for MsgSend {}

#[derive(Clone, Copy, Debug)]
pub struct MsgRecv {
  pub(crate) bufs: NonNull<MsgBufMut>,
  pub(crate) buf_count: NonZeroUsize,
  pub(crate) from: Option<NonNull<SocketAddrBuf>>,
}

impl MsgRecv {
  /// # Panics
  /// If `bufs` is empty.
  #[inline]
  pub fn new(bufs: &[MsgBufMut]) -> Self {
    let Some(buf_count) = NonZeroUsize::new(bufs.len()) else {
      panic!("MsgRecv must contain at least one buffer");
    };
    let bufs = NonNull::from(bufs).cast();
    Self { bufs, buf_count, from: None }
  }

  #[inline]
  pub fn with_from(bufs: &[MsgBufMut], from: NonNull<SocketAddrBuf>) -> Self {
    let mut msg = Self::new(bufs);
    msg.from = Some(from);
    msg
  }
}

// SAFETY: `MsgRecv` is metadata over caller-owned mutable buffers.
unsafe impl Send for MsgRecv {}
// SAFETY: sharing the descriptor does not by itself dereference or mutate buffers.
unsafe impl Sync for MsgRecv {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
  Unknown,
  File,
  Directory,
  Symlink,
  BlockDevice,
  CharDevice,
  Fifo,
  Socket,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirEntryRef {
  pub name_offset: u32,
  pub name_len: u16,
  pub file_type: Option<FileType>,
  pub ino: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadDirResult {
  /// Number of valid `DirEntryRef`s written to the front of
  /// `ReadDirBuf::entries`.
  pub entries: usize,
  /// Number of bytes written into the provided raw buffer for this batch.
  pub raw_written: usize,
  /// True only when the backend knows the directory stream is exhausted.
  pub eof: bool,
}

pub type OpaqueDropFn = unsafe fn(*mut ());

#[derive(Debug, Default)]
pub struct ReadDirBuf {
  pub raw: Vec<u8>,
  pub entries: Vec<DirEntryRef>,
  pub result: ReadDirResult,
  pub(crate) opaque: *mut (),
  pub(crate) opaque_drop: Option<OpaqueDropFn>,
}

// SAFETY: The opaque pointer is backend-managed continuation state. `ReadDirBuf`
// only transports that opaque value between calls; dereferencing and meaning
// remain backend-specific.
unsafe impl Send for ReadDirBuf {}
// SAFETY: Shared access does not by itself dereference or mutate the opaque
// pointer. Backends are responsible for its interpretation.
unsafe impl Sync for ReadDirBuf {}

impl Drop for ReadDirBuf {
  fn drop(&mut self) {
    if self.opaque.is_null() {
      return;
    }
    if let Some(drop_fn) = self.opaque_drop {
      // SAFETY: `opaque` and `opaque_drop` are paired backend-owned continuation
      // state set by the backend that created this `ReadDirBuf`.
      unsafe {
        drop_fn(self.opaque);
      }
    }
    self.opaque = std::ptr::null_mut();
    self.opaque_drop = None;
  }
}

impl ReadDirBuf {
  pub fn with_capacity(scratch_bytes: usize, entries_cap: usize) -> Self {
    Self {
      raw: vec![0; scratch_bytes],
      entries: vec![DirEntryRef::default(); entries_cap],
      result: ReadDirResult::default(),
      opaque: std::ptr::null_mut(),
      opaque_drop: None,
    }
  }

  pub fn iter(&self) -> ReadDirIter<'_> {
    ReadDirIter {
      raw: &self.raw[..self.result.raw_written],
      iter: self.entries[..self.result.entries].iter(),
    }
  }
}

pub struct DirEntryView<'a> {
  pub name: &'a [u8],
  pub file_type: Option<FileType>,
  pub ino: Option<u64>,
}

pub struct ReadDirIter<'a> {
  raw: &'a [u8],
  iter: std::slice::Iter<'a, DirEntryRef>,
}

impl<'a> Iterator for ReadDirIter<'a> {
  type Item = DirEntryView<'a>;

  fn next(&mut self) -> Option<Self::Item> {
    let entry = self.iter.next()?;
    let start = entry.name_offset as usize;
    let end = start + entry.name_len as usize;
    Some(DirEntryView {
      name: &self.raw[start..end],
      file_type: entry.file_type,
      ino: entry.ino,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStat {
  pub file_type: FileType,
  pub size: u64,
  pub permissions: u32,
  pub mode: u32,
  pub nlink: u64,
  pub uid: u32,
  pub gid: u32,
}

impl FileStat {
  pub const fn zeroed() -> Self {
    Self {
      file_type: FileType::Unknown,
      size: 0,
      permissions: 0,
      mode: 0,
      nlink: 0,
      uid: 0,
      gid: 0,
    }
  }

  pub fn is_file(&self) -> bool {
    matches!(self.file_type, FileType::File)
  }

  pub fn is_dir(&self) -> bool {
    matches!(self.file_type, FileType::Directory)
  }

  pub fn is_symlink(&self) -> bool {
    matches!(self.file_type, FileType::Symlink)
  }

  pub fn len(&self) -> u64 {
    self.size
  }

  pub fn is_empty(&self) -> bool {
    self.size == 0
  }
}

#[derive(Clone, Debug)]
pub enum StatTarget {
  Path { dir_fd: Resource, path: OsString, follow_symlinks: bool },
  Fd { fd: Resource },
}

/// All I/O operations as pure data.
///
/// Each variant contains only the data needed to execute the operation.
/// Backends match on this enum to create submission entries or execute syscalls.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum Op {
  // ═══════════════════════════════════════════════════════════════════════════════
  // Buffer operations - RawBuf (ptr/len) points to TypedOp's buffer
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Generic read operation supporting all read variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Read (readv): offset == None
  /// - Positioned read (preadv): offset == Some(_)
  /// - With flags (preadv2): Any above + flags != 0
  ///
  /// The buffer/iovec pointers are owned by the OpModel and must remain valid.
  Read {
    /// File descriptor to read from
    fd: Resource,
    /// Scatter buffers. Each one is writable.
    iovecs: NonNull<RawBufMut<'static>>,
    /// Number of iovecs
    iov_count: usize,
    /// Absolute file offset, or `None` for the current file position.
    ///
    /// `Some(offset)` is at most [`MAX_FILE_OFFSET`]; backends complete a
    /// larger one with `EINVAL`.
    offset: Option<u64>,
    /// Portable read flags.
    flags: ReadFlags,
  },

  /// Generic write operation supporting all write variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Write (writev): offset == None
  /// - Positioned write (pwritev): offset == Some(_)
  /// - With flags (pwritev2): Any above + flags != 0
  ///
  /// The buffer/iovec pointers are owned by the OpModel and must remain valid.
  Write {
    /// File descriptor to write to
    fd: Resource,
    /// Gather buffers
    iovecs: NonNull<RawBuf<'static>>,
    /// Number of iovecs
    iov_count: usize,
    /// Absolute file offset, or `None` for the current file position.
    ///
    /// `Some(offset)` is at most [`MAX_FILE_OFFSET`]; backends complete a
    /// larger one with `EINVAL`.
    offset: Option<u64>,
    /// Portable write flags.
    flags: WriteFlags,
  },

  /// Generic receive operation supporting all recv variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple recv: buf.ptr != null, iovecs == null, addr == null, msg == null
  /// - Receive with address (recvfrom): addr != null
  /// - Vectored receive: iovecs != null
  /// - Full control (recvmsg): msg != null (all other fields ignored)
  ///
  /// The buffer/iovec/msg pointers are owned by the OpModel and must remain valid.
  Recv {
    /// Socket to receive from
    fd: Resource,
    /// Portable message descriptor lowered by the backend.
    msg: MsgRecv,
    /// Portable receive flags.
    flags: RecvFlags,
  },

  /// Generic send operation supporting all send variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple send: buf.ptr != null, iovecs == null, addr == null, msg == null
  /// - Send to address (sendto): addr != null
  /// - Vectored send: iovecs != null
  /// - Full control (sendmsg): msg != null (all other fields ignored)
  ///
  /// The buffer/iovec/msg pointers are owned by the OpModel and must remain valid.
  Send {
    /// Socket to send to
    fd: Resource,
    /// Portable message descriptor lowered by the backend.
    msg: MsgSend,
    /// Portable send flags.
    flags: SendFlags,
  },

  /// Read metadata for either a path lookup or an already-open file descriptor.
  Stat {
    /// Stat target, either a path-based lookup or direct fd query.
    target: StatTarget,
    /// Portable metadata output storage.
    out: NonNull<FileStat>,
  },
  /// Read directory entries from an open directory resource.
  ///
  /// The backend fills `raw_buf` with opaque raw directory data, writes parsed
  /// portable entry refs into `entries`, and stores batch metadata in `out`.
  /// Entries for `"."` and `".."` are omitted.
  ///
  /// Repeated calls on the same directory fd continue from that fd's native
  /// directory-stream position.
  ReadDir {
    /// Open directory resource.
    fd: Resource,
    /// Caller-owned opaque scratch memory.
    raw_buf: NonNull<u8>,
    /// Size of `raw_buf`.
    raw_cap: usize,
    /// Caller-owned parsed entry refs.
    entries: NonNull<DirEntryRef>,
    /// Max number of entry refs the backend may write.
    entries_cap: usize,
    /// Caller-owned opaque backend state slot used across repeated calls.
    opaque: NonNull<*mut ()>,
    /// Caller-owned opaque destructor slot for backend state.
    opaque_drop: NonNull<Option<OpaqueDropFn>>,
    /// Batch result metadata.
    out: NonNull<ReadDirResult>,
  },

  // ═══════════════════════════════════════════════════════════════════════════════
  // Socket operations
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Accept one incoming connection from a listening socket.
  ///
  /// Backends typically execute this with `accept(2)` or an equivalent platform
  /// primitive once the socket becomes readable.
  ///
  /// `addr` and `len` follow the normal `accept` contract:
  /// - if `addr` is non-null, the peer address is written there
  /// - `len` must point to the size of the storage on input
  /// - on success, `*len` is updated to the actual peer address length
  ///
  /// The address storage pointers are owned by the `OpModel` and must remain
  /// valid until the operation completes.
  Accept {
    /// Listening socket to accept from.
    fd: Resource,
    /// Peer address output storage.
    addr: NonNull<SocketAddrBuf>,
  },
  /// Initiate a connection on a socket.
  ///
  /// Backends typically call `connect(2)` immediately and either:
  /// - complete the op at once if the connection succeeds or fails immediately
  /// - keep it pending until the socket becomes writable, then check the final
  ///   connection result
  ///
  /// `addr` points to the destination socket address and must remain valid until
  /// the operation completes.
  Connect {
    /// Socket to connect.
    fd: Resource,
    /// Destination socket address.
    addr: SocketAddrBuf,
  },
  /// Open a path relative to a directory file descriptor.
  ///
  /// This is an immediate operation on readiness backends: it should execute
  /// during `flush()` and surface its completion on the next `wait()`.
  OpenAt {
    /// Directory file descriptor used as the base for relative paths.
    dir_fd: Resource,
    /// Platform-native path.
    path: OsString,
    /// Semantic open flags.
    flags: OpenFlags,
    /// Permissions for a file the open creates.
    mode: FileMode,
  },
  /// Remove a file or directory relative to a directory file descriptor.
  ///
  /// This is an immediate operation on readiness backends: it should execute
  /// during `flush()` and surface its completion on the next `wait()`.
  UnlinkAt {
    /// Directory file descriptor used as the base for relative paths.
    dir_fd: Resource,
    /// Platform-native path.
    path: OsString,
    /// Whether to remove a file-like object or directory.
    kind: UnlinkKind,
  },
  /// Rename a file or directory relative to directory file descriptors.
  ///
  /// This is an immediate operation on readiness backends: it should execute
  /// during `flush()` and surface its completion on the next `wait()`.
  RenameAt {
    /// Directory file descriptor used as the base for the old path.
    old_dir_fd: Resource,
    /// Null-terminated old pathname pointer.
    old_path: OsString,
    /// Directory file descriptor used as the base for the new path.
    new_dir_fd: Resource,
    /// Null-terminated new pathname pointer.
    new_path: OsString,
  },
  /// Create a directory relative to a directory file descriptor.
  ///
  /// This is an immediate operation on readiness backends: it should execute
  /// during `flush()` and surface its completion on the next `wait()`.
  MkdirAt {
    /// Directory file descriptor used as the base for relative paths.
    dir_fd: Resource,
    /// Platform-native path.
    path: OsString,
    /// Permissions for the new directory.
    mode: FileMode,
  },
  /// Create a hard or symbolic link relative to directory file descriptors.
  ///
  /// For symbolic links, `source_dir_fd` is ignored on Unix and `source_path`
  /// is used as the target string for the new link.
  LinkAt {
    /// Link flavor to create.
    kind: LinkKind,
    /// Directory file descriptor used as the base for the source path.
    source_dir_fd: Resource,
    /// Null-terminated source pathname pointer or symlink target string.
    source_path: OsString,
    /// Directory file descriptor used as the base for the new path.
    new_dir_fd: Resource,
    /// Null-terminated new pathname pointer.
    new_path: OsString,
  },
  /// Read the target of a symbolic link relative to a directory file descriptor.
  ///
  /// This is an immediate operation on readiness backends. On io_uring it is
  /// completed through a userspace immediate syscall path because there is no
  /// native opcode in the bundled `lio-uring` surface.
  ReadlinkAt {
    /// Directory file descriptor used as the base for relative paths.
    dir_fd: Resource,
    /// Null-terminated pathname pointer.
    path: OsString,
    /// Output buffer pointer.
    buf: NonNull<u8>,
    /// Output buffer length.
    buf_len: usize,
  },
  /// Read the current working directory as a platform-native string.
  GetCwd {
    /// Output storage for the platform-native current working directory.
    out: NonNull<OsString>,
  },
  /// Spawn a new process.
  ///
  /// The operation carries platform-neutral process data. Backends are
  /// responsible for converting it to native process creation arguments.
  Spawn {
    spec: SpawnSpec,
  },
  /// Create a new socket descriptor.
  ///
  /// This is an immediate operation on readiness backends: it should execute
  /// during `flush()` and surface its completion on the next `wait()`.
  Socket {
    /// Cross-platform semantic socket domain.
    domain: SockDomain,
    /// Cross-platform semantic socket type.
    ty: SockType,
    /// Cross-platform semantic socket protocol.
    proto: SockProto,
  },
  /// Bind a socket to a local address.
  ///
  /// This is an immediate operation on readiness backends: it should execute
  /// during `flush()` and surface its completion on the next `wait()`.
  Bind {
    /// Socket resource to bind.
    fd: Resource,
    /// Local socket address (IP or Unix path).
    addr: SocketAddrBuf,
  },
  Listen {
    /// Socket resource to mark listening.
    fd: Resource,
    /// Backlog depth passed to listen(2).
    backlog: i32,
  },
  Shutdown {
    /// Socket resource to shut down.
    fd: Resource,
    /// Which side(s) of the socket to shut down.
    how: ShutdownHow,
  },
  /// Synchronize a file's in-core state with stable storage.
  ///
  /// This is an immediate operation on readiness backends and a plain submit
  /// on completion-based backends.
  Fsync {
    /// Resource to synchronize.
    fd: Resource,
  },
  Nop,
}

/// An [`Op`] whose buffers are borrowed for `'a`.
///
/// Constructing one does not require `unsafe`. The addresses stay valid for
/// `'a`. Passing it to [`IoBackend::push`](crate::backend::IoBackend::push)
/// is `unsafe` because the backend may use those addresses after `push`
/// returns; the caller promises they outlive the submission.
#[derive(Clone, Debug)]
pub struct BorrowedOp<'a> {
  op: Op,
  _lt: PhantomData<&'a ()>,
}

fn descriptor_list<T>(bufs: &[T]) -> (NonNull<T>, usize) {
  let count = bufs.len();
  let ptr =
    if count == 0 { NonNull::dangling() } else { NonNull::from(bufs).cast() };
  (ptr, count)
}

/// Scratch a [`BorrowedOp::recv_buf`] or [`BorrowedOp::recv_from_buf`] fills
/// with descriptors of the buffer being received into.
pub struct RecvSlots {
  bufs: [MsgBufMut; crate::buf::MAX_IOV_COUNT],
  count: usize,
}

impl RecvSlots {
  /// Empty scratch. [`BorrowedOp::recv_buf`] fills it.
  pub fn new() -> Self {
    Self { bufs: [MsgBufMut::empty(); crate::buf::MAX_IOV_COUNT], count: 0 }
  }
}

impl Default for RecvSlots {
  fn default() -> Self {
    Self::new()
  }
}

/// Scratch a [`BorrowedOp::send_buf`] fills with descriptors of the bytes
/// being sent.
pub struct SendSlots {
  bufs: [MsgBuf; crate::buf::MAX_IOV_COUNT],
  count: usize,
}

impl SendSlots {
  /// Empty scratch. [`BorrowedOp::send_buf`] fills it.
  pub fn new() -> Self {
    // SAFETY: length 0 means the dangling address is never read. `send_buf`
    // overwrites every descriptor it submits.
    let empty = unsafe { MsgBuf::from_raw_parts(NonNull::dangling(), 0) };
    Self { bufs: [empty; crate::buf::MAX_IOV_COUNT], count: 0 }
  }
}

impl Default for SendSlots {
  fn default() -> Self {
    Self::new()
  }
}

fn branded<'b>(op: Op) -> BorrowedOp<'b> {
  BorrowedOp { op, _lt: PhantomData }
}

impl<'a> BorrowedOp<'a> {
  /// The operation a backend matches on.
  ///
  /// Cloning this [`Op`] copies addresses and drops the `'a` brand. Keeping
  /// the copy, and dereferencing it after `'a` ends, is the
  /// [`IoBackend::push`](crate::backend::IoBackend::push) contract.
  pub fn as_op(&self) -> &Op {
    &self.op
  }

  /// Brands `op` as `'static`.
  ///
  /// # Safety
  ///
  /// Every address in `op` must remain valid for as long as the result is
  /// used. This does not check that.
  pub(crate) unsafe fn from_op(op: Op) -> BorrowedOp<'static> {
    BorrowedOp { op, _lt: PhantomData }
  }

  /// A no-op. It borrows nothing.
  pub fn nop() -> BorrowedOp<'static> {
    branded(Op::Nop)
  }

  /// Reads into `iovecs`.
  ///
  /// Each descriptor is a [`RawBufMut`]: the backend may write those bytes.
  /// A shared slice cannot build one.
  pub fn read(
    fd: Resource,
    iovecs: &'a [RawBufMut<'a>],
    offset: Option<u64>,
    flags: ReadFlags,
  ) -> Self {
    let (iovecs, iov_count) = descriptor_list(iovecs);
    let iovecs = iovecs.cast::<RawBufMut<'static>>();
    branded(Op::Read { fd, iovecs, iov_count, offset, flags })
  }

  /// Writes `iovecs`.
  ///
  /// Each descriptor is a [`RawBuf`]: the bytes are initialized, and the
  /// backend may read them. An uninitialized buffer cannot build one.
  pub fn write(
    fd: Resource,
    iovecs: &'a [RawBuf<'a>],
    offset: Option<u64>,
    flags: WriteFlags,
  ) -> Self {
    let (iovecs, iov_count) = descriptor_list(iovecs);
    let iovecs = iovecs.cast::<RawBuf<'static>>();
    branded(Op::Write { fd, iovecs, iov_count, offset, flags })
  }

  /// Receives into `buf`, recording the descriptors in `slots`.
  ///
  /// # Panics
  ///
  /// Panics if `buf` exposes no regions. A message needs at least one
  /// descriptor.
  pub fn recv_buf<B: crate::buf::IoBufMutVec>(
    fd: Resource,
    buf: &'a mut B,
    slots: &'a mut RecvSlots,
    flags: RecvFlags,
  ) -> Self {
    fill_recv_slots(buf, slots);
    let msg = MsgRecv::new(&slots.bufs[..slots.count]);
    branded(Op::Recv { fd, msg, flags })
  }

  /// Receives into `buf` and writes the sender address to `from`.
  ///
  /// # Panics
  ///
  /// As [`recv_buf`](Self::recv_buf).
  pub fn recv_from_buf<B: crate::buf::IoBufMutVec>(
    fd: Resource,
    buf: &'a mut B,
    slots: &'a mut RecvSlots,
    from: &'a mut SocketAddrBuf,
    flags: RecvFlags,
  ) -> Self {
    fill_recv_slots(buf, slots);
    let msg =
      MsgRecv::with_from(&slots.bufs[..slots.count], NonNull::from(from));
    branded(Op::Recv { fd, msg, flags })
  }

  /// Receives into `bufs`. Each descriptor borrows the bytes it names.
  ///
  /// # Panics
  ///
  /// Panics if `bufs` is empty.
  pub fn recv(
    fd: Resource,
    bufs: &'a [BorrowedMsgBufMut<'a>],
    flags: RecvFlags,
  ) -> Self {
    branded(Op::Recv { fd, msg: MsgRecv::new(as_msg_buf_mut(bufs)), flags })
  }

  /// Sends `buf`, recording the descriptors in `slots`.
  ///
  /// # Panics
  ///
  /// Panics if `buf` exposes no regions.
  pub fn send_buf<B: crate::buf::IoBufVec>(
    fd: Resource,
    buf: &'a B,
    slots: &'a mut SendSlots,
    to: Option<SocketAddr>,
    flags: SendFlags,
  ) -> Self {
    fill_send_slots(buf, slots);
    let msg = MsgSend::new(&slots.bufs[..slots.count], to);
    branded(Op::Send { fd, msg, flags })
  }

  /// Sends `bufs`. Each descriptor borrows the bytes it names.
  ///
  /// # Panics
  ///
  /// Panics if `bufs` is empty.
  pub fn send(
    fd: Resource,
    bufs: &'a [BorrowedMsgBuf<'a>],
    to: Option<SocketAddr>,
    flags: SendFlags,
  ) -> Self {
    branded(Op::Send { fd, msg: MsgSend::new(as_msg_buf(bufs), to), flags })
  }

  /// Stats `target`, writing the result to `out`.
  pub fn stat(target: StatTarget, out: &'a mut FileStat) -> Self {
    branded(Op::Stat { target, out: NonNull::from(out) })
  }

  /// Reads directory entries.
  ///
  /// `raw`, `entries` and `result` are the buffers the backend fills.
  /// `opaque` and `opaque_drop` are continuation slots the backend may set;
  /// [`ReadDirBuf`] owns that pair when the higher-level API is used.
  pub fn read_dir(
    fd: Resource,
    raw: &'a mut [u8],
    entries: &'a mut [DirEntryRef],
    result: &'a mut ReadDirResult,
    opaque: &'a mut *mut (),
    opaque_drop: &'a mut Option<OpaqueDropFn>,
  ) -> Self {
    branded(Op::ReadDir {
      fd,
      raw_buf: nonnull_mut(raw),
      raw_cap: raw.len(),
      entries: nonnull_mut(entries),
      entries_cap: entries.len(),
      opaque: NonNull::from(opaque),
      opaque_drop: NonNull::from(opaque_drop),
      out: NonNull::from(result),
    })
  }

  /// Accepts a connection, writing the peer address to `addr`.
  pub fn accept(fd: Resource, addr: &'a mut SocketAddrBuf) -> Self {
    branded(Op::Accept { fd, addr: NonNull::from(addr) })
  }

  /// Connects `fd` to `addr`.
  pub fn connect(fd: Resource, addr: SocketAddrBuf) -> BorrowedOp<'static> {
    branded(Op::Connect { fd, addr })
  }

  /// Opens `path` relative to `dir_fd`.
  pub fn open_at(
    dir_fd: Resource,
    path: OsString,
    flags: OpenFlags,
    mode: FileMode,
  ) -> BorrowedOp<'static> {
    branded(Op::OpenAt { dir_fd, path, flags, mode })
  }

  /// Unlinks `path` relative to `dir_fd`.
  pub fn unlink_at(
    dir_fd: Resource,
    path: OsString,
    kind: UnlinkKind,
  ) -> BorrowedOp<'static> {
    branded(Op::UnlinkAt { dir_fd, path, kind })
  }

  /// Renames `old_path` to `new_path`.
  pub fn rename_at(
    old_dir_fd: Resource,
    old_path: OsString,
    new_dir_fd: Resource,
    new_path: OsString,
  ) -> BorrowedOp<'static> {
    branded(Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path })
  }

  /// Creates a directory.
  pub fn mkdir_at(
    dir_fd: Resource,
    path: OsString,
    mode: FileMode,
  ) -> BorrowedOp<'static> {
    branded(Op::MkdirAt { dir_fd, path, mode })
  }

  /// Creates a link.
  pub fn link_at(
    kind: LinkKind,
    source_dir_fd: Resource,
    source_path: OsString,
    new_dir_fd: Resource,
    new_path: OsString,
  ) -> BorrowedOp<'static> {
    branded(Op::LinkAt {
      kind,
      source_dir_fd,
      source_path,
      new_dir_fd,
      new_path,
    })
  }

  /// Reads a symlink into `buf`.
  pub fn readlink_at(
    dir_fd: Resource,
    path: OsString,
    buf: &'a mut [u8],
  ) -> Self {
    branded(Op::ReadlinkAt {
      dir_fd,
      path,
      buf: nonnull_mut(buf),
      buf_len: buf.len(),
    })
  }

  /// Writes the current directory into `out`.
  pub fn getcwd(out: &'a mut OsString) -> Self {
    branded(Op::GetCwd { out: NonNull::from(out) })
  }

  /// Spawns a process.
  pub fn spawn(spec: SpawnSpec) -> BorrowedOp<'static> {
    branded(Op::Spawn { spec })
  }

  /// Creates a socket.
  pub fn socket(
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
  ) -> BorrowedOp<'static> {
    branded(Op::Socket { domain, ty, proto })
  }

  /// Binds `fd` to `addr`.
  pub fn bind(fd: Resource, addr: SocketAddrBuf) -> BorrowedOp<'static> {
    branded(Op::Bind { fd, addr })
  }

  /// Listens on `fd`.
  pub fn listen(fd: Resource, backlog: i32) -> BorrowedOp<'static> {
    branded(Op::Listen { fd, backlog })
  }

  /// Shuts down `fd`.
  pub fn shutdown(fd: Resource, how: ShutdownHow) -> BorrowedOp<'static> {
    branded(Op::Shutdown { fd, how })
  }

  /// Syncs `fd`.
  pub fn fsync(fd: Resource) -> BorrowedOp<'static> {
    branded(Op::Fsync { fd })
  }
}

/// A [`MsgBuf`] that borrows the bytes it describes.
#[repr(transparent)]
pub struct BorrowedMsgBuf<'a> {
  inner: MsgBuf,
  _lt: PhantomData<&'a [u8]>,
}

impl<'a> BorrowedMsgBuf<'a> {
  /// Describes `buf`.
  pub fn from_slice(buf: &'a [u8]) -> Self {
    Self { inner: MsgBuf::from_slice(buf), _lt: PhantomData }
  }
}

/// A [`MsgBufMut`] that borrows the bytes it describes.
#[repr(transparent)]
pub struct BorrowedMsgBufMut<'a> {
  inner: MsgBufMut,
  _lt: PhantomData<&'a mut [u8]>,
}

impl<'a> BorrowedMsgBufMut<'a> {
  /// Describes `buf`.
  pub fn from_slice(buf: &'a mut [u8]) -> Self {
    Self { inner: MsgBufMut::from_slice(buf), _lt: PhantomData }
  }

  /// Describes uninitialized `buf`.
  pub fn from_uninit(buf: &'a mut [MaybeUninit<u8>]) -> Self {
    let len = buf.len();
    let ptr = NonNull::from(buf).cast();
    Self {
      // SAFETY: `ptr` addresses `buf` for `len` bytes, and the descriptor
      // cannot outlive `buf`.
      inner: unsafe { MsgBufMut::from_raw_parts(ptr, len) },
      _lt: PhantomData,
    }
  }
}

fn as_msg_buf<'a>(bufs: &'a [BorrowedMsgBuf<'a>]) -> &'a [MsgBuf] {
  // SAFETY: `BorrowedMsgBuf` is `repr(transparent)` over `MsgBuf`. The
  // lifetime brand is zero-sized, so the slice has the same layout.
  unsafe { &*(bufs as *const [BorrowedMsgBuf<'a>] as *const [MsgBuf]) }
}

fn as_msg_buf_mut<'a>(bufs: &'a [BorrowedMsgBufMut<'a>]) -> &'a [MsgBufMut] {
  // SAFETY: `BorrowedMsgBufMut` is `repr(transparent)` over `MsgBufMut`.
  unsafe { &*(bufs as *const [BorrowedMsgBufMut<'a>] as *const [MsgBufMut]) }
}

fn nonnull_mut<T>(buf: &mut [T]) -> NonNull<T> {
  if buf.is_empty() { NonNull::dangling() } else { NonNull::from(&mut buf[0]) }
}

fn fill_recv_slots(
  buf: &mut impl crate::buf::IoBufMutVec,
  slots: &mut RecvSlots,
) {
  let mut regions = crate::buf::IoSlicesMut::new();
  buf.bufs_mut(&mut regions);
  let count = regions.len();
  assert!(count > 0, "recv requires at least one buffer");
  for (slot, region) in slots.bufs.iter_mut().zip(regions) {
    let len = region.len();
    let ptr = NonNull::from(region).cast();
    // SAFETY: `region` borrows `buf`, and the `BorrowedOp` this fills borrows
    // both `buf` and `slots` until the backend is finished with the op. The
    // pointer stays inside that allocation; `len` is the region's length.
    *slot = unsafe { MsgBufMut::from_raw_parts(ptr, len) };
  }
  slots.count = count;
}

fn fill_send_slots(buf: &impl crate::buf::IoBufVec, slots: &mut SendSlots) {
  let count = buf.buf_count().min(crate::buf::MAX_IOV_COUNT);
  assert!(count > 0, "send requires at least one buffer");
  for (slot, bytes) in slots.bufs.iter_mut().zip((0..count).map(|i| buf.buf(i)))
  {
    // SAFETY: `bytes` borrows `buf`, and the `BorrowedOp` this fills borrows
    // `buf` and `slots` for the submission. The descriptor only copies the
    // address and length.
    *slot = unsafe {
      MsgBuf::from_raw_parts(NonNull::from(bytes).cast(), bytes.len())
    };
  }
  slots.count = count;
}
