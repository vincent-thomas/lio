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
  api::{self, ops::std_socketaddr_into_libc, resource::Resource},
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
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::write_at(&resource, vec, offset as u32)
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
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::read_at(&resource, vec, offset as u32)
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
  let (domain, ty, proto) =
    match crate::backend::op::socket_from_raw(domain, ty, proto) {
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
  let resource = unsafe { fd_to_borrowed_resource(fd) };
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
  let resource = unsafe { fd_to_borrowed_resource(fd) };
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
/// Ownership of `buf` transfers to lio.
///
/// - `callback(result, buf, len)`: bytes sent (or negative errno), buffer
///
/// # Safety
/// `lio` must be valid; `buf` must be allocated with malloc; `addr` must be
/// valid.
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
  let socket_addr = match sockaddr_to_socketaddr(addr, addr_len) {
    Some(a) => a,
    None => {
      callback(-libc::EINVAL, buf, buf_len);
      return;
    }
  };
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  crate::api::io::Io::from_op(
    api::send(&resource, vec, Some(flags)).into_inner().to(socket_addr),
  )
  .with_lio(&unsafe { handle(lio) }.inner)
  .when_done(move |(res, mut buf)| {
    let code = match res {
      Ok(n) => n,
      Err(e) => -e.raw_os_error().unwrap_or(1),
    };
    let ptr = buf.as_mut_ptr();
    let len = buf.len();
    std::mem::forget(buf);
    callback(code, ptr, len);
  });
}

// /// Receive data and get the sender's address (UDP).
// ///
// /// Ownership of `buf` transfers to lio.
// ///
// /// - `callback(result, buf, len, addr)`: bytes received, buffer, sender address
// ///   (heap-allocated, caller must free)
// ///
// /// # Safety
// /// `lio` must be valid; `buf` must be allocated with malloc.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_recvfrom(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   buf: *mut u8,
//   buf_len: usize,
//   flags: libc::c_int,
//   callback: extern "C" fn(
//     libc::c_int,
//     *mut u8,
//     usize,
//     *const libc::sockaddr_storage,
//   ),
// ) {
//   // SAFETY: C caller transfers malloc ownership of buf with size buf_len
//   let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::recvfrom(&resource, vec, Some(flags))
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |(res, mut buf, addr)| {
//       let (code, addr_ptr): (i32, *const libc::sockaddr_storage) = match res {
//         Ok(n) => {
//           let addr_ptr = match addr {
//             Some(a) => Box::into_raw(Box::new(std_socketaddr_into_libc(a))),
//             None => ptr::null_mut(),
//           };
//           (n, addr_ptr)
//         }
//         Err(e) => (-e.raw_os_error().unwrap_or(1), ptr::null()),
//       };
//       let ptr = buf.as_mut_ptr();
//       let len = buf.len();
//       std::mem::forget(buf);
//       callback(code, ptr, len, addr_ptr);
//     });
// }

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
  _mode: libc::mode_t,
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
  // SAFETY: caller guarantees lio is valid per fn contract
  api::openat(&dir_resource, path_owned, flags)
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
/// Ownership of `buf` transfers to lio.
///
/// - `callback(result, buf, len)`: bytes read (or negative errno), buffer
///
/// # Safety
/// `lio` must be valid; `buf` must be allocated with malloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_read(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::read(&resource, vec).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |(res, mut buf)| {
      let code = match res {
        Ok(n) => n,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      };
      let ptr = buf.as_mut_ptr();
      let len = buf.len();
      std::mem::forget(buf);
      callback(code, ptr, len);
    },
  );
}

/// Write to fd at current position.
///
/// Ownership of `buf` transfers to lio.
///
/// - `callback(result, buf, len)`: bytes written (or negative errno), buffer
///
/// # Safety
/// `lio` must be valid; `buf` must be allocated with malloc.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lio_write(
  lio: *mut lio_handle_t,
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  callback: extern "C" fn(libc::c_int, *mut u8, usize),
) {
  // SAFETY: C caller transfers malloc ownership of buf with size buf_len
  let vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  // SAFETY: caller guarantees fd is valid per fn contract
  let resource = unsafe { fd_to_borrowed_resource(fd) };
  // SAFETY: caller guarantees lio is valid per fn contract
  api::write(&resource, vec).with_lio(&unsafe { handle(lio) }.inner).when_done(
    move |(res, mut buf)| {
      let code = match res {
        Ok(n) => n,
        Err(e) => -e.raw_os_error().unwrap_or(1),
      };
      let ptr = buf.as_mut_ptr();
      let len = buf.len();
      std::mem::forget(buf);
      callback(code, ptr, len);
    },
  );
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
  api::unlinkat(&dir_res, path_cstr, flags)
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
  api::mkdirat(&dir_res, path_cstr, mode.into())
    .with_lio(&unsafe { handle(lio) }.inner)
    .when_done(move |res| {
      callback(match res {
        Ok(_) => 0,
        Err(e) => -e.raw_os_error().unwrap_or(1),
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
// /// - `buf`: Buffer to read entries into (must be malloc'd)
// /// - `buf_len`: Size of buffer
// /// - `callback(result, buf, len)`: bytes read (0=EOF, negative=error), buffer
// ///
// /// # Safety
// /// `lio` must be valid; `fd` must be an open directory; `buf` must be malloc'd.
// #[unsafe(no_mangle)]
// pub unsafe extern "C" fn lio_getdents(
//   lio: *mut lio_handle_t,
//   fd: libc::intptr_t,
//   buf: *mut u8,
//   buf_len: libc::size_t,
//   callback: extern "C" fn(libc::c_int, *mut u8, libc::size_t),
// ) {
//   // SAFETY: C caller transfers malloc ownership of buf with size buf_len
//   let vec = unsafe { Vec::from_raw_parts(buf, 0, buf_len) };
//   // SAFETY: caller guarantees fd is valid per fn contract
//   let resource = unsafe { fd_to_borrowed_resource(fd) };
//   // SAFETY: caller guarantees lio is valid per fn contract
//   api::getdents(&resource, vec)
//     .with_lio(&unsafe { handle(lio) }.inner)
//     .when_done(move |(res, mut buf, _entries)| {
//       // We ignore the parsed entries and just return raw bytes
//       let code = match res {
//         Ok(n) => n,
//         Err(e) => -e.raw_os_error().unwrap_or(1),
//       };
//       let ptr = buf.as_mut_ptr();
//       let len = buf.len();
//       std::mem::forget(buf);
//       callback(code, ptr, len);
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
