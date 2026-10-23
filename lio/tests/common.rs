//! Test helpers shared by the integration tests.
//!
//! Platform differences live here so the tests themselves stay portable.

use std::{
  ffi::CString,
  fs::File,
  mem::ManuallyDrop,
  net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
  path::PathBuf,
  sync::mpsc,
  time::Duration,
};

use lio::{Lio, api::io::Receiver, api::resource::Resource};

// ============================================================================
// Ownership transfer between std types and `Resource`
// ============================================================================

/// Transfers ownership of a std socket to a `Resource`.
#[cfg(unix)]
#[allow(dead_code)]
pub fn socket_resource(socket: impl std::os::fd::IntoRawFd) -> Resource {
  use std::os::fd::FromRawFd;
  // SAFETY: the socket hands its owned descriptor to exactly one Resource.
  unsafe { Resource::from_raw_fd(socket.into_raw_fd()) }
}

/// Transfers ownership of a std socket to a `Resource`.
#[cfg(windows)]
#[allow(dead_code)]
pub fn socket_resource(
  socket: impl std::os::windows::io::IntoRawSocket,
) -> Resource {
  use std::os::windows::io::FromRawHandle;
  // SAFETY: the socket hands its owned handle to exactly one Resource.
  unsafe { Resource::from_raw_handle(socket.into_raw_socket() as _) }
}

/// Transfers ownership of a file to a `Resource`.
#[allow(dead_code)]
pub fn file_resource(file: File) -> Resource {
  #[cfg(unix)]
  {
    use std::os::fd::{FromRawFd, IntoRawFd};
    // SAFETY: the file hands its owned descriptor to exactly one Resource.
    unsafe { Resource::from_raw_fd(file.into_raw_fd()) }
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};
    // SAFETY: the file hands its owned handle to exactly one Resource.
    unsafe { Resource::from_raw_handle(file.into_raw_handle()) }
  }
}

/// A [`FileMode`](lio::api::FileMode) written in octal, such as `0o644`.
#[allow(dead_code)]
pub fn file_mode(octal: u32) -> lio::api::FileMode {
  lio::api::FileMode::from_octal(octal).expect("mode sets only permission bits")
}

/// Opens a directory as a `Resource` (for `readdir` and `*at` operations).
#[allow(dead_code)]
pub fn open_dir(path: &std::path::Path) -> Resource {
  #[cfg(unix)]
  let dir = File::open(path);
  #[cfg(windows)]
  let dir = {
    use std::os::windows::fs::OpenOptionsExt;
    // Directories can only be opened with FILE_FLAG_BACKUP_SEMANTICS.
    std::fs::OpenOptions::new().read(true).custom_flags(0x0200_0000).open(path)
  };
  file_resource(dir.expect("failed to open directory"))
}

/// A resource that is never a valid descriptor/handle. It is borrowed, so
/// dropping it never closes anything.
#[allow(dead_code)]
pub fn invalid_resource() -> Resource {
  #[cfg(unix)]
  // SAFETY: -1 is never a valid descriptor and is never closed.
  return unsafe { Resource::borrow(-1) };
  #[cfg(windows)]
  // SAFETY: the null handle is never valid and is never closed.
  return unsafe { Resource::borrow(std::ptr::null_mut()) };
}

/// Borrows a socket `Resource` as a std `TcpStream`, for inspecting it or for
/// synchronous I/O in tests. The resource stays owned by `res`.
#[allow(dead_code)]
pub fn as_tcp_stream(res: &Resource) -> ManuallyDrop<TcpStream> {
  #[cfg(unix)]
  {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: `ManuallyDrop` never closes the borrowed descriptor.
    ManuallyDrop::new(unsafe { TcpStream::from_raw_fd(res.as_raw_fd()) })
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::{AsRawHandle, FromRawSocket};
    // SAFETY: `ManuallyDrop` never closes the borrowed socket.
    ManuallyDrop::new(unsafe {
      TcpStream::from_raw_socket(res.as_raw_handle() as u64)
    })
  }
}

/// Borrows a socket `Resource` as a std `UdpSocket`.
#[allow(dead_code)]
pub fn as_udp_socket(res: &Resource) -> ManuallyDrop<UdpSocket> {
  #[cfg(unix)]
  {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: `ManuallyDrop` never closes the borrowed descriptor.
    ManuallyDrop::new(unsafe { UdpSocket::from_raw_fd(res.as_raw_fd()) })
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::{AsRawHandle, FromRawSocket};
    // SAFETY: `ManuallyDrop` never closes the borrowed socket.
    ManuallyDrop::new(unsafe {
      UdpSocket::from_raw_socket(res.as_raw_handle() as u64)
    })
  }
}

/// Borrows a file `Resource` as a std `File`.
#[allow(dead_code)]
pub fn as_file(res: &Resource) -> ManuallyDrop<File> {
  #[cfg(unix)]
  {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: `ManuallyDrop` never closes the borrowed descriptor.
    ManuallyDrop::new(unsafe { File::from_raw_fd(res.as_raw_fd()) })
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    // SAFETY: `ManuallyDrop` never closes the borrowed handle.
    ManuallyDrop::new(unsafe { File::from_raw_handle(res.as_raw_handle()) })
  }
}

/// The raw descriptor/handle value, for identity comparisons.
#[allow(dead_code)]
pub fn raw_id(res: &Resource) -> u64 {
  #[cfg(unix)]
  {
    use std::os::fd::AsRawFd;
    res.as_raw_fd() as u64
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::AsRawHandle;
    res.as_raw_handle() as u64
  }
}

// ============================================================================
// Socket creation and inspection
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SocketKind {
  Stream,
  Datagram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum Family {
  Ipv4,
  Ipv6,
  Unix,
}

#[cfg(unix)]
fn create_socket(family: Family, kind: SocketKind) -> Resource {
  use std::os::fd::FromRawFd;
  let domain = match family {
    Family::Ipv4 => libc::AF_INET,
    Family::Ipv6 => libc::AF_INET6,
    Family::Unix => libc::AF_UNIX,
  };
  let ty = match kind {
    SocketKind::Stream => libc::SOCK_STREAM,
    SocketKind::Datagram => libc::SOCK_DGRAM,
  };
  // SAFETY: plain socket creation.
  let fd = unsafe { libc::socket(domain, ty, 0) };
  assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
  // SAFETY: `fd` is a fresh descriptor owned by the returned Resource.
  unsafe { Resource::from_raw_fd(fd) }
}

#[cfg(windows)]
fn create_socket(family: Family, kind: SocketKind) -> Resource {
  use std::os::windows::io::FromRawHandle;
  use windows_sys::Win32::Networking::WinSock as ws;
  // std initializes Winsock on first use.
  drop(UdpSocket::bind("127.0.0.1:0"));
  let domain = match family {
    Family::Ipv4 => ws::AF_INET,
    Family::Ipv6 => ws::AF_INET6,
    Family::Unix => ws::AF_UNIX,
  };
  let ty = match kind {
    SocketKind::Stream => ws::SOCK_STREAM,
    SocketKind::Datagram => ws::SOCK_DGRAM,
  };
  // SAFETY: plain socket creation.
  let socket = unsafe {
    ws::WSASocketW(
      domain as i32,
      ty,
      0,
      std::ptr::null(),
      0,
      ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT,
    )
  };
  assert_ne!(
    socket,
    ws::INVALID_SOCKET,
    "WSASocketW failed: {}",
    std::io::Error::last_os_error()
  );
  // SAFETY: `socket` is fresh and owned by the returned Resource.
  unsafe { Resource::from_raw_handle(socket as _) }
}

/// Creates a Unix stream socket resource.
#[allow(dead_code)]
pub fn unix_stream_socket() -> Resource {
  create_socket(Family::Unix, SocketKind::Stream)
}

/// Creates a Unix datagram socket resource (Windows has no `AF_UNIX`
/// datagrams).
#[cfg(unix)]
#[allow(dead_code)]
pub fn unix_dgram_socket() -> Resource {
  create_socket(Family::Unix, SocketKind::Datagram)
}

/// Creates a TCP IPv4 socket resource.
#[allow(dead_code)]
pub fn tcp_socket() -> Resource {
  create_socket(Family::Ipv4, SocketKind::Stream)
}

/// Creates a TCP IPv6 socket resource.
#[allow(dead_code)]
pub fn tcp6_socket() -> Resource {
  create_socket(Family::Ipv6, SocketKind::Stream)
}

/// Creates a UDP IPv4 socket resource.
#[allow(dead_code)]
pub fn udp_socket() -> Resource {
  create_socket(Family::Ipv4, SocketKind::Datagram)
}

/// Creates a UDP IPv6 socket resource.
#[allow(dead_code)]
pub fn udp6_socket() -> Resource {
  create_socket(Family::Ipv6, SocketKind::Datagram)
}

/// Reports whether a socket is a stream or datagram socket (`SO_TYPE`).
#[allow(dead_code)]
pub fn socket_kind(res: &Resource) -> SocketKind {
  let mut ty: i32 = 0;
  #[cfg(unix)]
  {
    use std::os::fd::AsRawFd;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    // SAFETY: `ty`/`len` are valid output storage for `SO_TYPE`.
    let rc = unsafe {
      libc::getsockopt(
        res.as_raw_fd(),
        libc::SOL_SOCKET,
        libc::SO_TYPE,
        (&mut ty as *mut i32).cast(),
        &mut len,
      )
    };
    assert_eq!(rc, 0, "getsockopt: {}", std::io::Error::last_os_error());
    match ty {
      libc::SOCK_STREAM => SocketKind::Stream,
      libc::SOCK_DGRAM => SocketKind::Datagram,
      other => panic!("unexpected socket type {other}"),
    }
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Networking::WinSock as ws;
    let mut len = std::mem::size_of::<i32>() as i32;
    // SAFETY: `ty`/`len` are valid output storage for `SO_TYPE`.
    let rc = unsafe {
      ws::getsockopt(
        res.as_raw_handle() as ws::SOCKET,
        ws::SOL_SOCKET,
        ws::SO_TYPE,
        (&mut ty as *mut i32).cast(),
        &mut len,
      )
    };
    assert_eq!(rc, 0, "getsockopt: {}", std::io::Error::last_os_error());
    match ty {
      ws::SOCK_STREAM => SocketKind::Stream,
      ws::SOCK_DGRAM => SocketKind::Datagram,
      other => panic!("unexpected socket type {other}"),
    }
  }
}

/// Sets `SO_REUSEADDR` on a socket.
#[allow(dead_code)]
pub fn set_reuse_addr(res: &Resource) {
  let on: i32 = 1;
  #[cfg(unix)]
  // SAFETY: `on` is a valid `int` option value.
  let rc = unsafe {
    use std::os::fd::AsRawFd;
    libc::setsockopt(
      res.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      (&on as *const i32).cast(),
      std::mem::size_of::<i32>() as libc::socklen_t,
    )
  };
  #[cfg(windows)]
  // SAFETY: `on` is a valid `int` option value.
  let rc = unsafe {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Networking::WinSock as ws;
    ws::setsockopt(
      res.as_raw_handle() as ws::SOCKET,
      ws::SOL_SOCKET,
      ws::SO_REUSEADDR,
      (&on as *const i32).cast(),
      std::mem::size_of::<i32>() as i32,
    )
  };
  assert_eq!(rc, 0, "setsockopt: {}", std::io::Error::last_os_error());
}

/// The error code for "the caller's buffer is too small" (`getcwd`):
/// `ERANGE` on Unix, `ERROR_INSUFFICIENT_BUFFER` on Windows.
#[cfg(unix)]
#[allow(dead_code)]
pub const BUFFER_TOO_SMALL: i32 = libc::ERANGE;
#[cfg(windows)]
#[allow(dead_code)]
pub const BUFFER_TOO_SMALL: i32 = 122;

// ============================================================================
// Paths
// ============================================================================

/// Creates a symlink to a file.
#[allow(dead_code)]
pub fn symlink_file(target: &std::path::Path, link: &std::path::Path) {
  #[cfg(unix)]
  std::os::unix::fs::symlink(target, link).expect("symlink failed");
  #[cfg(windows)]
  std::os::windows::fs::symlink_file(target, link).expect("symlink failed");
}

/// A path in the system temp directory, unique to this process and `name`.
#[allow(dead_code)]
pub fn temp_path(name: &str) -> PathBuf {
  std::env::temp_dir().join(format!("lio_test_{}_{}", name, std::process::id()))
}

/// Like [`temp_path`], as the `CString` the path-based APIs take.
#[allow(dead_code)]
pub fn temp_cpath(name: &str) -> CString {
  path_to_cstring(&temp_path(name))
}

#[allow(dead_code)]
pub fn path_to_cstring(path: &std::path::Path) -> CString {
  CString::new(path.to_str().expect("test paths are UTF-8"))
    .expect("test paths have no NUL bytes")
}

/// A unique path in the *simulation* backend's synthetic filesystem, for
/// tests that run on `DSBackend`. The simulator models Unix-style absolute
/// paths, so this is `/tmp/...` on every platform; use [`temp_path`] for tests
/// that touch the real filesystem.
#[allow(dead_code)]
pub fn make_temp_path(test_name: &str, unique_value: u64) -> CString {
  CString::new(format!(
    "/tmp/lio_test_{}_{}_{}.txt",
    test_name,
    std::process::id(),
    unique_value
  ))
  .expect("Failed to create CString path")
}

/// A connected TCP client-server pair.
#[allow(dead_code)]
pub struct TcpPair {
  pub server_sock: Resource,
  pub client_sock: Resource,
  pub accepted_fd: Resource,
}

/// Setup a connected TCP client-server pair.
/// Returns a `TcpPair` with the listening socket, the connected client socket,
/// and the accepted server-side connection.
#[allow(dead_code)]
pub fn setup_tcp_pair(_lio: &mut Lio) -> TcpPair {
  let listener =
    TcpListener::bind("127.0.0.1:0").expect("Failed to bind server socket");
  let addr =
    listener.local_addr().expect("Failed to query bound server address");
  let client =
    TcpStream::connect(addr).expect("Failed to connect client socket");
  let (accepted, _) =
    listener.accept().expect("Failed to accept client connection");

  TcpPair {
    server_sock: socket_resource(listener),
    client_sock: socket_resource(client),
    accepted_fd: socket_resource(accepted),
  }
}

/// Get the bound address of a socket.
#[allow(dead_code)]
pub fn get_bound_addr(sock: &Resource) -> SocketAddr {
  as_tcp_stream(sock).local_addr().expect("getsockname failed")
}

/// RAII temporary file that cleans up on drop.
#[allow(dead_code)]
pub struct TempFile {
  pub path: CString,
}

impl TempFile {
  #[allow(dead_code)]
  pub fn new(name: &str) -> Self {
    Self { path: temp_cpath(&format!("{name}.txt")) }
  }
}

impl Drop for TempFile {
  fn drop(&mut self) {
    let _ = std::fs::remove_file(self.path.to_str().unwrap());
  }
}

/// Poll the lio event loop until a result is received on the channel.
/// Blocks in kqueue/epoll for up to 5ms per iteration — no busy-spin, no attempt cap.
#[allow(dead_code)]
pub fn poll_until_recv<T>(lio: &mut Lio, receiver: &mpsc::Receiver<T>) -> T {
  {
    for _ in 0..1_000 {
      match receiver.try_recv() {
        Ok(result) => return result,
        Err(mpsc::TryRecvError::Empty) => {
          let _ = lio.run_timeout(Duration::from_millis(5));
        }
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
    panic!("poll_until_recv exceeded 1000 waits under Miri");
  }
}

/// Poll the lio event loop until a lio [`Receiver`] has a result.
///
/// This helper runs the event loop in short iterations (5ms timeout each) until
/// the async operation completes. Unlike blocking `.recv()`, this ensures the
/// event loop continues processing, preventing deadlocks in tests.
///
/// # Example
///
/// ```ignore
/// let mut recv = api::connect(&sock, addr).with_lio(&mut lio).send();
/// let result = poll_recv(&mut lio, &mut recv);
/// ```
///
/// # Panics
///
/// - Panics if the operation doesn't complete within 5 seconds.
/// - Panics if the sender is dropped without sending (internal lio error).
#[allow(dead_code)]
pub fn poll_recv<T>(lio: &mut Lio, receiver: &mut Receiver<T>) -> T {
  #[cfg(miri)]
  {
    for _ in 0..1_000 {
      if let Some(result) = receiver.try_recv() {
        return result;
      }
      let _ = lio.run().unwrap();
    }
    panic!("poll_recv exceeded 1000 waits under Miri");
  }

  #[cfg(not(miri))]
  let start = std::time::Instant::now();
  #[cfg(not(miri))]
  let timeout = Duration::from_secs(5);

  #[cfg(not(miri))]
  loop {
    if let Some(result) = receiver.try_recv() {
      return result;
    }
    if start.elapsed() > timeout {
      panic!(
        "poll_recv timed out after {:?} waiting for operation to complete",
        timeout
      );
    }
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  }
}
