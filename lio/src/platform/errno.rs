/// Returns the current platform error code for the last failed raw OS call.
///
/// This centralizes the OS-specific errno/GetLastError plumbing so call sites
/// do not need to carry target-specific cfgs.
#[inline]
pub(crate) fn last_os_error_code() -> i32 {
  #[cfg(target_os = "linux")]
  {
    // SAFETY: `__errno_location` is the libc errno accessor on Linux.
    unsafe { *libc::__errno_location() }
  }

  #[cfg(any(target_os = "macos", target_os = "freebsd"))]
  {
    // SAFETY: `__error` is the libc errno accessor on these targets.
    unsafe { *libc::__error() }
  }

  #[cfg(windows)]
  {
    // SAFETY: `GetLastError` returns the thread-local Win32 error code.
    unsafe { windows_sys::Win32::Foundation::GetLastError() as i32 }
  }
}
