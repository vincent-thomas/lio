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
    type Result = std::io::Result<std::os::fd::RawFd>;

    /// File descriptor returned from the operation.
    fn result(&mut self, fd: std::io::Result<i32>) -> Self::Result {
      fd
    }
  };
}

macro_rules! impl_no_readyness {
  () => {
    const EVENT_TYPE: Option<crate::op::EventType> = None;

    fn fd(&self) -> Option<std::os::fd::RawFd> {
      None
    }
  };
}
