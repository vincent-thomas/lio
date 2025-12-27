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
      impl_result!(|_this, res: std::io::Result<i32>| -> std::io::Result<()> {
        res.map(drop)
      });
  };

  (fd) => {
    impl_result!(|_this, res: std::io::Result<i32>| -> std::io::Result<i32> {
      res
    });
  };

  (|$this:ident, $res:ident: $res_ty:ty| -> $ret_ty:ty { $($body:tt)* }) => {
    fn result(&mut self, res: std::io::Result<i32>) -> *const () {
      let __impl_result = |$this: &mut Self, $res: $res_ty| -> $ret_ty { $($body)* };
      Box::into_raw(Box::new(__impl_result(self, res))) as *const ()
    }
  };
}

macro_rules! impl_no_readyness {
  () => {
    #[cfg(unix)]
    fn meta(&self) -> crate::op::OpMeta {
      crate::op::OpMeta::CAP_NONE
    }
  };
}
