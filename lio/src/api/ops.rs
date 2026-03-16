//! I/O operation definitions.
//!
//! This module contains all the typed operation structs that implement `TypedOp`.

use std::cell::UnsafeCell;
use std::ffi::CString;
use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
#[cfg(windows)]
use std::os::windows::io::RawHandle;

use crate::api::op::{StreamOp, StreamResult, TypedOp};
use crate::api::resource::Resource;
use crate::backend::op::{Op, RawBuf};
use crate::{BufResult, IoBuf, IoBufMut, IoBufMutVec, IoBufVec, MAX_IOV_COUNT};

// ============================================================================
// Socket address conversion utilities
// ============================================================================

/// Converts a libc sockaddr_storage to a std SocketAddr.
///
/// Returns `None` if the address family is not supported (only AF_INET and AF_INET6).
fn libc_socketaddr_into_std(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
  // SAFETY: storage is a valid reference, so the pointer is valid
  unsafe { libc_socketaddr_into_std_raw(storage as *const _) }.ok()
}

/// # Safety
/// `storage` must point to a valid, initialized `sockaddr_storage`.
unsafe fn libc_socketaddr_into_std_raw(
  storage: *const libc::sockaddr_storage,
) -> io::Result<SocketAddr> {
  // SAFETY: correct pointer.
  let sockaddr = unsafe { *storage };

  if sockaddr.ss_family == libc::AF_INET as libc::sa_family_t {
    let ipv4_ptr = storage.cast::<libc::sockaddr_in>();
    // SAFETY: We've verified ss_family is AF_INET, so the storage pointer can be safely
    // cast to sockaddr_in. The caller guarantees storage points to valid memory.
    let ipv4 = Ipv4Addr::from(unsafe { *ipv4_ptr }.sin_addr.s_addr.to_be());
    // SAFETY: Same as above - pointer is valid and properly aligned for sockaddr_in.
    let port = u16::from_be(unsafe { *ipv4_ptr }.sin_port);

    Ok(SocketAddr::from(SocketAddrV4::new(ipv4, port)))
  } else if sockaddr.ss_family == libc::AF_INET6 as libc::sa_family_t {
    let ipv6_ptr = storage.cast::<libc::sockaddr_in6>();
    // SAFETY: correct.
    let in6 = unsafe { *ipv6_ptr };
    let ipv6 =
      Ipv6Addr::from(u128::from_le_bytes(in6.sin6_addr.s6_addr).to_be());
    let port = u16::from_be(in6.sin6_port);

    Ok(SocketAddr::from(SocketAddrV6::new(
      ipv6,
      port,
      in6.sin6_flowinfo,
      in6.sin6_scope_id,
    )))
  } else {
    Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
  }
}

pub(crate) fn std_socketaddr_into_libc(addr: SocketAddr) -> libc::sockaddr_storage {
  // SAFETY: sockaddr_storage is a C struct designed to hold any socket address type.
  // Zero-initialization is valid - all fields are primitive types where zero is safe.
  let storage: UnsafeCell<libc::sockaddr_storage> =
    UnsafeCell::new(unsafe { mem::zeroed() });
  match addr {
    // SAFETY: copy_nonoverlapping is safe because:
    // 1. Source (&into_addr(v4)) is a valid, aligned sockaddr_in on the stack
    // 2. Destination (storage.get()) is valid - we just created it
    // 3. Size is correct (size_of::<sockaddr_in>())
    // 4. Regions don't overlap (source is on stack, dest is in UnsafeCell)
    // 5. sockaddr_in fits in sockaddr_storage by design
    SocketAddr::V4(v4) => unsafe {
      // We copy the bytes from the source pointer (&v4)
      // to the destination pointer (&mut storage)
      ptr::copy_nonoverlapping(
        &into_addr(v4) as *const _ as *const u8,
        storage.get() as *mut u8,
        // We calculate the size of the IPv4 address structure
        mem::size_of::<libc::sockaddr_in>(),
      );
    },
    // SAFETY: copy_nonoverlapping is safe because:
    // 1. Source (&into_addr6(v6)) is a valid, aligned sockaddr_in6 on the stack
    // 2. Destination (storage.get()) is valid - we just created it
    // 3. Size is correct (size_of::<sockaddr_in6>())
    // 4. Regions don't overlap (source is on stack, dest is in UnsafeCell)
    // 5. sockaddr_in6 fits in sockaddr_storage by design
    SocketAddr::V6(v6) => unsafe {
      // We copy the bytes from the source pointer (&v6)
      // to the destination pointer (&mut storage)
      ptr::copy_nonoverlapping(
        &into_addr6(v6) as *const _ as *const u8,
        storage.get() as *mut u8,
        // We calculate the size of the IPv6 address structure
        mem::size_of::<libc::sockaddr_in6>(),
      );
    },
  };

  storage.into_inner()
}

fn into_addr(addr: SocketAddrV4) -> libc::sockaddr_in {
  // SAFETY: sockaddr_in is a C struct with primitive integer fields.
  // Zero-initialization is safe - all fields accept zero as a valid value.
  let mut _addr: libc::sockaddr_in = unsafe { mem::zeroed() };

  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    _addr.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
  }
  _addr.sin_family = libc::AF_INET as libc::sa_family_t;
  _addr.sin_port = addr.port().to_be();
  _addr.sin_addr = libc::in_addr { s_addr: u32::from(*addr.ip()).to_be() };

  _addr
}

fn into_addr6(addr: SocketAddrV6) -> libc::sockaddr_in6 {
  // SAFETY: sockaddr_in6 is a C struct with primitive integer/array fields.
  // Zero-initialization is safe - all fields accept zero as a valid value.
  let mut _addr: libc::sockaddr_in6 = unsafe { mem::zeroed() };

  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    _addr.sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
  }
  _addr.sin6_family = libc::AF_INET6 as libc::sa_family_t;
  _addr.sin6_port = addr.port().to_be();
  _addr.sin6_addr = libc::in6_addr { s6_addr: addr.ip().octets() };

  _addr
}

// ============================================================================
// Accept
// ============================================================================

pub struct Accept {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
}

impl Accept {
  pub(crate) fn new(res: Resource) -> Self {
    // SAFETY: libc::sockaddr_storage is a C struct that is safe to zero-initialize.
    // It consists of primitive integer fields where zero is a valid value. The kernel
    // will fill this structure via the accept syscall's output parameter.
    let addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    Self { res, addr, len }
  }
}

impl TypedOp for Accept {
  type Result = io::Result<(Resource, SocketAddr)>;

  fn into_op(&mut self) -> Op {
    Op::Accept {
      fd: self.res.clone(),
      addr: &mut self.addr as *mut _,
      len: &mut self.len as *mut _,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let result = if res < 0 {
      return Err(io::Error::from_raw_os_error(-res as i32));
    } else {
      res as RawFd
    };

    // SAFETY: result is valid fd.
    let res = unsafe { Resource::from_raw_fd(result) };
    // SAFETY: self.addr was filled by the kernel via the accept syscall.
    let addr = unsafe { libc_socketaddr_into_std_raw(&self.addr as *const _) }?;
    Ok((res, addr))
  }
}

// ============================================================================
// AcceptUnix
// ============================================================================

/// Accept operation for Unix domain sockets.
///
/// This is a variant of `Accept` specifically for Unix domain sockets.
/// It returns only the accepted `Resource` without attempting to parse
/// the peer address into a `SocketAddr`.
pub struct AcceptUnix {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
}

impl AcceptUnix {
  pub(crate) fn new(res: Resource) -> Self {
    // SAFETY: libc::sockaddr_storage is a C struct that is safe to zero-initialize.
    let addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    Self { res, addr, len }
  }
}

impl TypedOp for AcceptUnix {
  impl_io_result!(Resource);

  fn into_op(&mut self) -> Op {
    Op::Accept {
      fd: self.res.clone(),
      addr: &mut self.addr as *mut _,
      len: &mut self.len as *mut _,
    }
  }
}

// ============================================================================
// AcceptStream
// ============================================================================

/// A streaming accept operation that yields multiple connections.
///
/// Unlike `Accept` which accepts a single connection, `AcceptStream` continues
/// to accept connections until the socket is closed or an error occurs.
///
/// # Example
///
/// ```no_run
/// use lio::{Lio, api};
/// use lio::api::resource::Resource;
///
/// async fn server(lio: &Lio, listener: &Resource) -> std::io::Result<()> {
///     let mut stream = api::accept_stream(listener).with_lio(lio);
///     while let Some(result) = stream.next().await {
///         let (client, addr) = result?;
///         println!("Accepted connection from {}", addr);
///     }
///     Ok(())
/// }
/// ```
pub struct AcceptStream {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
}

impl AcceptStream {
  pub(crate) fn new(res: Resource) -> Self {
    // SAFETY: libc::sockaddr_storage is a C struct that is safe to zero-initialize.
    let addr: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    Self { res, addr, len }
  }
}

impl StreamOp for AcceptStream {
  type Item = io::Result<(Resource, SocketAddr)>;

  fn into_op(&mut self) -> Op {
    Op::Accept {
      fd: self.res.clone(),
      addr: &mut self.addr as *mut _,
      len: &mut self.len as *mut _,
    }
  }

  fn extract_item(&mut self, res: isize) -> StreamResult<Self::Item> {
    if res < 0 {
      let err = -res as i32;
      // EAGAIN/EWOULDBLOCK means no more connections right now (for level-triggered)
      // ECONNABORTED means the connection was aborted before accept completed
      if err == libc::EAGAIN || err == libc::EWOULDBLOCK {
        // For non-blocking sockets, this means we've drained all pending connections
        // The stream isn't done - we'll wait for more
        return StreamResult::Item(Err(io::Error::from_raw_os_error(err)));
      }
      // Other errors indicate the stream is done
      StreamResult::Item(Err(io::Error::from_raw_os_error(err)))
    } else {
      let fd = res as RawFd;
      // SAFETY: fd is valid from accept syscall.
      let resource = unsafe { Resource::from_raw_fd(fd) };
      // SAFETY: self.addr was filled by the kernel via the accept syscall.
      match unsafe { libc_socketaddr_into_std_raw(&self.addr as *const _) } {
        Ok(addr) => StreamResult::Item(Ok((resource, addr))),
        Err(e) => StreamResult::Item(Err(e)),
      }
    }
  }

  fn reset(&mut self) {
    // Reset the address storage for the next accept
    self.addr = unsafe { mem::zeroed() };
    self.len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
  }

  // Note: io_uring multishot accept is now implemented in the backend layer.
  // The backend's push_stream() uses AcceptMulti for native multishot support.
}

// ============================================================================
// Bind
// ============================================================================

pub struct Bind {
  res: Resource,
  addr: libc::sockaddr_storage,
}

assert_op_max_size!(Bind);

impl Bind {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    Self { res, addr: std_socketaddr_into_libc(addr) }
  }
}

impl TypedOp for Bind {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    let addrlen = if self.addr.ss_family == libc::AF_INET as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
    } else if self.addr.ss_family == libc::AF_INET6 as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
    } else {
      mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t
    };

    Op::Bind {
      fd: self.res.clone(),
      addr: &self.addr as *const _,
      addrlen,
    }
  }
}

// ============================================================================
// Close
// ============================================================================

pub struct Close {
  #[cfg(unix)]
  fd: RawFd,
  #[cfg(windows)]
  handle: RawHandle,
  #[cfg(windows)]
  is_socket: bool,
}

assert_op_max_size!(Close);

impl Close {
  #[cfg(unix)]
  pub(crate) fn new(fd: RawFd) -> Self {
    Self { fd }
  }

  #[cfg(windows)]
  pub(crate) fn new(handle: RawHandle, is_socket: bool) -> Self {
    Self { handle, is_socket }
  }
}

impl TypedOp for Close {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    #[cfg(unix)]
    {
      Op::Close { fd: self.fd }
    }
    #[cfg(windows)]
    {
      Op::Close { handle: self.handle, is_socket: self.is_socket }
    }
  }
}

// ============================================================================
// Connect
// ============================================================================

pub struct Connect {
  res: Resource,
  addr: libc::sockaddr_storage,
  len: libc::socklen_t,
  connect_called: AtomicBool,
}

assert_op_max_size!(Connect);

impl Connect {
  pub(crate) fn new(res: Resource, addr: SocketAddr) -> Self {
    let addr = std_socketaddr_into_libc(addr);
    let len = if addr.ss_family == libc::AF_INET as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in>()
    } else if addr.ss_family == libc::AF_INET6 as libc::sa_family_t {
      mem::size_of::<libc::sockaddr_in6>()
    } else {
      mem::size_of::<libc::sockaddr_storage>()
    } as libc::socklen_t;
    Self { res, addr, len, connect_called: AtomicBool::new(false) }
  }
}

impl TypedOp for Connect {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    let connect_called = self.connect_called.load(Ordering::Relaxed);
    Op::Connect {
      fd: self.res.clone(),
      addr: &self.addr as *const _,
      len: self.len,
      connect_called,
    }
  }
}

// ============================================================================
// CopyFileRange (Linux only)
// ============================================================================

/// Operation to copy data between files without going through userspace (Linux only).
///
/// This performs a server-side copy when possible (e.g., on NFS or reflink-capable
/// filesystems), avoiding data transfer through the application.
#[cfg(target_os = "linux")]
pub struct CopyFileRange {
  fd_in: Resource,
  off_in: i64,
  fd_out: Resource,
  off_out: i64,
  len: usize,
  flags: u32,
}

#[cfg(target_os = "linux")]
assert_op_max_size!(CopyFileRange);

#[cfg(target_os = "linux")]
impl CopyFileRange {
  pub(crate) fn new(
    fd_in: Resource,
    off_in: i64,
    fd_out: Resource,
    off_out: i64,
    len: usize,
    flags: u32,
  ) -> Self {
    Self { fd_in, off_in, fd_out, off_out, len, flags }
  }
}

#[cfg(target_os = "linux")]
impl TypedOp for CopyFileRange {
  impl_io_result!(i32);

  fn into_op(&mut self) -> Op {
    Op::CopyFileRange {
      fd_in: self.fd_in.clone(),
      off_in: self.off_in,
      fd_out: self.fd_out.clone(),
      off_out: self.off_out,
      len: self.len,
      flags: self.flags,
    }
  }
}

// ============================================================================
// Fsync
// ============================================================================

pub struct Fsync {
  res: Resource,
}

assert_op_max_size!(Fsync);

impl Fsync {
  pub(crate) fn new(res: Resource) -> Self {
    Self { res }
  }
}

impl TypedOp for Fsync {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::Fsync { fd: self.res.clone() }
  }
}

// ============================================================================
// LinkAt
// ============================================================================

pub struct LinkAt {
  old_dir_res: Resource,
  old_path: CString,
  new_dir_res: Resource,
  new_path: CString,
}

assert_op_max_size!(LinkAt);

impl LinkAt {
  pub(crate) fn new(
    old_dir_res: Resource,
    old_path: CString,
    new_dir_res: Resource,
    new_path: CString,
  ) -> Self {
    Self { old_dir_res, old_path, new_dir_res, new_path }
  }
}

impl TypedOp for LinkAt {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::LinkAt {
      old_dir_fd: self.old_dir_res.clone(),
      old_path: self.old_path.as_ptr(),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: self.new_path.as_ptr(),
    }
  }
}

// ============================================================================
// Listen
// ============================================================================

pub struct Listen {
  res: Resource,
  backlog: i32,
}

assert_op_max_size!(Listen);

impl Listen {
  pub(crate) fn new(res: Resource, backlog: i32) -> Self {
    Self { res, backlog }
  }
}

impl TypedOp for Listen {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::Listen {
      fd: self.res.clone(),
      backlog: self.backlog,
    }
  }
}

// ============================================================================
// MkdirAt
// ============================================================================

/// Operation to create a directory.
pub struct MkdirAt {
  dir_res: Resource,
  path: CString,
  mode: u32,
}

assert_op_max_size!(MkdirAt);

impl MkdirAt {
  pub(crate) fn new(dir_res: Resource, path: CString, mode: u32) -> Self {
    Self { dir_res, path, mode }
  }
}

impl TypedOp for MkdirAt {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::MkdirAt {
      dir_fd: self.dir_res.clone(),
      path: self.path.as_ptr(),
      mode: self.mode,
    }
  }
}

// ============================================================================
// Nop
// ============================================================================

pub struct Nop;

assert_op_max_size!(Nop);

impl TypedOp for Nop {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::Nop
  }
}

// ============================================================================
// OpenAt
// ============================================================================

pub struct OpenAt {
  dir_res: Resource,
  pathname: CString,
  flags: i32,
  mode: u32,
}

assert_op_max_size!(OpenAt);

impl OpenAt {
  /// Creates a new OpenAt operation with default mode (0o666).
  pub(crate) fn new(dir_res: Resource, pathname: CString, flags: i32) -> Self {
    Self { dir_res, pathname, flags, mode: 0o666 }
  }

  /// Creates a new OpenAt operation with explicit mode.
  pub(crate) fn with_mode(
    dir_res: Resource,
    pathname: CString,
    flags: i32,
    mode: u32,
  ) -> Self {
    Self { dir_res, pathname, flags, mode }
  }
}

impl TypedOp for OpenAt {
  impl_io_result!(Resource);

  fn into_op(&mut self) -> Op {
    Op::OpenAt {
      dir_fd: self.dir_res.clone(),
      path: self.pathname.as_ptr(),
      flags: self.flags,
      mode: self.mode,
    }
  }
}

// ============================================================================
// ReadV
// ============================================================================

pub struct ReadV<B: std::marker::Send + std::marker::Sync> {
  res: Resource,
  bufs: Option<B>,
  iovecs: [libc::iovec; MAX_IOV_COUNT],
  iov_count: usize,
}

unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for ReadV<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for ReadV<B> {}

impl<B: std::marker::Send + std::marker::Sync> ReadV<B> {
  pub(crate) fn new(res: Resource, bufs: B) -> Self
  where
    B: IoBufMutVec,
  {
    let iov_count = bufs.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      bufs: Some(bufs),
      iovecs: unsafe { mem::zeroed() },
      iov_count,
    }
  }
}

impl<B: IoBufMutVec> TypedOp for ReadV<B> {
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let bufs = self.bufs.as_mut().expect("buffers not available");

    for i in 0..self.iov_count {
      let (ptr, cap) = bufs.buf_mut(i);
      self.iovecs[i].iov_base = ptr as *mut _;
      self.iovecs[i].iov_len = cap;
    }

    Op::ReadV {
      fd: self.res.clone(),
      buf: RawBuf::empty(),
      iovecs: self.iovecs.as_ptr(),
      iov_count: self.iov_count,
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut bufs = self.bufs.take().expect("buffers not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      // Distribute total bytes read across buffers using stored capacities
      let mut remaining = res as usize;
      for i in 0..self.iov_count {
        let cap = self.iovecs[i].iov_len;
        let len = remaining.min(cap);
        bufs.set_buf_len(i, len);
        remaining = remaining.saturating_sub(cap);
      }
      (Ok(res as i32), bufs)
    }
  }
}

// ============================================================================
// ReadVAt
// ============================================================================

pub struct ReadVAt<B: std::marker::Send + std::marker::Sync> {
  res: Resource,
  bufs: Option<B>,
  iovecs: [libc::iovec; MAX_IOV_COUNT],
  iov_count: usize,
  offset: i64,
}

unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for ReadVAt<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for ReadVAt<B> {}

impl<B: std::marker::Send + std::marker::Sync> ReadVAt<B> {
  pub(crate) fn new(res: Resource, bufs: B, offset: i64) -> Self
  where
    B: IoBufMutVec,
  {
    let iov_count = bufs.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      bufs: Some(bufs),
      iovecs: unsafe { mem::zeroed() },
      iov_count,
      offset,
    }
  }
}

impl<B: IoBufMutVec> TypedOp for ReadVAt<B> {
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let bufs = self.bufs.as_mut().expect("buffers not available");

    for i in 0..self.iov_count {
      let (ptr, cap) = bufs.buf_mut(i);
      self.iovecs[i].iov_base = ptr as *mut _;
      self.iovecs[i].iov_len = cap;
    }

    Op::ReadVAt {
      fd: self.res.clone(),
      buf: RawBuf::empty(),
      iovecs: self.iovecs.as_ptr(),
      iov_count: self.iov_count,
      offset: self.offset,
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut bufs = self.bufs.take().expect("buffers not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      // Distribute total bytes read across buffers using stored capacities
      let mut remaining = res as usize;
      for i in 0..self.iov_count {
        let cap = self.iovecs[i].iov_len;
        let len = remaining.min(cap);
        bufs.set_buf_len(i, len);
        remaining = remaining.saturating_sub(cap);
      }
      (Ok(res as i32), bufs)
    }
  }
}

// ============================================================================
// Recv
// ============================================================================

pub struct Recv<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
}

impl<B> Recv<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<B> TypedOp for Recv<B>
where
  B: IoBufMut,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let buf = self.buf.as_mut().expect("buffer not available");
    let ptr = buf.as_mut_ptr();
    let len = buf.capacity();
    Op::Recv {
      fd: self.res.clone(),
      flags: self.flags,
      buf: RawBuf::new(ptr, len),
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut buf = self.buf.take().expect("buffer not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      buf.set_len(res as usize);
      (Ok(res as i32), buf)
    }
  }
}

// ============================================================================
// RecvFrom
// ============================================================================

pub struct RecvFrom<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
  addr: libc::sockaddr_storage,
  addrlen: libc::socklen_t,
  /// iovec for io_uring recvmsg (stored here so it persists)
  iovec: libc::iovec,
  /// msghdr for io_uring recvmsg (stored here so it persists)
  msghdr: libc::msghdr,
}

// SAFETY: The iovec/msghdr contain raw pointers that point to data within
// this same struct (addr, buf) or to the buffer owned by this struct.
// The operation is only accessed from the owning thread (thread-per-core model).
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for RecvFrom<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for RecvFrom<B> {}

impl<B> RecvFrom<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self {
      res,
      buf: Some(buf),
      flags: flags.unwrap_or(0),
      // SAFETY: sockaddr_storage, iovec, msghdr are C structs safe to zero-initialize
      addr: unsafe { mem::zeroed() },
      addrlen: mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      iovec: unsafe { mem::zeroed() },
      msghdr: unsafe { mem::zeroed() },
    }
  }
}

/// Result type for recvfrom: (io::Result<bytes_received>, buffer, Option<peer_addr>)
pub type RecvFromResult<B> = (io::Result<i32>, B, Option<SocketAddr>);

impl<B> TypedOp for RecvFrom<B>
where
  B: IoBufMut,
{
  type Result = RecvFromResult<B>;

  fn into_op(&mut self) -> Op {
    let buf = self.buf.as_mut().expect("buffer not available");
    let ptr = buf.as_mut_ptr();
    let len = buf.capacity();

    // Set up iovec pointing to the buffer
    self.iovec.iov_base = ptr as *mut _;
    self.iovec.iov_len = len;

    // Set up msghdr pointing to addr and iovec
    self.msghdr.msg_name = &mut self.addr as *mut _ as *mut _;
    self.msghdr.msg_namelen = self.addrlen;
    self.msghdr.msg_iov = &mut self.iovec as *mut _;
    self.msghdr.msg_iovlen = 1;

    Op::RecvFrom {
      fd: self.res.clone(),
      flags: self.flags,
      buf: RawBuf::new(ptr, len),
      addr: &mut self.addr as *mut _,
      addrlen: &mut self.addrlen as *mut _,
      msghdr: &mut self.msghdr as *mut _,
    }
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let mut buf = self.buf.take().expect("buffer not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), buf, None)
    } else {
      buf.set_len(res as usize);
      // For recvmsg, the actual address length is in msghdr.msg_namelen
      self.addrlen = self.msghdr.msg_namelen;
      let peer_addr = libc_socketaddr_into_std(&self.addr);
      (Ok(res as i32), buf, peer_addr)
    }
  }
}

// ============================================================================
// RenameAt
// ============================================================================

/// Operation to rename a file or directory.
pub struct RenameAt {
  old_dir_res: Resource,
  old_path: CString,
  new_dir_res: Resource,
  new_path: CString,
}

assert_op_max_size!(RenameAt);

impl RenameAt {
  pub(crate) fn new(
    old_dir_res: Resource,
    old_path: CString,
    new_dir_res: Resource,
    new_path: CString,
  ) -> Self {
    Self { old_dir_res, old_path, new_dir_res, new_path }
  }
}

impl TypedOp for RenameAt {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::RenameAt {
      old_dir_fd: self.old_dir_res.clone(),
      old_path: self.old_path.as_ptr(),
      new_dir_fd: self.new_dir_res.clone(),
      new_path: self.new_path.as_ptr(),
    }
  }
}

// ============================================================================
// Send
// ============================================================================

// Note: Using std::marker::Send/Sync because the struct is named `Send`
pub struct Send<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
}

impl<B> Send<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(res: Resource, buf: B, flags: Option<i32>) -> Self {
    Self { res, buf: Some(buf), flags: flags.unwrap_or(0) }
  }
}

impl<B> TypedOp for Send<B>
where
  B: IoBuf,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let buf = self.buf.as_ref().expect("buffer not available");
    let ptr = buf.as_ptr() as *mut u8;
    let len = buf.len();
    Op::Send {
      fd: self.res.clone(),
      flags: self.flags,
      buf: RawBuf::new(ptr, len),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      (Ok(res as i32), buf)
    }
  }
}

// ============================================================================
// SendFile (Unix only)
// ============================================================================

/// Operation to send file data to a socket without copying through userspace.
///
/// This is commonly used for serving static files over network sockets.
#[cfg(unix)]
pub struct SendFile {
  out_fd: Resource,
  in_fd: Resource,
  offset: i64,
  count: usize,
}

#[cfg(unix)]
assert_op_max_size!(SendFile);

#[cfg(unix)]
impl SendFile {
  pub(crate) fn new(
    out_fd: Resource,
    in_fd: Resource,
    offset: Option<i64>,
    count: usize,
  ) -> Self {
    Self {
      out_fd,
      in_fd,
      offset: offset.unwrap_or(0),
      count,
    }
  }
}

#[cfg(unix)]
impl TypedOp for SendFile {
  impl_io_result!(i32);

  fn into_op(&mut self) -> Op {
    Op::SendFile {
      out_fd: self.out_fd.clone(),
      in_fd: self.in_fd.clone(),
      offset: self.offset,
      count: self.count,
    }
  }
}

// ============================================================================
// SendTo
// ============================================================================

// Note: Using std::marker::Send/Sync because the module contains `Send` struct
pub struct SendTo<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  res: Resource,
  buf: Option<B>,
  flags: i32,
  addr: libc::sockaddr_storage,
  addrlen: libc::socklen_t,
  /// iovec for io_uring sendmsg (stored here so it persists)
  iovec: libc::iovec,
  /// msghdr for io_uring sendmsg (stored here so it persists)
  msghdr: libc::msghdr,
}

// SAFETY: The iovec/msghdr contain raw pointers that point to data within
// this same struct (addr, buf) or to the buffer owned by this struct.
// The operation is only accessed from the owning thread (thread-per-core model).
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for SendTo<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for SendTo<B> {}

impl<B> SendTo<B>
where
  B: std::marker::Send + std::marker::Sync,
{
  pub(crate) fn new(res: Resource, buf: B, addr: SocketAddr, flags: Option<i32>) -> Self {
    let storage = std_socketaddr_into_libc(addr);
    let addrlen = if addr.is_ipv4() {
      mem::size_of::<libc::sockaddr_in>()
    } else {
      mem::size_of::<libc::sockaddr_in6>()
    } as libc::socklen_t;
    // SAFETY: iovec and msghdr are C structs safe to zero-initialize
    Self {
      res,
      buf: Some(buf),
      flags: flags.unwrap_or(0),
      addr: storage,
      addrlen,
      iovec: unsafe { mem::zeroed() },
      msghdr: unsafe { mem::zeroed() },
    }
  }
}

impl<B> TypedOp for SendTo<B>
where
  B: IoBuf,
{
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let buf = self.buf.as_ref().expect("buffer not available");
    let ptr = buf.as_ptr() as *mut u8;
    let len = buf.len();

    // Set up iovec pointing to the buffer
    self.iovec.iov_base = ptr as *mut _;
    self.iovec.iov_len = len;

    // Set up msghdr pointing to addr and iovec
    self.msghdr.msg_name = &self.addr as *const _ as *mut _;
    self.msghdr.msg_namelen = self.addrlen;
    self.msghdr.msg_iov = &mut self.iovec as *mut _;
    self.msghdr.msg_iovlen = 1;

    Op::SendTo {
      fd: self.res.clone(),
      flags: self.flags,
      buf: RawBuf::new(ptr, len),
      addr: &self.addr as *const _,
      addrlen: self.addrlen,
      msghdr: &self.msghdr as *const _,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let buf = self.buf.expect("buffer not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), buf)
    } else {
      (Ok(res as i32), buf)
    }
  }
}

// ============================================================================
// Shutdown
// ============================================================================

pub struct Shutdown {
  res: Resource,
  how: i32,
}

assert_op_max_size!(Shutdown);

impl Shutdown {
  pub(crate) fn new(res: Resource, how: i32) -> Self {
    Self { res, how }
  }
}

impl TypedOp for Shutdown {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::Shutdown { fd: self.res.clone(), how: self.how }
  }
}

// ============================================================================
// Socket
// ============================================================================

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
}

impl TypedOp for Socket {
  type Result = io::Result<Resource>;

  fn into_op(&mut self) -> Op {
    Op::Socket {
      domain: self.domain,
      ty: self.ty,
      proto: self.proto,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      let fd = res as RawFd;

      // Set SO_REUSEADDR for stream sockets to allow quick rebind after close
      if self.ty == libc::SOCK_STREAM {
        let optval: libc::c_int = 1;
        // SAFETY: fd is valid, optval is a valid pointer to c_int
        unsafe {
          libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &optval as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
          );
          // Also set SO_REUSEPORT on platforms that support it (BSD/macOS)
          #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
          ))]
          libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEPORT,
            &optval as *const _ as *const libc::c_void,
            mem::size_of::<libc::c_int>() as libc::socklen_t,
          );
        }
      }

      // SAFETY: 'res' is valid fd.
      let resource = unsafe { Resource::from_raw_fd(fd) };
      Ok(resource)
    }
  }
}

// ============================================================================
// Splice (Linux only)
// ============================================================================

/// Operation to splice data between file descriptors (Linux only).
///
/// At least one of `fd_in` or `fd_out` must be a pipe. This enables zero-copy
/// data transfer between a pipe and another file descriptor.
#[cfg(target_os = "linux")]
pub struct Splice {
  fd_in: Resource,
  off_in: i64,
  fd_out: Resource,
  off_out: i64,
  len: u32,
  flags: u32,
}

#[cfg(target_os = "linux")]
assert_op_max_size!(Splice);

#[cfg(target_os = "linux")]
impl Splice {
  pub(crate) fn new(
    fd_in: Resource,
    off_in: Option<i64>,
    fd_out: Resource,
    off_out: Option<i64>,
    len: u32,
    flags: u32,
  ) -> Self {
    Self {
      fd_in,
      off_in: off_in.unwrap_or(-1),
      fd_out,
      off_out: off_out.unwrap_or(-1),
      len,
      flags,
    }
  }
}

#[cfg(target_os = "linux")]
impl TypedOp for Splice {
  impl_io_result!(i32);

  fn into_op(&mut self) -> Op {
    Op::Splice {
      fd_in: self.fd_in.clone(),
      off_in: self.off_in,
      fd_out: self.fd_out.clone(),
      off_out: self.off_out,
      len: self.len,
      flags: self.flags,
    }
  }
}

// ============================================================================
// SymlinkAt
// ============================================================================

pub struct SymlinkAt {
  dir_res: Resource,
  target: CString,
  linkpath: CString,
}

assert_op_max_size!(SymlinkAt);

impl SymlinkAt {
  pub(crate) fn new(
    dir_res: Resource,
    target: CString,
    linkpath: CString,
  ) -> Self {
    Self { dir_res, target, linkpath }
  }
}

impl TypedOp for SymlinkAt {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::SymlinkAt {
      dir_fd: self.dir_res.clone(),
      target: self.target.as_ptr(),
      linkpath: self.linkpath.as_ptr(),
    }
  }
}

// ============================================================================
// Tee (Linux only)
// ============================================================================

#[cfg(target_os = "linux")]
pub struct Tee {
  res_in: Resource,
  res_out: Resource,
  size: u32,
}

#[cfg(target_os = "linux")]
assert_op_max_size!(Tee);

#[cfg(target_os = "linux")]
impl Tee {
  pub(crate) fn new(res_in: Resource, res_out: Resource, size: u32) -> Self {
    Self { res_in, res_out, size }
  }
}

#[cfg(target_os = "linux")]
impl TypedOp for Tee {
  impl_io_result!(i32);

  fn into_op(&mut self) -> Op {
    Op::Tee {
      fd_in: self.res_in.clone(),
      fd_out: self.res_out.clone(),
      size: self.size,
    }
  }
}

// ============================================================================
// Sleep
// ============================================================================

pub struct Sleep {
  duration: Duration,
  #[cfg(target_os = "linux")]
  timespec: libc::timespec,
  #[cfg(target_os = "linux")]
  timer_res: Resource,
  #[cfg(all(unix, not(target_os = "linux")))]
  timer_id: u64,
}

assert_op_max_size!(Sleep);

impl Sleep {
  pub(crate) fn new(duration: Duration) -> Self {
    Self::new_with_id(duration, 0)
  }

  pub(crate) fn new_with_id(
    duration: Duration,
    #[allow(unused)] id: u64,
  ) -> Self {
    #[cfg(target_os = "linux")]
    let timer_fd =
      Self::create_timer_fd(duration).expect("Failed to create timerfd");

    Self {
      duration,
      #[cfg(target_os = "linux")]
      timespec: libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
      },
      #[cfg(target_os = "linux")]
      // SAFETY: timer_fd is valid, just created by create_timer_fd above
      timer_res: unsafe { Resource::from_raw_fd(timer_fd) },
      #[cfg(all(unix, not(target_os = "linux")))]
      timer_id: id,
    }
  }

  #[cfg(target_os = "linux")]
  fn create_timer_fd(duration: Duration) -> io::Result<RawFd> {
    use std::mem::MaybeUninit;

    // Create timerfd
    let fd = syscall!(timerfd_create(
      libc::CLOCK_MONOTONIC,
      libc::TFD_NONBLOCK | libc::TFD_CLOEXEC
    ))?;

    // Set the sleep duration
    // SAFETY: itimerspec is a C struct where all-zeros is a valid representation
    let mut new_value: libc::itimerspec =
      unsafe { MaybeUninit::zeroed().assume_init() };
    new_value.it_value.tv_sec = duration.as_secs() as libc::time_t;
    new_value.it_value.tv_nsec = duration.subsec_nanos() as libc::c_long;
    // it_interval is zero (no repeat)

    syscall!(timerfd_settime(
      fd,
      0,
      &new_value as *const libc::itimerspec,
      std::ptr::null_mut(),
    ))?;

    Ok(fd)
  }

  pub fn duration(&self) -> Duration {
    self.duration
  }

  #[cfg(all(unix, not(target_os = "linux")))]
  pub fn timer_id(&self) -> u64 {
    self.timer_id
  }
}

impl TypedOp for Sleep {
  type Result = io::Result<()>;

  fn into_op(&mut self) -> Op {
    Op::Sleep {
      duration: self.duration,
      #[cfg(target_os = "linux")]
      timer_fd: self.timer_res.clone(),
      #[cfg(target_os = "linux")]
      timespec: &self.timespec as *const libc::timespec,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res == 0 {
      Ok(())
    } else {
      match res.abs() as i32 {
        #[cfg(target_os = "linux")]
        libc::ETIME => Ok(()),
        #[cfg(any(target_os = "freebsd", target_os = "macos"))]
        libc::ETIMEDOUT => Ok(()),
        _ => Err(io::Error::last_os_error()),
      }
    }
  }
}

// ============================================================================
// Timeout (wrapper)
// ============================================================================

/// Error type indicating an operation timed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

impl std::fmt::Display for TimedOut {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "operation timed out")
  }
}

impl std::error::Error for TimedOut {}

/// Wraps an operation with a timeout deadline.
///
/// If the timeout fires before the inner operation completes, the inner operation
/// is cancelled and `Err(TimedOut)` is returned.
///
/// If the inner operation completes before the timeout, the timeout is cancelled
/// and the inner operation's result is returned.
///
/// # Platform Support
///
/// - **Linux (io_uring)**: Uses `IORING_OP_LINK_TIMEOUT` for efficient kernel-native
///   timeout handling. The kernel races the operations and cancels the loser.
/// - **Other Unix (pollingv2)**: Uses userspace timeout coordination via TimeManager.
#[cfg(unix)]
pub struct Timeout<T: TypedOp> {
  inner: T,
  duration: Duration,
  #[cfg(target_os = "linux")]
  timespec: libc::timespec,
}

#[cfg(unix)]
impl<T: TypedOp> Timeout<T> {
  pub(crate) fn new(inner: T, duration: Duration) -> Self {
    Self {
      inner,
      duration,
      #[cfg(target_os = "linux")]
      timespec: libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
      },
    }
  }
}

#[cfg(unix)]
impl<T: TypedOp> TypedOp for Timeout<T> {
  type Result = Result<T::Result, TimedOut>;

  fn into_op(&mut self) -> Op {
    let inner_op = self.inner.into_op();
    Op::Timeout {
      inner: Box::new(inner_op),
      duration: self.duration,
      #[cfg(target_os = "linux")]
      timespec: &self.timespec as *const libc::timespec,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    // -ECANCELED indicates the timeout fired and cancelled the inner operation.
    // Any other result is the inner op completing normally.
    if res == -(libc::ECANCELED as isize) {
      Err(TimedOut)
    } else {
      Ok(self.inner.extract_result(res))
    }
  }
}

// ============================================================================
// Truncate
// ============================================================================

pub struct Truncate {
  res: Resource,
  size: u64,
}

assert_op_max_size!(Truncate);

impl Truncate {
  pub(crate) fn new(res: Resource, size: u64) -> Self {
    Self { res, size }
  }
}

impl TypedOp for Truncate {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::Truncate { fd: self.res.clone(), size: self.size }
  }
}

// ============================================================================
// Watch
// ============================================================================

/// Mask of events to watch for on a file or directory.
///
/// These flags are cross-platform and get translated to the appropriate
/// platform-specific flags (inotify on Linux, EVFILT_VNODE on BSD/macOS).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchMask(u32);

#[cfg(unix)]
impl WatchMask {
  /// File was modified (content changed).
  pub const MODIFY: Self = Self(1 << 0);
  /// File metadata changed (permissions, timestamps, etc.).
  pub const ATTRIB: Self = Self(1 << 1);
  /// File was deleted.
  pub const DELETE: Self = Self(1 << 2);
  /// File was renamed.
  pub const RENAME: Self = Self(1 << 3);
  /// File was extended (size increased).
  pub const EXTEND: Self = Self(1 << 4);

  /// Create a new empty mask.
  pub const fn empty() -> Self {
    Self(0)
  }

  /// Check if the mask contains a specific flag.
  pub const fn contains(self, other: Self) -> bool {
    (self.0 & other.0) == other.0
  }

  /// Combine two masks.
  pub const fn union(self, other: Self) -> Self {
    Self(self.0 | other.0)
  }

  /// Get the raw bits.
  pub const fn bits(self) -> u32 {
    self.0
  }

  /// Create from raw bits.
  pub const fn from_bits(bits: u32) -> Self {
    Self(bits)
  }

  /// Convert to platform-specific flags.
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub(crate) fn to_kqueue_fflags(self) -> u32 {
    let mut fflags = 0u32;
    if self.contains(Self::MODIFY) {
      fflags |= libc::NOTE_WRITE;
    }
    if self.contains(Self::ATTRIB) {
      fflags |= libc::NOTE_ATTRIB;
    }
    if self.contains(Self::DELETE) {
      fflags |= libc::NOTE_DELETE;
    }
    if self.contains(Self::RENAME) {
      fflags |= libc::NOTE_RENAME;
    }
    if self.contains(Self::EXTEND) {
      fflags |= libc::NOTE_EXTEND;
    }
    fflags
  }

  /// Convert from platform-specific flags (kqueue fflags).
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
  ))]
  pub(crate) fn from_kqueue_fflags(fflags: u32) -> Self {
    let mut mask = Self::empty();
    if (fflags & libc::NOTE_WRITE) != 0 {
      mask = mask.union(Self::MODIFY);
    }
    if (fflags & libc::NOTE_ATTRIB) != 0 {
      mask = mask.union(Self::ATTRIB);
    }
    if (fflags & libc::NOTE_DELETE) != 0 {
      mask = mask.union(Self::DELETE);
    }
    if (fflags & libc::NOTE_RENAME) != 0 {
      mask = mask.union(Self::RENAME);
    }
    if (fflags & libc::NOTE_EXTEND) != 0 {
      mask = mask.union(Self::EXTEND);
    }
    mask
  }

  /// Convert to platform-specific flags (inotify).
  #[cfg(target_os = "linux")]
  pub(crate) fn to_inotify_mask(self) -> u32 {
    let mut mask = 0u32;
    if self.contains(Self::MODIFY) {
      mask |= libc::IN_MODIFY;
    }
    if self.contains(Self::ATTRIB) {
      mask |= libc::IN_ATTRIB;
    }
    if self.contains(Self::DELETE) {
      mask |= libc::IN_DELETE_SELF;
    }
    if self.contains(Self::RENAME) {
      mask |= libc::IN_MOVE_SELF;
    }
    // IN_MODIFY covers extend on Linux
    if self.contains(Self::EXTEND) {
      mask |= libc::IN_MODIFY;
    }
    mask
  }

  /// Convert from platform-specific flags (inotify).
  #[cfg(target_os = "linux")]
  pub(crate) fn from_inotify_mask(mask: u32) -> Self {
    let mut result = Self::empty();
    if (mask & libc::IN_MODIFY) != 0 {
      result = result.union(Self::MODIFY);
    }
    if (mask & libc::IN_ATTRIB) != 0 {
      result = result.union(Self::ATTRIB);
    }
    if (mask & libc::IN_DELETE_SELF) != 0 {
      result = result.union(Self::DELETE);
    }
    if (mask & libc::IN_MOVE_SELF) != 0 {
      result = result.union(Self::RENAME);
    }
    result
  }
}

#[cfg(unix)]
impl std::ops::BitOr for WatchMask {
  type Output = Self;
  fn bitor(self, rhs: Self) -> Self {
    self.union(rhs)
  }
}

/// Operation to watch a file or directory for changes.
///
/// This is a one-shot operation: it completes when the file changes,
/// returning what changed.
#[cfg(unix)]
pub struct Watch {
  path: CString,
  mask: WatchMask,
}

#[cfg(unix)]
impl Watch {
  pub(crate) fn new(path: CString, mask: WatchMask) -> Self {
    Self { path, mask }
  }
}

#[cfg(unix)]
impl TypedOp for Watch {
  /// Returns the mask of events that actually occurred.
  type Result = io::Result<WatchMask>;

  fn into_op(&mut self) -> Op {
    Op::Watch {
      path: self.path.as_ptr(),
      mask: self.mask.bits(),
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    if res < 0 {
      Err(io::Error::from_raw_os_error((-res) as i32))
    } else {
      // Result contains the actual events that occurred
      Ok(WatchMask::from_bits(res as u32))
    }
  }
}

// ============================================================================
// WatchStream
// ============================================================================

/// A streaming watch operation that yields multiple file change events.
///
/// Unlike `Watch` which completes after a single event, `WatchStream` continues
/// watching the file and yields events as they occur.
///
/// # Example
///
/// ```no_run
/// use lio::{Lio, api};
/// use lio::api::ops::WatchMask;
///
/// async fn watch_file(lio: &Lio) -> std::io::Result<()> {
///     let mut stream = api::watch_stream("/tmp/myfile.txt", WatchMask::MODIFY | WatchMask::DELETE)
///         .with_lio(lio);
///
///     while let Some(result) = stream.next().await {
///         let events = result?;
///         if events.contains(WatchMask::MODIFY) {
///             println!("File was modified!");
///         }
///         if events.contains(WatchMask::DELETE) {
///             println!("File was deleted!");
///             break; // Stop watching after deletion
///         }
///     }
///     Ok(())
/// }
/// ```
#[cfg(unix)]
pub struct WatchStream {
  path: CString,
  mask: WatchMask,
}

#[cfg(unix)]
impl WatchStream {
  pub(crate) fn new(path: CString, mask: WatchMask) -> Self {
    Self { path, mask }
  }
}

#[cfg(unix)]
impl StreamOp for WatchStream {
  type Item = io::Result<WatchMask>;

  fn into_op(&mut self) -> Op {
    Op::Watch {
      path: self.path.as_ptr(),
      mask: self.mask.bits(),
    }
  }

  fn extract_item(&mut self, res: isize) -> StreamResult<Self::Item> {
    if res < 0 {
      let err = -res as i32;
      // ENOENT means file was deleted - stream is done
      if err == libc::ENOENT {
        return StreamResult::Done;
      }
      StreamResult::Item(Err(io::Error::from_raw_os_error(err)))
    } else {
      let events = WatchMask::from_bits(res as u32);
      // If DELETE event occurred, the stream is done after this item
      if events.contains(WatchMask::DELETE) {
        // Return the delete event but mark that we should stop
        StreamResult::Item(Ok(events))
      } else {
        StreamResult::Item(Ok(events))
      }
    }
  }
}

// ============================================================================
// UnlinkAt
// ============================================================================

/// Operation to remove a file or directory.
pub struct UnlinkAt {
  dir_res: Resource,
  path: CString,
  flags: i32,
}

assert_op_max_size!(UnlinkAt);

impl UnlinkAt {
  pub(crate) fn new(dir_res: Resource, path: CString, flags: i32) -> Self {
    Self { dir_res, path, flags }
  }
}

impl TypedOp for UnlinkAt {
  impl_io_result!();

  fn into_op(&mut self) -> Op {
    Op::UnlinkAt {
      dir_fd: self.dir_res.clone(),
      path: self.path.as_ptr(),
      flags: self.flags,
    }
  }
}

// ============================================================================
// WriteV
// ============================================================================

pub struct WriteV<B: std::marker::Send + std::marker::Sync> {
  res: Resource,
  bufs: Option<B>,
  iovecs: [libc::iovec; MAX_IOV_COUNT],
  iov_count: usize,
}

unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for WriteV<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for WriteV<B> {}

impl<B: std::marker::Send + std::marker::Sync> WriteV<B> {
  pub(crate) fn new(res: Resource, bufs: B) -> Self
  where
    B: IoBufVec,
  {
    let iov_count = bufs.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      bufs: Some(bufs),
      iovecs: unsafe { mem::zeroed() },
      iov_count,
    }
  }
}

impl<B: IoBufVec> TypedOp for WriteV<B> {
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let bufs = self.bufs.as_ref().expect("buffers not available");

    for i in 0..self.iov_count {
      let (ptr, len) = bufs.buf(i);
      self.iovecs[i].iov_base = ptr as *mut _;
      self.iovecs[i].iov_len = len;
    }

    Op::WriteV {
      fd: self.res.clone(),
      buf: RawBuf::empty(),
      iovecs: self.iovecs.as_ptr(),
      iov_count: self.iov_count,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let bufs = self.bufs.expect("buffers not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      (Ok(res as i32), bufs)
    }
  }
}

// ============================================================================
// WriteVAt
// ============================================================================

pub struct WriteVAt<B: std::marker::Send + std::marker::Sync> {
  res: Resource,
  bufs: Option<B>,
  iovecs: [libc::iovec; MAX_IOV_COUNT],
  iov_count: usize,
  offset: i64,
}

unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Send for WriteVAt<B> {}
unsafe impl<B: std::marker::Send + std::marker::Sync> std::marker::Sync for WriteVAt<B> {}

impl<B: std::marker::Send + std::marker::Sync> WriteVAt<B> {
  pub(crate) fn new(res: Resource, bufs: B, offset: i64) -> Self
  where
    B: IoBufVec,
  {
    let iov_count = bufs.buf_count().min(MAX_IOV_COUNT);
    Self {
      res,
      bufs: Some(bufs),
      iovecs: unsafe { mem::zeroed() },
      iov_count,
      offset,
    }
  }
}

impl<B: IoBufVec> TypedOp for WriteVAt<B> {
  type Result = BufResult<i32, B>;

  fn into_op(&mut self) -> Op {
    let bufs = self.bufs.as_ref().expect("buffers not available");

    for i in 0..self.iov_count {
      let (ptr, len) = bufs.buf(i);
      self.iovecs[i].iov_base = ptr as *mut _;
      self.iovecs[i].iov_len = len;
    }

    Op::WriteVAt {
      fd: self.res.clone(),
      buf: RawBuf::empty(),
      iovecs: self.iovecs.as_ptr(),
      iov_count: self.iov_count,
      offset: self.offset,
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let bufs = self.bufs.expect("buffers not available");
    if res < 0 {
      (Err(io::Error::from_raw_os_error((-res) as i32)), bufs)
    } else {
      (Ok(res as i32), bufs)
    }
  }
}
