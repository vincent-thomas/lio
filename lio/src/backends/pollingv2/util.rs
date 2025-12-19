use std::io;
use std::time::Duration;

/// Convert Duration to libc::timespec
///
/// Shared between kqueue and epoll implementations
pub fn duration_to_timespec(duration: Duration) -> libc::timespec {
  libc::timespec {
    tv_sec: duration.as_secs() as libc::time_t,
    tv_nsec: duration.subsec_nanos() as libc::c_long,
  }
}

/// Convert Option<Duration> to timespec storage
///
/// Returns the timespec value that must be kept alive for the duration of the syscall.
/// The caller should create a pointer from a reference to this storage.
pub fn timeout_to_timespec(
  timeout: Option<Duration>,
) -> Option<libc::timespec> {
  timeout.map(duration_to_timespec)
}

/// Check if an error is "not found" (ENOENT)
///
/// Used when deleting non-existent fd interests - should not error
pub fn is_not_found_error(err: &io::Error) -> bool {
  err.raw_os_error() == Some(libc::ENOENT)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_duration_to_timespec() {
    let dur = Duration::from_millis(1500);
    let ts = duration_to_timespec(dur);
    assert_eq!(ts.tv_sec, 1);
    assert_eq!(ts.tv_nsec, 500_000_000);
  }

  #[test]
  fn test_timeout_to_timespec_some() {
    let dur = Duration::from_millis(2500);
    let timeout = Some(dur);
    let storage = timeout_to_timespec(timeout);

    // Verify storage contains the correct value
    assert!(storage.is_some());
    let ts = storage.unwrap();
    assert_eq!(ts.tv_sec, 2);
    assert_eq!(ts.tv_nsec, 500_000_000);

    // Verify that creating a pointer from storage is valid
    // This mimics what the caller should do
    let ptr = &ts as *const libc::timespec;
    assert!(!ptr.is_null());

    // Verify the pointer points to valid data
    unsafe {
      assert_eq!((*ptr).tv_sec, 2);
      assert_eq!((*ptr).tv_nsec, 500_000_000);
    }
  }

  #[test]
  fn test_timeout_to_timespec_none() {
    let storage = timeout_to_timespec(None);
    assert!(storage.is_none());
  }

  #[test]
  fn test_is_not_found_error() {
    let err = io::Error::from_raw_os_error(libc::ENOENT);
    assert!(is_not_found_error(&err));

    let err = io::Error::from_raw_os_error(libc::EINVAL);
    assert!(!is_not_found_error(&err));
  }
}
