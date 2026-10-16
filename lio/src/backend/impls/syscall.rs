//! Raw syscall helpers shared by the Unix backend implementations.

/// Returns the errno of the last failed raw OS call on this thread.
#[inline]
pub(crate) fn last_os_error_code() -> i32 {
  std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

macro_rules! syscall {
  (raw $fn: ident ( $($arg: expr),* $(,)* ) ? ) => {{
      let val = syscall!(raw $fn ($($arg),*));

      if val < 0 {
          return val;
      }

      val
  }};
  (raw $fn: ident ( $($arg: expr),* $(,)* ) ) => {{
    #[allow(unused_unsafe)]
    {
      // SAFETY: This exists on the platform in cfg.
      let res = unsafe { libc::$fn($($arg, )*) };
      if res != -1 {
        res as isize
      }
      else {
        // Return negative errno - this will cause early return from function.
        let err = $crate::backend::impls::syscall::last_os_error_code();

        -(err as isize)
      }
    }
  }};

  ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
      let result = syscall!(raw $fn($($arg),*));
      if result >= 0 {
          Ok(result as i32)
      } else {
          Err(std::io::Error::from_raw_os_error(-(result as i32)))
      }
  }};
}
