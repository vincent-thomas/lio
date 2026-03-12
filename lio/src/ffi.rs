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
//! Operations that accept a `buf` pointer take **ownership** of that buffer.
//! The buffer is returned via the callback and must be freed by the caller.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{
  mem,
  net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
  ptr,
  time::Duration,
};

use crate::{
  Lio,
  api::{self, resource::Resource},
  net_utils,
};

#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};

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

/// Convert a C `intptr_t` fd to a `Resource`.
///
/// # Safety
/// `fd` must be a valid, open file descriptor.
/// The returned Resource must be forgotten (via `std::mem::forget`) after use
/// to prevent it from closing the C-owned fd.
#[cfg(unix)]
unsafe fn fd_to_resource(fd: libc::intptr_t) -> Resource {
  // SAFETY: caller guarantees fd is valid
  unsafe { Resource::from_raw_fd(fd as RawFd) }
}

/// Convert a `Resource` to a C `intptr_t`.
#[cfg(unix)]
fn resource_to_fd(r: &Resource) -> libc::intptr_t {
  use std::os::fd::AsRawFd;
  r.as_raw_fd() as libc::intptr_t
}

/// Converts a raw libc::sockaddr pointer and length into a safe std::net::SocketAddr.
fn sockaddr_to_socketaddr(
  raw_addr_ptr: *const libc::sockaddr,
  addr_len: libc::socklen_t,
) -> Option<SocketAddr> {
  if raw_addr_ptr.is_null() {
    return None;
  }

  // SAFETY: Caller guarantees raw_addr_ptr is valid and non-null (checked above).
  let family = unsafe { *raw_addr_ptr }.sa_family as i32;

  match family {
    libc::AF_INET => {
      if addr_len < mem::size_of::<libc::sockaddr_in>() as libc::socklen_t {
        return None;
      }
      let raw_v4 = raw_addr_ptr as *const libc::sockaddr_in;
      // SAFETY: We verified family == AF_INET and addr_len >= sizeof(sockaddr_in).
      let sockaddr_v4 = unsafe { *raw_v4 };
      let port = u16::from_be(sockaddr_v4.sin_port);
      let ipv4_addr = Ipv4Addr::from(u32::from_be(sockaddr_v4.sin_addr.s_addr));
      Some(SocketAddr::V4(SocketAddrV4::new(ipv4_addr, port)))
    }
    libc::AF_INET6 => {
      if addr_len < mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t {
        return None;
      }
      let raw_v6 = raw_addr_ptr as *const libc::sockaddr_in6;
      // SAFETY: We verified family == AF_INET6 and addr_len >= sizeof(sockaddr_in6).
      let sockaddr_v6 = unsafe { *raw_v6 };
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

/// Shut down part of a full-duplex connection.
///
/// - `fd`: Socket file descriptor
/// - `how`: `SHUT_RD`=0, `SHUT_WR`=1, `SHUT_RDWR`=2
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be a valid handle and `fd` a valid socket.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_shutdown(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  how: libc::c_int,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::shutdown(&resource, how)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
}

/// Synchronize a file's in-core state with the storage device.
///
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be a valid handle and `fd` a valid file descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_fsync(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::fsync(&resource).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    },
  );
}

/// Truncate a file to `len` bytes.
///
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be a valid handle and `fd` a valid file descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_truncate(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  len: u64,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::truncate(&resource, len)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
}

/// Write data to `fd` at `offset`.  Pass `offset = -1` for current position.
///
/// Ownership of `buf` (which must have been `malloc`'d with at least `buf_len`
/// bytes) transfers to lio.  The callback receives the original pointer so the
/// caller can `free` it.
///
/// - `callback(result, buf, len)`: bytes written (or negative errno), buffer
///
/// # Safety
/// `lio` must be valid; `buf` must point to at least `buf_len` bytes allocated
/// with `malloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_write_at(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  offset: i64,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::write_at(&resource, vec, offset)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, mut buf)| {
      let code = match res {
        Ok(n) => n,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      };
      let ptr = buf.as_mut_ptr();
      let len = buf.len();
      // Return buffer ownership to C - caller will free it
      std::mem::forget(buf);
      callback(code, ptr, len);
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
}

/// Read from `fd` at `offset` into `buf`.  Pass `offset = -1` for current
/// position.
///
/// Ownership of `buf` transfers to lio (see [`lio_write_at`]).
///
/// - `callback(result, buf, len)`: bytes read (or negative errno), buffer
///
/// # Safety
/// Same requirements as [`lio_write_at`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_read_at(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  offset: i64,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::read_at(&resource, vec, offset)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, mut buf)| {
      let code = match res {
        Ok(n) => n,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      };
      let ptr = buf.as_mut_ptr();
      let len = buf.len();
      // Return buffer ownership to C - caller will free it
      std::mem::forget(buf);
      callback(code, ptr, len);
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
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

/// Bind a socket to an address.
///
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; `sock` must point to `sock_len` bytes of a valid
/// `sockaddr`.
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
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::bind(&resource, addr).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    },
  );
}

/// Accept a connection.
///
/// - `callback(result, addr)`: new socket fd on success (negative errno on
///   error); `addr` is heap-allocated `sockaddr_storage` — **caller must free
///   it** — or null on error.
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
  let resource = unsafe { fd_to_resource(fd) };
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
            Box::into_raw(Box::new(net_utils::std_socketaddr_into_libc(addr)))
              as *const _,
          )
        }
        Err(e) => {
          (-e.raw_os_error().unwrap_or(1) as libc::intptr_t, ptr::null())
        }
      };
      callback(code, addr_ptr);
      // Don't close the listener fd - C owns it
      std::mem::forget(resource);
    },
  );
}

/// Listen for connections on a socket.
///
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` and `fd` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_listen(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  backlog: libc::c_int,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::listen(&resource, backlog)
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
}

/// Send data on a connected socket.
///
/// Ownership of `buf` transfers to lio (see [`lio_write_at`]).
///
/// - `callback(result, buf, len)`: bytes sent (or negative errno), buffer
///
/// # Safety
/// `lio` must be valid; `buf` must be at least `buf_len` bytes allocated with
/// `malloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_send(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: libc::c_int,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::send(&resource, vec, Some(flags))
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, mut buf)| {
      let code = match res {
        Ok(n) => n,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      };
      let ptr = buf.as_mut_ptr();
      let len = buf.len();
      // Return buffer ownership to C - caller will free it
      std::mem::forget(buf);
      callback(code, ptr, len);
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
}

/// Receive data from a socket.
///
/// Ownership of `buf` transfers to lio (see [`lio_write_at`]).
///
/// - `callback(result, buf, len)`: bytes received (or negative errno), buffer
///
/// # Safety
/// `lio` must be valid; `buf` must be at least `buf_len` bytes allocated with
/// `malloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_recv(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: libc::c_int,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::recv(&resource, vec, Some(flags))
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |(res, mut buf)| {
      let code = match res {
        Ok(n) => n,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      };
      let ptr = buf.as_mut_ptr();
      let len = buf.len();
      // Return buffer ownership to C - caller will free it
      std::mem::forget(buf);
      callback(code, ptr, len);
      // Don't close the fd - C owns it
      std::mem::forget(resource);
    });
}

/// Close a file descriptor.
///
/// The fd must be a C-owned fd, not a [`Resource`] managed by Rust.
///
/// - `callback(result)`: 0 on success, negative errno on error
///
/// # Safety
/// `lio` must be valid; `fd` must be a valid open file descriptor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_close(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees lio is valid per fn contract
  api::close(fd as RawFd).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    },
  );
}

/// Wait for `millis` milliseconds.
///
/// - `callback(result)`: 0 on success
///
/// # Safety
/// `lio` must be a valid handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_timeout(
  lio: *mut lio_handle_t,
  millis: libc::c_uint,
  callback: extern "C" fn(libc::c_int),
) {
  // SAFETY: caller guarantees lio is valid per fn contract
  api::timeout(Duration::from_millis(millis as u64))
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      });
    });
}
