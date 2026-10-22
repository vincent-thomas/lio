#![allow(
  clippy::undocumented_unsafe_blocks,
  clippy::unnecessary_safety_comment
)]

//! # `lio` C API
//!
//! The C API is built around an opaque `lio_t` handle that wraps a [`Lio`]
//! driver. Each handle is single-threaded; the caller is responsible for
//! ensuring that no two threads call functions on the same handle concurrently.
//!
//! ## Typical usage
//!
//! ```c
//! lio_t *lio = lio_create(1024);  // capacity
//! if (!lio) { /* handle error */ }
//!
//! // Submit an operation
//! lio_timeout(lio, 100, my_callback);
//!
//! // Drive the event loop (non-blocking)
//! while (pending_work) {
//!     lio_tick(lio);
//! }
//!
//! lio_destroy(lio);
//! ```
//!
//! ## File descriptor / handle types
//!
//! `intptr_t` is used for file descriptors / handles so that the same header
//! works on both Unix (32-bit `int` fd) and Windows (pointer-sized `HANDLE`).
//!
//! ## Buffer ownership
//!
//! The caller always owns the buffers it passes in; lio only **borrows** them.
//! An operation that takes `buf`/`buf_len` reads from or writes into that
//! memory and never frees it, so it can come from `malloc`, [`lio_buf_alloc`],
//! the stack, or anywhere else. Until the callback fires, the memory must stay
//! valid and the caller must not touch it. The callback receives the same
//! pointer and the number of bytes in it that hold data: the bytes read for
//! read-like operations (0 on error), or `buf_len` for write-like ones.
//!
//! If the handle is destroyed while an operation is in flight, its callback
//! never fires and the buffer simply stays with the caller; it must remain
//! valid until [`lio_destroy`] returns.
//!
//! Memory lio allocates and hands out has exactly one release function, which
//! takes only the pointer: [`lio_buf_free`] for bytes, [`lio_dir_entries_free`]
//! for directory entries and [`lio_sockaddr_free`] for addresses.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{
  mem::{self, MaybeUninit},
  net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
  ptr::{self, NonNull},
  time::Duration,
};

use crate::{
  IoBuf, IoBufMut, Lio,
  api::{
    self, FileStat, FileType, ReadDirBuf, RecvFlags, SendFlags, ShutdownHow,
    SockDomain, SockProto, SockType, resource::Resource,
  },
  backend::{
    impls::sockaddr::std_socketaddr_into_libc, op::is_valid_file_offset,
  },
};

#[cfg(unix)]
use std::os::fd::RawFd;

// ─── Watch mask constants ─────────────────────────────────────────────────────

/// Watch for file content modifications
pub const WATCH_MODIFY: u32 = 1;
/// Watch for attribute changes (permissions, ownership, etc.)
pub const WATCH_ATTRIB: u32 = 2;
/// Watch for file deletion
pub const WATCH_DELETE: u32 = 4;
/// Watch for file rename
pub const WATCH_RENAME: u32 = 8;
/// Watch for file size extension (BSD/macOS)
pub const WATCH_EXTEND: u32 = 16;

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default)]
pub struct lio_file_stat_t {
  pub file_type: libc::c_int,
  pub size: u64,
  pub permissions: u32,
  pub mode: u32,
  pub nlink: u64,
  pub uid: u32,
  pub gid: u32,
}

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default)]
pub struct lio_dir_entry_t {
  pub name_offset: u32,
  pub name_len: u16,
  pub file_type: libc::c_int,
  pub has_ino: libc::c_int,
  pub ino: u64,
}

// ─── Opaque handle ────────────────────────────────────────────────────────────

/// Opaque lio driver handle.  Create with [`lio_create`], destroy with
/// [`lio_destroy`].  Not thread-safe; use one handle per thread.
#[allow(non_camel_case_types)]
pub struct lio_handle_t {
  inner: Lio,
}

/// Cast a raw `*mut lio_handle_t` back to `&mut lio_handle_t`.
///
/// # Safety
/// `ptr` must be non-null and point to a valid, live `lio_handle_t` returned by
/// `lio_create`.
#[inline]
unsafe fn handle(ptr: *mut lio_handle_t) -> &'static mut lio_handle_t {
  // SAFETY: caller guarantees validity
  unsafe { &mut *ptr }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Convert a C `intptr_t` to a borrowed `Resource` (won't close on drop).
///
/// # Safety
///
/// `fd` must be a valid, open file descriptor that outlives the Resource.
#[cfg(unix)]
unsafe fn fd_to_borrowed_resource(fd: libc::intptr_t) -> Resource {
  // SAFETY: caller guarantees fd is valid and will outlive this Resource
  unsafe { Resource::borrow(fd as RawFd) }
}

/// Convert a `Resource` to a C `intptr_t`.
#[cfg(unix)]
fn resource_to_fd(r: &Resource) -> libc::intptr_t {
  use std::os::fd::AsRawFd;
  r.as_raw_fd() as libc::intptr_t
}

/// Allocate `len` zeroed bytes.
///
/// A convenience only: operations borrow their buffers, so any memory works.
/// Release the result with [`lio_buf_free`], never `free`.
///
/// Returns null if the allocation fails.
#[unsafe(no_mangle)]
pub extern "C" fn lio_buf_alloc(len: usize) -> *mut u8 {
  // At least one byte, so null always means failure.
  unsafe { libc::calloc(len.max(1), 1) }.cast()
}

/// Free a byte buffer from [`lio_buf_alloc`] or one lio handed out, such as the
/// names buffer of [`lio_readdir`]. Null is ignored.
///
/// Do not pass it memory from anywhere else; lio never frees buffers it
/// borrowed for an operation, so those stay the caller's to release.
///
/// # Safety
/// `buf` must be null or such a buffer, not freed before, and must not be used
/// after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_buf_free(buf: *mut u8) {
  unsafe { libc::free(buf.cast()) };
}

/// A byte count as the C API's `int` result code.
///
/// A count above `INT_MAX` cannot be represented and saturates; callbacks
/// that hand a buffer back still report the real count in `len`.
fn ffi_count(n: usize) -> i32 {
  i32::try_from(n).unwrap_or(i32::MAX)
}

/// A buffer the C caller owns and lends to one operation.
///
/// lio reads from or fills the memory but never frees it: dropping a `CBuf`,
/// whether the operation completed or [`lio_destroy`] discarded it in flight,
/// leaves the allocation to C, whichever allocator it came from.
struct CBuf {
  /// The caller's pointer, handed back unchanged. Null only if `cap` is 0.
  ptr: *mut u8,
  /// Bytes lent.
  cap: usize,
  /// Leading bytes that hold data, and so are initialized.
  len: usize,
}

// SAFETY: a `CBuf` is only a pointer and lengths. The caller of the operation
// that holds it guarantees the memory is valid and untouched by anything else
// until the operation ends, and memory has no thread affinity.
unsafe impl Send for CBuf {}
// SAFETY: as for `Send`; `&CBuf` only exposes the bytes for reading.
unsafe impl Sync for CBuf {}

impl CBuf {
  /// Lends `len` initialized bytes at `ptr` for an operation to read from.
  ///
  /// Returns `None` if `ptr` is null but `len` is not 0.
  ///
  /// # Safety
  /// Unless `len` is 0, `ptr` must be valid for reads of `len` initialized
  /// bytes, and for writes if the operation fills the buffer, until the
  /// operation ends. Nothing else may access the bytes in that time.
  unsafe fn filled(ptr: *mut u8, len: usize) -> Option<Self> {
    (!ptr.is_null() || len == 0).then_some(Self { ptr, cap: len, len })
  }

  /// Lends `cap` bytes at `ptr`, which need not be initialized, for an
  /// operation to fill.
  ///
  /// Returns `None` if `ptr` is null but `cap` is not 0.
  ///
  /// # Safety
  /// Unless `cap` is 0, `ptr` must be valid for reads and writes of `cap`
  /// bytes until the operation ends. Nothing else may access them in that
  /// time.
  unsafe fn empty(ptr: *mut u8, cap: usize) -> Option<Self> {
    (!ptr.is_null() || cap == 0).then_some(Self { ptr, cap, len: 0 })
  }

  /// The start of the region, which slices need non-null even when empty.
  fn base(&self) -> *mut u8 {
    if self.ptr.is_null() { NonNull::dangling().as_ptr() } else { self.ptr }
  }
}

impl IoBuf for CBuf {
  fn as_slice(&self) -> &[u8] {
    // SAFETY: `base` is non-null, and the first `len` bytes are initialized and
    // lent for as long as `self` lives (or `len` is 0).
    unsafe { std::slice::from_raw_parts(self.base(), self.len) }
  }
}

impl IoBufMut for CBuf {
  fn as_uninit_mut(&mut self) -> &mut [MaybeUninit<u8>] {
    // SAFETY: `base` is non-null, and `cap` bytes are lent exclusively for as
    // long as `self` lives (or `cap` is 0). `MaybeUninit<u8>` permits
    // uninitialized contents.
    unsafe { std::slice::from_raw_parts_mut(self.base().cast(), self.cap) }
  }

  unsafe fn set_len(&mut self, len: usize) {
    assert!(len <= self.cap);
    self.len = len;
  }
}

/// Lends a C buffer holding `len` bytes of data, or reports `EINVAL` through
/// `callback` if `buf` is null with a nonzero length.
///
/// # Safety
/// As [`CBuf::filled`].
unsafe fn lend_filled(
  buf: *mut u8,
  len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) -> Option<CBuf> {
  let lent = unsafe { CBuf::filled(buf, len) };
  if lent.is_none() {
    callback(-libc::EINVAL, buf, 0);
  }
  lent
}

/// Lends a C buffer of `cap` bytes to fill, or reports `EINVAL` through
/// `callback` if `buf` is null with a nonzero capacity.
///
/// # Safety
/// As [`CBuf::empty`].
unsafe fn lend_empty(
  buf: *mut u8,
  cap: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) -> Option<CBuf> {
  let lent = unsafe { CBuf::empty(buf, cap) };
  if lent.is_none() {
    callback(-libc::EINVAL, buf, 0);
  }
  lent
}

/// Hands a lent buffer back to C with the operation's result: the caller's
/// pointer and the number of bytes in it that hold data.
fn return_buf(
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
  res: std::io::Result<usize>,
  buf: CBuf,
) {
  let code = match res {
    Ok(n) => ffi_count(n),
    Err(e) => -e.raw_os_error().unwrap_or(1),
  };
  callback(code, buf.ptr, buf.len);
}

/// Free a socket address allocated by lio, such as the address returned by
/// [`lio_accept`] or [`lio_recvfrom`]. Null is ignored.
///
/// # Safety
/// `addr` must be null or a pointer returned by lio, not freed before, and
/// must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_sockaddr_free(
  addr: *const libc::sockaddr_storage,
) {
  if !addr.is_null() {
    drop(unsafe { Box::from_raw(addr.cast_mut()) });
  }
}

/// Free a directory entry array returned by [`lio_readdir`]. Null is ignored.
///
/// # Safety
/// `entries` must be null or an array returned by [`lio_readdir`], not freed
/// before. The entries must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_dir_entries_free(entries: *mut lio_dir_entry_t) {
  unsafe { libc::free(entries.cast()) };
}

/// Copies `items` into a `malloc` allocation, the kind [`lio_buf_free`] and
/// [`lio_dir_entries_free`] release, so freeing needs only the pointer.
///
/// Returns null for an empty slice, and `None` if the allocation fails.
fn malloc_copy<T: Copy>(items: &[T]) -> Option<*mut T> {
  // `malloc` aligns for every fundamental type, which covers 8 bytes.
  const { assert!(mem::align_of::<T>() <= 8) };
  if items.is_empty() {
    return Some(ptr::null_mut());
  }
  let out = unsafe { libc::malloc(mem::size_of_val(items)) }.cast::<T>();
  if out.is_null() {
    return None;
  }
  // SAFETY: `out` is a fresh, suitably aligned allocation of `items.len()`
  // elements.
  unsafe { ptr::copy_nonoverlapping(items.as_ptr(), out, items.len()) };
  Some(out)
}

fn file_type_to_ffi(file_type: FileType) -> libc::c_int {
  match file_type {
    FileType::Unknown => 0,
    FileType::File => 1,
    FileType::Directory => 2,
    FileType::Symlink => 3,
    FileType::BlockDevice => 4,
    FileType::CharDevice => 5,
    FileType::Fifo => 6,
    FileType::Socket => 7,
  }
}

fn file_stat_to_ffi(stat: FileStat) -> lio_file_stat_t {
  lio_file_stat_t {
    file_type: file_type_to_ffi(stat.file_type),
    size: stat.size,
    permissions: stat.permissions,
    mode: stat.mode,
    nlink: stat.nlink,
    uid: stat.uid,
    gid: stat.gid,
  }
}

unsafe fn cstr_to_owned(ptr: *const libc::c_char) -> Option<std::ffi::CString> {
  if ptr.is_null() {
    return None;
  }
  Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_owned())
}

/// Decode a C address. Non-null pointers must be readable for addr_len bytes,
/// but need not be aligned.
fn sockaddr_to_socketaddr(
  raw_addr_ptr: *const libc::sockaddr,
  addr_len: libc::socklen_t,
) -> Option<SocketAddr> {
  if raw_addr_ptr.is_null() {
    return None;
  }

  // On some platforms sa_family is not the first byte (sa_len precedes it).
  // Check the field's complete byte range before reading even the family.
  let family_offset = mem::offset_of!(libc::sockaddr, sa_family);
  if (addr_len as usize) < family_offset + mem::size_of::<libc::sa_family_t>() {
    return None;
  }
  // SAFETY: The field lies within the caller-provided readable range. The
  // address may be unaligned, so do not dereference a sockaddr reference.
  let family = unsafe {
    (raw_addr_ptr.cast::<u8>().add(family_offset) as *const libc::sa_family_t)
      .read_unaligned()
  } as i32;

  match family {
    libc::AF_INET => {
      if addr_len < mem::size_of::<libc::sockaddr_in>() as libc::socklen_t {
        return None;
      }
      let raw_v4 = raw_addr_ptr as *const libc::sockaddr_in;
      // SAFETY: The full structure is readable, but alignment is not guaranteed.
      let sockaddr_v4 = unsafe { raw_v4.read_unaligned() };
      let port = u16::from_be(sockaddr_v4.sin_port);
      let ipv4_addr = Ipv4Addr::from(u32::from_be(sockaddr_v4.sin_addr.s_addr));
      Some(SocketAddr::V4(SocketAddrV4::new(ipv4_addr, port)))
    }
    libc::AF_INET6 => {
      if addr_len < mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t {
        return None;
      }
      let raw_v6 = raw_addr_ptr as *const libc::sockaddr_in6;
      // SAFETY: The full structure is readable, but alignment is not guaranteed.
      let sockaddr_v6 = unsafe { raw_v6.read_unaligned() };
      let port = u16::from_be(sockaddr_v6.sin6_port);
      let ipv6_addr = Ipv6Addr::from(sockaddr_v6.sin6_addr.s6_addr);
      Some(SocketAddr::V6(SocketAddrV6::new(
        ipv6_addr,
        port,
        u32::from_be(sockaddr_v6.sin6_flowinfo),
        u32::from_be(sockaddr_v6.sin6_scope_id),
      )))
    }
    _ => None,
  }
}

// ─── Lifecycle ───────────────────────────────────────────────────────────────

/// Create a new lio driver with the given operation capacity.
///
/// Returns a non-null opaque pointer on success, or null on failure.
/// The caller owns the returned handle and must pass it to [`lio_destroy`]
/// when done.
#[unsafe(no_mangle)]
pub extern "C" fn lio_create(capacity: libc::c_uint) -> *mut lio_handle_t {
  match Lio::new(capacity as usize) {
    Ok(inner) => Box::into_raw(Box::new(lio_handle_t { inner })),
    Err(_) => ptr::null_mut(),
  }
}

/// Destroy a lio handle created by [`lio_create`].
///
/// After this call `lio` is invalid.
///
/// Operations still in flight are cancelled and their callbacks never fire.
/// lio does not free their buffers: they stay owned by the caller, and must
/// remain valid until this call returns. (If a backend cannot confirm in time
/// that the kernel let go of an operation, lio leaks its own state rather than
/// free it under the kernel; a caller that cannot rule this out should not
/// reuse the buffers of operations it destroyed in flight.)
///
/// # Safety
/// `lio` must have been returned by [`lio_create`] and must not be used
/// after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_destroy(lio: *mut lio_handle_t) {
  if !lio.is_null() {
    // SAFETY: caller guarantees lio is valid and no longer used
    drop(unsafe { Box::from_raw(lio) });
  }
}

/// Drive the event loop once (non-blocking).
///
/// Returns the number of operations that completed, or -1 on error.
///
/// # Safety
/// `lio` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_tick(lio: *mut lio_handle_t) -> libc::c_int {
  // SAFETY: caller guarantees lio is valid per fn contract
  match unsafe { handle(lio) }.inner.try_run() {
    Ok(n) => n as libc::c_int,
    Err(_) => -1,
  }
}

// ─── Socket / fd operations ───────────────────────────────────────────────────

fn errno_result(res: std::io::Result<impl Sized>) -> libc::c_int {
  match res {
    Ok(_) => 0,
    Err(e) => -e.raw_os_error().unwrap_or(1),
  }
}

fn socket_parts_from_ffi(
  domain: libc::c_int,
  ty: libc::c_int,
  proto: libc::c_int,
) -> Result<(SockDomain, SockType, SockProto), libc::c_int> {
  let domain = if domain == 1 || domain == libc::AF_INET {
    SockDomain::IPV4
  } else if domain == 2 || domain == libc::AF_INET6 {
    SockDomain::IPV6
  } else if domain == 3 || domain == libc::AF_UNIX {
    SockDomain::UNIX
  } else {
    return Err(libc::EAFNOSUPPORT);
  };
  let ty = if ty == 1 || ty == libc::SOCK_STREAM {
    SockType::STREAM
  } else if ty == 2 || ty == libc::SOCK_DGRAM {
    SockType::DGRAM
  } else {
    return Err(libc::EINVAL);
  };
  let proto = if proto == 0 {
    SockProto::DEFAULT
  } else if proto == 1 || proto == libc::IPPROTO_TCP {
    SockProto::TCP
  } else if proto == 2 || proto == libc::IPPROTO_UDP {
    SockProto::UDP
  } else {
    return Err(libc::EPROTONOSUPPORT);
  };

  if matches!(domain, SockDomain::UNIX) && !matches!(proto, SockProto::DEFAULT)
  {
    return Err(libc::EINVAL);
  }
  if !matches!(
    (ty, proto),
    (SockType::STREAM, SockProto::DEFAULT)
      | (SockType::STREAM, SockProto::TCP)
      | (SockType::DGRAM, SockProto::DEFAULT)
      | (SockType::DGRAM, SockProto::UDP)
  ) {
    return Err(libc::EINVAL);
  }

  Ok((domain, ty, proto))
}

/// Shut down part of a full-duplex connection.
///
/// # Safety
/// `lio` must be a valid handle, and `fd` must be a valid open socket that
/// remains open until the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_shutdown(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  how: libc::c_int,
  callback: extern "C" fn(libc::c_int),
) {
  let how = match how {
    x if x == libc::SHUT_RD => ShutdownHow::Read,
    x if x == libc::SHUT_WR => ShutdownHow::Write,
    x if x == libc::SHUT_RDWR => ShutdownHow::Both,
    _ => {
      callback(-libc::EINVAL);
      return;
    }
  };
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::shutdown(&resource, how)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| callback(errno_result(res)));
}

/// Synchronize a file's in-core state with the storage device.
///
/// # Safety
/// `lio` must be a valid handle, and `fd` must be a valid open file descriptor
/// that remains open until the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_fsync(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  callback: extern "C" fn(libc::c_int),
) {
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::fsync(&resource)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| callback(errno_result(res)));
}

/// Bind a socket to an address.
///
/// # Safety
/// `lio` must be a valid handle, and `fd` must be a valid open socket. If
/// `sock` is non-null, it must point to a readable socket address of at least
/// `sock_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_bind(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  sock: *const libc::sockaddr,
  sock_len: libc::socklen_t,
  callback: extern "C" fn(libc::c_int),
) {
  let addr = match sockaddr_to_socketaddr(sock, sock_len) {
    Some(a) => a,
    None => {
      callback(-libc::EINVAL);
      return;
    }
  };
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::bind(&resource, addr)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| callback(errno_result(res)));
}

/// Listen for connections on a socket.
///
/// # Safety
/// `lio` must be a valid handle, and `fd` must be a valid open socket that
/// remains open until the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_listen(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  backlog: libc::c_int,
  callback: extern "C" fn(libc::c_int),
) {
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::listen(&resource, backlog)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| callback(errno_result(res)));
}

// /// Shut down part of a full-duplex connection.
// ///
// /// - `fd`: Socket file descriptor
// /// - `how`: `SHUT_RD`=0, `SHUT_WR`=1, `SHUT_RDWR`=2
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be a valid handle and `fd` a valid socket.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_shutdown(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   how: libc::c_int,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::shutdown(&resource, how)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     });
// }

// /// Synchronize a file's in-core state with the storage device.
// ///
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be a valid handle and `fd` a valid file descriptor.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_fsync(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::fsync(&resource).with_lio(&unsafe { handle(lio) }.inner).when_done(
//     move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     },
//   );
// }

// /// Truncate a file to `len` bytes.
// ///
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be a valid handle and `fd` a valid file descriptor.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_truncate(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   len: u64,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::truncate(&resource, len)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     });
// }

/// Write data to `fd` at the absolute file `offset`.  Use [`lio_write`] to
/// write at the current position.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: bytes written (or negative errno), the
///   same `buf`, and `len = buf_len`; `-EINVAL` without writing when `offset`
///   exceeds `INT64_MAX`
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid open file descriptor; and `buf`
/// must point to `buf_len` initialized bytes (it may be null if `buf_len` is
/// 0) that stay valid and untouched until the callback fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_write_at(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  offset: u64,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  if !is_valid_file_offset(Some(offset)) {
    callback(-libc::EINVAL, buf, buf_len);
    return;
  }
  // SAFETY: the caller lends `buf_len` initialized bytes per fn contract.
  let Some(buf) = (unsafe { lend_filled(buf, buf_len, callback) }) else {
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::write_at(&resource, buf, offset)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Read from `fd` at the absolute file `offset` into `buf`.  Use [`lio_read`]
/// to read from the current position.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: bytes read (or negative errno), the same
///   `buf`, and `len` = bytes read (0 on error); `-EINVAL` without reading
///   when `offset` exceeds `INT64_MAX`
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid open file descriptor; and `buf`
/// must point to `buf_len` writable bytes, which need not be initialized (it
/// may be null if `buf_len` is 0), that stay valid and untouched until the
/// callback fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_read_at(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  offset: u64,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  if !is_valid_file_offset(Some(offset)) {
    callback(-libc::EINVAL, buf, 0);
    return;
  }
  // SAFETY: the caller lends `buf_len` writable bytes per fn contract.
  let Some(buf) = (unsafe { lend_empty(buf, buf_len, callback) }) else {
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::read_at(&resource, buf, offset)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Create a socket.
///
/// - `callback(result)`: new socket fd (`intptr_t`) on success, negative errno
///   on error
///
/// # Safety
/// `lio` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_socket(
  lio: *mut lio_handle_t,
  domain: libc::c_int,
  ty: libc::c_int,
  proto: libc::c_int,
  callback: extern "C" fn(libc::intptr_t),
) {
  let (domain, ty, proto) = match socket_parts_from_ffi(domain, ty, proto) {
    Ok(parts) => parts,
    Err(errno) => {
      callback(-(errno as libc::intptr_t));
      return;
    }
  };

  // SAFETY: caller guarantees lio is valid per fn contract
  api::socket(domain, ty, proto)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(r) => {
          let fd = resource_to_fd(&r);
          // Prevent Resource from being dropped (which would close the fd).
          // C caller now owns the fd and is responsible for closing it.
          std::mem::forget(r);
          fd
        }
        Err(e) => -e.raw_os_error().unwrap_or(1) as libc::intptr_t,
      });
    });
}

// /// Bind a socket to an address.
// ///
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; `sock` must point to `sock_len` bytes of a valid
// /// `sockaddr`.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_bind(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   sock: *const libc::sockaddr,
//   sock_len: libc::socklen_t,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   let addr = match sockaddr_to_socketaddr(sock, sock_len) {
//     Some(a) => a,
//     None => {
//       callback(-libc::EINVAL);
//       return;
//     }
//   };
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::bind(&resource, addr).with_lio(&unsafe { handle(lio) }.inner).when_done(
//     move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     },
//   );
// }

/// Accept a connection.
///
/// - `callback(result, addr)`: new socket fd on success (negative errno on
///   error); `addr` is the peer address, allocated by lio, which the caller
///   must release with [`lio_sockaddr_free`] — or null on error.
///
/// # Safety
/// `lio` must be valid; `fd` must be a listening socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_accept(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  callback: extern "C" fn(libc::intptr_t, *const libc::sockaddr_storage),
) {
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::accept(&resource).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |res| {
      let (code, addr_ptr) = match res {
        Ok((new_res, addr)) => {
          let fd = resource_to_fd(&new_res);
          // Prevent Resource from being dropped (which would close the fd).
          // C caller now owns the fd and is responsible for closing it.
          std::mem::forget(new_res);
          (
            fd,
            Box::into_raw(Box::new(std_socketaddr_into_libc(addr))) as *const _,
          )
        }
        Err(e) => {
          (-e.raw_os_error().unwrap_or(1) as libc::intptr_t, ptr::null())
        }
      };
      callback(code, addr_ptr);
    },
  );
}

// /// Listen for connections on a socket.
// ///
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` and `fd` must be valid.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_listen(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   backlog: libc::c_int,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::listen(&resource, backlog)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     });
// }

/// Send data on a connected socket.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: bytes sent (or negative errno), the same
///   `buf`, and `len = buf_len`
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid open socket; and `buf` must point
/// to `buf_len` initialized bytes (it may be null if `buf_len` is 0) that stay
/// valid and untouched until the callback fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_send(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: libc::c_int,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: the caller lends `buf_len` initialized bytes per fn contract.
  let Some(buf) = (unsafe { lend_filled(buf, buf_len, callback) }) else {
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::send(
    &resource,
    buf,
    Some(SendFlags::from_bits(flags).unwrap_or(SendFlags::EMPTY)),
  )
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Receive data from a socket.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: bytes received (or negative errno), the
///   same `buf`, and `len` = bytes received (0 on error)
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid open socket; and `buf` must point
/// to `buf_len` writable bytes, which need not be initialized (it may be null
/// if `buf_len` is 0), that stay valid and untouched until the callback fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_recv(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: libc::c_int,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: the caller lends `buf_len` writable bytes per fn contract.
  let Some(buf) = (unsafe { lend_empty(buf, buf_len, callback) }) else {
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::recv(
    &resource,
    buf,
    Some(RecvFlags::from_bits(flags).unwrap_or(RecvFlags::EMPTY)),
  )
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Receive data and sender address from a socket.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len, addr)`: bytes received (or negative errno),
///   the same `buf`, `len` = bytes received (0 on error), and the sender
///   address, which the caller must release with [`lio_sockaddr_free`], or
///   null
///
/// # Safety
/// `lio` must be a valid handle; `fd` must be a valid open socket; and `buf`
/// must point to `buf_len` writable bytes, which need not be initialized (it
/// may be null if `buf_len` is 0), that stay valid and untouched until the
/// callback fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_recvfrom(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: libc::c_int,
  callback: extern "C" fn(
    libc::c_int,
    *mut u8,
    usize,
    *const libc::sockaddr_storage,
  ),
) {
  let Some(buf) = (unsafe { CBuf::empty(buf, buf_len) }) else {
    callback(-libc::EINVAL, buf, 0, ptr::null());
    return;
  };
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::recvfrom(
    &resource,
    buf,
    Some(RecvFlags::from_bits(flags).unwrap_or(RecvFlags::EMPTY)),
  )
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |(res, buf, addr)| {
    let (code, addr_ptr): (libc::c_int, *const libc::sockaddr_storage) =
      match res {
        Ok(n) => {
          let addr_ptr = addr
            .map(|addr| {
              Box::into_raw(Box::new(std_socketaddr_into_libc(addr)))
                as *const _
            })
            .unwrap_or(ptr::null());
          (ffi_count(n), addr_ptr)
        }
        Err(e) => (-e.raw_os_error().unwrap_or(1), ptr::null()),
      };
    callback(code, buf.ptr, buf.len, addr_ptr);
  });
}

// /// Close a file descriptor.
// ///
// /// The fd must be a C-owned fd, not a [`Resource`] managed by Rust.
// ///
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; `fd` must be a valid open file descriptor.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_close(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::close(fd as RawFd).with_lio(&unsafe { handle(lio) }.inner).when_done(
//     move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     },
//   );
// }

/// Wait for `millis` milliseconds.
///
/// - `callback(result)`: 0 on success
///
/// # Safety
/// `lio` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_sleep(
  lio: *mut lio_handle_t,
  millis: libc::c_uint,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees lio is valid per fn contract
  api::sleep(Duration::from_millis(millis as u64))
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    });
}

/// Runs an interval and invokes `callback` for every tick until the lio handle is destroyed.
///
/// # Safety
/// `lio` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_interval(
  lio: *mut lio_handle_t,
  millis: libc::c_uint,
  callback: extern "C" fn(libc::c_int),
) {
  let receiver = api::interval(Duration::from_millis(millis as u64))
    .with_lio(&unsafe { handle(lio) }.inner)
    .send();
  std::thread::spawn(move || {
    while let Ok(res) = receiver.recv() {
      callback(errno_result(res));
    }
  });
}

/// A no-op that completes immediately.
///
/// Useful for waking up the event loop or testing.
///
/// - `callback(result)`: always 0
///
/// # Safety
/// `lio` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_nop(
  lio: *mut lio_handle_t,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees lio is valid per fn contract
  api::nop().with_lio(&unsafe { handle(lio) }.inner).when_done(move |res| {
    callback(match res {
      Ok(_) => 0,
      Err(e) => -e.raw_os_error().unwrap_or(1),
    });
  });
}

/// Connect a socket to an address.
///
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid socket; `addr` must point to a
/// valid sockaddr.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_connect(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  addr: *const libc::sockaddr,
  addr_len: libc::socklen_t,
  callback: extern "C" fn(libc::c_int),
) {
  let socket_addr = match sockaddr_to_socketaddr(addr, addr_len) {
    Some(a) => a,
    None => {
      callback(-libc::EINVAL);
      return;
    }
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::connect(&resource, socket_addr)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    });
}

/// Send data to a specific address (UDP).
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: bytes sent (or negative errno), the same
///   `buf`, and `len = buf_len`
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid open socket; `buf` must point to
/// `buf_len` initialized bytes (it may be null if `buf_len` is 0) that stay
/// valid and untouched until the callback fires; and a non-null `addr` must be
/// readable for `addr_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_sendto(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: libc::c_int,
  addr: *const libc::sockaddr,
  addr_len: libc::socklen_t,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: the caller lends `buf_len` initialized bytes per fn contract.
  let Some(buf) = (unsafe { lend_filled(buf, buf_len, callback) }) else {
    return;
  };
  let Some(socket_addr) = sockaddr_to_socketaddr(addr, addr_len) else {
    return_buf(
      callback,
      Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
      buf,
    );
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  crate::api::io::Io::from_op(api::ops::Send::new(
    resource.clone(),
    buf,
    Some(socket_addr),
    Some(SendFlags::from_bits(flags).unwrap_or(SendFlags::EMPTY)),
  ))
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Open a file relative to a directory fd.
///
/// - `path`: null-terminated path string
/// - `flags`: open flags (O_RDONLY, O_WRONLY, O_RDWR, O_CREAT, etc.)
/// - `mode`: file mode if O_CREAT is set
/// - `callback(result)`: new fd on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; `dir_fd` must be a valid directory fd or AT_FDCWD;
/// `path` must be a valid null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_openat(
  lio: *mut lio_handle_t,
  dir_fd: libc::intptr_t,
  path: *const libc::c_char,
  flags: libc::c_int,
  mode: libc::mode_t,
  callback: extern "C" fn(libc::intptr_t),
) {
  if path.is_null() {
    callback(-libc::EINVAL as libc::intptr_t);
    return;
  }
  // SAFETY: caller guarantees path is valid null-terminated string
  let c_path = unsafe { std::ffi::CStr::from_ptr(path) };
  let path_owned = std::ffi::CString::from(c_path);

  // SAFETY: caller guarantees dir_fd is valid per fn contract
  let dir_resource = unsafe { fd_to_borrowed_resource(dir_fd) };
  #[allow(clippy::unnecessary_cast)]
  let mode = mode as u32;
  let Some(flags) = open_flags_from_posix(flags) else {
    callback(-libc::EINVAL as libc::intptr_t);
    return;
  };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::openat(&dir_resource, path_owned, flags, mode)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(r) => {
          let fd = resource_to_fd(&r);
          std::mem::forget(r);
          fd
        }
        Err(e) => -e.raw_os_error().unwrap_or(1) as libc::intptr_t,
      });
    });
}

/// Read from fd at current position.
///
/// Equivalent to [`lio_read_at`] with `offset = -1`, with the same buffer
/// rules and callback.
///
/// # Safety
/// Same requirements as [`lio_read_at`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_read(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: the caller lends `buf_len` writable bytes per fn contract.
  let Some(buf) = (unsafe { lend_empty(buf, buf_len, callback) }) else {
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::read(&resource, buf)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Write to fd at current position.
///
/// Equivalent to [`lio_write_at`] with `offset = -1`, with the same buffer
/// rules and callback.
///
/// # Safety
/// Same requirements as [`lio_write_at`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_write(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: the caller lends `buf_len` initialized bytes per fn contract.
  let Some(buf) = (unsafe { lend_filled(buf, buf_len, callback) }) else {
    return;
  };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::write(&resource, buf)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

// /// Apply or remove an advisory lock on a file.
// ///
// /// - `operation`: LOCK_SH, LOCK_EX, LOCK_UN, optionally OR'd with LOCK_NB
// /// - `callback(result)`: 0 on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; `fd` must be a valid file descriptor.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_flock(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   operation: libc::c_int,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::flock(&resource, operation)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(_) => 0,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     });
// }

/// Create a hard or symbolic link.
///
/// - `kind`: 0 for hard links, 1 for symbolic links
/// - `source_dir_fd`: Directory fd for source path (ignored for symbolic links)
/// - `source_path`: Existing path or symlink target (null-terminated)
/// - `new_dir_fd`: Directory fd for new path
/// - `new_path`: New link path (null-terminated)
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; paths must be valid null-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_linkat(
  lio: *mut lio_handle_t,
  kind: libc::c_int,
  source_dir_fd: libc::intptr_t,
  source_path: *const libc::c_char,
  new_dir_fd: libc::intptr_t,
  new_path: *const libc::c_char,
  callback: extern "C" fn(libc::c_int),
) {
  if source_path.is_null() || new_path.is_null() {
    callback(-libc::EINVAL);
    return;
  }
  let Ok(kind) = link_kind_from_ffi(kind) else {
    callback(-libc::EINVAL);
    return;
  };
  let source_path_cstr =
    unsafe { std::ffi::CStr::from_ptr(source_path) }.to_owned();
  let new_path_cstr = unsafe { std::ffi::CStr::from_ptr(new_path) }.to_owned();
  let source_dir_res = unsafe { fd_to_borrowed_resource(source_dir_fd) };
  let new_dir_res = unsafe { fd_to_borrowed_resource(new_dir_fd) };
  api::linkat(
    &source_dir_res,
    source_path_cstr,
    &new_dir_res,
    new_path_cstr,
    kind,
  )
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |res| {
    callback(match res {
      Ok(_) => 0,
      Err(e) => -e.raw_os_error().unwrap_or(1),
    });
  });
}

/// Remove a file or directory.
///
/// - `dir_fd`: Directory fd (or AT_FDCWD for current directory)
/// - `path`: Path to remove (null-terminated)
/// - `flags`: 0 for files, AT_REMOVEDIR for directories
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; `path` must be a valid null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_unlinkat(
  lio: *mut lio_handle_t,
  dir_fd: libc::intptr_t,
  path: *const libc::c_char,
  flags: libc::c_int,
  callback: extern "C" fn(libc::c_int),
) {
  if path.is_null() {
    callback(-libc::EINVAL);
    return;
  }
  // SAFETY: caller guarantees string is valid per fn contract
  let path_cstr = unsafe { std::ffi::CStr::from_ptr(path) }.to_owned();
  // SAFETY: caller guarantees dir_fd is valid per fn contract
  let dir_res = unsafe { fd_to_borrowed_resource(dir_fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  let kind = if flags & libc::AT_REMOVEDIR != 0 {
    api::UnlinkKind::Directory
  } else {
    api::UnlinkKind::File
  };
  api::unlinkat(&dir_res, path_cstr, kind)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    });
}

/// Rename a file or directory.
///
/// - `old_dir_fd`: Directory fd for old path
/// - `old_path`: Current path (null-terminated)
/// - `new_dir_fd`: Directory fd for new path
/// - `new_path`: New path (null-terminated)
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; paths must be valid null-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_renameat(
  lio: *mut lio_handle_t,
  old_dir_fd: libc::intptr_t,
  old_path: *const libc::c_char,
  new_dir_fd: libc::intptr_t,
  new_path: *const libc::c_char,
  callback: extern "C" fn(libc::c_int),
) {
  if old_path.is_null() || new_path.is_null() {
    callback(-libc::EINVAL);
    return;
  }
  let old_path_cstr = unsafe { std::ffi::CStr::from_ptr(old_path) }.to_owned();
  let new_path_cstr = unsafe { std::ffi::CStr::from_ptr(new_path) }.to_owned();
  let old_dir_res = unsafe { fd_to_borrowed_resource(old_dir_fd) };
  let new_dir_res = unsafe { fd_to_borrowed_resource(new_dir_fd) };
  api::renameat(&old_dir_res, old_path_cstr, &new_dir_res, new_path_cstr)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    });
}

/// Create a directory.
///
/// - `dir_fd`: Directory fd (or AT_FDCWD for current directory)
/// - `path`: Path to create (null-terminated)
/// - `mode`: Permission bits (e.g., 0755)
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; `path` must be a valid null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_mkdirat(
  lio: *mut lio_handle_t,
  dir_fd: libc::intptr_t,
  path: *const libc::c_char,
  mode: libc::mode_t,
  callback: extern "C" fn(libc::c_int),
) {
  if path.is_null() {
    callback(-libc::EINVAL);
    return;
  }
  let path_cstr = unsafe { std::ffi::CStr::from_ptr(path) }.to_owned();
  let dir_res = unsafe { fd_to_borrowed_resource(dir_fd) };
  #[allow(clippy::unnecessary_cast)]
  let mode = mode as u32;
  api::mkdirat(&dir_res, path_cstr, mode)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    });
}

/// Reads metadata for a path relative to a directory file descriptor.
///
/// # Safety
/// `lio` must be a valid handle; `dir_fd` must be a valid open directory file
/// descriptor; and a non-null `path` must point to a valid NUL-terminated
/// string. The descriptor must remain open until the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_statat(
  lio: *mut lio_handle_t,
  dir_fd: libc::intptr_t,
  path: *const libc::c_char,
  follow_symlinks: libc::c_int,
  callback: extern "C" fn(libc::c_int, lio_file_stat_t),
) {
  let Some(path) = (unsafe { cstr_to_owned(path) }) else {
    callback(-libc::EINVAL, lio_file_stat_t::default());
    return;
  };
  let dir_res = unsafe { fd_to_borrowed_resource(dir_fd) };
  api::statat(&dir_res, path, follow_symlinks != 0)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| match res {
      Ok(stat) => callback(0, file_stat_to_ffi(stat)),
      Err(e) => {
        callback(-e.raw_os_error().unwrap_or(1), lio_file_stat_t::default())
      }
    });
}

/// Reads metadata for an open file descriptor.
///
/// # Safety
/// `lio` must be a valid handle, and `fd` must be a valid open file descriptor
/// that remains open until the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_fstat(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  callback: extern "C" fn(libc::c_int, lio_file_stat_t),
) {
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::fstat(&resource).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |res| match res {
      Ok(stat) => callback(0, file_stat_to_ffi(stat)),
      Err(e) => {
        callback(-e.raw_os_error().unwrap_or(1), lio_file_stat_t::default())
      }
    },
  );
}

/// Reads the target of a symbolic link relative to a directory file descriptor.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: the target's length (or negative errno),
///   the same `buf`, and `len` = bytes of the target written into `buf` (0 on
///   error). The target is not NUL-terminated.
///
/// # Safety
/// `lio` must be a valid handle; `dir_fd` must be a valid open directory file
/// descriptor; a non-null `path` must point to a valid NUL-terminated string;
/// and `buf` must point to `buf_len` writable bytes, which need not be
/// initialized (it may be null if `buf_len` is 0), that stay valid and
/// untouched until the callback fires. The descriptor must remain open until
/// the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_readlinkat(
  lio: *mut lio_handle_t,
  dir_fd: libc::intptr_t,
  path: *const libc::c_char,
  buf: *mut u8,
  buf_len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  let Some(buf) = (unsafe { lend_empty(buf, buf_len, callback) }) else {
    return;
  };
  let Some(path) = (unsafe { cstr_to_owned(path) }) else {
    return_buf(
      callback,
      Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
      buf,
    );
    return;
  };
  let dir_res = unsafe { fd_to_borrowed_resource(dir_fd) };
  crate::api::io::Io::from_op(api::ops::ReadlinkAt::new(
    dir_res.clone(),
    path,
    buf,
  ))
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Reads the current working directory into a caller-provided buffer.
///
/// lio borrows `buf` until the callback fires and never frees it.
///
/// - `callback(result, buf, len)`: the path's length (or negative errno), the
///   same `buf`, and `len` = bytes of the path written into `buf` (0 on
///   error). The path is not NUL-terminated.
///
/// # Safety
/// `lio` must be a valid handle, and `buf` must point to `buf_len` writable
/// bytes, which need not be initialized (it may be null if `buf_len` is 0),
/// that stay valid and untouched until the callback fires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_getcwd(
  lio: *mut lio_handle_t,
  buf: *mut u8,
  buf_len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  let Some(buf) = (unsafe { lend_empty(buf, buf_len, callback) }) else {
    return;
  };
  crate::api::io::Io::from_op(api::ops::GetCwd::new(buf))
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, buf)| return_buf(callback, res, buf));
}

/// Reads one batch of directory entries from an open directory descriptor.
///
/// - `callback(result, names, names_len, entries, entries_len, eof)`: 0 (or
///   negative errno); the entry names, `names_len` bytes, which each entry
///   indexes with `name_offset`/`name_len`; `entries_len` entries; and whether
///   the end of the directory was reached. Both arrays are allocated by lio
///   (null when empty or on error) and the caller must release them with
///   [`lio_buf_free`] and [`lio_dir_entries_free`], respectively.
///
/// # Safety
/// `lio` must be a valid handle, and `fd` must be a valid open directory file
/// descriptor that remains open until the operation completes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_readdir(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  raw_capacity: usize,
  entries_capacity: usize,
  callback: extern "C" fn(
    libc::c_int,
    *mut u8,
    usize,
    *mut lio_dir_entry_t,
    usize,
    libc::c_int,
  ),
) {
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  api::readdir(
    &resource,
    ReadDirBuf::with_capacity(raw_capacity, entries_capacity),
  )
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |res| match res {
    Ok(buf) => {
      let entries: Vec<lio_dir_entry_t> = buf
        .entries
        .iter()
        .take(buf.result.entries)
        .map(|entry| lio_dir_entry_t {
          name_offset: entry.name_offset,
          name_len: entry.name_len,
          file_type: entry.file_type.map(file_type_to_ffi).unwrap_or(0),
          has_ino: i32::from(entry.ino.is_some()),
          ino: entry.ino.unwrap_or(0),
        })
        .collect();
      let raw = &buf.raw[..buf.result.raw_written];
      let eof = i32::from(buf.result.eof);
      match (malloc_copy(raw), malloc_copy(&entries)) {
        (Some(raw_ptr), Some(entries_ptr)) => {
          callback(0, raw_ptr, raw.len(), entries_ptr, entries.len(), eof)
        }
        (raw_ptr, entries_ptr) => {
          // SAFETY: whichever copy succeeded is ours alone and unused.
          unsafe {
            lio_buf_free(raw_ptr.unwrap_or(ptr::null_mut()));
            lio_dir_entries_free(entries_ptr.unwrap_or(ptr::null_mut()));
          }
          callback(-libc::ENOMEM, ptr::null_mut(), 0, ptr::null_mut(), 0, 1)
        }
      }
    }
    Err(e) => callback(
      -e.raw_os_error().unwrap_or(1),
      ptr::null_mut(),
      0,
      ptr::null_mut(),
      0,
      1,
    ),
  });
}

/// Spawn a process.
///
/// # Safety
/// `lio` must be a valid handle. A non-null `path` must point to a valid
/// NUL-terminated string. `argv` and `envp` must each be null or point to a
/// null-terminated array of pointers to valid NUL-terminated strings.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_spawn(
  lio: *mut lio_handle_t,
  path: *const libc::c_char,
  argv: *const *const libc::c_char,
  envp: *const *const libc::c_char,
  callback: extern "C" fn(libc::intptr_t),
) {
  let Some(path) = (unsafe { cstr_to_owned(path) }) else {
    callback(-libc::EINVAL as libc::intptr_t);
    return;
  };

  unsafe fn collect_cstr_array(
    ptrs: *const *const libc::c_char,
  ) -> Option<Vec<std::ffi::CString>> {
    if ptrs.is_null() {
      return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut i = 0;
    loop {
      let ptr = unsafe { *ptrs.add(i) };
      if ptr.is_null() {
        break;
      }
      out.push(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_owned());
      i += 1;
    }
    Some(out)
  }

  let Some(argv) = (unsafe { collect_cstr_array(argv) }) else {
    callback(-libc::EINVAL as libc::intptr_t);
    return;
  };
  let envp = if envp.is_null() {
    None
  } else {
    Some(unsafe { collect_cstr_array(envp) }.unwrap_or_default())
  };

  api::spawn(path, argv, envp)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(pid) => pid.as_raw() as libc::intptr_t,
        Err(e) => -e.raw_os_error().unwrap_or(1) as libc::intptr_t,
      });
    });
}

// /// Send file data to a socket without copying through userspace.
// ///
// /// - `out_fd`: Destination socket
// /// - `in_fd`: Source file
// /// - `offset`: Starting offset in source (-1 for current position)
// /// - `count`: Number of bytes to send
// /// - `callback(result)`: bytes sent on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; `out_fd` must be a socket; `in_fd` must be a file.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_sendfile(
//   lio: *mut lio_handle_t,
//   out_fd: libc::intptr_t,
//   in_fd: libc::intptr_t,
//   offset: i64,
//   count: libc::size_t,
//   callback: extern "C" fn(libc::ssize_t),
// ) {
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let out_res = unsafe { fd_to_borrowed_resource(out_fd) };
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let in_res = unsafe { fd_to_borrowed_resource(in_fd) };
//   let off = if offset < 0 { None } else { Some(offset) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::sendfile(&out_res, &in_res, off, count)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(n) => n as libc::ssize_t,
//         Err(e) => -(e.raw_os_error().unwrap_or(1) as libc::ssize_t),
//       });
//     });
// }

// /// Spawn a new process.
// ///
// /// - `path`: Path to executable (null-terminated)
// /// - `argv`: Null-terminated array of argument strings
// /// - `envp`: Null-terminated array of environment strings, or NULL to inherit
// /// - `callback(result)`: child PID on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; `path` and `argv` must be valid.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_spawn(
//   lio: *mut lio_handle_t,
//   path: *const libc::c_char,
//   argv: *const *const libc::c_char,
//   envp: *const *const libc::c_char,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   if path.is_null() || argv.is_null() {
//     callback(-libc::EINVAL);
//     return;
//   }
//
//   // SAFETY: caller guarantees path is valid per fn contract
//   let path_cstr = unsafe { std::ffi::CStr::from_ptr(path) }.to_owned();
//
//   // Parse argv
//   let mut argv_vec = Vec::new();
//   let mut i = 0;
//   loop {
//     // SAFETY: caller guarantees argv is null-terminated per fn contract
//     let arg = unsafe { *argv.add(i) };
//     if arg.is_null() {
//       break;
//     }
//     // SAFETY: arg is a valid pointer from the argv array
//     argv_vec.push(unsafe { std::ffi::CStr::from_ptr(arg) }.to_owned());
//     i += 1;
//   }
//
//   // Parse envp if provided
//   let envp_opt = if envp.is_null() {
//     None
//   } else {
//     let mut envp_vec = Vec::new();
//     let mut i = 0;
//     loop {
//       // SAFETY: caller guarantees envp is null-terminated per fn contract
//       let env = unsafe { *envp.add(i) };
//       if env.is_null() {
//         break;
//       }
//       // SAFETY: env is a valid pointer from the envp array
//       envp_vec.push(unsafe { std::ffi::CStr::from_ptr(env) }.to_owned());
//       i += 1;
//     }
//     Some(envp_vec)
//   };
//
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::spawn(path_cstr, argv_vec, envp_opt)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(pid) => pid,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     });
// }

// /// Wait for a child process to change state.
// ///
// /// - `id_type`: 0=P_ALL (any child), 1=P_PID (specific pid), 2=P_PGID (process group)
// /// - `id`: PID or PGID depending on id_type (ignored for P_ALL)
// /// - `options`: Wait options (WEXITED=4, WSTOPPED=2, WCONTINUED=8, WNOHANG=1, WNOWAIT=0x1000000)
// /// - `callback(result, pid, status, code)`:
// ///   - result: 0 on success, negative errno on error
// ///   - pid: child PID that changed state (0 if WNOHANG and no child ready)
// ///   - status: 1=exited, 2=signaled, 3=stopped, 4=continued
// ///   - code: exit code or signal number
// ///
// /// # Safety
// /// `lio` must be valid.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_waitid(
//   lio: *mut lio_handle_t,
//   id_type: libc::c_int,
//   id: libc::c_int,
//   options: libc::c_int,
//   callback: extern "C" fn(libc::c_int, libc::c_int, libc::c_int, libc::c_int),
// ) {
//   use api::ops::{WaitOptions, WaitTarget};
//
//   let target = match id_type {
//     0 => WaitTarget::Any,      // P_ALL
//     1 => WaitTarget::Pid(id),  // P_PID
//     2 => WaitTarget::Pgid(id), // P_PGID
//     _ => {
//       callback(-libc::EINVAL, 0, 0, 0);
//       return;
//     }
//   };
//
//   // Construct WaitOptions from raw flags
//   let mut opts = WaitOptions::empty();
//   if (options & libc::WEXITED) != 0 {
//     opts |= WaitOptions::EXITED;
//   }
//   if (options & libc::WSTOPPED) != 0 {
//     opts |= WaitOptions::STOPPED;
//   }
//   if (options & libc::WCONTINUED) != 0 {
//     opts |= WaitOptions::CONTINUED;
//   }
//   if (options & libc::WNOHANG) != 0 {
//     opts |= WaitOptions::NOHANG;
//   }
//   if (options & libc::WNOWAIT) != 0 {
//     opts |= WaitOptions::NOWAIT;
//   }
//
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::waitid(target, opts).with_lio(&unsafe { handle(lio) }.inner).when_done(
//     move |res| match res {
//       Ok(Some(status)) => {
//         let (status_type, code) = if status.exited() {
//           (1, status.exit_code().unwrap_or(0))
//         } else if status.signaled() {
//           (2, status.signal().unwrap_or(0))
//         } else if status.stopped() {
//           (3, status.signal().unwrap_or(0))
//         } else if status.continued() {
//           (4, 0)
//         } else {
//           (0, 0)
//         };
//         callback(0, status.pid, status_type, code);
//       }
//       Ok(None) => callback(0, 0, 0, 0), // WNOHANG and no child ready
//       Err(e) => callback(-e.raw_os_error().unwrap_or(1), 0, 0, 0),
//     },
//   );
// }

// /// Copy data between file descriptors without copying to userspace (Linux only).
// ///
// /// Both `fd_in` and `fd_out` must be pipes. This duplicates data from one pipe
// /// to another without consuming it from the source.
// ///
// /// - `fd_in`: Source pipe
// /// - `fd_out`: Destination pipe
// /// - `len`: Maximum bytes to copy
// /// - `callback(result)`: bytes copied on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; both fds must be pipes.
// #[cfg(target_os = "linux")]
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_tee(
//   lio: *mut lio_handle_t,
//   fd_in: libc::intptr_t,
//   fd_out: libc::intptr_t,
//   len: libc::c_uint,
//   callback: extern "C" fn(libc::ssize_t),
// ) {
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let res_in = unsafe { fd_to_borrowed_resource(fd_in) };
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let res_out = unsafe { fd_to_borrowed_resource(fd_out) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::tee(&res_in, res_out, len)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(n) => n as libc::ssize_t,
//         Err(e) => -(e.raw_os_error().unwrap_or(1) as libc::ssize_t),
//       });
//     });
// }
//
// /// Splice data between file descriptors via a pipe (Linux only).
// ///
// /// At least one of `fd_in` or `fd_out` must be a pipe.
// ///
// /// - `fd_in`: Source file descriptor
// /// - `off_in`: Offset for source (-1 for pipes or current position)
// /// - `fd_out`: Destination file descriptor
// /// - `off_out`: Offset for destination (-1 for pipes or current position)
// /// - `len`: Maximum bytes to transfer
// /// - `flags`: Splice flags (SPLICE_F_MOVE=1, SPLICE_F_NONBLOCK=2, SPLICE_F_MORE=4)
// /// - `callback(result)`: bytes transferred on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; at least one fd must be a pipe.
// #[cfg(target_os = "linux")]
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_splice(
//   lio: *mut lio_handle_t,
//   fd_in: libc::intptr_t,
//   off_in: i64,
//   fd_out: libc::intptr_t,
//   off_out: i64,
//   len: libc::c_uint,
//   flags: libc::c_uint,
//   callback: extern "C" fn(libc::ssize_t),
// ) {
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let res_in = unsafe { fd_to_borrowed_resource(fd_in) };
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let res_out = unsafe { fd_to_borrowed_resource(fd_out) };
//   let off_in_opt = if off_in < 0 { None } else { Some(off_in) };
//   let off_out_opt = if off_out < 0 { None } else { Some(off_out) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::splice(&res_in, off_in_opt, &res_out, off_out_opt, len, flags)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(n) => n as libc::ssize_t,
//         Err(e) => -(e.raw_os_error().unwrap_or(1) as libc::ssize_t),
//       });
//     });
// }
//
// /// Copy data between files without going through userspace (Linux only).
// ///
// /// This performs a server-side copy when possible (NFS, Btrfs reflinks).
// ///
// /// - `fd_in`: Source file
// /// - `off_in`: Starting offset in source
// /// - `fd_out`: Destination file
// /// - `off_out`: Starting offset in destination
// /// - `len`: Number of bytes to copy
// /// - `callback(result)`: bytes copied on success, negative errno on error
// ///
// /// # Safety
// /// `lio` must be valid; both fds must be regular files.
// #[cfg(target_os = "linux")]
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_copy_file_range(
//   lio: *mut lio_handle_t,
//   fd_in: libc::intptr_t,
//   off_in: i64,
//   fd_out: libc::intptr_t,
//   off_out: i64,
//   len: libc::size_t,
//   callback: extern "C" fn(libc::ssize_t),
// ) {
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let res_in = unsafe { fd_to_borrowed_resource(fd_in) };
//   // SAFETY: caller guarantees fds are valid per fn contract
//   let res_out = unsafe { fd_to_borrowed_resource(fd_out) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::copy_file_range(&res_in, off_in, &res_out, off_out, len, 0)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(n) => n as libc::ssize_t,
//         Err(e) => -(e.raw_os_error().unwrap_or(1) as libc::ssize_t),
//       });
//     });
// }
//
// /// Watch a file or directory for changes.
// ///
// /// - `path`: Path to watch (null-terminated)
// /// - `mask`: Events to watch for (WATCH_MODIFY=1, WATCH_ATTRIB=2, WATCH_DELETE=4,
// ///   WATCH_RENAME=8, WATCH_EXTEND=16)
// /// - `callback(result)`: events that occurred (positive mask) or negative errno
// ///
// /// # Safety
// /// `lio` must be valid; `path` must be a valid null-terminated string.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_watch(
//   lio: *mut lio_handle_t,
//   path: *const libc::c_char,
//   mask: libc::c_uint,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   if path.is_null() {
//     callback(-libc::EINVAL);
//     return;
//   }
//   // SAFETY: caller guarantees path is valid per fn contract
//   let path_str = unsafe { std::ffi::CStr::from_ptr(path) };
//   // SAFETY: CStr bytes are valid UTF-8 or platform-native encoding
//   let path_os = unsafe {
//     std::ffi::OsStr::from_encoded_bytes_unchecked(path_str.to_bytes())
//   };
//   let watch_mask = api::ops::WatchMask::from_bits(mask);
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::watch(path_os, watch_mask)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |res| {
//       callback(match res {
//         Ok(events) => events.bits() as libc::c_int,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     });
// }
//
// /// Read directory entries from an open directory fd.
// ///
// /// Returns raw directory entries in kernel format. The buffer should be at least
// /// 4096 bytes. Returns 0 when end of directory is reached.
// ///
// /// - `fd`: Open directory file descriptor
// /// - `buf`: Buffer to read entries into, borrowed until the callback fires
// /// - `buf_len`: Size of buffer
// /// - `callback(result, buf, len)`: bytes read (0=EOF, negative=error), the
// ///   same buffer, bytes read
// ///
// /// # Safety
// /// `lio` must be valid; `fd` must be an open directory; `buf` must point to
// /// `buf_len` writable bytes that stay valid until the callback fires.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_getdents(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   buf: *mut u8,
//   buf_len: libc::size_t,
//   callback: extern "C" fn(libc::c_int, *mut u8, libc::size_t),
// ) {
//   let Some(buf) = (unsafe { lend_empty(buf, buf_len, callback) }) else {
//     return;
//   };
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::getdents(&resource, buf)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |(res, buf, _entries)| {
//       // We ignore the parsed entries and just return raw bytes
//       return_buf(callback, res, buf);
//     });
// }
//
// /// Wait for a signal from the specified set.
// ///
// /// The signals must be blocked (via sigprocmask) before calling this function.
// ///
// /// - `signals`: Array of signal numbers to wait for
// /// - `num_signals`: Number of signals in array
// /// - `callback(result)`: signal number received (positive) or negative errno
// ///
// /// # Safety
// /// `lio` must be valid; `signals` must point to `num_signals` valid signal numbers.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_signal(
//   lio: *mut lio_handle_t,
//   signals: *const libc::c_int,
//   num_signals: libc::size_t,
//   callback: extern "C" fn(libc::c_int),
// ) {
//   if signals.is_null() && num_signals > 0 {
//     callback(-libc::EINVAL);
//     return;
//   }
//
//   let mut sigset = api::ops::SignalSet::empty();
//   for i in 0..num_signals {
//     // SAFETY: caller guarantees signals array is valid per fn contract
//     let sig = unsafe { *signals.add(i) };
//     sigset.add(sig);
//   }
//
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::signal(sigset).with_lio(&unsafe { handle(lio) }.inner).when_done(
//     move |res| {
//       callback(match res {
//         Ok(sig) => sig,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       });
//     },
//   );
// }
fn link_kind_from_ffi(kind: libc::c_int) -> Result<api::ops::LinkKind, ()> {
  match kind {
    0 => Ok(api::ops::LinkKind::Hard),
    1 => Ok(api::ops::LinkKind::Soft),
    _ => Err(()),
  }
}

#[cfg(all(test, unix))]
mod sockaddr_tests {
  use super::*;
  use std::sync::atomic::{AtomicI32, Ordering};

  // Separate callback slots: these tests run concurrently by default.
  const PENDING: libc::c_int = 12345;
  static INVALID_ADDR_RESULT: AtomicI32 = AtomicI32::new(PENDING);
  static BIND_ROUTE_RESULT: AtomicI32 = AtomicI32::new(PENDING);
  extern "C" fn invalid_addr_callback(code: libc::c_int) {
    INVALID_ADDR_RESULT.store(code, Ordering::SeqCst);
  }
  extern "C" fn invalid_sendto_callback(
    code: libc::c_int,
    buf: *mut u8,
    len: usize,
  ) {
    assert!(buf.is_null());
    assert_eq!(len, 0);
    INVALID_ADDR_RESULT.store(code, Ordering::SeqCst);
  }
  extern "C" fn bind_route_callback(code: libc::c_int) {
    BIND_ROUTE_RESULT.store(code, Ordering::SeqCst);
  }

  #[test]
  fn short_and_unaligned_addresses() {
    for addr in [
      SocketAddr::from(([127, 0, 0, 1], 4321)),
      SocketAddr::from((Ipv6Addr::LOCALHOST, 4321)),
    ] {
      let len = match addr {
        SocketAddr::V4(_) => mem::size_of::<libc::sockaddr_in>(),
        SocketAddr::V6(_) => mem::size_of::<libc::sockaddr_in6>(),
      };
      let storage = std_socketaddr_into_libc(addr);
      let mut bytes = vec![0u8; len + 1];
      // SAFETY: Both byte ranges are valid and do not overlap.
      unsafe {
        ptr::copy_nonoverlapping(
          (&raw const storage).cast::<u8>(),
          bytes.as_mut_ptr().add(1),
          len,
        );
      }
      let raw = unsafe { bytes.as_ptr().add(1) }.cast::<libc::sockaddr>();
      assert_eq!(
        sockaddr_to_socketaddr(raw, len as libc::socklen_t),
        Some(addr)
      );
      let family_end = mem::offset_of!(libc::sockaddr, sa_family)
        + mem::size_of::<libc::sa_family_t>();
      for short in [0, family_end - 1, family_end, len - 1] {
        assert_eq!(sockaddr_to_socketaddr(raw, short as libc::socklen_t), None);
      }
      assert_eq!(
        sockaddr_to_socketaddr(ptr::null(), len as libc::socklen_t),
        None
      );

      // The bind FFI path must reject truncated addresses before accessing the
      // handle or fd, and preserve its EINVAL callback contract.
      INVALID_ADDR_RESULT.store(PENDING, Ordering::SeqCst);
      unsafe {
        lio_bind(
          ptr::null_mut(),
          -1,
          raw,
          (len - 1) as _,
          invalid_addr_callback,
        )
      };
      assert_eq!(INVALID_ADDR_RESULT.load(Ordering::SeqCst), -libc::EINVAL);
      INVALID_ADDR_RESULT.store(PENDING, Ordering::SeqCst);
      unsafe {
        lio_connect(
          ptr::null_mut(),
          -1,
          raw,
          (len - 1) as _,
          invalid_addr_callback,
        )
      };
      assert_eq!(INVALID_ADDR_RESULT.load(Ordering::SeqCst), -libc::EINVAL);
      INVALID_ADDR_RESULT.store(PENDING, Ordering::SeqCst);
      unsafe {
        lio_sendto(
          ptr::null_mut(),
          -1,
          ptr::null_mut(),
          0,
          0,
          raw,
          (len - 1) as _,
          invalid_sendto_callback,
        )
      };
      assert_eq!(INVALID_ADDR_RESULT.load(Ordering::SeqCst), -libc::EINVAL);
    }
  }

  #[test]
  fn unaligned_ipv4_bind_route() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let storage = std_socketaddr_into_libc(addr);
    let len = mem::size_of::<libc::sockaddr_in>();
    let mut bytes = vec![0u8; len + 1];
    unsafe {
      ptr::copy_nonoverlapping(
        (&raw const storage).cast::<u8>(),
        bytes.as_mut_ptr().add(1),
        len,
      );
    }
    let raw = unsafe { bytes.as_ptr().add(1) }.cast::<libc::sockaddr>();
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0);
    let lio = lio_create(16);
    assert!(!lio.is_null());
    BIND_ROUTE_RESULT.store(PENDING, Ordering::SeqCst);
    unsafe { lio_bind(lio, fd as _, raw, len as _, bind_route_callback) };
    for _ in 0..1000 {
      if BIND_ROUTE_RESULT.load(Ordering::SeqCst) != PENDING {
        break;
      }
      unsafe { lio_tick(lio) };
    }
    assert_eq!(BIND_ROUTE_RESULT.load(Ordering::SeqCst), 0);
    unsafe {
      lio_destroy(lio);
      libc::close(fd);
    }
  }
}

#[cfg(all(test, unix))]
mod rw_offset_tests {
  use super::*;
  use std::os::fd::AsRawFd;
  use std::sync::Mutex;

  /// `(result, buf, len)` from the last read or write callback.
  static DONE: Mutex<Option<(libc::c_int, usize, usize)>> = Mutex::new(None);
  /// Held by each test: they share `DONE`.
  static SERIAL: Mutex<()> = Mutex::new(());
  extern "C" fn on_rw(code: libc::c_int, buf: *mut u8, len: usize) {
    *DONE.lock().unwrap() = Some((code, buf as usize, len));
  }

  /// Ticks `lio` until the callback ran; returns its result and the buffer.
  fn wait(lio: *mut lio_handle_t) -> (libc::c_int, Vec<u8>) {
    for _ in 0..5_000 {
      if let Some((code, buf, len)) = DONE.lock().unwrap().take() {
        // SAFETY: the callback hands back the buffer from `lio_buf_alloc`.
        let bytes =
          unsafe { std::slice::from_raw_parts(buf as *const u8, len) }.to_vec();
        // SAFETY: as above; it is not used after this.
        unsafe { lio_buf_free(buf as *mut u8) };
        return (code, bytes);
      }
      // SAFETY: `lio` is a live handle from `lio_create`.
      unsafe { lio_tick(lio) };
      std::thread::sleep(Duration::from_millis(1));
    }
    panic!("read/write callback never ran");
  }

  fn buf_with(bytes: &[u8]) -> *mut u8 {
    let buf = lio_buf_alloc(bytes.len());
    // SAFETY: `buf` holds `bytes.len()` bytes.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len()) };
    buf
  }

  /// Offsets past 4 GiB reach the file. `lio_write_at` used to truncate them
  /// to 32 bits, so a write at 5 GiB landed at 1 GiB. The file is sparse.
  #[test]
  #[cfg_attr(miri, ignore = "needs a real backend")]
  fn read_write_at_past_4_gib() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    const OFFSET: u64 = 5 << 30;
    let data = b"past 4 GiB";
    let path = std::env::temp_dir()
      .join(format!("lio_ffi_rw_past_4_gib_{}", std::process::id()));
    let file = std::fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(true)
      .open(&path)
      .unwrap();
    let fd = file.as_raw_fd() as libc::intptr_t;
    let lio = lio_create(16);
    assert!(!lio.is_null());

    // SAFETY: valid handle, fd and `lio_buf_alloc` buffer of `data.len()`.
    unsafe { lio_write_at(lio, fd, buf_with(data), data.len(), OFFSET, on_rw) };
    assert_eq!(wait(lio), (data.len() as libc::c_int, data.to_vec()));
    assert_eq!(file.metadata().unwrap().len(), OFFSET + data.len() as u64);

    let zeros = vec![0; data.len()];
    // SAFETY: as above.
    unsafe {
      lio_read_at(lio, fd, buf_with(&zeros), data.len(), OFFSET, on_rw)
    };
    assert_eq!(wait(lio), (data.len() as libc::c_int, data.to_vec()));

    // Where the truncated offset pointed, the file is still a hole.
    // SAFETY: as above.
    unsafe {
      lio_read_at(
        lio,
        fd,
        buf_with(data),
        data.len(),
        OFFSET as u32 as u64,
        on_rw,
      )
    };
    assert_eq!(wait(lio), (data.len() as libc::c_int, zeros));

    // SAFETY: `lio` is not used after this.
    unsafe { lio_destroy(lio) };
    drop(file);
    std::fs::remove_file(path).unwrap();
  }

  /// An offset past `INT64_MAX` fails with `-EINVAL` and hands the buffer
  /// back untouched, without a handle or descriptor being used. A write
  /// reports `len = buf_len`, a failed read `len = 0`.
  #[test]
  fn offset_past_int64_max_is_einval() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for offset in [i64::MAX as u64 + 1, u64::MAX] {
      // SAFETY: rejected before `lio` or the fd are used; the buffer comes
      // from `lio_buf_alloc`.
      unsafe {
        lio_write_at(ptr::null_mut(), -1, buf_with(b"data"), 4, offset, on_rw)
      };
      assert_eq!(wait(ptr::null_mut()), (-libc::EINVAL, b"data".to_vec()));
      // SAFETY: as above.
      unsafe {
        lio_read_at(ptr::null_mut(), -1, buf_with(b"data"), 4, offset, on_rw)
      };
      assert_eq!(wait(ptr::null_mut()), (-libc::EINVAL, Vec::new()));
    }
  }
}

/// Translates POSIX `O_*` open flags from C callers to [`api::OpenFlags`].
///
/// Returns `None` for flags lio does not model, so they are rejected rather
/// than silently dropped. `O_CLOEXEC` is accepted; lio always sets it.
fn open_flags_from_posix(bits: libc::c_int) -> Option<api::OpenFlags> {
  use api::OpenFlags;
  let mut flags = match bits & libc::O_ACCMODE {
    libc::O_RDONLY => OpenFlags::READ,
    libc::O_WRONLY => OpenFlags::WRITE,
    libc::O_RDWR => OpenFlags::READ | OpenFlags::WRITE,
    _ => return None,
  };
  let mut rest = bits & !libc::O_ACCMODE & !libc::O_CLOEXEC;
  for (native, flag) in [
    (libc::O_APPEND, OpenFlags::APPEND),
    (libc::O_CREAT, OpenFlags::CREATE),
    (libc::O_TRUNC, OpenFlags::TRUNCATE),
    (libc::O_EXCL, OpenFlags::EXCLUSIVE),
    (libc::O_DIRECTORY, OpenFlags::DIRECTORY),
  ] {
    if rest & native != 0 {
      flags |= flag;
      rest &= !native;
    }
  }
  (rest == 0).then_some(flags)
}

#[cfg(test)]
mod buffer_ownership_tests {
  //! lio borrows C buffers and never frees them. These tests drive the
  //! scripted backend, which writes into read buffers on completion and drain
  //! as the kernel would, so Miri also checks that lio neither frees nor
  //! touches C memory it does not own at the wrong time.

  use super::*;
  use crate::backend::testing::{Checked, POISON, ScriptedBackend};
  use std::sync::Mutex;

  /// What a buffer callback reported: result, pointer and length.
  type Report = Mutex<Vec<(libc::c_int, usize, usize)>>;

  fn record(report: &Report, code: libc::c_int, buf: *mut u8, len: usize) {
    report.lock().unwrap().push((code, buf as usize, len));
  }

  fn take(report: &Report) -> Vec<(libc::c_int, usize, usize)> {
    mem::take(&mut *report.lock().unwrap())
  }

  /// A C handle over a scripted backend, which never touches a descriptor.
  fn scripted_handle(backend: &ScriptedBackend) -> *mut lio_handle_t {
    let inner =
      Lio::new_with_backend(Checked::new(backend.clone()), 8).unwrap();
    Box::into_raw(Box::new(lio_handle_t { inner }))
  }

  /// Completes every in-flight operation with `result` and runs callbacks.
  fn complete_all(lio: *mut lio_handle_t, backend: &ScriptedBackend, n: isize) {
    unsafe { lio_tick(lio) };
    backend.complete_all(n);
    unsafe { lio_tick(lio) };
  }

  #[test]
  fn read_reports_bytes_read_and_frees_by_pointer_alone() {
    static READ: Report = Mutex::new(Vec::new());
    extern "C" fn on_read(code: libc::c_int, buf: *mut u8, len: usize) {
      record(&READ, code, buf, len);
    }

    let backend = ScriptedBackend::new();
    let lio = scripted_handle(&backend);
    let buf = lio_buf_alloc(4096);
    assert!(!buf.is_null());
    unsafe { lio_read(lio, 0, buf, 4096, on_read) };
    complete_all(lio, &backend, 100);

    assert_eq!(take(&READ), [(100, buf as usize, 100)]);
    let read = unsafe { std::slice::from_raw_parts(buf, 100) };
    assert!(read.iter().all(|&b| b == POISON));
    // The documented release takes no length. It used to take one, and C
    // passing the reported `len` (100, not 4096) freed with the wrong layout.
    unsafe {
      lio_buf_free(buf);
      lio_destroy(lio);
    }
  }

  #[test]
  fn buffers_from_any_allocator_stay_with_the_caller() {
    static DONE: Report = Mutex::new(Vec::new());
    extern "C" fn on_done(code: libc::c_int, buf: *mut u8, len: usize) {
      record(&DONE, code, buf, len);
    }

    let backend = ScriptedBackend::new();
    let lio = scripted_handle(&backend);

    // Part of a Rust allocation, a `malloc` allocation, and a stack array.
    let mut vec = vec![7u8; 64];
    let part = unsafe { vec.as_mut_ptr().add(16) };
    let malloced = unsafe { libc::malloc(32) }.cast::<u8>();
    assert!(!malloced.is_null());
    let mut stack = *b"stack data";
    unsafe {
      lio_read(lio, 0, part, 8, on_done);
      lio_recv(lio, 0, malloced, 32, 0, on_done);
      lio_send(lio, 0, stack.as_mut_ptr(), stack.len(), 0, on_done);
    }
    complete_all(lio, &backend, 3);

    let mut reports = take(&DONE);
    reports.sort_unstable();
    let mut expected = [
      (3, part as usize, 3),
      (3, malloced as usize, 3),
      (3, stack.as_ptr() as usize, stack.len()),
    ];
    expected.sort_unstable();
    assert_eq!(reports, expected);

    // Only the lent bytes were written, and every buffer is still ours.
    assert!(vec[..16].iter().all(|&b| b == 7));
    assert!(vec[16..24].iter().all(|&b| b == POISON));
    assert!(vec[24..].iter().all(|&b| b == 7));
    assert_eq!(&stack, b"stack data");
    vec.extend_from_slice(&[1; 1024]);
    unsafe {
      libc::free(malloced.cast());
      lio_destroy(lio);
    }
  }

  #[test]
  fn destroy_with_operations_in_flight_leaves_buffers_to_the_caller() {
    static NEVER: Report = Mutex::new(Vec::new());
    extern "C" fn on_done(code: libc::c_int, buf: *mut u8, len: usize) {
      record(&NEVER, code, buf, len);
    }

    let backend = ScriptedBackend::new();
    let lio = scripted_handle(&backend);
    let mut read_buf = vec![0u8; 32];
    let mut write_buf = b"in flight".to_vec();
    unsafe {
      lio_read(lio, 0, read_buf.as_mut_ptr(), read_buf.len(), on_done);
      lio_write(lio, 0, write_buf.as_mut_ptr(), write_buf.len(), on_done);
      lio_tick(lio);
    }
    assert_eq!(backend.in_flight().len(), 2);

    unsafe { lio_destroy(lio) };

    assert!(take(&NEVER).is_empty(), "destroyed operations never call back");
    // Draining wrote into the read buffer like a kernel finishing the request,
    // and both buffers are still valid and owned here.
    assert!(read_buf.iter().all(|&b| b == POISON));
    assert_eq!(write_buf, b"in flight");
    read_buf.extend_from_slice(&[1; 1024]);
    write_buf.extend_from_slice(&[1; 1024]);
  }

  #[test]
  fn null_buffer_is_rejected_unless_empty() {
    static DONE: Report = Mutex::new(Vec::new());
    extern "C" fn on_done(code: libc::c_int, buf: *mut u8, len: usize) {
      record(&DONE, code, buf, len);
    }

    // Rejected before the handle is used.
    unsafe {
      lio_read(ptr::null_mut(), 0, ptr::null_mut(), 16, on_done);
      lio_write(ptr::null_mut(), 0, ptr::null_mut(), 16, on_done);
    }
    assert_eq!(take(&DONE), [(-libc::EINVAL, 0, 0), (-libc::EINVAL, 0, 0)]);

    let backend = ScriptedBackend::new();
    let lio = scripted_handle(&backend);
    unsafe { lio_read(lio, 0, ptr::null_mut(), 0, on_done) };
    complete_all(lio, &backend, 0);
    assert_eq!(take(&DONE), [(0, 0, 0)]);
    unsafe { lio_destroy(lio) };
  }

  #[test]
  fn readdir_allocations_free_by_pointer_alone() {
    for len in [0, 1, 2, 3, 7, 32, 100, 1024] {
      let raw: Vec<u8> = (0..len).map(|i| i as u8).collect();
      let raw_ptr = malloc_copy(&raw).unwrap();
      assert_eq!(raw_ptr.is_null(), len == 0);
      if len > 0 {
        assert_eq!(unsafe { std::slice::from_raw_parts(raw_ptr, len) }, raw);
      }
      unsafe { lio_buf_free(raw_ptr) };

      let entries: Vec<lio_dir_entry_t> = (0..len)
        .map(|i| lio_dir_entry_t {
          name_offset: i as u32,
          ..Default::default()
        })
        .collect();
      let entries_ptr = malloc_copy(&entries).unwrap();
      if len > 0 {
        assert!(entries_ptr.is_aligned());
        let copied = unsafe { std::slice::from_raw_parts(entries_ptr, len) };
        for (i, entry) in copied.iter().enumerate() {
          assert_eq!(entry.name_offset, i as u32);
        }
      }
      unsafe { lio_dir_entries_free(entries_ptr) };
    }
    unsafe {
      lio_buf_free(ptr::null_mut());
      lio_dir_entries_free(ptr::null_mut());
      lio_sockaddr_free(ptr::null());
    }
  }
}
