pub(crate) mod net_utils;

mod ops;
mod progress;
pub use ops::*;
pub use progress::*;
use std::io;
use std::ops::BitOr;

trait Sealed {}
/// Done to disallow someone creating a operation outside of lio, which will cause issues.
impl<O: Operation> Sealed for O {}

// Safety: Decides if resources can be leaked when using OperationProgress::detach
#[allow(private_bounds)]
pub unsafe trait DetachSafe: Sealed {}

// Things that implement this trait represent a command that can be executed using io-uring.
pub trait Operation: Sealed {
  // type Result;
  /// This is guarranteed to fire after this has completed and only fire ONCE.
  /// i32 is guarranteed to be >= 0.
  fn result(&mut self, _ret: io::Result<i32>) -> *const ();

  // #[cfg(linux)]
  // const OPCODE: u8;
  //
  // #[cfg(linux)]
  // fn entry_supported(probe: &io_uring::Probe) -> bool {
  //   probe.is_supported(Self::OPCODE)
  // }

  #[cfg(linux)]
  fn create_entry(&mut self) -> io_uring::squeue::Entry;

  #[cfg(unix)]
  fn meta(&self) -> OpMeta;

  // #[cfg(unix)]
  // const INTEREST: Option<crate::backends::pollingv2::Interest> = None;

  #[cfg(unix)]
  fn cap(&self) -> i32 {
    let meta: OpMeta = self.meta();
    if meta.is_cap_none() {
      panic!(
        "Operation::cap default impl, caller error: operation has no cap."
      );
    } else {
      panic!(
        "Operation::cap default impl, implemntee error: operation has cap, but cap fn isn't defined."
      );
    }
  }

  fn run_blocking(&self) -> io::Result<i32>;
}

pub trait OperationExt: Operation {
  type Result;

  // Helper to get typed result without manual pointer casting
  fn result_typed(&mut self, ret: io::Result<i32>) -> Self::Result {
    unsafe {
      let ptr = self.result(ret);
      *Box::from_raw(ptr as *mut Self::Result)
    }
  }
}

/// Operation flags using bitflags pattern
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpMeta(u8);

impl OpMeta {
  pub const CAP_NONE: Self = Self(0);

  pub const CAP_FD: Self = Self(1 << 7);
  pub const CAP_TIMER: Self = Self(1 << 6);

  pub const BEH_NEEDS_RUN: Self = Self(1 << 3);
  pub const FD_WRITE: Self = Self(1 << 1);
  pub const FD_READ: Self = Self(1 << 0);

  pub const fn is_cap_none(self) -> bool {
    !matches!(self, Self::CAP_FD | Self::CAP_TIMER)
  }

  pub const fn is_cap_fd(self) -> bool {
    let value = matches!(self, Self::CAP_FD);
    if value {
      assert!(!matches!(self, Self::CAP_TIMER));
      true
    } else {
      false
    }
  }
  /// Check if this operation is a timer
  pub const fn is_cap_timer(self) -> bool {
    let value = matches!(self, Self::CAP_TIMER);
    if value {
      assert!(!matches!(self, Self::CAP_FD));
      true
    } else {
      false
    }
  }
  /// Check if this operation waits for read readiness
  pub const fn is_beh_run_before_noti(self) -> bool {
    assert!(matches!(self, Self::CAP_FD));
    matches!(self, Self::BEH_NEEDS_RUN)
  }

  /// Check if this operation waits for read readiness
  pub const fn is_fd_readable(self) -> bool {
    assert!(matches!(self, Self::CAP_FD));
    matches!(self, Self::FD_READ)
  }

  /// Check if this operation waits for write readiness
  pub const fn is_fd_writable(self) -> bool {
    assert!(matches!(self, Self::CAP_FD));
    matches!(self, Self::FD_WRITE)
  }
}

impl BitOr for OpMeta {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_op_meta_cap_none() {
    let meta = OpMeta::CAP_NONE;
    assert!(meta.is_cap_none());
    assert!(!meta.is_cap_fd());
    assert!(!meta.is_cap_timer());
  }

  #[test]
  fn test_op_meta_cap_fd() {
    let meta = OpMeta::CAP_FD;
    assert!(!meta.is_cap_none());
    assert!(meta.is_cap_fd());
    assert!(!meta.is_cap_timer());
  }

  #[test]
  fn test_op_meta_cap_timer() {
    let meta = OpMeta::CAP_TIMER;
    assert!(!meta.is_cap_none());
    assert!(!meta.is_cap_fd());
    assert!(meta.is_cap_timer());
  }

  #[test]
  fn test_op_meta_fd_read() {
    let meta = OpMeta::CAP_FD | OpMeta::FD_READ;
    assert!(meta.is_fd_readable());
    assert!(!meta.is_fd_writable());
  }

  #[test]
  fn test_op_meta_fd_write() {
    let meta = OpMeta::CAP_FD | OpMeta::FD_WRITE;
    assert!(!meta.is_fd_readable());
    assert!(meta.is_fd_writable());
  }

  #[test]
  fn test_op_meta_size() {
    // Ensure OpMeta fits in a single byte
    assert_eq!(std::mem::size_of::<OpMeta>(), 1);
  }

  #[test]
  fn test_op_meta_values_unique() {
    // Ensure all variants have unique bit patterns
    let cap_none: u8 = OpMeta::CAP_NONE.into();
    let cap_fd: u8 = OpMeta::CAP_FD.into();
    let cap_timer: u8 = OpMeta::CAP_TIMER.into();
    let fd_read: u8 = (OpMeta::CAP_FD | OpMeta::FD_READ).into();
    let fd_write: u8 = (OpMeta::CAP_FD | OpMeta::FD_WRITE).into();

    let values = [cap_none, cap_fd, cap_timer, fd_read, fd_write];
    for (i, &val1) in values.iter().enumerate() {
      for &val2 in values.iter().skip(i + 1) {
        assert_ne!(val1, val2, "OpMeta variants must have unique values");
      }
    }
  }

  #[test]
  #[should_panic]
  fn test_op_meta_readable_panics_on_non_fd() {
    let meta = OpMeta::CAP_TIMER;
    let _ = meta.is_fd_readable(); // Should panic
  }

  #[test]
  #[should_panic]
  fn test_op_meta_writable_panics_on_non_fd() {
    let meta = OpMeta::CAP_NONE;
    let _ = meta.is_fd_writable(); // Should panic
  }
}
