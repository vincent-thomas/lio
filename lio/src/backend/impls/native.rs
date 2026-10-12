//! The host OS's own resource primitives.
//!
//! Closing a native descriptor/handle and naming the process-wide standard
//! handles is OS-specific, so it lives with the backends rather than in
//! [`Resource`](crate::api::resource::Resource). This is the close used for
//! resources that are real OS objects: those created by the native backends
//! and those adopted through `FromRawFd`/`FromRawHandle`.

use crate::api::resource::RawResource;

/// Handle value standing in for "the current working directory" on Windows,
/// mirroring `AT_FDCWD`. It is never a valid kernel handle (those are
/// multiples of four) nor one of the documented pseudo-handles.
#[cfg(windows)]
pub(crate) const CWD: RawResource = -100isize as RawResource;

/// Value meaning "the current working directory" for `*at` operations.
#[cfg(unix)]
pub(crate) const CWD: RawResource = libc::AT_FDCWD;

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
