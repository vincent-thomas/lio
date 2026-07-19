//! Operation data enum for zero-box I/O operations.
//!
//! This module defines the [`Op`] enum which represents all I/O operations
//! as pure data. Backends match on this enum to execute operations.

use crate::api::resource::Resource;
use std::ffi::OsString;
use std::io;
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

macro_rules! op_flags {
  ($name:ident) => {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct $name(i32);

    impl $name {
      pub const EMPTY: Self = Self(0);

      pub const fn from_bits(bits: i32) -> Option<Self> {
        if bits < 0 { None } else { Some(Self(bits)) }
      }

      pub const fn bits(self) -> i32 {
        self.0
      }
    }
  };
}

op_flags!(ReadFlags);
op_flags!(WriteFlags);
op_flags!(RecvFlags);
op_flags!(SendFlags);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenFlags(i32);

impl OpenFlags {
  pub const EMPTY: Self = Self(0);

  pub const fn from_bits(bits: i32) -> Self {
    Self(bits)
  }

  pub const fn bits(self) -> i32 {
    self.0
  }
}

impl From<i32> for OpenFlags {
  fn from(bits: i32) -> Self {
    Self::from_bits(bits)
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileMode(u32);

impl FileMode {
  pub const fn from_bits(bits: u32) -> Self {
    Self(bits)
  }

  pub const fn bits(self) -> u32 {
    self.0
  }
}

impl From<u32> for FileMode {
  fn from(bits: u32) -> Self {
    Self::from_bits(bits)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlinkKind {
  File,
  Directory,
}

impl From<i32> for UnlinkKind {
  fn from(flags: i32) -> Self {
    if flags == libc::AT_REMOVEDIR { Self::Directory } else { Self::File }
  }
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
    SocketAddrFamily::Unspecified => {
      Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
    }
    SocketAddrFamily::Unix => {
      Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
    }
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Abstract type → raw OS constant conversion (platform-independent contract)
// ═══════════════════════════════════════════════════════════════════════════════

/// Translates abstract socket domain/type/proto to raw OS constants.
pub fn socket_to_raw(
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
) -> Result<(i32, i32, i32), i32> {
  if matches!(domain, SockDomain::UNIX) && !matches!(proto, SockProto::DEFAULT)
  {
    return Err(libc::EINVAL);
  }

  let domain = match domain {
    SockDomain::IPV4 => raw_af_inet(),
    SockDomain::IPV6 => raw_af_inet6(),
    SockDomain::UNIX => raw_af_unix()?,
    _ => return Err(libc::EAFNOSUPPORT),
  };

  let (ty, proto) = match (ty, proto) {
    (SockType::STREAM, SockProto::DEFAULT) => {
      (raw_sock_stream(), raw_proto_default())
    }
    (SockType::STREAM, SockProto::TCP) => (raw_sock_stream(), raw_proto_tcp()),
    (SockType::DGRAM, SockProto::DEFAULT) => {
      (raw_sock_dgram(), raw_proto_default())
    }
    (SockType::DGRAM, SockProto::UDP) => (raw_sock_dgram(), raw_proto_udp()),
    _ => return Err(libc::EINVAL),
  };

  Ok((domain, ty, proto))
}

#[cfg(unix)]
const fn raw_af_inet() -> i32 {
  libc::AF_INET
}

#[cfg(windows)]
const fn raw_af_inet() -> i32 {
  windows_sys::Win32::Networking::WinSock::AF_INET
}

#[cfg(unix)]
const fn raw_af_inet6() -> i32 {
  libc::AF_INET6
}

#[cfg(windows)]
const fn raw_af_inet6() -> i32 {
  windows_sys::Win32::Networking::WinSock::AF_INET6
}

#[cfg(unix)]
const fn raw_af_unix() -> Result<i32, i32> {
  Ok(libc::AF_UNIX)
}

#[cfg(windows)]
const fn raw_af_unix() -> Result<i32, i32> {
  Ok(windows_sys::Win32::Networking::WinSock::AF_UNIX)
}

#[cfg(unix)]
const fn raw_sock_stream() -> i32 {
  libc::SOCK_STREAM
}

#[cfg(windows)]
const fn raw_sock_stream() -> i32 {
  windows_sys::Win32::Networking::WinSock::SOCK_STREAM
}

#[cfg(unix)]
const fn raw_sock_dgram() -> i32 {
  libc::SOCK_DGRAM
}

#[cfg(windows)]
const fn raw_sock_dgram() -> i32 {
  windows_sys::Win32::Networking::WinSock::SOCK_DGRAM
}

const fn raw_proto_default() -> i32 {
  0
}

#[cfg(unix)]
const fn raw_proto_tcp() -> i32 {
  libc::IPPROTO_TCP
}

#[cfg(windows)]
const fn raw_proto_tcp() -> i32 {
  windows_sys::Win32::Networking::WinSock::IPPROTO_TCP
}

#[cfg(unix)]
const fn raw_proto_udp() -> i32 {
  libc::IPPROTO_UDP
}

#[cfg(windows)]
const fn raw_proto_udp() -> i32 {
  windows_sys::Win32::Networking::WinSock::IPPROTO_UDP
}

// ═══════════════════════════════════════════════════════════════════════════════
// ErasedBuffer - Type-erased buffer storage
// ═══════════════════════════════════════════════════════════════════════════════

/// Non-owning raw byte span used by all backends.
///
/// The actual buffer is owned by the typed op (e.g., `Send<Vec<u8>>`). This struct
/// holds only a pointer + length so backends can lower it to native descriptors
/// without taking ownership, leaving the buffer available for result extraction.
#[derive(Clone, Copy, Debug)]
pub struct RawBuf {
  pub ptr: *mut u8,
  pub len: usize,
}

impl RawBuf {
  /// Creates a RawBuf from a pointer and length.
  #[inline]
  pub const fn new(buf: &[u8]) -> Self {
    Self { ptr: buf.as_ptr().cast_mut(), len: buf.len() }
  }

  /// Creates a RawBuf from raw parts.
  ///
  /// # Safety
  ///
  /// `ptr` must be valid for reads or writes of `len` bytes for the duration of
  /// the submitted operation.
  #[inline]
  pub const unsafe fn from_raw_parts(ptr: *mut u8, len: usize) -> Self {
    Self { ptr, len }
  }
}

// SAFETY: The pointed-to data is owned by a TypedOp which is Send + Sync.
// We only use this pointer from the thread that scheduled the operation.
unsafe impl Send for RawBuf {}
// SAFETY: Same as Send - we only access through the owning TypedOp.
unsafe impl Sync for RawBuf {}

#[derive(Clone, Copy, Debug)]
pub struct MsgBuf {
  pub ptr: NonNull<u8>,
  pub len: usize,
}

impl MsgBuf {
  #[inline]
  pub fn from_slice(buf: &[u8]) -> Self {
    Self {
      ptr: NonNull::new(buf.as_ptr().cast_mut())
        .expect("slice pointer must be non-null"),
      len: buf.len(),
    }
  }

  /// # Safety
  /// `ptr` must be non-null and valid for reads of `len` bytes for the operation lifetime.
  #[inline]
  pub const unsafe fn from_raw_parts(ptr: NonNull<u8>, len: usize) -> Self {
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
  pub ptr: NonNull<u8>,
  pub len: usize,
}

impl MsgBufMut {
  #[inline]
  pub fn from_slice(buf: &mut [u8]) -> Self {
    Self {
      ptr: NonNull::new(buf.as_mut_ptr())
        .expect("slice pointer must be non-null"),
      len: buf.len(),
    }
  }

  /// # Safety
  /// `ptr` must be non-null and valid for writes of `len` bytes for the operation lifetime.
  #[inline]
  pub const unsafe fn from_raw_parts(ptr: NonNull<u8>, len: usize) -> Self {
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
  pub bufs: NonNull<MsgBuf>,
  pub buf_count: NonZeroUsize,
  pub to: Option<SocketAddr>,
}

impl MsgSend {
  #[inline]
  pub fn new(bufs: &[MsgBuf], to: Option<SocketAddr>) -> Self {
    let buf_count = NonZeroUsize::new(bufs.len())
      .expect("MsgSend must contain at least one buffer");
    let bufs = NonNull::new(bufs.as_ptr().cast_mut())
      .expect("buffer slice pointer must be non-null");
    Self { bufs, buf_count, to }
  }
}

// SAFETY: `MsgSend` is metadata over caller-owned buffers and socket address data.
unsafe impl Send for MsgSend {}
// SAFETY: it contains no interior mutability beyond raw pointers.
unsafe impl Sync for MsgSend {}

#[derive(Clone, Copy, Debug)]
pub struct MsgRecv {
  pub bufs: NonNull<MsgBufMut>,
  pub buf_count: NonZeroUsize,
  pub from: Option<NonNull<SocketAddrBuf>>,
}

impl MsgRecv {
  #[inline]
  pub fn new(bufs: &[MsgBufMut]) -> Self {
    let buf_count = NonZeroUsize::new(bufs.len())
      .expect("MsgRecv must contain at least one buffer");
    let bufs = NonNull::new(bufs.as_ptr().cast_mut())
      .expect("buffer slice pointer must be non-null");
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

#[cfg(all(feature = "backend_impls", unix))]
#[allow(clippy::useless_conversion)]
pub(crate) fn file_stat_from_raw(stat: &libc::stat) -> FileStat {
  let file_type = match stat.st_mode & libc::S_IFMT {
    libc::S_IFREG => FileType::File,
    libc::S_IFDIR => FileType::Directory,
    libc::S_IFLNK => FileType::Symlink,
    libc::S_IFBLK => FileType::BlockDevice,
    libc::S_IFCHR => FileType::CharDevice,
    libc::S_IFIFO => FileType::Fifo,
    libc::S_IFSOCK => FileType::Socket,
    _ => FileType::Unknown,
  };

  FileStat {
    file_type,
    size: stat.st_size as u64,
    permissions: (stat.st_mode & 0o7777).into(),
    mode: stat.st_mode.into(),
    nlink: stat.st_nlink.into(),
    uid: stat.st_uid,
    gid: stat.st_gid,
  }
}

#[cfg(unix)]
pub(crate) fn file_type_from_dirent_dtype(dtype: u8) -> Option<FileType> {
  match dtype {
    libc::DT_BLK => Some(FileType::BlockDevice),
    libc::DT_CHR => Some(FileType::CharDevice),
    libc::DT_DIR => Some(FileType::Directory),
    libc::DT_FIFO => Some(FileType::Fifo),
    libc::DT_LNK => Some(FileType::Symlink),
    libc::DT_REG => Some(FileType::File),
    libc::DT_SOCK => Some(FileType::Socket),
    libc::DT_UNKNOWN => None,
    _ => None,
  }
}

// pub struct IoSlice(libc::iovec);

/// All I/O operations as pure data.
///
/// Each variant contains only the data needed to execute the operation.
/// Backends match on this enum to create submission entries or execute syscalls.
#[derive(Clone, Debug)]
pub enum Op {
  // ═══════════════════════════════════════════════════════════════════════════════
  // Buffer operations - RawBuf (ptr/len) points to TypedOp's buffer
  // ═══════════════════════════════════════════════════════════════════════════════
  /// Generic read operation supporting all read variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple read: buf.ptr != null, iovecs == null, offset == -1
  /// - Positioned read (pread): buf.ptr != null, offset >= 0
  /// - Vectored read (readv): iovecs != null, offset == -1
  /// - Vectored positioned (preadv): iovecs != null, offset >= 0
  /// - With flags (preadv2): Any above + flags != 0
  ///
  /// The buffer/iovec pointers are owned by the OpModel and must remain valid.
  Read {
    /// File descriptor to read from
    fd: Resource,
    /// Scatter buffers
    iovecs: NonNull<RawBuf>,
    /// Number of iovecs
    iov_count: usize,
    /// File offset (-1 for current position)
    offset: i64,
    /// Backend-interpreted read flags.
    flags: ReadFlags,
  },

  /// Generic write operation supporting all write variants.
  ///
  /// Backends select the appropriate syscall based on parameters:
  /// - Simple write: buf.ptr != null, iovecs == null, offset == -1
  /// - Positioned write (pwrite): buf.ptr != null, offset >= 0
  /// - Vectored write (writev): iovecs != null, offset == -1
  /// - Vectored positioned (pwritev): iovecs != null, offset >= 0
  /// - With flags (pwritev2): Any above + flags != 0
  ///
  /// The buffer/iovec pointers are owned by the OpModel and must remain valid.
  Write {
    /// File descriptor to write to
    fd: Resource,
    /// Gather buffers
    iovecs: NonNull<RawBuf>,
    /// Number of iovecs
    iov_count: usize,
    /// File offset (-1 for current position)
    offset: i64,
    /// Backend-interpreted write flags.
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
    /// Backend-interpreted receive flags.
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
    /// Backend-interpreted send flags.
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

  //
  // // ═══════════════════════════════════════════════════════════════════════════════
  // // Socket operations
  // // ═══════════════════════════════════════════════════════════════════════════════
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
    /// File creation mode used when creating a file.
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
    /// Directory creation mode.
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
  Bind {
    /// Socket resource to bind.
    fd: Resource,
    /// Socket address to bind to.
    addr: SocketAddr,
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

// /// Streaming accept - yields multiple connections from a single submission.
// ///
// /// On io_uring: Uses IORING_OP_ACCEPT with multishot flag.
// /// On pollingv2: Auto-resubmits after each accept.
// AcceptStream {
//   fd: Resource,
// },
// // ═══════════════════════════════════════════════════════════════════════════════
// // File operations
// // ═══════════════════════════════════════════════════════════════════════════════
// Close {
//   /// Raw file descriptor - we do not hold a Resource here to avoid
//   /// double-close (the op itself performs the close syscall).
//   #[cfg(unix)]
//   fd: RawFd,
//   #[cfg(windows)]
//   handle: std::os::windows::io::RawHandle,
//   /// Whether this is a socket (use closesocket()) or a file handle (use CloseHandle())
//   #[cfg(windows)]
//   is_socket: bool,
// },
// Truncate {
//   fd: Resource,
//   size: u64,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Link operations
// // ═══════════════════════════════════════════════════════════════════════════════
// LinkAt {
//   old_dir_fd: Resource,
//   old_path: *const c_char,
//   new_dir_fd: Resource,
//   new_path: *const c_char,
// },
// SymlinkAt {
//   dir_fd: Resource,
//   target: *const c_char,
//   linkpath: *const c_char,
// },
// UnlinkAt {
//   dir_fd: Resource,
//   path: *const c_char,
//   flags: i32,
// },
// RenameAt {
//   old_dir_fd: Resource,
//   old_path: *const c_char,
//   new_dir_fd: Resource,
//   new_path: *const c_char,
// },
// MkdirAt {
//   dir_fd: Resource,
//   path: *const c_char,
//   mode: u32,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Misc
// // ═══════════════════════════════════════════════════════════════════════════════
// #[cfg(target_os = "linux")]
// Tee {
//   fd_in: Resource,
//   fd_out: Resource,
//   size: u32,
// },
// /// Splice data between file descriptors via pipe (Linux only).
// #[cfg(target_os = "linux")]
// Splice {
//   fd_in: Resource,
//   off_in: i64,
//   fd_out: Resource,
//   off_out: i64,
//   len: u32,
//   flags: u32,
// },
// /// Send file data to a socket (Unix).
// #[cfg(unix)]
// SendFile {
//   out_fd: Resource,
//   in_fd: Resource,
//   offset: i64,
//   count: usize,
// },
// /// Copy data between files server-side (Linux only).
// #[cfg(target_os = "linux")]
// CopyFileRange {
//   fd_in: Resource,
//   off_in: i64,
//   fd_out: Resource,
//   off_out: i64,
//   len: usize,
//   flags: u32,
// },
// Sleep {
//   duration: Duration,
//   #[cfg(target_os = "linux")]
//   timer_fd: Resource,
//   #[cfg(target_os = "linux")]
//   timespec: *const libc::timespec,
// },
// /// Wraps an inner operation with a timeout deadline.
// ///
// /// On io_uring: Uses IORING_OP_LINK_TIMEOUT for kernel-native timeout handling.
// /// On pollingv2: Uses userspace timeout coordination via TimeManager.
// ///
// /// If the timeout fires first, the inner operation is cancelled.
// /// If the inner operation completes first, the timeout is cancelled.
// #[cfg(unix)]
// Timeout {
//   /// The wrapped operation
//   inner: Box<Op>,
//   /// Timeout duration
//   duration: Duration,
//   /// Pointer to timespec in the TypedOp (used by io_uring)
//   #[cfg(target_os = "linux")]
//   timespec: *const libc::timespec,
// },
// /// Watch a file or directory for changes.
// ///
// /// On BSD/macOS: Uses EVFILT_VNODE with kqueue.
// /// On Linux: Uses inotify.
// #[cfg(unix)]
// Watch {
//   /// Path to watch (C string, owned by TypedOp)
//   path: *const c_char,
//   /// Events to watch for (platform-specific flags)
//   mask: u32,
// },
// // ═══════════════════════════════════════════════════════════════════════════════
// // Process operations
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Wait for a child process to change state.
// ///
// /// On Linux with io_uring 6.7+: Uses IORING_OP_WAITID.
// /// On other platforms: Uses blocking waitid() syscall.
// #[cfg(unix)]
// Waitid {
//   /// What type of ID to wait for (P_PID, P_PGID, P_ALL, P_PIDFD).
//   idtype: libc::idtype_t,
//   /// The ID value (pid, pgid, or pidfd depending on idtype).
//   id: libc::id_t,
//   /// Options (WEXITED, WSTOPPED, WCONTINUED, WNOHANG, WNOWAIT).
//   options: libc::c_int,
//   /// Pointer to siginfo_t storage in the TypedOp.
//   infop: *mut libc::siginfo_t,
// },

// /// Spawn a new process.
// ///
// /// Uses posix_spawn() to create a new process.
// #[cfg(unix)]
// Spawn {
//   /// Path to executable (C string, owned by TypedOp).
//   path: *const c_char,
//   /// Argument vector (null-terminated array, owned by TypedOp).
//   argv: *const *const c_char,
//   /// Environment vector (null-terminated array, owned by TypedOp).
//   envp: *const *const c_char,
//   /// Pointer to pid_t storage in the TypedOp.
//   pid: *mut libc::pid_t,
//   /// File actions for stdio redirection (null for inherit).
//   file_actions: *const libc::posix_spawn_file_actions_t,
// },

// ═══════════════════════════════════════════════════════════════════════════════
// Vectored I/O operations (scatter/gather)
// ═══════════════════════════════════════════════════════════════════════════════
// ReadVAt {
//   fd: Resource,
//   iovecs: *const libc::iovec,
//   iov_count: usize,
//   offset: i64,
// },
// WriteVAt {
//   fd: Resource,
//   iovecs: *const libc::iovec,
//   iov_count: usize,
//   offset: i64,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // File locking
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Advisory file locking.
// ///
// /// # Platform-specific behavior
// ///
// /// This operation corresponds to `flock()` on Unix with `LOCK_SH`/`LOCK_EX`/`LOCK_UN`,
// /// and `LockFileEx`/`UnlockFileEx` on Windows.
// ///
// /// - **Unix**: Advisory locking (other processes can ignore locks)
// /// - **Windows**: Mandatory locking (OS enforces). Locking fails if file is opened
// ///   only for append; open with `.read(true)` or `.write(true)`.
// ///
// /// Operations: `LOCK_SH` (shared), `LOCK_EX` (exclusive), `LOCK_UN` (unlock).
// /// Can be combined with `LOCK_NB` for non-blocking behavior.
// Flock {
//   fd: Resource,
//   /// Locking operation: LOCK_SH, LOCK_EX, LOCK_UN (optionally | LOCK_NB)
//   operation: i32,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Directory operations
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Read directory entries (getdents64 on Linux, getdirentries on BSD).
// #[cfg(unix)]
// GetDents {
//   fd: Resource,
//   buf: RawBuf,
// },

// // ═══════════════════════════════════════════════════════════════════════════════
// // Signal handling
// // ═══════════════════════════════════════════════════════════════════════════════
// /// Wait for a signal from the specified signal set.
// ///
// /// On Linux: Uses signalfd.
// /// On BSD/macOS: Uses kqueue EVFILT_SIGNAL.
// #[cfg(unix)]
// Signal {
//   /// Pointer to sigset_t in the TypedOp.
//   sigset: *const libc::sigset_t,
// },
