//! OS-dependent socket address conversion utilities.
//!
//! These helpers convert between lio's abstract [`SocketAddrBuf`] and the
//! platform-native sockaddr types used by syscalls.  They live in
//! `backend/impls/` because every line of code here depends on a specific
//! OS (`libc` on Unix, `windows-sys` on Windows).

use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::backend::op::{
  SockDomain, SockProto, SockType, SocketAddrBuf, SocketAddrFamily,
  is_supported_socket, socket_addr_into_buf, unix_socket_addr_buf,
};

pub(crate) use sys::{SockLen, Storage};

#[cfg(unix)]
mod sys {
  pub(crate) type Storage = libc::sockaddr_storage;
  pub(crate) type SockLen = libc::socklen_t;
  pub(super) type SockaddrIn = libc::sockaddr_in;
  pub(super) type SockaddrIn6 = libc::sockaddr_in6;
  pub(super) type SockaddrUn = libc::sockaddr_un;

  pub(super) const AF_INET: i32 = libc::AF_INET;
  pub(super) const AF_INET6: i32 = libc::AF_INET6;
  pub(super) const AF_UNIX: i32 = libc::AF_UNIX;
  pub(super) const SOCK_STREAM: i32 = libc::SOCK_STREAM;
  pub(super) const SOCK_DGRAM: i32 = libc::SOCK_DGRAM;
  pub(super) const IPPROTO_TCP: i32 = libc::IPPROTO_TCP;
  pub(super) const IPPROTO_UDP: i32 = libc::IPPROTO_UDP;

  pub(super) const EINVAL: i32 = libc::EINVAL;
  pub(super) const EAFNOSUPPORT: i32 = libc::EAFNOSUPPORT;
  pub(super) const ENAMETOOLONG: i32 = libc::ENAMETOOLONG;
}

#[cfg(windows)]
mod sys {
  use windows_sys::Win32::Networking::WinSock as ws;

  pub(crate) type Storage = ws::SOCKADDR_STORAGE;
  pub(crate) type SockLen = i32;
  pub(super) type SockaddrIn = ws::SOCKADDR_IN;
  pub(super) type SockaddrIn6 = ws::SOCKADDR_IN6;
  pub(super) type SockaddrUn = ws::SOCKADDR_UN;

  pub(super) const AF_INET: i32 = ws::AF_INET as i32;
  pub(super) const AF_INET6: i32 = ws::AF_INET6 as i32;
  pub(super) const AF_UNIX: i32 = ws::AF_UNIX as i32;
  pub(super) const SOCK_STREAM: i32 = ws::SOCK_STREAM;
  pub(super) const SOCK_DGRAM: i32 = ws::SOCK_DGRAM;
  pub(super) const IPPROTO_TCP: i32 = ws::IPPROTO_TCP;
  pub(super) const IPPROTO_UDP: i32 = ws::IPPROTO_UDP;

  pub(super) const EINVAL: i32 = ws::WSAEINVAL;
  pub(super) const EAFNOSUPPORT: i32 = ws::WSAEAFNOSUPPORT;
  pub(super) const ENAMETOOLONG: i32 = ws::WSAENAMETOOLONG;
}

/// Translates abstract socket domain/type/proto to raw OS constants.
///
/// Returns the OS error code to complete the op with when the triple is
/// unsupported.
pub(crate) fn socket_to_raw(
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
) -> Result<(i32, i32, i32), i32> {
  if !is_supported_socket(domain, ty, proto) {
    return Err(sys::EINVAL);
  }

  let domain = match domain {
    SockDomain::IPV4 => sys::AF_INET,
    SockDomain::IPV6 => sys::AF_INET6,
    SockDomain::UNIX => sys::AF_UNIX,
    _ => return Err(sys::EAFNOSUPPORT),
  };
  let ty = match ty {
    SockType::STREAM => sys::SOCK_STREAM,
    SockType::DGRAM => sys::SOCK_DGRAM,
    _ => return Err(sys::EINVAL),
  };
  let proto = match proto {
    SockProto::TCP => sys::IPPROTO_TCP,
    SockProto::UDP => sys::IPPROTO_UDP,
    _ => 0,
  };

  Ok((domain, ty, proto))
}

fn zeroed_storage() -> Storage {
  // SAFETY: socket address storage is plain old data; all-zero is valid.
  unsafe { mem::zeroed() }
}

/// Writes `value` to the front of `storage`.
fn write_into<T: Copy>(storage: &mut Storage, value: T) {
  const { assert!(mem::size_of::<T>() <= mem::size_of::<Storage>()) };
  // SAFETY: `T` fits in `Storage` (checked above) and `Storage` is aligned
  // for every sockaddr type by definition.
  unsafe { (storage as *mut Storage).cast::<T>().write(value) };
}

/// Reads a `T` from the front of `storage`.
fn read_from<T: Copy>(storage: &Storage) -> T {
  const { assert!(mem::size_of::<T>() <= mem::size_of::<Storage>()) };
  // SAFETY: as in `write_into`; every sockaddr type is plain old data.
  unsafe { (storage as *const Storage).cast::<T>().read() }
}

fn family(storage: &Storage) -> i32 {
  storage.ss_family as i32
}

/// Converts a `SocketAddr` to a native `sockaddr_storage` + length.
pub(crate) fn socket_addr_to_storage(addr: SocketAddr) -> (Storage, SockLen) {
  let mut storage = zeroed_storage();
  let len = match addr {
    SocketAddr::V4(v4) => {
      write_into(&mut storage, sockaddr_in(v4));
      mem::size_of::<sys::SockaddrIn>()
    }
    SocketAddr::V6(v6) => {
      write_into(&mut storage, sockaddr_in6(v6));
      mem::size_of::<sys::SockaddrIn6>()
    }
  };
  (storage, len as SockLen)
}

/// Converts a `SocketAddr` to a native `sockaddr_storage`.
#[cfg(all(unix, feature = "unstable_ffi"))]
pub(crate) fn std_socketaddr_into_libc(addr: SocketAddr) -> Storage {
  socket_addr_to_storage(addr).0
}

/// Converts a `SocketAddrBuf` to a native `sockaddr_storage` + length.
pub(crate) fn socket_addr_buf_to_storage(
  addr: &SocketAddrBuf,
) -> io::Result<(Storage, SockLen)> {
  match addr.family {
    SocketAddrFamily::Ipv4 | SocketAddrFamily::Ipv6 => {
      let std_addr = crate::backend::op::socket_addr_from_buf(addr)?;
      Ok(socket_addr_to_storage(std_addr))
    }
    SocketAddrFamily::Unix => {
      unix_path_to_storage(&addr.unix_path[..addr.unix_path_len as usize])
    }
    SocketAddrFamily::Unspecified => {
      Err(io::Error::from_raw_os_error(sys::EAFNOSUPPORT))
    }
  }
}

/// Converts a native `sockaddr_storage` back to a `SocketAddrBuf`.
pub(crate) fn socket_addr_buf_from_storage(
  storage: &Storage,
  len: SockLen,
) -> io::Result<SocketAddrBuf> {
  match family(storage) {
    sys::AF_INET => {
      let addr: sys::SockaddrIn = read_from(storage);
      Ok(socket_addr_into_buf(SocketAddr::V4(socket_addr_v4(&addr))))
    }
    sys::AF_INET6 => {
      let addr: sys::SockaddrIn6 = read_from(storage);
      Ok(socket_addr_into_buf(SocketAddr::V6(socket_addr_v6(&addr))))
    }
    sys::AF_UNIX => {
      let addr: sys::SockaddrUn = read_from(storage);
      let offset = mem::offset_of!(sys::SockaddrUn, sun_path);
      let path_len =
        (len as usize).saturating_sub(offset).min(addr.sun_path.len());
      // SAFETY: `sun_path` is a C char array; reading it as bytes is valid.
      let bytes = unsafe {
        std::slice::from_raw_parts(
          addr.sun_path.as_ptr().cast::<u8>(),
          path_len,
        )
      };
      // Pathname addresses may include the trailing NUL in `len`.
      let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
      unix_socket_addr_buf(&bytes[..end])
    }
    _ => Err(io::Error::from_raw_os_error(sys::EAFNOSUPPORT)),
  }
}

fn unix_path_to_storage(path: &[u8]) -> io::Result<(Storage, SockLen)> {
  // SAFETY: `sockaddr_un` is plain old data; all-zero is valid.
  let mut addr: sys::SockaddrUn = unsafe { mem::zeroed() };
  // `sun_path` is shorter than `SocketAddrBuf::unix_path` on some
  // platforms (104 bytes on the BSDs), and needs a trailing NUL.
  if path.len() >= addr.sun_path.len() {
    return Err(io::Error::from_raw_os_error(sys::ENAMETOOLONG));
  }
  for (dst, src) in addr.sun_path.iter_mut().zip(path) {
    *dst = *src as _;
  }
  addr.sun_family = sys::AF_UNIX as _;
  let len = mem::offset_of!(sys::SockaddrUn, sun_path) + path.len() + 1;
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    addr.sun_len = len as u8;
  }

  let mut storage = zeroed_storage();
  write_into(&mut storage, addr);
  Ok((storage, len as SockLen))
}

#[cfg(unix)]
fn sockaddr_in(addr: SocketAddrV4) -> sys::SockaddrIn {
  // SAFETY: `sockaddr_in` is plain old data; all-zero is valid.
  let mut out: sys::SockaddrIn = unsafe { mem::zeroed() };
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    out.sin_len = mem::size_of::<sys::SockaddrIn>() as u8;
  }
  out.sin_family = sys::AF_INET as _;
  out.sin_port = addr.port().to_be();
  out.sin_addr =
    libc::in_addr { s_addr: u32::from_ne_bytes(addr.ip().octets()) };
  out
}

#[cfg(unix)]
fn sockaddr_in6(addr: SocketAddrV6) -> sys::SockaddrIn6 {
  // SAFETY: `sockaddr_in6` is plain old data; all-zero is valid.
  let mut out: sys::SockaddrIn6 = unsafe { mem::zeroed() };
  #[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
  ))]
  {
    out.sin6_len = mem::size_of::<sys::SockaddrIn6>() as u8;
  }
  out.sin6_family = sys::AF_INET6 as _;
  out.sin6_port = addr.port().to_be();
  out.sin6_flowinfo = addr.flowinfo();
  out.sin6_addr = libc::in6_addr { s6_addr: addr.ip().octets() };
  out.sin6_scope_id = addr.scope_id();
  out
}

#[cfg(unix)]
fn socket_addr_v4(addr: &sys::SockaddrIn) -> SocketAddrV4 {
  SocketAddrV4::new(
    Ipv4Addr::from(addr.sin_addr.s_addr.to_ne_bytes()),
    u16::from_be(addr.sin_port),
  )
}

#[cfg(unix)]
fn socket_addr_v6(addr: &sys::SockaddrIn6) -> SocketAddrV6 {
  SocketAddrV6::new(
    Ipv6Addr::from(addr.sin6_addr.s6_addr),
    u16::from_be(addr.sin6_port),
    addr.sin6_flowinfo,
    addr.sin6_scope_id,
  )
}

#[cfg(windows)]
fn sockaddr_in(addr: SocketAddrV4) -> sys::SockaddrIn {
  use windows_sys::Win32::Networking::WinSock::{IN_ADDR, IN_ADDR_0};
  sys::SockaddrIn {
    sin_family: sys::AF_INET as _,
    sin_port: addr.port().to_be(),
    sin_addr: IN_ADDR {
      S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(addr.ip().octets()) },
    },
    sin_zero: [0; 8],
  }
}

#[cfg(windows)]
fn sockaddr_in6(addr: SocketAddrV6) -> sys::SockaddrIn6 {
  use windows_sys::Win32::Networking::WinSock::{
    IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN6_0,
  };
  sys::SockaddrIn6 {
    sin6_family: sys::AF_INET6 as _,
    sin6_port: addr.port().to_be(),
    sin6_flowinfo: addr.flowinfo(),
    sin6_addr: IN6_ADDR { u: IN6_ADDR_0 { Byte: addr.ip().octets() } },
    Anonymous: SOCKADDR_IN6_0 { sin6_scope_id: addr.scope_id() },
  }
}

#[cfg(windows)]
fn socket_addr_v4(addr: &sys::SockaddrIn) -> SocketAddrV4 {
  // SAFETY: every variant of the `IN_ADDR` union is a view of the same 4 bytes.
  let raw = unsafe { addr.sin_addr.S_un.S_addr };
  SocketAddrV4::new(
    Ipv4Addr::from(raw.to_ne_bytes()),
    u16::from_be(addr.sin_port),
  )
}

#[cfg(windows)]
fn socket_addr_v6(addr: &sys::SockaddrIn6) -> SocketAddrV6 {
  // SAFETY: both union variants are views of the same plain-old-data bytes.
  let (octets, scope_id) =
    unsafe { (addr.sin6_addr.u.Byte, addr.Anonymous.sin6_scope_id) };
  SocketAddrV6::new(
    Ipv6Addr::from(octets),
    u16::from_be(addr.sin6_port),
    addr.sin6_flowinfo,
    scope_id,
  )
}

/// Looks up the local (`peer == false`) or remote address of a socket.
#[cfg(feature = "high")]
pub(crate) fn socket_name(
  socket: &crate::api::resource::Resource,
  peer: bool,
) -> io::Result<SocketAddr> {
  let mut storage = zeroed_storage();
  let mut len = mem::size_of::<Storage>() as SockLen;
  let addr = (&mut storage as *mut Storage).cast();

  #[cfg(unix)]
  // SAFETY: `storage`/`len` are valid output storage for the call.
  let rc = unsafe {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    if peer {
      libc::getpeername(fd, addr, &mut len)
    } else {
      libc::getsockname(fd, addr, &mut len)
    }
  };
  #[cfg(windows)]
  // SAFETY: `storage`/`len` are valid output storage for the call.
  let rc = unsafe {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Networking::WinSock as ws;
    let s = socket.as_raw_handle() as ws::SOCKET;
    if peer {
      ws::getpeername(s, addr, &mut len)
    } else {
      ws::getsockname(s, addr, &mut len)
    }
  };

  if rc != 0 {
    #[cfg(windows)]
    // SAFETY: reads the calling thread's last Winsock error.
    return Err(io::Error::from_raw_os_error(unsafe {
      windows_sys::Win32::Networking::WinSock::WSAGetLastError()
    }));
    #[cfg(unix)]
    return Err(io::Error::last_os_error());
  }
  let buf = socket_addr_buf_from_storage(&storage, len)?;
  crate::backend::op::socket_addr_from_buf(&buf)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn round_trip(addr: SocketAddrBuf) -> SocketAddrBuf {
    let (storage, len) = socket_addr_buf_to_storage(&addr).unwrap();
    socket_addr_buf_from_storage(&storage, len).unwrap()
  }

  #[test]
  fn ip_addresses_round_trip() {
    for addr in ["127.0.0.1:8080", "[fe80::1%3]:443"] {
      let addr: SocketAddr = addr.parse().unwrap();
      let buf = socket_addr_into_buf(addr);
      assert_eq!(round_trip(buf), buf, "{addr}");
    }
  }

  #[test]
  fn unix_path_round_trips() {
    let buf = unix_socket_addr_buf(b"/tmp/lio.sock").unwrap();
    assert_eq!(round_trip(buf), buf);
  }

  #[test]
  fn unix_path_longer_than_sun_path_is_rejected() {
    // SAFETY: plain old data.
    let sun_path_len =
      unsafe { mem::zeroed::<sys::SockaddrUn>() }.sun_path.len();
    let path = vec![b'x'; sun_path_len];
    if let Ok(buf) = unix_socket_addr_buf(&path) {
      // `Storage` is not `Debug` on every platform, so no `expect_err`.
      let Err(err) = socket_addr_buf_to_storage(&buf) else {
        panic!("oversized unix path must be rejected");
      };
      assert_eq!(err.raw_os_error(), Some(sys::ENAMETOOLONG));
    }
  }
}
