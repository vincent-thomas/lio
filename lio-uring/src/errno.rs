use core::fmt;

/// An error the kernel reported, as its positive `errno` value.
///
/// With the `std` feature (on by default) it converts into
/// [`std::io::Error`], so `?` works in functions that return
/// `std::io::Result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(i32);

impl Errno {
  /// Wraps a positive `errno` value.
  pub const fn new(errno: i32) -> Self {
    Self(errno)
  }

  /// The positive `errno` value.
  pub const fn raw(self) -> i32 {
    self.0
  }
}

impl fmt::Display for Errno {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "os error {}", self.0)
  }
}

impl core::error::Error for Errno {}

#[cfg(feature = "std")]
impl From<Errno> for std::io::Error {
  fn from(errno: Errno) -> Self {
    std::io::Error::from_raw_os_error(errno.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn raw_returns_the_wrapped_errno() {
    assert_eq!(Errno::new(libc::EBADF).raw(), libc::EBADF);
  }

  #[cfg(feature = "std")]
  #[test]
  fn queue_full_maps_to_would_block() {
    let error = std::io::Error::from(Errno::new(libc::EAGAIN));
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(error.raw_os_error(), Some(libc::EAGAIN));
  }
}
