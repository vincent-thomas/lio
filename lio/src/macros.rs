macro_rules! syscall {
  ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
      #[allow(unused_unsafe)]
      let res = unsafe { libc::$fn($($arg, )*) };
      if res == -1 {
          Err(std::io::Error::last_os_error())
      } else {
          Ok(res)
      }
  }};
}

macro_rules! impl_result {
  (()) => {
    type Result = std::io::Result<()>;

    fn result(&mut self, res: std::io::Result<i32>) -> Self::Result {
      res.map(|code| {
        assert!(code == 0);
      })
    }
  };

  (fd) => {
    #[cfg(unix)]
    type Result = std::io::Result<std::os::fd::RawFd>;

    /// File descriptor returned from the operation.
    #[cfg(unix)]
    fn result(&mut self, fd: std::io::Result<i32>) -> Self::Result {
      fd
    }
  };
}

macro_rules! impl_no_readyness {
  () => {
    #[cfg(unix)]
    const INTEREST: Option<crate::backends::pollingv2::Interest> = None;

    #[cfg(unix)]
    fn fd(&self) -> Option<std::os::fd::RawFd> {
      None
    }
  };
}
