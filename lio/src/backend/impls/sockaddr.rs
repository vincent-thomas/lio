//! OS-dependent socket address conversion utilities.
//!
//! These helpers convert between lio's abstract [`SocketAddrBuf`] and the
//! platform-native sockaddr types used by syscalls.  They live in
//! `backend/impls/` because every line of code here depends on a specific
//! OS (`libc` on Unix, `windows-sys` on Windows).

use std::io;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::ptr;

use crate::backend::op::{SocketAddrBuf, SocketAddrFamily};

// ═══════════════════════════════════════════════════════════════════════════════
// Raw constants
// ═══════════════════════════════════════════════════════════════════════════════

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


// ═══════════════════════════════════════════════════════════════════════════════
// Socket addr ↔ libc::sockaddr_*  conversion
// ═══════════════════════════════════════════════════════════════════════════════

/// # Safety
/// `storage` must point to a valid, initialized `sockaddr_storage`.
pub(crate) unsafe fn libc_socketaddr_into_std_raw(
  storage: *const libc::sockaddr_storage,
) -> io::Result<SocketAddr> {
  // SAFETY: correct pointer.
  let sockaddr = unsafe { *storage };

  if sockaddr.ss_family == raw_af_inet() as libc::sa_family_t {
    let ipv4_ptr = storage.cast::<libc::sockaddr_in>();
    // SAFETY: We've verified ss_family is AF_INET, so the storage pointer can be safely
    // cast to sockaddr_in. The caller guarantees storage points to valid memory.
    let ipv4 = Ipv4Addr::from(unsafe { *ipv4_ptr }.sin_addr.s_addr.to_be());
    // SAFETY: Same as above - pointer is valid and properly aligned for sockaddr_in.
    let port = u16::from_be(unsafe { *ipv4_ptr }.sin_port);

    Ok(SocketAddr::from(SocketAddrV4::new(ipv4, port)))
  } else if sockaddr.ss_family == raw_af_inet6() as libc::sa_family_t {
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

pub(crate) fn std_socketaddr_into_libc(
  addr: SocketAddr,
) -> libc::sockaddr_storage {
  // SAFETY: sockaddr_storage is a C struct designed to hold any socket address type.
  // Zero-initialization is valid - all fields are primitive types where zero is safe.
  let storage: std::cell::UnsafeCell<libc::sockaddr_storage> =
    std::cell::UnsafeCell::new(unsafe { mem::zeroed() });
  match addr {
    // SAFETY: copy_nonoverlapping is safe because:
    // 1. Source (&into_addr(v4)) is a valid, aligned sockaddr_in on the stack
    // 2. Destination (storage.get()) is valid - we just created it
    // 3. Size is correct (size_of::<sockaddr_in>())
    // 4. Regions don't overlap (source is on stack, dest is in UnsafeCell)
    // 5. sockaddr_in fits in sockaddr_storage by design
    SocketAddr::V4(v4) => unsafe {
      ptr::copy_nonoverlapping(
        &into_addr(v4) as *const _ as *const u8,
        storage.get() as *mut u8,
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
      ptr::copy_nonoverlapping(
        &into_addr6(v6) as *const _ as *const u8,
        storage.get() as *mut u8,
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
  _addr.sin_family = raw_af_inet() as libc::sa_family_t;
  _addr.sin_port = addr.port().to_be();
  _addr.sin_addr =
    libc::in_addr { s_addr: u32::from(*addr.ip()).to_be() };

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
  _addr.sin6_family = raw_af_inet6() as libc::sa_family_t;
  _addr.sin6_port = addr.port().to_be();
  _addr.sin6_addr =
    libc::in6_addr { s6_addr: addr.ip().octets() };

  _addr
}

// ═══════════════════════════════════════════════════════════════════════════════
// SocketAddrBuf ↔ libc::sockaddr_storage  conversion
// ═══════════════════════════════════════════════════════════════════════════════

/// Converts a `SocketAddrBuf` to a native `sockaddr_storage` + length.
pub(crate) fn socket_addr_buf_to_storage(
  addr: &SocketAddrBuf,
) -> io::Result<(libc::sockaddr_storage, libc::socklen_t)> {
  match addr.family {
    SocketAddrFamily::Ipv4 | SocketAddrFamily::Ipv6 => {
      let std_addr = crate::backend::op::socket_addr_from_buf(addr)?;
      Ok(socket_addr_to_storage(std_addr))
    }
    #[cfg(unix)]
    SocketAddrFamily::Unix => {
      // SAFETY: `sockaddr_storage` is plain old data and may be zero-initialized.
      let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
      // SAFETY: a `sockaddr_un` fits inside `sockaddr_storage` and we only
      // write fields belonging to the Unix socket representation.
      let unix =
        unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_un) };
      unix.sun_family = libc::AF_UNIX as libc::sa_family_t;
      let len = addr.unix_path_len as usize;
      for (dst, src) in
        unix.sun_path[..len].iter_mut().zip(addr.unix_path[..len].iter())
      {
        *dst = *src as libc::c_char;
      }
      let socklen =
        (mem::size_of::<libc::sa_family_t>() + len + 1) as libc::socklen_t;
      Ok((storage, socklen))
    }
    #[cfg(not(unix))]
    SocketAddrFamily::Unix => {
      Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
    }
    SocketAddrFamily::Unspecified => {
      Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
    }
  }
}

/// Converts a native `sockaddr_storage` back to a `SocketAddrBuf`.
pub(crate) fn socket_addr_buf_from_storage(
  storage: &libc::sockaddr_storage,
  len: libc::socklen_t,
) -> io::Result<SocketAddrBuf> {
  if storage.ss_family == raw_af_inet() as libc::sa_family_t
    || storage.ss_family == raw_af_inet6() as libc::sa_family_t
  {
    // SAFETY: `storage` points to a valid initialized sockaddr storage value
    // received from the OS, and the helper only reads from it.
    let std_addr =
      unsafe { libc_socketaddr_into_std_raw(storage) }?;
    return Ok(crate::backend::op::socket_addr_into_buf(std_addr));
  }

  #[cfg(unix)]
  if storage.ss_family == raw_af_unix().unwrap_or_default() as libc::sa_family_t
  {
    // SAFETY: when `ss_family` is AF_UNIX, the storage bytes are laid out as
    // a `sockaddr_un`.
    let unix = unsafe { &*(storage as *const _ as *const libc::sockaddr_un) };
    let base = mem::size_of::<libc::sa_family_t>();
    let path_len = (len as usize).saturating_sub(base).saturating_sub(1);
    // SAFETY: `sun_path` is valid for `path_len` bytes as computed from the
    // sockaddr length returned by the OS.
    let bytes = unsafe {
      std::slice::from_raw_parts(unix.sun_path.as_ptr().cast::<u8>(), path_len)
    };
    return unix_socket_addr_buf(bytes);
  }

  Err(io::Error::from_raw_os_error(libc::EAFNOSUPPORT))
}

/// Converts a `SocketAddr` to a native `sockaddr_storage` + length.
pub(crate) fn socket_addr_to_storage(
  addr: SocketAddr,
) -> (libc::sockaddr_storage, libc::socklen_t) {
  let storage = std_socketaddr_into_libc(addr);
  let len = match addr {
    SocketAddr::V4(_) => mem::size_of::<libc::sockaddr_in>(),
    SocketAddr::V6(_) => mem::size_of::<libc::sockaddr_in6>(),
  } as libc::socklen_t;
  (storage, len)
}


// ═══════════════════════════════════════════════════════════════════════════════
// SocketAddrBuf helpers
// ═══════════════════════════════════════════════════════════════════════════════

pub(crate) fn unix_socket_addr_buf(
  path: &[u8],
) -> io::Result<SocketAddrBuf> {
  if path.len() >= 108 {
    return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
  }
  let mut buf = SocketAddrBuf::unspecified();
  buf.family = SocketAddrFamily::Unix;
  buf.unix_path_len = path.len() as u16;
  buf.unix_path[..path.len()].copy_from_slice(path);
  Ok(buf)
}