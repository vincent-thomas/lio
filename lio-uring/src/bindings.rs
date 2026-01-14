#[allow(
  unused,
  non_upper_case_globals,
  non_snake_case,
  non_camel_case_types,
  unsafe_op_in_unsafe_fn
)]
mod bindings {
  include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

pub use bindings::*;

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn liburing_smoke() {
    use std::mem::MaybeUninit;
    unsafe {
      let mut ring = MaybeUninit::zeroed();
      let ret = self::io_uring_queue_init(5, ring.as_mut_ptr(), 0);
      assert_eq!(ret, 0, "queue_init failed: {}", ret);
      let mut ring = ring.assume_init();

      let sqe = self::io_uring_get_sqe(&raw mut ring);
      assert!(!sqe.is_null());

      self::io_uring_prep_nop(sqe);
      self::io_uring_sqe_set_data(sqe, 0x1337 as *mut _);
      let submitted = self::io_uring_submit(&raw mut ring);
      assert_eq!(submitted, 1, "submit failed: {}", submitted);

      let mut cqe_ptr: *mut io_uring_cqe = std::ptr::null_mut();
      let ret = io_uring_wait_cqe(&raw mut ring, &raw mut cqe_ptr);
      assert_eq!(ret, 0, "wait_cqe failed: {}", ret);
      assert!(!cqe_ptr.is_null());

      let user_data = io_uring_cqe_get_data(cqe_ptr);
      println!("user_data = {:p}", user_data);

      io_uring_cqe_seen(&raw mut ring, cqe_ptr);

      io_uring_queue_exit(&raw mut ring);
    }
  }
}
