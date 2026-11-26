//! WASI 0.3 backend, for WebAssembly components.
//!
//! Built on [`lio_wasi::Queue`], which is completion-based like io_uring:
//! an operation is started with caller memory and reports once it is done.
//!
//! # Resources
//!
//! A WASI resource is a typed handle, not a descriptor number, and one lio
//! resource needs more than a handle: a file also has a cursor, and a TCP
//! socket has a stream in each direction. So a [`Resource`] made by this
//! backend holds an id into a per-thread table of what it stands for. The
//! table is thread-local because closing a resource is a plain function with
//! no backend to reach through; a component has one thread, so nothing is
//! shared.
//!
//! Ids `0`, `1` and `2` are the component's standard streams, and
//! [`Resource::cwd`](crate::api::resource::Resource) resolves against the
//! preopened directories.
//!
//! # One operation per stream
//!
//! A WASI stream carries one read or write at a time. Operations on the
//! same standard stream, or the same direction of a TCP socket, wait their
//! turn in a [`Lane`].
//!
//! # What is supported
//!
//! Files, directories and their listings, standard streams, TCP and UDP
//! sockets, timeouts and cancellation. Process spawning and Unix-domain
//! sockets complete with an error.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::AsRawFd;
use std::time::Duration;

use bumpalo::Bump;
use lio_wasi::clock::WaitFor;
use lio_wasi::fs::{
  self, Descriptor, DescriptorFlags, DescriptorStat, DescriptorType,
  DirEntryBuf, DirStream, DirectoryEntry, ErrorCode, PathFlags,
};
use lio_wasi::sockets::{
  self, Accepted, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress,
  Ipv6SocketAddress, Listener, TcpSocket, UdpSocket,
};
use lio_wasi::wasip3;
use lio_wasi::{Completion, Outcome, Queue, ReadStream, WriteStream};

use crate::api::resource::{RawResource, ResourceClose};
use crate::backend::native;
use crate::backend::op::{
  ByteSpan, DirEntryRef, FileStat, FileType, LinkKind, Op, OpenFlags,
  ReadDirResult, ShutdownHow, SockDomain, SockType, SocketAddrBuf, StatTarget,
  UnlinkKind, WriteFlags, is_supported_socket, is_valid_file_offset,
  socket_addr_from_buf, socket_addr_into_buf,
};
use crate::backend::{InFlight, IoBackend, Keep, OpCompleted};

#[cfg(test)]
mod tests;

/// `user_data` of the timer that bounds a [`wait`](IoBackend::wait). No
/// registration id is this value.
const TIMER: u64 = u64::MAX;

/// The first id this backend gives a resource. Ids below it are the
/// standard streams and whatever descriptors wasi-libc hands out, which this
/// backend does not own.
const FIRST_ID: RawResource = 1 << 16;

fn fail(errno: i32) -> isize {
  -(errno as isize)
}

const CANCELLED: isize = -(libc::ECANCELED as isize);

/// The errno closest to a WASI filesystem error.
fn errno(error: &ErrorCode) -> i32 {
  match error {
    ErrorCode::Access => libc::EACCES,
    ErrorCode::Already => libc::EALREADY,
    ErrorCode::BadDescriptor => libc::EBADF,
    ErrorCode::Busy => libc::EBUSY,
    ErrorCode::Deadlock => libc::EDEADLK,
    ErrorCode::Quota => libc::EDQUOT,
    ErrorCode::Exist => libc::EEXIST,
    ErrorCode::FileTooLarge => libc::EFBIG,
    ErrorCode::IllegalByteSequence => libc::EILSEQ,
    ErrorCode::InProgress => libc::EINPROGRESS,
    ErrorCode::Interrupted => libc::EINTR,
    ErrorCode::Invalid => libc::EINVAL,
    ErrorCode::Io => libc::EIO,
    ErrorCode::IsDirectory => libc::EISDIR,
    ErrorCode::Loop => libc::ELOOP,
    ErrorCode::TooManyLinks => libc::EMLINK,
    ErrorCode::MessageSize => libc::EMSGSIZE,
    ErrorCode::NameTooLong => libc::ENAMETOOLONG,
    ErrorCode::NoDevice => libc::ENODEV,
    ErrorCode::NoEntry => libc::ENOENT,
    ErrorCode::NoLock => libc::ENOLCK,
    ErrorCode::InsufficientMemory => libc::ENOMEM,
    ErrorCode::InsufficientSpace => libc::ENOSPC,
    ErrorCode::NotDirectory => libc::ENOTDIR,
    ErrorCode::NotEmpty => libc::ENOTEMPTY,
    ErrorCode::NotRecoverable => libc::ENOTRECOVERABLE,
    ErrorCode::Unsupported => libc::ENOTSUP,
    ErrorCode::NoTty => libc::ENOTTY,
    ErrorCode::NoSuchDevice => libc::ENXIO,
    ErrorCode::Overflow => libc::EOVERFLOW,
    ErrorCode::NotPermitted => libc::EPERM,
    ErrorCode::Pipe => libc::EPIPE,
    ErrorCode::ReadOnly => libc::EROFS,
    ErrorCode::InvalidSeek => libc::ESPIPE,
    ErrorCode::TextFileBusy => libc::ETXTBSY,
    ErrorCode::CrossDevice => libc::EXDEV,
    ErrorCode::Other(_) => libc::EIO,
  }
}

/// The errno closest to a WASI socket error.
fn socket_errno(error: &sockets::ErrorCode) -> i32 {
  use sockets::ErrorCode as E;
  match error {
    E::AccessDenied => libc::EACCES,
    E::NotSupported => libc::ENOTSUP,
    E::InvalidArgument => libc::EINVAL,
    E::OutOfMemory => libc::ENOMEM,
    E::Timeout => libc::ETIMEDOUT,
    // The socket is in the wrong state for the call, such as connecting a
    // connected socket. WASI does not say which state.
    E::InvalidState => libc::EINVAL,
    E::AddressNotBindable => libc::EADDRNOTAVAIL,
    E::AddressInUse => libc::EADDRINUSE,
    E::RemoteUnreachable => libc::EHOSTUNREACH,
    E::ConnectionRefused => libc::ECONNREFUSED,
    E::ConnectionBroken => libc::EPIPE,
    E::ConnectionReset => libc::ECONNRESET,
    E::ConnectionAborted => libc::ECONNABORTED,
    E::DatagramTooLarge => libc::EMSGSIZE,
    E::Other(_) => libc::EIO,
  }
}

fn to_wasi(addr: SocketAddr) -> IpSocketAddress {
  match addr {
    SocketAddr::V4(v4) => {
      let [a, b, c, d] = v4.ip().octets();
      IpSocketAddress::Ipv4(Ipv4SocketAddress {
        port: v4.port(),
        address: (a, b, c, d),
      })
    }
    SocketAddr::V6(v6) => {
      let [a, b, c, d, e, f, g, h] = v6.ip().segments();
      IpSocketAddress::Ipv6(Ipv6SocketAddress {
        port: v6.port(),
        flow_info: v6.flowinfo(),
        address: (a, b, c, d, e, f, g, h),
        scope_id: v6.scope_id(),
      })
    }
  }
}

fn from_wasi(addr: IpSocketAddress) -> SocketAddr {
  match addr {
    IpSocketAddress::Ipv4(v4) => {
      let (a, b, c, d) = v4.address;
      SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(a, b, c, d), v4.port))
    }
    IpSocketAddress::Ipv6(v6) => {
      let (a, b, c, d, e, f, g, h) = v6.address;
      SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(a, b, c, d, e, f, g, h),
        v6.port,
        v6.flow_info,
        v6.scope_id,
      ))
    }
  }
}

/// The address an operation carries, as WASI takes it.
fn address(buf: &SocketAddrBuf) -> Result<IpSocketAddress, isize> {
  match socket_addr_from_buf(buf) {
    Ok(addr) => Ok(to_wasi(addr)),
    // A Unix-domain or unspecified address: WASI has only IP sockets.
    Err(_) => Err(fail(libc::EAFNOSUPPORT)),
  }
}

// ───────────────────────────── resource table ───────────────────────────────

/// An open file or directory.
struct File {
  descriptor: Descriptor,
  /// Where the next read or write without an offset happens. WASI files
  /// have no position of their own.
  cursor: u64,
  /// Opened for appending: writes without an offset go to the end.
  append: bool,
  /// The directory's listing, once one was asked for. Boxed so the entry
  /// buffer stays put while the host fills it.
  listing: Option<Box<Listing>>,
}

/// How many entries are asked of the host per read.
const LISTING_BATCH: usize = 64;

/// How far a directory's listing has got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListingState {
  /// The stream still has entries.
  Reading,
  /// The stream ended; its status has not been read.
  Ending,
  /// The stream ended without an error. Nothing more will come.
  Ended,
}

/// A directory being listed. It lives with the directory, so that each
/// `ReadDir` continues where the last one stopped.
struct Listing {
  /// The backend whose queue holds the stream.
  owner: u64,
  /// Open until the listing has ended.
  stream: Option<DirStream>,
  status: fs::StreamStatus,
  state: ListingState,
  /// Where the host writes a batch.
  buf: DirEntryBuf,
  /// Entries read from the host that an operation had no room for.
  carry: VecDeque<DirectoryEntry>,
}

/// Something closed whose streams still sit in a live backend's queue.
enum Orphan {
  Tcp(Tcp),
  Listing(Listing),
}

impl Orphan {
  fn owner(&self) -> u64 {
    match self {
      Orphan::Tcp(tcp) => tcp.owner,
      Orphan::Listing(listing) => listing.owner,
    }
  }
}

/// A TCP socket and the streams WASI gave for it. Each can be asked for
/// once, so they are kept for the socket's life.
struct Tcp {
  socket: TcpSocket,
  /// The backend whose queue holds the streams below.
  owner: u64,
  connected: bool,
  /// Incoming connections, once listening.
  listener: Option<Listener>,
  input: Option<ReadStream>,
  /// How the input ended. Taken by the receive that sees the end.
  input_status: Option<sockets::StreamStatus>,
  /// The read side was shut down: receives report the end.
  input_shut: bool,
  output: Option<WriteStream>,
  /// How the output ended. Taken by the send that sees the end.
  output_status: Option<sockets::StreamStatus>,
  /// The write side was shut down: sends fail with `EPIPE`.
  output_shut: bool,
}

impl Tcp {
  fn new(socket: TcpSocket, owner: u64, connected: bool) -> Box<Self> {
    Box::new(Self {
      socket,
      owner,
      connected,
      listener: None,
      input: None,
      input_status: None,
      input_shut: false,
      output: None,
      output_status: None,
      output_shut: false,
    })
  }

  fn has_streams(&self) -> bool {
    self.listener.is_some() || self.input.is_some() || self.output.is_some()
  }
}

enum Entry {
  File(File),
  Tcp(Box<Tcp>),
  Udp(UdpSocket),
}

/// What kind of thing a resource id is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
  /// One of the component's standard streams.
  Standard,
  File,
  Tcp,
  Udp,
}

/// What this thread's resource ids stand for.
#[derive(Default)]
struct Table {
  entries: Vec<Option<Entry>>,
  free: Vec<usize>,
  /// The preopened directories and their names, fetched on first use.
  preopens: Option<Vec<(Descriptor, String)>>,
}

thread_local! {
  static TABLE: RefCell<Table> = RefCell::new(Table::default());
  /// Resources closed while the table was in use. A backend closes them the
  /// next time it runs.
  static DEFERRED: RefCell<Vec<RawResource>> = const { RefCell::new(Vec::new()) };
  /// Closed resources whose streams still sit in a live backend's queue.
  /// That backend closes the streams the next time it runs.
  static ORPHANS: RefCell<Vec<Orphan>> = const { RefCell::new(Vec::new()) };
  /// The backends alive on this thread.
  static LIVE: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
  static NEXT_BACKEND: Cell<u64> = const { Cell::new(0) };
}

fn with_table<R>(run: impl FnOnce(&mut Table) -> R) -> R {
  TABLE.with(|table| run(&mut table.borrow_mut()))
}

/// Closes a resource this backend made. This is its [`ResourceClose`].
fn close_resource(raw: RawResource) {
  let removed = TABLE.with(|table| match table.try_borrow_mut() {
    Ok(mut table) => Some(table.remove(raw)),
    Err(_) => None,
  });
  match removed {
    // Disposed of here, after the table borrow ended.
    Some(entry) => dispose(entry),
    None => DEFERRED.with(|deferred| deferred.borrow_mut().push(raw)),
  }
}

/// Drops a removed entry. One whose streams are in a live backend's queue
/// is handed to that backend instead, which has to close them first.
fn dispose(entry: Option<Entry>) {
  let orphan = match entry {
    Some(Entry::Tcp(tcp)) if tcp.has_streams() => Orphan::Tcp(*tcp),
    Some(Entry::File(File { listing: Some(listing), .. }))
      if listing.stream.is_some() =>
    {
      Orphan::Listing(*listing)
    }
    _ => return,
  };
  if LIVE.with(|live| live.borrow().contains(&orphan.owner())) {
    ORPHANS.with(|orphans| orphans.borrow_mut().push(orphan));
  }
}

impl Table {
  fn index(raw: RawResource) -> Option<usize> {
    usize::try_from(raw.checked_sub(FIRST_ID)?).ok()
  }

  fn insert(&mut self, entry: Entry) -> isize {
    let index = match self.free.pop() {
      Some(index) => {
        self.entries[index] = Some(entry);
        index
      }
      None => {
        self.entries.push(Some(entry));
        self.entries.len() - 1
      }
    };
    // The table holds far fewer than `isize::MAX` entries.
    FIRST_ID as isize + index as isize
  }

  fn entry(&mut self, raw: RawResource) -> Result<&mut Entry, i32> {
    Self::index(raw)
      .and_then(|index| self.entries.get_mut(index))
      .and_then(Option::as_mut)
      .ok_or(libc::EBADF)
  }

  fn kind(&mut self, raw: RawResource) -> Option<Kind> {
    if (0..3).contains(&raw) {
      return Some(Kind::Standard);
    }
    Some(match self.entry(raw).ok()? {
      Entry::File(_) => Kind::File,
      Entry::Tcp(_) => Kind::Tcp,
      Entry::Udp(_) => Kind::Udp,
    })
  }

  fn file(&mut self, raw: RawResource) -> Result<&mut File, i32> {
    match self.entry(raw)? {
      Entry::File(file) => Ok(file),
      Entry::Tcp(_) | Entry::Udp(_) => Err(libc::EBADF),
    }
  }

  fn tcp(&mut self, raw: RawResource) -> Result<&mut Tcp, i32> {
    match self.entry(raw)? {
      Entry::Tcp(tcp) => Ok(tcp),
      Entry::Udp(_) => Err(libc::EOPNOTSUPP),
      Entry::File(_) => Err(libc::ENOTSOCK),
    }
  }

  fn udp(&mut self, raw: RawResource) -> Result<&mut UdpSocket, i32> {
    match self.entry(raw)? {
      Entry::Udp(udp) => Ok(udp),
      Entry::Tcp(_) => Err(libc::EOPNOTSUPP),
      Entry::File(_) => Err(libc::ENOTSOCK),
    }
  }

  fn remove(&mut self, raw: RawResource) -> Option<Entry> {
    let index = Self::index(raw)?;
    let entry = self.entries.get_mut(index)?.take()?;
    self.free.push(index);
    Some(entry)
  }

  fn preopens(&mut self) -> &[(Descriptor, String)] {
    self
      .preopens
      .get_or_insert_with(wasip3::filesystem::preopens::get_directories)
  }

  /// Resolves a path the way `openat` would: relative to `dir`, or, for an
  /// absolute path or the working directory, against the preopened
  /// directories. Returns the directory to call and the path below it.
  fn locate<'p>(
    &mut self,
    dir: RawResource,
    path: &'p str,
  ) -> Result<(&Descriptor, &'p str), i32> {
    if !path.starts_with('/') && dir != native::CWD {
      return Ok((&self.file(dir)?.descriptor, path));
    }
    let preopens = self.preopens();
    if let Some(below_root) = path.strip_prefix('/') {
      // The preopen with the longest name that is a whole-component prefix.
      let best = preopens
        .iter()
        .filter_map(|(descriptor, name)| {
          let name = name.trim_end_matches('/');
          let rest = path.strip_prefix(name)?;
          (rest.is_empty() || rest.starts_with('/')).then_some((
            descriptor,
            name.len(),
            rest,
          ))
        })
        .max_by_key(|(_, len, _)| *len);
      let Some((descriptor, _, rest)) = best else {
        return Err(libc::ENOENT);
      };
      let _ = below_root;
      let rest = rest.trim_start_matches('/');
      return Ok((descriptor, if rest.is_empty() { "." } else { rest }));
    }
    let cwd = wasip3::cli::environment::get_initial_cwd();
    let here = preopens
      .iter()
      .find(|(_, name)| name == "." || Some(name) == cwd.as_ref())
      .or_else(|| preopens.first());
    match here {
      Some((descriptor, _)) => Ok((descriptor, path)),
      None => Err(libc::ENOENT),
    }
  }
}

/// Looks up the local (`peer == false`) or remote address of a socket.
#[cfg(feature = "high")]
pub(crate) fn socket_name(
  raw: RawResource,
  peer: bool,
) -> io::Result<SocketAddr> {
  let address = with_table(|table| match table.entry(raw)? {
    Entry::Tcp(tcp) if peer => {
      tcp.socket.get_remote_address().map_err(|e| socket_errno(&e))
    }
    Entry::Tcp(tcp) => {
      tcp.socket.get_local_address().map_err(|e| socket_errno(&e))
    }
    Entry::Udp(udp) if peer => {
      udp.get_remote_address().map_err(|e| socket_errno(&e))
    }
    Entry::Udp(udp) => udp.get_local_address().map_err(|e| socket_errno(&e)),
    Entry::File(_) => Err(libc::ENOTSOCK),
  });
  match address {
    Ok(address) => Ok(from_wasi(address)),
    // WASI reports an unconnected socket's missing peer as a wrong state.
    Err(libc::EINVAL) if peer => {
      Err(io::Error::from_raw_os_error(libc::ENOTCONN))
    }
    Err(errno) => Err(io::Error::from_raw_os_error(errno)),
  }
}

// ───────────────────────────── in-flight state ──────────────────────────────

/// Treats a borrow as lasting as long as the pending operation it is stored
/// in.
///
/// # Safety
///
/// The referent stays allocated and unmoved until the value built from the
/// result is dropped. For a path this is the `Op` in the same boxed
/// [`Pending`]; for a descriptor it is the table entry, kept open by the
/// `Resource` that `Op` holds.
unsafe fn held<T: ?Sized>(value: &T) -> &'static T {
  // SAFETY: guaranteed by the caller.
  unsafe { &*std::ptr::from_ref(value) }
}

/// One WASI call that completes the operation when it returns.
enum Call {
  Open {
    call: fs::OpenAt<'static>,
    append: bool,
  },
  Stat(fs::Stat<'static>),
  StatAt(fs::StatAt<'static>),
  Mkdir(fs::CreateDirectoryAt<'static>),
  Unlink(fs::UnlinkFileAt<'static>),
  Rmdir(fs::RemoveDirectoryAt<'static>),
  Rename(fs::RenameAt<'static>),
  Link(fs::LinkAt<'static>),
  Symlink(fs::SymlinkAt<'static>),
  Readlink(fs::ReadlinkAt<'static>),
  Sync(fs::Sync<'static>),
  Connect {
    call: sockets::Connect<'static>,
    raw: RawResource,
  },
  SendTo {
    call: sockets::SendTo<'static>,
    len: usize,
    /// The datagram, when the operation's buffers had to be joined. `call`
    /// points into it.
    _joined: Vec<u8>,
  },
  ReceiveFrom(sockets::ReceiveFrom<'static>),
}

impl Call {
  /// # Safety
  ///
  /// `self` stays allocated and unmoved until its completion is handled.
  unsafe fn push(&mut self, queue: &mut Queue, id: u64) {
    // SAFETY: guaranteed by the caller.
    unsafe {
      match self {
        Call::Open { call, .. } => queue.push_call(call, id),
        Call::Stat(call) => queue.push_call(call, id),
        Call::StatAt(call) => queue.push_call(call, id),
        Call::Mkdir(call) => queue.push_call(call, id),
        Call::Unlink(call) => queue.push_call(call, id),
        Call::Rmdir(call) => queue.push_call(call, id),
        Call::Rename(call) => queue.push_call(call, id),
        Call::Link(call) => queue.push_call(call, id),
        Call::Symlink(call) => queue.push_call(call, id),
        Call::Readlink(call) => queue.push_call(call, id),
        Call::Sync(call) => queue.push_call(call, id),
        Call::Connect { call, .. } => queue.push_call(call, id),
        Call::SendTo { call, .. } => queue.push_call(call, id),
        Call::ReceiveFrom(call) => queue.push_call(call, id),
      }
    }
  }

  /// Turns the returned call's result into the operation's result, writing
  /// any output the operation asked for.
  fn finish(&mut self, op: &Op) -> isize {
    fn unit(result: Option<Result<(), ErrorCode>>) -> isize {
      match result {
        Some(Ok(())) => 0,
        Some(Err(error)) => fail(errno(&error)),
        None => unreachable!("a returned call has a result"),
      }
    }
    match self {
      Call::Open { call, append } => match call.result() {
        Some(Ok(descriptor)) => with_table(|table| {
          table.insert(Entry::File(File {
            descriptor,
            cursor: 0,
            append: *append,
            listing: None,
          }))
        }),
        Some(Err(error)) => fail(errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
      Call::Stat(call) => write_stat(op, call.result()),
      Call::StatAt(call) => write_stat(op, call.result()),
      Call::Readlink(call) => match call.result() {
        Some(Ok(target)) => {
          let Op::ReadlinkAt { buf, buf_len, .. } = op else {
            unreachable!("a readlink call belongs to a readlink operation")
          };
          let len = target.len().min(*buf_len);
          // SAFETY: the driver keeps `buf` valid for `buf_len` bytes until
          // this operation is reported, and `len` is within both.
          unsafe {
            std::ptr::copy_nonoverlapping(target.as_ptr(), buf.as_ptr(), len);
          }
          len as isize
        }
        Some(Err(error)) => fail(errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
      Call::Mkdir(call) => unit(call.result()),
      Call::Unlink(call) => unit(call.result()),
      Call::Rmdir(call) => unit(call.result()),
      Call::Rename(call) => unit(call.result()),
      Call::Link(call) => unit(call.result()),
      Call::Symlink(call) => unit(call.result()),
      Call::Sync(call) => unit(call.result()),
      Call::Connect { call, raw } => match call.result() {
        Some(Ok(())) => {
          with_table(|table| {
            if let Ok(tcp) = table.tcp(*raw) {
              tcp.connected = true;
            }
          });
          0
        }
        Some(Err(error)) => fail(socket_errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
      Call::SendTo { call, len, .. } => match call.result() {
        Some(Ok(())) => *len as isize,
        Some(Err(error)) => fail(socket_errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
      Call::ReceiveFrom(call) => match call.result() {
        Some(Ok((datagram, from))) => write_datagram(op, &datagram, from),
        Some(Err(error)) => fail(socket_errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
    }
  }
}

fn write_stat(
  op: &Op,
  result: Option<Result<DescriptorStat, ErrorCode>>,
) -> isize {
  let Op::Stat { out, .. } = op else {
    unreachable!("a stat call belongs to a stat operation")
  };
  let stat = match result {
    Some(Ok(stat)) => stat,
    Some(Err(error)) => return fail(errno(&error)),
    None => unreachable!("a returned call has a result"),
  };
  let file_type = file_type(&stat.type_);
  // WASI reports no permissions or ownership.
  let stat = FileStat {
    file_type,
    size: stat.size,
    nlink: stat.link_count,
    ..FileStat::zeroed()
  };
  // SAFETY: the driver keeps `out` valid and unaliased until this operation
  // is reported.
  unsafe { out.as_ptr().write(stat) };
  0
}

fn file_type(kind: &DescriptorType) -> FileType {
  match kind {
    DescriptorType::RegularFile => FileType::File,
    DescriptorType::Directory => FileType::Directory,
    DescriptorType::SymbolicLink => FileType::Symlink,
    DescriptorType::BlockDevice => FileType::BlockDevice,
    DescriptorType::CharacterDevice => FileType::CharDevice,
    DescriptorType::Fifo => FileType::Fifo,
    DescriptorType::Socket => FileType::Socket,
    DescriptorType::Other(_) => FileType::Unknown,
  }
}

/// Moves entries from `carry` into a `ReadDir`'s buffers, in order, until
/// one does not fit. Returns how many entries and name bytes were written.
fn write_entries(
  op: &Op,
  carry: &mut VecDeque<DirectoryEntry>,
) -> (usize, usize) {
  let Op::ReadDir { raw_buf, raw_cap, entries, entries_cap, .. } = op else {
    unreachable!("a listing belongs to a directory read")
  };
  let (mut written, mut raw_written) = (0, 0);
  while let Some(entry) = carry.front() {
    let name = entry.name.as_bytes();
    let (Ok(offset), Ok(len)) =
      (u32::try_from(raw_written), u16::try_from(name.len()))
    else {
      break;
    };
    if written == *entries_cap || raw_written + name.len() > *raw_cap {
      break;
    }
    // SAFETY: the driver keeps `raw_cap` bytes at `raw_buf` and
    // `entries_cap` slots at `entries` valid and unaliased until this
    // operation is reported. The checks above keep both writes in range.
    unsafe {
      std::ptr::copy_nonoverlapping(
        name.as_ptr(),
        raw_buf.as_ptr().add(raw_written),
        name.len(),
      );
      entries.as_ptr().add(written).write(DirEntryRef {
        name_offset: offset,
        name_len: len,
        file_type: match file_type(&entry.type_) {
          FileType::Unknown => None,
          known => Some(known),
        },
        // WASI does not report inode numbers in a listing.
        ino: None,
      });
    }
    raw_written += name.len();
    written += 1;
    carry.pop_front();
  }
  (written, raw_written)
}

/// Copies a received datagram into the operation's buffers and reports its
/// sender. What does not fit is dropped, as `recvfrom` does.
fn write_datagram(op: &Op, datagram: &[u8], from: IpSocketAddress) -> isize {
  let Op::Recv { msg, .. } = op else {
    unreachable!("a receive call belongs to a receive operation")
  };
  let mut copied = 0;
  let mut index = 0;
  while copied < datagram.len()
    && let Some((ptr, len)) = buffer(op, index)
  {
    let part = len.min(datagram.len() - copied);
    // SAFETY: the driver keeps the buffer valid for `len` bytes until this
    // operation is reported, and `part` is within both ranges.
    unsafe {
      std::ptr::copy_nonoverlapping(datagram.as_ptr().add(copied), ptr, part);
    }
    copied += part;
    index += 1;
  }
  if let Some(out) = msg.from {
    // SAFETY: the driver keeps `from` valid and unaliased until this
    // operation is reported.
    unsafe { out.as_ptr().write(socket_addr_into_buf(from_wasi(from))) };
  }
  copied as isize
}

/// Which stream a transfer moves bytes on.
#[derive(Clone, Copy)]
enum Stream {
  Read(ReadStream),
  Write(WriteStream),
}

/// The future that says how a stream ended.
enum Status {
  File(fs::StreamStatus),
  Socket(sockets::StreamStatus),
}

impl Status {
  /// # Safety
  ///
  /// `self` stays allocated and unmoved until its completion is handled.
  unsafe fn push(&mut self, queue: &mut Queue, id: u64) {
    // SAFETY: guaranteed by the caller.
    unsafe {
      match self {
        Status::File(status) => queue.push_call(status, id),
        Status::Socket(status) => queue.push_call(status, id),
      }
    }
  }

  /// The status as an errno, once it has arrived.
  fn result(&mut self) -> Option<Result<(), i32>> {
    match self {
      Status::File(status) => {
        status.result().map(|result| result.map_err(|e| errno(&e)))
      }
      Status::Socket(status) => {
        status.result().map(|result| result.map_err(|e| socket_errno(&e)))
      }
    }
  }
}

/// What a transfer is waiting for.
enum Phase {
  /// A read or write of the current buffer.
  Data,
  /// The stream ended; its status says whether that was an error.
  Status,
  /// The data is written; the requested sync is running.
  Sync,
}

/// A read or write: bytes moved over a stream, buffer by buffer.
struct Transfer {
  /// The resource, to move its cursor or find its status.
  raw: RawResource,
  stream: Stream,
  writing: bool,
  /// The stream belongs to this operation and is closed when it ends. A
  /// standard stream or a socket's stream is shared and stays open.
  owned: bool,
  /// The stream is a TCP socket's: its status is in the table.
  socket: bool,
  /// How the stream ended. A file has it from the start; a socket's is
  /// taken when the stream ends; a standard stream has none.
  status: Option<Status>,
  /// The other end stopped taking data while this was writing.
  broken: bool,
  /// A sync to run after the data is written.
  sync: Option<fs::Sync<'static>>,
  sync_data: Option<fs::SyncData<'static>>,
  phase: Phase,
  /// The buffer being filled or drained, and how far into it.
  index: usize,
  within: usize,
  total: usize,
  /// The operation used the file's cursor, which moves by `total`.
  moves_cursor: bool,
}

impl Transfer {
  fn new(raw: RawResource, stream: Stream) -> Self {
    Self {
      raw,
      stream,
      writing: matches!(stream, Stream::Write(_)),
      owned: false,
      socket: false,
      status: None,
      broken: false,
      sync: None,
      sync_data: None,
      phase: Phase::Data,
      index: 0,
      within: 0,
      total: 0,
      moves_cursor: false,
    }
  }
}

/// The `index`th buffer of a read, write, receive or send.
fn buffer(op: &Op, index: usize) -> Option<(*mut u8, usize)> {
  /// # Safety
  ///
  /// `count` buffers at `first` are valid, and `index < count`.
  unsafe fn at<B: ByteSpan>(first: *const B, index: usize) -> (*mut u8, usize) {
    // SAFETY: guaranteed by the caller.
    let buf = unsafe { &*first.add(index) };
    (buf.bytes_ptr(), buf.bytes_len())
  }
  // SAFETY (every arm): the driver keeps the counted buffers at the pointer
  // valid until this operation is reported, and the guard checks `index`.
  match op {
    Op::Read { iovecs, iov_count, .. } if index < *iov_count => {
      // SAFETY: see above.
      Some(unsafe { at(iovecs.as_ptr(), index) })
    }
    Op::Write { iovecs, iov_count, .. } if index < *iov_count => {
      // SAFETY: see above.
      Some(unsafe { at(iovecs.as_ptr(), index) })
    }
    Op::Recv { msg, .. } if index < msg.buf_count.get() => {
      // SAFETY: see above.
      Some(unsafe { at(msg.bufs.as_ptr(), index) })
    }
    Op::Send { msg, .. } if index < msg.buf_count.get() => {
      // SAFETY: see above.
      Some(unsafe { at(msg.bufs.as_ptr(), index) })
    }
    _ => None,
  }
}

/// Which direction of a resource an operation uses.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Way {
  In,
  Out,
}

/// One direction of a standard stream or TCP socket.
type LaneKey = (RawResource, Way);

/// A stream that carries one operation at a time. The rest wait their turn.
#[derive(Default)]
struct Lane {
  busy: bool,
  waiting: VecDeque<u64>,
}

/// An operation the host has not finished.
struct Pending {
  /// Owns the paths the calls point into and keeps the resources open.
  op: Op,
  /// Keeps the operation's buffers allocated.
  _keep: Keep,
  /// The lane this operation holds until it is reported.
  lane: Option<LaneKey>,
  stage: Stage,
}

enum Stage {
  /// Not started: waiting for its lane, or about to begin.
  Queued,
  Call(Call),
  Transfer(Transfer),
  Accept(Accepted),
  /// Listing a directory. The listing itself is with the directory.
  ReadDir(ListStep),
}

/// What a directory listing operation is waiting for.
#[derive(Clone, Copy)]
enum ListStep {
  /// A batch of entries from the host.
  Entries,
  /// The status of a stream that ended.
  Status,
}

/// The WASI 0.3 backend.
pub struct Wasi {
  /// Tells this backend's sockets from another's on the same thread.
  id: u64,
  queue: Queue,
  /// Pushed but not yet flushed.
  queued: Vec<(u64, Op, Keep)>,
  /// Boxed so that an operation's storage does not move while the host
  /// writes into it.
  ops: HashMap<u64, Box<Pending>>,
  /// Results waiting for the next `wait`.
  ready: Vec<OpCompleted>,
  lanes: HashMap<LaneKey, Lane>,
  /// Standard input, output and error, by resource id, opened on first
  /// use.
  standard: [Option<Stream>; 3],
  /// The futures that report how the standard streams ended. They are
  /// never read, but have to stay open as long as the streams do.
  _standard_status: Vec<Box<dyn Any>>,
  timer: WaitFor,
  timer_pending: bool,
}

impl Wasi {
  pub fn new() -> Self {
    let id = NEXT_BACKEND.with(|next| next.replace(next.get() + 1));
    LIVE.with(|live| live.borrow_mut().push(id));
    Self {
      id,
      queue: Queue::new(),
      queued: Vec::new(),
      ops: HashMap::new(),
      ready: Vec::new(),
      lanes: HashMap::new(),
      standard: [None; 3],
      _standard_status: Vec::new(),
      timer: WaitFor::new(Duration::ZERO),
      timer_pending: false,
    }
  }

  /// Finishes closing resources that could not be closed where they were
  /// dropped.
  fn close_deferred(&mut self) {
    for raw in DEFERRED.with(|deferred| deferred.take()) {
      let entry = with_table(|table| table.remove(raw));
      dispose(entry);
    }
    let mine = ORPHANS.with(|orphans| {
      let mut orphans = orphans.borrow_mut();
      let (mine, others) =
        orphans.drain(..).partition(|orphan| orphan.owner() == self.id);
      *orphans = others;
      mine
    });
    let mine: Vec<Orphan> = mine;
    for orphan in mine {
      // Nothing is in flight on these: an operation keeps its resource
      // open. A stream this queue does not know was already closed.
      match orphan {
        Orphan::Tcp(tcp) => {
          if let Some(listener) = tcp.listener {
            let _ = self.queue.close_listener(listener);
          }
          if let Some(input) = tcp.input {
            let _ = self.queue.close_reader(input);
          }
          if let Some(output) = tcp.output {
            let _ = self.queue.close_writer(output);
          }
        }
        Orphan::Listing(listing) => {
          if let Some(stream) = listing.stream {
            let _ = self.queue.close_dir_reader(stream);
          }
        }
      }
    }
  }

  /// The lane an operation needs, if its stream is shared.
  fn lane_of(op: &Op) -> Option<LaneKey> {
    let (raw, way) = match op {
      // A directory has one listing, read by one operation at a time.
      Op::ReadDir { fd, .. } => return Some((fd.as_raw_fd(), Way::In)),
      Op::Read { fd, .. } | Op::Recv { fd, .. } | Op::Accept { fd, .. } => {
        (fd.as_raw_fd(), Way::In)
      }
      Op::Write { fd, .. } | Op::Send { fd, .. } => (fd.as_raw_fd(), Way::Out),
      _ => return None,
    };
    match with_table(|table| table.kind(raw)) {
      Some(Kind::Standard | Kind::Tcp) => Some((raw, way)),
      _ => None,
    }
  }

  /// Starts an operation. It finishes at once, joins `ops`, or waits there
  /// for its lane.
  fn start(&mut self, id: u64, mut pending: Box<Pending>) {
    if let Some(key) = Self::lane_of(&pending.op) {
      let lane = self.lanes.entry(key).or_default();
      if lane.busy {
        lane.waiting.push_back(id);
        self.ops.insert(id, pending);
        return;
      }
      lane.busy = true;
      pending.lane = Some(key);
    }
    match self.begin(id, &mut pending) {
      Ok(()) => {
        self.ops.insert(id, pending);
      }
      Err(result) => self.finish(id, pending, result),
    }
  }

  /// Reports an operation, frees what it held, and lets the next one onto
  /// its lane.
  fn finish(&mut self, id: u64, pending: Box<Pending>, result: isize) {
    let lane = pending.lane;
    self.ready.push(OpCompleted::new(id, result));
    drop(pending);
    self.close_deferred();
    let Some(key) = lane else {
      return;
    };
    let Some(lane) = self.lanes.get_mut(&key) else {
      unreachable!("a held lane exists")
    };
    lane.busy = false;
    let Some(next) = lane.waiting.pop_front() else {
      self.lanes.remove(&key);
      return;
    };
    let Some(waiting) = self.ops.remove(&next) else {
      unreachable!("a waiting operation is in `ops`")
    };
    self.start(next, waiting);
  }

  /// Starts `pending`. `Err` carries the result of an operation that is
  /// already finished, successfully or not.
  fn begin(&mut self, id: u64, pending: &mut Pending) -> Result<(), isize> {
    // SAFETY: `pending` is boxed and outlives every call built from `op`.
    let op: &'static Op = unsafe { held(&pending.op) };
    let call = match op {
      Op::Nop => return Err(0),
      Op::Read { offset, .. } | Op::Write { offset, .. } => {
        if !is_valid_file_offset(*offset) {
          return Err(fail(libc::EINVAL));
        }
        return self.begin_transfer(id, pending);
      }
      Op::Recv { fd, flags, .. } => {
        if !flags.is_empty() {
          return Err(fail(libc::ENOTSUP));
        }
        let raw = fd.as_raw_fd();
        if with_table(|table| table.kind(raw)) != Some(Kind::Udp) {
          return self.begin_transfer(id, pending);
        }
        Call::ReceiveFrom(with_table(|table| {
          let socket = table.udp(raw).map_err(fail)?;
          // SAFETY: the `Resource` in `pending.op` keeps the socket open
          // until the call is dropped.
          Ok::<_, isize>(sockets::ReceiveFrom::new(unsafe { held(socket) }))
        })?)
      }
      Op::Send { fd, msg, flags } => {
        if !flags.is_empty() {
          return Err(fail(libc::ENOTSUP));
        }
        let raw = fd.as_raw_fd();
        if with_table(|table| table.kind(raw)) != Some(Kind::Udp) {
          return self.begin_transfer(id, pending);
        }
        // A datagram is one buffer. Several are joined into one.
        let mut joined = Vec::new();
        let data: &'static [u8] = if msg.buf_count.get() == 1 {
          let Some((ptr, len)) = buffer(op, 0) else {
            unreachable!("a send has at least one buffer")
          };
          // SAFETY: the driver keeps the buffer valid and unchanged until
          // this operation is reported, which is after the call is dropped.
          unsafe { std::slice::from_raw_parts(ptr, len) }
        } else {
          let mut index = 0;
          while let Some((ptr, len)) = buffer(op, index) {
            // SAFETY: as above, for the duration of this copy.
            joined.extend_from_slice(unsafe {
              std::slice::from_raw_parts(ptr, len)
            });
            index += 1;
          }
          // SAFETY: `joined` is stored next to the call and not changed.
          // Moving a `Vec` does not move its bytes.
          unsafe { held(joined.as_slice()) }
        };
        let remote = msg.to.map(to_wasi);
        let call = with_table(|table| {
          let socket = table.udp(raw).map_err(fail)?;
          // SAFETY: as for `ReceiveFrom`.
          let socket = unsafe { held(socket) };
          Ok::<_, isize>(sockets::SendTo::new(socket, data, remote))
        })?;
        Call::SendTo { call, len: data.len(), _joined: joined }
      }
      Op::Socket { domain, ty, proto } => {
        return Err(self.socket(
          *domain,
          *ty,
          is_supported_socket(*domain, *ty, *proto),
        ));
      }
      Op::Bind { fd, addr } => {
        let local = address(addr)?;
        return Err(with_table(|table| {
          let bound = match table.entry(fd.as_raw_fd()) {
            Ok(Entry::Tcp(tcp)) => tcp.socket.bind(local),
            Ok(Entry::Udp(udp)) => udp.bind(local),
            Ok(Entry::File(_)) => return fail(libc::ENOTSOCK),
            Err(errno) => return fail(errno),
          };
          match bound {
            Ok(()) => 0,
            Err(error) => fail(socket_errno(&error)),
          }
        }));
      }
      Op::Listen { fd, backlog } => {
        return Err(self.listen(fd.as_raw_fd(), *backlog));
      }
      Op::Accept { fd, .. } => {
        let listener = with_table(|table| {
          let tcp = table.tcp(fd.as_raw_fd()).map_err(fail)?;
          if tcp.owner != self.id {
            return Err(fail(libc::EBADF));
          }
          // Not listening: `accept` on such a socket is `EINVAL`.
          tcp.listener.ok_or(fail(libc::EINVAL))
        })?;
        pending.stage = Stage::Accept(Accepted::new());
        let Stage::Accept(slot) = &mut pending.stage else {
          unreachable!("the stage was just set")
        };
        // SAFETY: `pending` is boxed and stays in `ops` until the
        // completion.
        let pushed = unsafe { self.queue.push_accept(listener, slot, id) };
        if pushed.is_err() {
          unreachable!("a listener is open and its lane carries one accept")
        }
        return Ok(());
      }
      Op::Connect { fd, addr } => {
        let remote = address(addr)?;
        let raw = fd.as_raw_fd();
        if with_table(|table| table.kind(raw)) == Some(Kind::Udp) {
          return Err(with_table(|table| {
            match table.udp(raw).map(|udp| udp.connect(remote)) {
              Ok(Ok(())) => 0,
              Ok(Err(error)) => fail(socket_errno(&error)),
              Err(errno) => fail(errno),
            }
          }));
        }
        let call = with_table(|table| {
          let tcp = table.tcp(raw).map_err(fail)?;
          // SAFETY: the `Resource` in `pending.op` keeps the socket open
          // until the call is dropped.
          let socket = unsafe { held(&tcp.socket) };
          Ok::<_, isize>(sockets::Connect::new(socket, remote))
        })?;
        Call::Connect { call, raw }
      }
      Op::Shutdown { fd, how } => {
        return Err(self.shutdown(fd.as_raw_fd(), *how));
      }
      Op::OpenAt { dir_fd, path, flags, mode: _ } => {
        let (open_flags, descriptor_flags) = open_flags(*flags)?;
        let call = locate(dir_fd.as_raw_fd(), path, |dir, path| {
          fs::OpenAt::new(
            dir,
            PathFlags::SYMLINK_FOLLOW,
            path,
            open_flags,
            descriptor_flags,
          )
        })?;
        Call::Open { call, append: flags.contains(OpenFlags::APPEND) }
      }
      Op::Stat { target: StatTarget::Fd { fd }, .. } => {
        Call::Stat(descriptor(fd.as_raw_fd(), fs::Stat::new)?)
      }
      Op::Stat {
        target: StatTarget::Path { dir_fd, path, follow_symlinks },
        ..
      } => {
        let flags = if *follow_symlinks {
          PathFlags::SYMLINK_FOLLOW
        } else {
          PathFlags::empty()
        };
        Call::StatAt(locate(dir_fd.as_raw_fd(), path, |dir, path| {
          fs::StatAt::new(dir, flags, path)
        })?)
      }
      Op::MkdirAt { dir_fd, path, mode: _ } => Call::Mkdir(locate(
        dir_fd.as_raw_fd(),
        path,
        fs::CreateDirectoryAt::new,
      )?),
      Op::UnlinkAt { dir_fd, path, kind: UnlinkKind::File } => {
        Call::Unlink(locate(dir_fd.as_raw_fd(), path, fs::UnlinkFileAt::new)?)
      }
      Op::UnlinkAt { dir_fd, path, kind: UnlinkKind::Directory } => {
        Call::Rmdir(locate(
          dir_fd.as_raw_fd(),
          path,
          fs::RemoveDirectoryAt::new,
        )?)
      }
      Op::ReadlinkAt { dir_fd, path, .. } => {
        Call::Readlink(locate(dir_fd.as_raw_fd(), path, fs::ReadlinkAt::new)?)
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        Call::Rename(locate2(
          (old_dir_fd.as_raw_fd(), old_path),
          (new_dir_fd.as_raw_fd(), new_path),
          fs::RenameAt::new,
        )?)
      }
      Op::LinkAt {
        kind: LinkKind::Hard,
        source_dir_fd,
        source_path,
        new_dir_fd,
        new_path,
      } => Call::Link(locate2(
        (source_dir_fd.as_raw_fd(), source_path),
        (new_dir_fd.as_raw_fd(), new_path),
        |old_dir, old_path, new_dir, new_path| {
          fs::LinkAt::new(
            old_dir,
            PathFlags::empty(),
            old_path,
            new_dir,
            new_path,
          )
        },
      )?),
      Op::LinkAt {
        kind: LinkKind::Soft,
        source_path,
        new_dir_fd,
        new_path,
        ..
      } => {
        let target = text(source_path)?;
        Call::Symlink(locate(new_dir_fd.as_raw_fd(), new_path, |dir, path| {
          fs::SymlinkAt::new(dir, target, path)
        })?)
      }
      Op::Fsync { fd } => {
        Call::Sync(descriptor(fd.as_raw_fd(), fs::Sync::new)?)
      }
      Op::GetCwd { out } => {
        let cwd = wasip3::cli::environment::get_initial_cwd()
          .unwrap_or_else(|| String::from("/"));
        // SAFETY: the driver keeps `out` valid and unaliased until this
        // operation is reported.
        unsafe { *out.as_ptr() = OsString::from(cwd) };
        return Err(0);
      }
      Op::ReadDir { fd, .. } => {
        let raw = fd.as_raw_fd();
        with_table(|table| {
          let file = table.file(raw).map_err(fail)?;
          let listing = file.listing.get_or_insert_with(|| {
            let (entries, status) = file.descriptor.read_directory();
            Box::new(Listing {
              owner: self.id,
              stream: Some(self.queue.add_dir_reader(entries)),
              status: fs::StreamStatus::new(status),
              state: ListingState::Reading,
              buf: DirEntryBuf::with_capacity(LISTING_BATCH),
              carry: VecDeque::new(),
            })
          });
          if listing.owner != self.id {
            return Err(fail(libc::EBADF));
          }
          Ok(())
        })?;
        return match self.list(id, pending) {
          Some(result) => Err(result),
          None => Ok(()),
        };
      }
      // WASI has no processes to spawn.
      _ => return Err(fail(libc::ENOTSUP)),
    };
    pending.stage = Stage::Call(call);
    let Stage::Call(call) = &mut pending.stage else {
      unreachable!("the stage was just set")
    };
    // SAFETY: `pending` is boxed and stays in `ops` until the completion.
    unsafe { call.push(&mut self.queue, id) };
    Ok(())
  }

  /// Creates a socket. `supported` says whether lio allows the combination
  /// at all.
  fn socket(
    &mut self,
    domain: SockDomain,
    ty: SockType,
    supported: bool,
  ) -> isize {
    if !supported {
      return fail(libc::EINVAL);
    }
    let family = match domain {
      SockDomain::IPV4 => IpAddressFamily::Ipv4,
      SockDomain::IPV6 => IpAddressFamily::Ipv6,
      // WASI has no Unix-domain sockets.
      _ => return fail(libc::EAFNOSUPPORT),
    };
    let entry = match ty {
      SockType::STREAM => TcpSocket::create(family)
        .map(|socket| Entry::Tcp(Tcp::new(socket, self.id, false))),
      SockType::DGRAM => UdpSocket::create(family).map(Entry::Udp),
      _ => return fail(libc::EINVAL),
    };
    match entry {
      Ok(entry) => with_table(|table| table.insert(entry)),
      Err(error) => fail(socket_errno(&error)),
    }
  }

  fn listen(&mut self, raw: RawResource, backlog: i32) -> isize {
    with_table(|table| {
      let tcp = match table.tcp(raw) {
        Ok(tcp) => tcp,
        Err(errno) => return fail(errno),
      };
      if tcp.owner != self.id {
        return fail(libc::EBADF);
      }
      if tcp.listener.is_some() {
        // Listening twice only changes the backlog, as on POSIX.
        return 0;
      }
      // A hint: a host that does not take it still listens.
      let _ = tcp
        .socket
        .set_listen_backlog_size(u64::try_from(backlog).unwrap_or(1).max(1));
      match tcp.socket.listen() {
        Ok(connections) => {
          tcp.listener = Some(self.queue.add_listener(connections));
          0
        }
        Err(error) => fail(socket_errno(&error)),
      }
    })
  }

  /// Shuts down one or both directions of a TCP socket. WASI does this by
  /// closing the direction's stream.
  fn shutdown(&mut self, raw: RawResource, how: ShutdownHow) -> isize {
    let (read, write) = match how {
      ShutdownHow::Read => (true, false),
      ShutdownHow::Write => (false, true),
      ShutdownHow::Both => (true, true),
    };
    with_table(|table| {
      let tcp = match table.tcp(raw) {
        Ok(tcp) => tcp,
        Err(errno) => return fail(errno),
      };
      if tcp.owner != self.id {
        return fail(libc::EBADF);
      }
      if !tcp.connected {
        return fail(libc::ENOTCONN);
      }
      if write && !tcp.output_shut {
        tcp.output_shut = true;
        match tcp.output.take() {
          Some(output) => {
            if self.queue.close_writer(output).is_err() {
              unreachable!("a socket's output stream is open in this queue")
            }
          }
          None => {
            // Never written to: give the socket a stream and close it.
            let (writer, reader) = wasip3::wit_stream::new::<u8>();
            tcp.output_status =
              Some(sockets::StreamStatus::new(tcp.socket.send(reader)));
            drop(writer);
          }
        }
      }
      if read && !tcp.input_shut {
        tcp.input_shut = true;
        match tcp.input.take() {
          Some(input) => {
            if self.queue.close_reader(input).is_err() {
              unreachable!("a socket's input stream is open in this queue")
            }
          }
          None => {
            // Never read from: take the stream and drop it.
            let (reader, status) = tcp.socket.receive();
            tcp.input_status = Some(sockets::StreamStatus::new(status));
            drop(reader);
          }
        }
      }
      0
    })
  }

  /// Finds the stream for a read or write and starts its first buffer.
  fn begin_transfer(
    &mut self,
    id: u64,
    pending: &mut Pending,
  ) -> Result<(), isize> {
    let (raw, offset, writing, flags, socket_op) = match &pending.op {
      Op::Read { fd, offset, .. } => {
        (fd.as_raw_fd(), *offset, false, WriteFlags::empty(), false)
      }
      Op::Write { fd, offset, flags, .. } => {
        (fd.as_raw_fd(), *offset, true, *flags, false)
      }
      Op::Recv { fd, .. } => {
        (fd.as_raw_fd(), None, false, WriteFlags::empty(), true)
      }
      Op::Send { fd, .. } => {
        (fd.as_raw_fd(), None, true, WriteFlags::empty(), true)
      }
      _ => unreachable!("only reads, writes, receives and sends transfer"),
    };

    let mut transfer = match with_table(|table| table.kind(raw)) {
      None => return Err(fail(libc::EBADF)),
      Some(Kind::Udp) => return Err(fail(libc::EOPNOTSUPP)),
      Some(Kind::Standard | Kind::File) if socket_op => {
        return Err(fail(libc::ENOTSOCK));
      }
      Some(Kind::Standard) => {
        if !flags.is_empty() {
          return Err(fail(libc::ENOTSUP));
        }
        Transfer::new(raw, self.standard_stream(raw, writing)?)
      }
      Some(Kind::Tcp) => {
        if !flags.is_empty() {
          return Err(fail(libc::ENOTSUP));
        }
        let mut transfer = Transfer::new(raw, self.tcp_stream(raw, writing)?);
        transfer.socket = true;
        transfer
      }
      Some(Kind::File) => {
        let append = flags.contains(WriteFlags::APPEND);
        let (stream, status, sync, sync_data) = with_table(|table| {
          let file = table.file(raw).map_err(fail)?;
          let at = offset.unwrap_or(file.cursor);
          // SAFETY: the `Resource` in `pending.op` keeps the file open
          // until these calls are dropped.
          let descriptor = unsafe { held(&file.descriptor) };
          let (stream, status) = if writing {
            let (writer, reader) = wasip3::wit_stream::new::<u8>();
            let status = if append || (offset.is_none() && file.append) {
              descriptor.append_via_stream(reader)
            } else {
              descriptor.write_via_stream(reader, at)
            };
            (Stream::Write(self.queue.add_writer(writer)), status)
          } else {
            let (reader, status) = descriptor.read_via_stream(at);
            (Stream::Read(self.queue.add_reader(reader)), status)
          };
          let sync =
            flags.contains(WriteFlags::SYNC).then(|| fs::Sync::new(descriptor));
          let sync_data = (flags.contains(WriteFlags::DSYNC) && sync.is_none())
            .then(|| fs::SyncData::new(descriptor));
          Ok::<_, isize>((stream, status, sync, sync_data))
        })?;
        let mut transfer = Transfer::new(raw, stream);
        transfer.owned = true;
        transfer.status = Some(Status::File(fs::StreamStatus::new(status)));
        transfer.sync = sync;
        transfer.sync_data = sync_data;
        transfer.moves_cursor = offset.is_none();
        transfer
      }
    };

    let more = self.push_data(id, &pending.op, &mut transfer);
    pending.stage = Stage::Transfer(transfer);
    if more {
      return Ok(());
    }
    // There was nothing to move. A file write still has to close its stream
    // and learn its status.
    match self.end_of_data(id, pending) {
      Some(result) => Err(result),
      None => Ok(()),
    }
  }

  /// The shared stream of a standard resource, opened on first use.
  fn standard_stream(
    &mut self,
    raw: RawResource,
    writing: bool,
  ) -> Result<Stream, isize> {
    // Standard input is read-only; output and error are write-only.
    if writing == (raw == 0) {
      return Err(fail(libc::EBADF));
    }
    let Some(slot) =
      usize::try_from(raw).ok().and_then(|at| self.standard.get_mut(at))
    else {
      unreachable!("a standard resource is 0, 1 or 2")
    };
    if let Some(stream) = slot {
      return Ok(*stream);
    }
    let (stream, status): (Stream, Box<dyn Any>) = if raw == 0 {
      let (reader, status) = wasip3::cli::stdin::read_via_stream();
      (Stream::Read(self.queue.add_reader(reader)), Box::new(status))
    } else {
      let (writer, reader) = wasip3::wit_stream::new::<u8>();
      let status: Box<dyn Any> = if raw == 1 {
        Box::new(wasip3::cli::stdout::write_via_stream(reader))
      } else {
        Box::new(wasip3::cli::stderr::write_via_stream(reader))
      };
      (Stream::Write(self.queue.add_writer(writer)), status)
    };
    *slot = Some(stream);
    self._standard_status.push(status);
    Ok(stream)
  }

  /// One direction of a connected TCP socket, opened on first use.
  ///
  /// `Err(0)` is a receive after the read side was shut down: it reports
  /// the end of the data.
  fn tcp_stream(
    &mut self,
    raw: RawResource,
    writing: bool,
  ) -> Result<Stream, isize> {
    with_table(|table| {
      let tcp = table.tcp(raw).map_err(fail)?;
      if tcp.owner != self.id {
        return Err(fail(libc::EBADF));
      }
      if !tcp.connected {
        return Err(fail(libc::ENOTCONN));
      }
      if writing {
        if tcp.output_shut {
          return Err(fail(libc::EPIPE));
        }
        let output = *tcp.output.get_or_insert_with(|| {
          let (writer, reader) = wasip3::wit_stream::new::<u8>();
          tcp.output_status =
            Some(sockets::StreamStatus::new(tcp.socket.send(reader)));
          self.queue.add_writer(writer)
        });
        Ok(Stream::Write(output))
      } else {
        if tcp.input_shut {
          return Err(0);
        }
        let input = *tcp.input.get_or_insert_with(|| {
          let (reader, status) = tcp.socket.receive();
          tcp.input_status = Some(sockets::StreamStatus::new(status));
          self.queue.add_reader(reader)
        });
        Ok(Stream::Read(input))
      }
    })
  }

  /// Starts moving the next buffer that has room or data left. Returns
  /// `false` when every buffer is done.
  fn push_data(&mut self, id: u64, op: &Op, transfer: &mut Transfer) -> bool {
    loop {
      let Some((ptr, len)) = buffer(op, transfer.index) else {
        return false;
      };
      if transfer.within == len {
        transfer.index += 1;
        transfer.within = 0;
        continue;
      }
      // SAFETY: `within < len`, so the range is inside the buffer, which
      // the driver keeps valid until this operation is reported.
      let (ptr, len) =
        unsafe { (ptr.add(transfer.within), len - transfer.within) };
      // SAFETY: as above; nothing else touches the buffer meanwhile.
      let pushed = unsafe {
        match transfer.stream {
          Stream::Read(stream) => self.queue.push_read(stream, ptr, len, id),
          Stream::Write(stream) => self.queue.push_write(stream, ptr, len, id),
        }
      };
      if pushed.is_err() {
        unreachable!("a transfer's stream is open and carries one operation")
      }
      return true;
    }
  }

  /// Handles a completion from the queue.
  fn on_completion(&mut self, done: Completion) {
    let id = done.user_data();
    if id == TIMER {
      self.timer_pending = false;
      return;
    }
    let Some(mut pending) = self.ops.remove(&id) else {
      unreachable!("a completion for an operation that is not in flight")
    };
    match self.advance(id, &mut pending, done.outcome()) {
      Some(result) => self.finish(id, pending, result),
      None => {
        self.ops.insert(id, pending);
      }
    }
  }

  /// Moves an operation forward after one of its steps completed. Returns
  /// its result once it is finished.
  fn advance(
    &mut self,
    id: u64,
    pending: &mut Pending,
    outcome: Outcome,
  ) -> Option<isize> {
    let Pending { op, stage, .. } = pending;
    match stage {
      Stage::Queued => unreachable!("a queued operation has nothing in flight"),
      Stage::Call(call) => Some(match outcome {
        Outcome::Returned => call.finish(op),
        Outcome::Cancelled(_) => CANCELLED,
        other => unreachable!("a call ended as {other:?}"),
      }),
      Stage::Accept(slot) => Some(match outcome {
        Outcome::Transferred(_) => self.accepted(op, slot),
        // The listener is gone.
        Outcome::Closed(_) => fail(libc::EINVAL),
        Outcome::Cancelled(_) => CANCELLED,
        Outcome::Returned => unreachable!("an accept returned"),
      }),
      Stage::ReadDir(step) => {
        let step = *step;
        self.listed(id, pending, step, outcome)
      }
      Stage::Transfer(transfer) => match transfer.phase {
        Phase::Data => {
          transfer.total += outcome.count();
          transfer.within += outcome.count();
          match outcome {
            // A write sends everything, and a file is read until the
            // buffers are full or it ends. A socket or standard input
            // returns what arrived.
            Outcome::Transferred(_)
              if (transfer.owned || transfer.writing)
                && self.push_data(id, op, transfer) =>
            {
              None
            }
            Outcome::Transferred(_) => self.end_of_data(id, pending),
            Outcome::Closed(_) => {
              transfer.broken = transfer.writing;
              self.stream_ended(id, pending)
            }
            Outcome::Cancelled(_) => {
              let total = transfer.total;
              self.release(pending);
              Some(if total > 0 { total as isize } else { CANCELLED })
            }
            Outcome::Returned => unreachable!("a stream operation returned"),
          }
        }
        Phase::Status => {
          let status = match (&outcome, transfer.status.as_mut()) {
            (Outcome::Returned, Some(status)) => status.result(),
            _ => None,
          };
          let (total, writing, broken) =
            (transfer.total, transfer.writing, transfer.broken);
          match (outcome, status) {
            // The peer stopped reading without an error of its own.
            (Outcome::Returned, Some(Ok(()))) if broken => {
              self.release(pending);
              Some(if total > 0 { total as isize } else { fail(libc::EPIPE) })
            }
            (Outcome::Returned, Some(Ok(()))) => self.after_status(id, pending),
            (Outcome::Returned, Some(Err(errno))) => {
              self.release(pending);
              // Data that arrived before a read failed is still delivered.
              Some(if !writing && total > 0 {
                total as isize
              } else {
                fail(errno)
              })
            }
            (Outcome::Cancelled(_), _) => {
              self.release(pending);
              Some(if !writing && total > 0 {
                total as isize
              } else {
                CANCELLED
              })
            }
            (other, _) => unreachable!("a stream status ended as {other:?}"),
          }
        }
        Phase::Sync => {
          let result = match outcome {
            Outcome::Returned => {
              let synced = match (&mut transfer.sync, &mut transfer.sync_data) {
                (Some(sync), _) => sync.result(),
                (None, Some(sync)) => sync.result(),
                (None, None) => unreachable!("a sync phase has a sync call"),
              };
              match synced {
                Some(Ok(())) => transfer.total as isize,
                Some(Err(error)) => fail(errno(&error)),
                None => unreachable!("a returned call has a result"),
              }
            }
            Outcome::Cancelled(_) => CANCELLED,
            other => unreachable!("a sync ended as {other:?}"),
          };
          self.release(pending);
          Some(result)
        }
      },
    }
  }

  /// Gives a `ReadDir` what the listing has for it, or asks the host for
  /// more. Returns the operation's result once it has entries or the
  /// listing is over.
  fn list(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Pending { op, stage, .. } = pending;
    let Op::ReadDir { fd, out, .. } = &*op else {
      unreachable!("only a directory read lists")
    };
    with_table(|table| {
      let Ok(File { listing: Some(listing), .. }) = table.file(fd.as_raw_fd())
      else {
        unreachable!("a listing operation keeps its directory open")
      };
      let (entries, raw_written) = write_entries(op, &mut listing.carry);
      let ended = listing.state == ListingState::Ended;
      if entries > 0 || (ended && listing.carry.is_empty()) {
        let result = ReadDirResult {
          entries,
          raw_written,
          eof: ended && listing.carry.is_empty(),
        };
        // SAFETY: the driver keeps `out` valid and unaliased until this
        // operation is reported.
        unsafe { out.as_ptr().write(result) };
        return Some(0);
      }
      if !listing.carry.is_empty() {
        // The next entry fits no buffer this operation brought.
        return Some(fail(native::BUFFER_TOO_SMALL));
      }
      // SAFETY (both arms): the listing is boxed in the table and its
      // directory stays open until this operation's completion is handled.
      match (listing.state, listing.stream) {
        (ListingState::Reading, Some(stream)) => {
          *stage = Stage::ReadDir(ListStep::Entries);
          // SAFETY: see above.
          let pushed =
            unsafe { self.queue.push_read_dir(stream, &mut listing.buf, id) };
          if pushed.is_err() {
            unreachable!("a listing's stream is open and read one at a time")
          }
        }
        _ => {
          *stage = Stage::ReadDir(ListStep::Status);
          // SAFETY: see above.
          unsafe { self.queue.push_call(&mut listing.status, id) };
        }
      }
      None
    })
  }

  /// Handles a listing step that completed.
  fn listed(
    &mut self,
    id: u64,
    pending: &mut Pending,
    step: ListStep,
    outcome: Outcome,
  ) -> Option<isize> {
    let Op::ReadDir { fd, .. } = &pending.op else {
      unreachable!("only a directory read lists")
    };
    let raw = fd.as_raw_fd();
    let failed = with_table(|table| {
      let Ok(file) = table.file(raw) else {
        unreachable!("a listing operation keeps its directory open")
      };
      let Some(listing) = file.listing.as_mut() else {
        unreachable!("a listing operation has a listing")
      };
      match (step, outcome) {
        (ListStep::Entries, _) => {
          // Entries that arrived are kept, also when the read was
          // cancelled: the next operation delivers them.
          let Listing { buf, carry, .. } = &mut **listing;
          carry.extend(buf.drain().filter(|e| e.name != "." && e.name != ".."));
          if let Outcome::Closed(_) = outcome {
            listing.state = ListingState::Ending;
          }
          matches!(outcome, Outcome::Cancelled(_)).then_some(CANCELLED)
        }
        (ListStep::Status, Outcome::Returned) => {
          let stream = listing.stream.take();
          let result = listing.status.result();
          if let Some(stream) = stream
            && self.queue.close_dir_reader(stream).is_err()
          {
            unreachable!("a listing's stream is open until it is closed here")
          }
          match result {
            Some(Ok(())) => {
              listing.state = ListingState::Ended;
              None
            }
            Some(Err(error)) => {
              // The listing failed. The next one starts over.
              file.listing = None;
              Some(fail(errno(&error)))
            }
            None => unreachable!("a returned status has a result"),
          }
        }
        (ListStep::Status, Outcome::Cancelled(_)) => Some(CANCELLED),
        (ListStep::Status, other) => {
          unreachable!("a listing status ended as {other:?}")
        }
      }
    });
    match failed {
      Some(result) => Some(result),
      None => self.list(id, pending),
    }
  }

  /// Gives an accepted connection a resource id and reports its peer.
  fn accepted(&mut self, op: &Op, slot: &mut Accepted) -> isize {
    let Op::Accept { addr, .. } = op else {
      unreachable!("an accept slot belongs to an accept operation")
    };
    let Some(socket) = slot.socket() else {
      unreachable!("an accept that transferred has a socket")
    };
    let peer = match socket.get_remote_address() {
      Ok(peer) => peer,
      // The peer left before it was accepted.
      Err(_) => return fail(libc::ECONNABORTED),
    };
    // SAFETY: the driver keeps `addr` valid and unaliased until this
    // operation is reported.
    unsafe { addr.as_ptr().write(socket_addr_into_buf(from_wasi(peer))) };
    let entry = Entry::Tcp(Tcp::new(socket, self.id, true));
    with_table(|table| table.insert(entry))
  }

  /// Every buffer is done. A read is finished; a file write closes its
  /// stream and waits for the status.
  fn end_of_data(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      unreachable!("only a transfer has data")
    };
    if transfer.owned && transfer.writing {
      return self.stream_ended(id, pending);
    }
    let total = transfer.total;
    self.release(pending);
    Some(total as isize)
  }

  /// The stream is finished, by its end or by this side closing it. A file
  /// or socket then reports whether that was an error.
  fn stream_ended(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      unreachable!("only a transfer has a stream")
    };
    self.close_stream(transfer);
    if transfer.socket && transfer.status.is_none() {
      let (raw, writing) = (transfer.raw, transfer.writing);
      // The first operation to see the end takes the status. Later ones
      // find none and report a plain end.
      transfer.status = with_table(|table| {
        let tcp = table.tcp(raw).ok()?;
        if writing { tcp.output_status.take() } else { tcp.input_status.take() }
      })
      .map(Status::Socket);
    }
    match &mut transfer.status {
      Some(status) => {
        transfer.phase = Phase::Status;
        // SAFETY: `pending` is boxed and stays in `ops` until the
        // completion.
        unsafe { status.push(&mut self.queue, id) };
        None
      }
      None => {
        // An end with nothing more to learn: the end of input, or an
        // output nobody reads.
        let (total, writing) = (transfer.total, transfer.writing);
        self.release(pending);
        Some(if writing && total == 0 {
          fail(libc::EPIPE)
        } else {
          total as isize
        })
      }
    }
  }

  /// The stream ended without an error. Runs the sync a write asked for,
  /// or finishes.
  fn after_status(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      unreachable!("only a transfer has a status")
    };
    // SAFETY: `pending` is boxed and stays in `ops` until the completion.
    unsafe {
      match (&mut transfer.sync, &mut transfer.sync_data) {
        (Some(sync), _) => self.queue.push_call(sync, id),
        (None, Some(sync)) => self.queue.push_call(sync, id),
        (None, None) => {
          let total = transfer.total;
          self.release(pending);
          return Some(total as isize);
        }
      }
    }
    transfer.phase = Phase::Sync;
    None
  }

  /// Closes a stream that belongs to one operation.
  fn close_stream(&mut self, transfer: &mut Transfer) {
    if !std::mem::take(&mut transfer.owned) {
      return;
    }
    let closed = match transfer.stream {
      Stream::Read(stream) => self.queue.close_reader(stream),
      Stream::Write(stream) => self.queue.close_writer(stream),
    };
    if closed.is_err() {
      unreachable!("an owned stream is open until it is closed here")
    }
  }

  /// Ends a transfer: closes its stream and moves the file's cursor.
  fn release(&mut self, pending: &mut Pending) {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      return;
    };
    self.close_stream(transfer);
    if transfer.moves_cursor {
      let (raw, total) = (transfer.raw, transfer.total as u64);
      with_table(|table| {
        if let Ok(file) = table.file(raw) {
          file.cursor += total;
        }
      });
    }
  }

  /// Handles every completion that is ready without blocking.
  fn pump(&mut self) {
    while let Some(done) = self.queue.poll() {
      self.on_completion(done);
    }
  }
}

impl Default for Wasi {
  fn default() -> Self {
    Self::new()
  }
}

impl Drop for Wasi {
  fn drop(&mut self) {
    LIVE.with(|live| live.borrow_mut().retain(|id| *id != self.id));
    // This backend's queue closes its streams as it drops, so what was
    // waiting on it can go.
    ORPHANS.with(|orphans| {
      orphans.borrow_mut().retain(|orphan| orphan.owner() != self.id);
    });
  }
}

/// The path as the UTF-8 string WASI requires.
fn text(path: &'static OsString) -> Result<&'static str, isize> {
  path.to_str().ok_or(fail(libc::EILSEQ))
}

/// Builds a call on the open file `raw`.
fn descriptor<C>(
  raw: RawResource,
  build: impl FnOnce(&'static Descriptor) -> C,
) -> Result<C, isize> {
  with_table(|table| {
    let file = table.file(raw).map_err(fail)?;
    // SAFETY: the `Resource` in the pending operation keeps the file open
    // until the call is dropped.
    Ok(build(unsafe { held(&file.descriptor) }))
  })
}

/// Builds a call on `path` resolved against `dir`.
fn locate<C>(
  dir: RawResource,
  path: &'static OsString,
  build: impl FnOnce(&'static Descriptor, &'static str) -> C,
) -> Result<C, isize> {
  let path = text(path)?;
  with_table(|table| {
    let (descriptor, path) = table.locate(dir, path).map_err(fail)?;
    // SAFETY: an open file is kept open by the `Resource` in the pending
    // operation; a preopened directory is never closed.
    Ok(build(unsafe { held(descriptor) }, path))
  })
}

/// Builds a call on two paths, each resolved against its own directory.
fn locate2<C>(
  (old_dir, old_path): (RawResource, &'static OsString),
  (new_dir, new_path): (RawResource, &'static OsString),
  build: impl FnOnce(
    &'static Descriptor,
    &'static str,
    &'static Descriptor,
    &'static str,
  ) -> C,
) -> Result<C, isize> {
  let (old_path, new_path) = (text(old_path)?, text(new_path)?);
  with_table(|table| {
    let (old, old_path) = table.locate(old_dir, old_path).map_err(fail)?;
    // SAFETY: as in `locate`.
    let old = unsafe { held(old) };
    let (new, new_path) = table.locate(new_dir, new_path).map_err(fail)?;
    // SAFETY: as in `locate`.
    Ok(build(old, old_path, unsafe { held(new) }, new_path))
  })
}

/// Translates portable open flags to WASI's two flag sets.
fn open_flags(
  flags: OpenFlags,
) -> Result<(fs::OpenFlags, DescriptorFlags), isize> {
  let mut open = fs::OpenFlags::empty();
  let mut descriptor = DescriptorFlags::empty();
  if flags.reads() {
    descriptor |= DescriptorFlags::READ;
  }
  if flags.writes() {
    descriptor |= DescriptorFlags::WRITE;
  }
  if flags.contains(OpenFlags::DIRECTORY) {
    // A directory is opened to be the anchor of later path operations,
    // which need permission to change its entries.
    open |= fs::OpenFlags::DIRECTORY;
    descriptor = DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY;
  }
  if descriptor.is_empty() {
    return Err(fail(libc::EINVAL));
  }
  if flags.contains(OpenFlags::CREATE) {
    open |= fs::OpenFlags::CREATE;
  }
  if flags.contains(OpenFlags::TRUNCATE) {
    open |= fs::OpenFlags::TRUNCATE;
  }
  if flags.contains(OpenFlags::EXCLUSIVE) {
    open |= fs::OpenFlags::EXCLUSIVE;
  }
  Ok((open, descriptor))
}

// SAFETY:
// - Each pushed operation's `Keep` lives in its `Pending` (or in `queued`)
//   until the operation is reported or drained.
// - An operation is reported once: when its last step completes, or with a
//   cancellation after the host confirmed it stopped. `lio_wasi::Queue`
//   cancels synchronously, so the host no longer uses the operation's
//   memory when the cancelled completion is handled.
// - Results are what the host returned. A new resource is a fresh table
//   entry, closed by `close_resource`.
// - Nothing runs between calls: a component has one thread, and the host
//   only writes while this backend waits.
unsafe impl IoBackend for Wasi {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.queued.reserve(cap);
    self.ops.reserve(cap);
    self.ready.reserve(cap);
    Ok(())
  }

  fn resource_close(&self) -> ResourceClose {
    ResourceClose::with(close_resource)
  }

  fn push(&mut self, id: u64, op: InFlight<'_>, _step_bump: &mut Bump) {
    let (op, keep) = op.into_parts();
    self.queued.push((id, op, keep));
  }

  fn flush(&mut self) -> io::Result<()> {
    self.close_deferred();
    for (id, op, keep) in std::mem::take(&mut self.queued) {
      let pending =
        Box::new(Pending { op, _keep: keep, lane: None, stage: Stage::Queued });
      self.start(id, pending);
    }
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    if let Some(at) = self.queued.iter().position(|(queued, ..)| *queued == id)
    {
      self.queued.swap_remove(at);
      self.ready.push(OpCompleted::new(id, CANCELLED));
      return;
    }
    let waiting = match self.ops.get(&id).map(|pending| &pending.stage) {
      Some(Stage::Queued) => true,
      Some(_) => false,
      None => return,
    };
    if waiting {
      for lane in self.lanes.values_mut() {
        lane.waiting.retain(|queued| *queued != id);
      }
      self.ops.remove(&id);
      self.ready.push(OpCompleted::new(id, CANCELLED));
      self.close_deferred();
      return;
    }
    // The cancelled completion is queued and handled by the next `wait`.
    // If nothing was in flight, the operation's own completion is already
    // queued.
    self.queue.cancel(id);
  }

  fn drain(&mut self) -> bool {
    self.queued.clear();
    // Operations waiting for a lane never started. Dropping them first
    // keeps a finishing operation from starting one.
    for lane in self.lanes.values_mut() {
      for id in lane.waiting.drain(..) {
        self.ops.remove(&id);
      }
    }
    let ids: Vec<u64> = self.ops.keys().copied().collect();
    for id in ids {
      self.queue.cancel(id);
    }
    self.pump();
    self.ops.clear();
    self.ready.clear();
    self.close_deferred();
    true
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    self.close_deferred();
    self.pump();
    if self.ready.is_empty() {
      match timeout {
        Some(Duration::ZERO) => {}
        None => {
          while self.ready.is_empty() {
            let Some(done) = self.queue.wait() else {
              break;
            };
            self.on_completion(done);
          }
        }
        Some(limit) => {
          self.timer = WaitFor::new(limit);
          self.timer_pending = true;
          // SAFETY: `self.timer` is not touched until `timer_pending` is
          // cleared by its completion, which happens before this returns.
          unsafe { self.queue.push_call(&mut self.timer, TIMER) };
          while self.ready.is_empty() && self.timer_pending {
            let Some(done) = self.queue.wait() else {
              break;
            };
            self.on_completion(done);
          }
          if self.timer_pending {
            self.queue.cancel(TIMER);
            self.pump();
          }
        }
      }
    }
    completed.append(&mut self.ready);
    Ok(())
  }
}
