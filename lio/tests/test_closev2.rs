use lio::api::resource::Resource;
use std::{ffi::CString, os::fd::FromRawFd};

#[test]
fn test_close_basic() {
  let path = CString::new("/tmp/lio_test_close_basic.txt").unwrap();

  // Open a file
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };

  // Close it
  let resource = unsafe { Resource::from_raw_fd(fd) };

  assert!(resource.will_close());

  let _r2 = resource.clone();

  assert!(!resource.will_close());
  assert!(!_r2.will_close());
  drop(_r2);

  assert!(resource.will_close());

  drop(resource);

  // Cleanup
  unsafe { libc::unlink(path.as_ptr()) };
}
