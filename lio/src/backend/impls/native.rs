//! The host OS's own resource primitives.
//!
//! Closing a native descriptor/handle and naming the process-wide standard
//! handles is OS-specific, so it lives with the backends rather than in
//! [`Resource`](crate::api::resource::Resource). This is the close used for
//! resources that are real OS objects: those created by the native backends
//! and those adopted through `FromRawFd`/`FromRawHandle`.

use crate::api::resource::RawResource;
#[cfg(unix)]
use crate::backend::op::OpenFlags;

/// Handle value standing in for "the current working directory" on Windows,
/// mirroring `AT_FDCWD`. It is never a valid kernel handle (those are
/// multiples of four) nor one of the documented pseudo-handles.
#[cfg(windows)]
pub(crate) const CWD: RawResource = -100isize as RawResource;

/// Value meaning "the current working directory" for `*at` operations.
#[cfg(unix)]
pub(crate) const CWD: RawResource = libc::AT_FDCWD;

/// Error code for "the caller's buffer is too small" (as `getcwd(3)`
/// reports it).
#[cfg(unix)]
pub(crate) const BUFFER_TOO_SMALL: i32 = libc::ERANGE;

/// Error code for "the caller's buffer is too small".
#[cfg(windows)]
pub(crate) const BUFFER_TOO_SMALL: i32 =
  windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER as i32;

/// Error code for "the final path component is a symbolic link", as an open
/// with [`OpenFlags::NOFOLLOW`](crate::backend::op::OpenFlags::NOFOLLOW)
/// reports it.
#[cfg(unix)]
#[cfg_attr(not(feature = "high"), allow(dead_code))]
pub(crate) const SYMLINK_LOOP: i32 = libc::ELOOP;

/// Error code for "the final path component is a symbolic link", as an open
/// with [`OpenFlags::NOFOLLOW`](crate::backend::op::OpenFlags::NOFOLLOW)
/// reports it.
#[cfg(windows)]
#[cfg_attr(not(feature = "high"), allow(dead_code))]
pub(crate) const SYMLINK_LOOP: i32 =
  windows_sys::Win32::Foundation::ERROR_CANT_RESOLVE_FILENAME as i32;

/// Closes a native descriptor/handle.
///
/// # Safety
///
/// The caller must own `raw` and must not use it afterwards.
pub(crate) unsafe fn close(raw: RawResource) {
  // `OwnedFd` follows std's I/O-safety rules: debug builds abort on a double
  // close (EBADF) instead of silently closing someone else's descriptor.
  #[cfg(unix)]
  // SAFETY: guaranteed by the caller.
  drop(unsafe {
    <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw)
  });

  #[cfg(windows)]
  {
    use windows_sys::Win32::{
      Foundation::CloseHandle,
      Networking::WinSock::{SOCKET, closesocket},
    };
    // Sockets must be released with `closesocket`; anything else (or a
    // socket when Winsock is not initialized) falls back to `CloseHandle`.
    // SAFETY: guaranteed by the caller.
    unsafe {
      if closesocket(raw as SOCKET) != 0 {
        CloseHandle(raw);
      }
    }
  }
}

/// The process's standard input.
pub(crate) fn stdin() -> RawResource {
  #[cfg(unix)]
  return libc::STDIN_FILENO;
  #[cfg(windows)]
  return std_handle(windows_sys::Win32::System::Console::STD_INPUT_HANDLE);
}

/// The process's standard output.
pub(crate) fn stdout() -> RawResource {
  #[cfg(unix)]
  return libc::STDOUT_FILENO;
  #[cfg(windows)]
  return std_handle(windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE);
}

/// The process's standard error.
pub(crate) fn stderr() -> RawResource {
  #[cfg(unix)]
  return libc::STDERR_FILENO;
  #[cfg(windows)]
  return std_handle(windows_sys::Win32::System::Console::STD_ERROR_HANDLE);
}

#[cfg(windows)]
fn std_handle(
  which: windows_sys::Win32::System::Console::STD_HANDLE,
) -> RawResource {
  // SAFETY: querying the process std handles has no preconditions.
  unsafe { windows_sys::Win32::System::Console::GetStdHandle(which) }
}

/// Translates [`OpenFlags`] to `open(2)` flags, always adding `O_CLOEXEC`.
///
/// Returns `EINVAL` when no access mode is requested.
#[cfg(unix)]
#[cfg_attr(not(feature = "backend_impls"), allow(dead_code))]
pub(crate) fn open_flags(flags: OpenFlags) -> Result<i32, i32> {
  let mut bits = match (flags.reads(), flags.writes()) {
    (true, false) => libc::O_RDONLY,
    (false, true) => libc::O_WRONLY,
    (true, true) => libc::O_RDWR,
    (false, false) => return Err(libc::EINVAL),
  } | libc::O_CLOEXEC;
  for (flag, native) in [
    (OpenFlags::APPEND, libc::O_APPEND),
    (OpenFlags::CREATE, libc::O_CREAT),
    (OpenFlags::TRUNCATE, libc::O_TRUNC),
    (OpenFlags::EXCLUSIVE, libc::O_EXCL),
    (OpenFlags::DIRECTORY, libc::O_DIRECTORY),
    (OpenFlags::NOFOLLOW, libc::O_NOFOLLOW),
  ] {
    if flags.contains(flag) {
      bits |= native;
    }
  }
  Ok(bits)
}
