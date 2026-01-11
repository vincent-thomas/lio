//! # `lio` C API
//!
//! ## Cross-Platform Compatibility
//!
//! This FFI uses `intptr_t` for file descriptors/handles to support both Unix and Windows:
//! - **Unix/POSIX**: File descriptors are `int` (32-bit), which fits in `intptr_t`
//! - **Windows**: Handles are `HANDLE` (pointer-sized), which fit exactly in `intptr_t`
//!
//! This follows the C ecosystem standard (used by libuv, etc.) for cross-platform I/O APIs.
//! Include `<stdint.h>` in your C code to use `intptr_t`.
//!
//! ## Compiling
//! `lio` can be compiled using cargo with command:
//! ```sh
//! make cbuild
//! ```
//! `lio` dynamic library can be found at `target/release/liblio.{dylib,dll,so}`
//!
//! ## Buffer Ownership Model
//!
//! These I/O operations: `read`, `write`, `send`, `recv` use zero-copy ownership transfer:
//!
//! 1. The caller is responsible for buffer allocation.
//! 2. Pass to lio function - ownership transfers to Rust.
//! 3. Do not access or free buffer until callback. UB otherwise.
//! 4. Callback returns buffer, which the caller is responsible for de-allocating.
//!
//! Callbacks are guaranteed to be called. Cancellation is not supported.
//!
//! ## Example
//!
//! ```c
//! void write_done(int result, uint8_t *buf, size_t len) {
//!     printf("Wrote %d bytes\n", result);
//!     free(buf);  // Required
//! }
//!
//! uint8_t *buf = malloc(1024);
//! memcpy(buf, "data", 4);
//! lio_write(fd, buf, 1024, 0, write_done);
//! // buf is now owned by lio
//! ```
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::{ptr, time::Duration};

#[cfg(unix)]
use crate::api::resource::UniqueResource;
use crate::{
  driver::TryInitError,
  op::net_utils::{self, sockaddr_to_socketaddr},
  resource::Resource,
};

#[cfg(unix)]
use std::os::fd::FromRawFd;

#[cfg(windows)]
use std::os::windows::io::FromRawHandle;

/// Converts a C file descriptor/handle (intptr_t) to a Resource.
///
/// # Safety
/// The caller must ensure the fd/handle is valid.
#[cfg(unix)]
unsafe fn fd_to_resource(fd: libc::intptr_t) -> Resource {
  unsafe { Resource::from_raw_fd(fd as i32) }
}

/// Converts a C file descriptor/handle (intptr_t) to a Resource.
///
/// # Safety
/// The caller must ensure the fd/handle is valid.
#[cfg(unix)]
unsafe fn fd_to_unique(fd: libc::intptr_t) -> UniqueResource {
  use crate::api::resource::UniqueResource;

  unsafe { UniqueResource::from_raw_fd(fd as i32) }
}

#[cfg(windows)]
unsafe fn fd_to_resource(fd: libc::intptr_t) -> Resource {
  Resource::from_raw_handle(fd as *mut std::ffi::c_void)
}

/// Converts a Resource to a C file descriptor/handle (intptr_t).
#[cfg(unix)]
fn resource_to_fd(resource: &Resource) -> libc::intptr_t {
  use std::os::fd::{AsFd, AsRawFd};
  resource.as_fd().as_raw_fd() as libc::intptr_t
}

#[cfg(windows)]
fn resource_to_fd(resource: &Resource) -> libc::intptr_t {
  use std::os::windows::io::{AsHandle, AsRawHandle};
  resource.as_handle().as_raw_handle() as libc::intptr_t
}

#[unsafe(no_mangle)]
pub extern "C" fn lio_init() {
  crate::init()
}

#[unsafe(no_mangle)]
pub extern "C" fn lio_try_init() -> libc::c_int {
  match crate::try_init() {
    Ok(_) => 0,
    Err(err) => match err {
      TryInitError::AlreadyInit => -1,
      TryInitError::Io(io) => io.raw_os_error().unwrap_or(-1),
    },
  }
}

/// Shutdown the lio runtime and wait for all pending operations to complete.
///
/// This function blocks until all pending I/O operations finish and their callbacks are called.
/// After calling this, no new operations should be submitted.
#[unsafe(no_mangle)]
pub extern "C" fn lio_exit() {
  crate::exit()
}

#[unsafe(no_mangle)]
pub extern "C" fn lio_tick() {
  crate::tick()
}

/// Shut down part of a full-duplex connection.
///
/// # Parameters
/// - `fd`: Socket file descriptor (intptr_t for cross-platform compatibility)
/// - `how`: How to shutdown (SHUT_RD=0, SHUT_WR=1, SHUT_RDWR=2)
/// - `callback(result)`: Called when complete
///   - `result`: 0 on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_shutdown(
  fd: libc::intptr_t,
  how: i32,
  callback: extern "C" fn(i32),
) {
  let resource = unsafe { fd_to_resource(fd) };
  crate::shutdown(resource, how).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}

// TODO
#[allow(unused)]
#[unsafe(no_mangle)]
pub extern "C" fn lio_symlinkat(
  new_dir_fd: libc::intptr_t,
  target: *const libc::c_char,
  linkpath: *const libc::c_char,
  callback: extern "C" fn(i32),
) {
  todo!();
  // let resource = unsafe { fd_to_resource(new_dir_fd) };
  // crate::symlinkat(resource, target.).when_done(move |res| {
  //   let result_code = match res {
  //     Ok(_) => 0,
  //     Err(err) => err.raw_os_error().unwrap_or(-1),
  //   };
  //   callback(result_code);
  // });
}

// TODO
#[allow(unused)]
#[unsafe(no_mangle)]
pub extern "C" fn lio_linkat(
  old_dir_fd: libc::intptr_t,
  old_path: *const libc::c_char,
  new_dir_fd: libc::intptr_t,
  new_path: *const libc::c_char,
  callback: extern "C" fn(i32),
) {
  todo!();
  // let old_resource = unsafe { fd_to_resource(old_dir_fd) };
  // let new_resource = unsafe { fd_to_resource(new_dir_fd) };
  // crate::linkat(old_resource, target.).when_done(move |res| {
  //   let result_code = match res {
  //     Ok(_) => 0,
  //     Err(err) => err.raw_os_error().unwrap_or(-1),
  //   };
  //   callback(result_code);
  // });
}

/// Synchronize a file's in-core state with storage device.
///
/// # Parameters
/// - `fd`: File descriptor (intptr_t for cross-platform compatibility)
/// - `callback(result)`: Called when complete
///   - `result`: 0 on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_fsync(fd: libc::intptr_t, callback: extern "C" fn(i32)) {
  let resource = unsafe { fd_to_resource(fd) };
  crate::fsync(resource).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}

/// Write data to a file descriptor.
///
/// Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
///
/// # Parameters
/// - `fd`: File descriptor (intptr_t for cross-platform compatibility)
/// - `buf`: malloc-allocated buffer containing data to write
/// - `buf_len`: Buffer length in bytes
/// - `offset`: File offset, or -1 for current position
/// - `callback(result, buf, len)`: Called when complete
///   - `result`: Bytes written, or negative errno on error
///   - `buf`: Original buffer pointer (must free)
///   - `len`: Original buffer length
#[unsafe(no_mangle)]
pub extern "C" fn lio_write_at(
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  offset: i64,
  callback: extern "C" fn(i32, *mut u8, usize),
) {
  // SAFETY: Take ownership of the C buffer by reconstructing the Vec.
  // This is safe because:
  // 1. The C caller has allocated this with malloc (same allocator as Vec)
  // 2. We're taking exclusive ownership (C must not touch it anymore)
  // 3. We'll return it via the callback where C can free it
  let buf_vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  let resource = unsafe { fd_to_resource(fd) };

  crate::write_at_with_buf(resource, buf_vec, offset).when_done(
    move |(res, mut buf)| {
      let result_code = match res {
        Ok(n) => n,
        Err(err) => err.raw_os_error().unwrap_or(-1),
      };

      // Return buffer ownership to C caller
      let buf_ptr = buf.as_mut_ptr();
      let buf_len = buf.len();

      // SAFETY: Prevent Rust from freeing the buffer since we're giving ownership back to C.
      // C must now free this buffer to avoid memory leak.
      std::mem::forget(buf);

      callback(result_code, buf_ptr, buf_len);
    },
  );
}

/// Read data from a file descriptor.
///
/// Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
///
/// # Parameters
/// - `fd`: File descriptor (intptr_t for cross-platform compatibility)
/// - `buf`: malloc-allocated buffer to read into
/// - `buf_len`: Buffer length in bytes
/// - `offset`: File offset, or -1 for current position
/// - `callback(result, buf, len)`: Called when complete
///   - `result`: Bytes read (check this, not `len`), 0 on EOF, or negative errno on error
///   - `buf`: Original buffer pointer containing data (must free)
///   - `len`: Original buffer length
#[unsafe(no_mangle)]
pub extern "C" fn lio_read_at(
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  offset: i64,
  callback: extern "C" fn(i32, *mut u8, usize),
) {
  // SAFETY: Take ownership of the C buffer by reconstructing the Vec.
  // This is safe because:
  // 1. The C caller has allocated this with malloc (same allocator as Vec)
  // 2. We're taking exclusive ownership (C must not touch it anymore)
  // 3. We'll return it via the callback where C can free it
  let buf_vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  let resource = unsafe { fd_to_resource(fd) };

  crate::read_at_with_buf(resource, buf_vec, offset).when_done(
    move |(res, mut buf)| {
      let result_code = match res {
        Ok(n) => n,
        Err(err) => err.raw_os_error().unwrap_or(-1),
      };

      // Return buffer ownership to C caller
      let buf_ptr = buf.as_mut_ptr();
      let buf_len = buf.len();

      // SAFETY: Prevent Rust from freeing the buffer since we're giving ownership back to C.
      // C must now free this buffer to avoid memory leak.
      std::mem::forget(buf);

      callback(result_code, buf_ptr, buf_len);
    },
  );
}

/// Truncate a file to a specified length.
///
/// # Parameters
/// - `fd`: File descriptor (intptr_t for cross-platform compatibility)
/// - `len`: New file length in bytes
/// - `callback(result)`: Called when complete
///   - `result`: 0 on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_truncate(
  fd: libc::intptr_t,
  len: u64,
  callback: extern "C" fn(i32),
) {
  let resource = unsafe { fd_to_resource(fd) };
  crate::truncate(resource, len).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}

/// Create a socket.
///
/// # Parameters
/// - `domain`: Protocol family (AF_INET=2, AF_INET6=10, etc.)
/// - `ty`: Socket type (SOCK_STREAM=1, SOCK_DGRAM=2, etc.)
/// - `proto`: Protocol (IPPROTO_TCP=6, IPPROTO_UDP=17, or 0 for default)
/// - `callback(result)`: Called when complete
///   - `result`: Socket file descriptor (intptr_t) on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_socket(
  domain: i32,
  ty: i32,
  proto: i32,
  callback: extern "C" fn(libc::intptr_t),
) {
  crate::socket(domain, ty, proto).when_done(move |res| {
    let result_code = match res {
      Ok(resource) => resource_to_fd(&resource),
      Err(err) => err.raw_os_error().unwrap_or(-1) as libc::intptr_t,
    };
    callback(result_code);
  });
}

/// Bind a socket to an address.
///
/// # Parameters
/// - `fd`: Socket file descriptor (intptr_t for cross-platform compatibility)
/// - `sock`: Pointer to sockaddr structure (sockaddr_in or sockaddr_in6)
/// - `sock_len`: Pointer to size of sockaddr structure
/// - `callback(result)`: Called when complete
///   - `result`: 0 on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_bind(
  fd: libc::intptr_t,
  sock: *const libc::sockaddr,
  sock_len: *const libc::socklen_t,
  callback: extern "C" fn(i32),
) {
  // TODO: fix unwrap.
  let addr = sockaddr_to_socketaddr(sock, unsafe { *sock_len }).unwrap();
  let resource = unsafe { fd_to_resource(fd) };
  // TODO: Optimise
  crate::bind(resource, addr).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}

/// Accept a connection on a socket.
///
/// # Parameters
/// - `fd`: Listening socket file descriptor (intptr_t for cross-platform compatibility)
/// - `callback(result, addr)`: Called when complete
///   - `result`: New socket file descriptor (intptr_t) on success, or negative errno on error
///   - `addr`: Pointer to peer address (null on error, caller must free on success)
#[unsafe(no_mangle)]
pub extern "C" fn lio_accept(
  fd: libc::intptr_t,
  callback: extern "C" fn(libc::intptr_t, *const libc::sockaddr_storage),
) {
  let resource = unsafe { fd_to_resource(fd) };
  crate::accept(resource).when_done(move |res| {
    let (res, addr) = match res {
      Ok((new_resource, addr)) => (
        resource_to_fd(&new_resource),
        Box::into_raw(Box::new(net_utils::std_socketaddr_into_libc(addr)))
          as *const _,
      ),
      Err(err) => {
        (err.raw_os_error().unwrap_or(-1) as libc::intptr_t, ptr::null())
      }
    };

    callback(res, addr)
  });
}

/// Listen for connections on a socket.
///
/// # Parameters
/// - `fd`: Socket file descriptor (intptr_t for cross-platform compatibility)
/// - `backlog`: Maximum length of pending connections queue
/// - `callback(result)`: Called when complete
///   - `result`: 0 on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_listen(
  fd: libc::intptr_t,
  backlog: i32,
  callback: extern "C" fn(i32),
) {
  let resource = unsafe { fd_to_resource(fd) };
  crate::listen(resource, backlog).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}

/// Send data to a socket.
///
/// Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
///
/// # Parameters
/// - `fd`: Socket file descriptor (intptr_t for cross-platform compatibility)
/// - `buf`: malloc-allocated buffer containing data to send
/// - `buf_len`: Buffer length in bytes
/// - `flags`: Send flags (e.g., MSG_DONTWAIT, MSG_NOSIGNAL)
/// - `callback(result, buf, len)`: Called when complete
///   - `result`: Bytes sent, or negative errno on error
///   - `buf`: Original buffer pointer (must free)
///   - `len`: Original buffer length
#[unsafe(no_mangle)]
pub extern "C" fn lio_send(
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: i32,
  callback: extern "C" fn(i32, *mut u8, usize),
) {
  // SAFETY: Take ownership of the C buffer by reconstructing the Vec.
  // This is safe because:
  // 1. The C caller has allocated this with malloc (same allocator as Vec)
  // 2. We're taking exclusive ownership (C must not touch it anymore)
  // 3. We'll return it via the callback where C can free it
  let buf_vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  let resource = unsafe { fd_to_resource(fd) };

  crate::send_with_buf(resource, buf_vec, Some(flags)).when_done(
    move |(res, mut buf)| {
      let result_code = match res {
        Ok(n) => n,
        Err(err) => err.raw_os_error().unwrap_or(-1),
      };

      // Return buffer ownership to C caller
      let buf_ptr = buf.as_mut_ptr();
      let buf_len = buf.len();

      // SAFETY: Prevent Rust from freeing the buffer since we're giving ownership back to C.
      // C must now free this buffer to avoid memory leak.
      std::mem::forget(buf);

      callback(result_code, buf_ptr, buf_len);
    },
  );
}

/// Receive data from a socket.
///
/// Ownership of `buf` transfers to lio and returns via callback. See module docs for details.
///
/// # Parameters
/// - `fd`: Socket file descriptor (intptr_t for cross-platform compatibility)
/// - `buf`: malloc-allocated buffer to receive into
/// - `buf_len`: Buffer length in bytes
/// - `flags`: Receive flags (e.g., MSG_PEEK, MSG_WAITALL)
/// - `callback(result, buf, len)`: Called when complete
///   - `result`: Bytes received (check this, not `len`), or negative errno on error
///   - `buf`: Original buffer pointer containing data (must free)
///   - `len`: Original buffer length
#[unsafe(no_mangle)]
pub extern "C" fn lio_recv(
  fd: libc::intptr_t,
  buf: *mut u8,
  buf_len: usize,
  flags: i32,
  callback: extern "C" fn(i32, *mut u8, usize),
) {
  // SAFETY: Take ownership of the C buffer by reconstructing the Vec.
  // This is safe because:
  // 1. The C caller has allocated this with malloc (same allocator as Vec)
  // 2. We're taking exclusive ownership (C must not touch it anymore)
  // 3. We'll return it via the callback where C can free it
  let buf_vec = unsafe { Vec::from_raw_parts(buf, buf_len, buf_len) };
  let resource = unsafe { fd_to_resource(fd) };

  crate::recv_with_buf(resource, buf_vec, Some(flags)).when_done(
    move |(res, mut buf)| {
      let result_code = match res {
        Ok(n) => n,
        Err(err) => err.raw_os_error().unwrap_or(-1),
      };

      // Return buffer ownership to C caller
      let buf_ptr = buf.as_mut_ptr();
      let buf_len = buf.len();

      // SAFETY: Prevent Rust from freeing the buffer since we're giving ownership back to C.
      // C must now free this buffer to avoid memory leak.
      std::mem::forget(buf);

      callback(result_code, buf_ptr, buf_len);
    },
  );
}

/// Close a file descriptor.
///
/// # Parameters
/// - `fd`: File descriptor to close (intptr_t for cross-platform compatibility)
/// - `callback(result)`: Called when complete
/// - `result`: 0 on success, or negative errno on error
#[unsafe(no_mangle)]
pub extern "C" fn lio_close(fd: libc::intptr_t, callback: extern "C" fn(i32)) {
  let resource = unsafe { fd_to_unique(fd) };
  crate::close(resource).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}

/// Waits for a specified duration.
///
/// # Parameters
/// - `millis`: Duration to sleep in milliseconds.
/// - `callback(result)`: Called when complete
#[unsafe(no_mangle)]
pub extern "C" fn lio_timeout(
  millis: libc::c_int,
  callback: extern "C" fn(i32),
) {
  if millis < 0 {
    callback(libc::EINVAL);
    return;
  }
  crate::timeout(Duration::from_millis(millis as u64)).when_done(move |res| {
    let result_code = match res {
      Ok(_) => 0,
      Err(err) => err.raw_os_error().unwrap_or(-1),
    };
    callback(result_code);
  });
}
