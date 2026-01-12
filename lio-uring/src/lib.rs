use std::{cell::UnsafeCell, mem::MaybeUninit, os::fd::RawFd, ptr, sync::Arc};

mod bindings {
  #![allow(
    unused,
    non_upper_case_globals,
    non_snake_case,
    non_camel_case_types,
    unsafe_op_in_unsafe_fn
  )]

  use std::mem::MaybeUninit;
  include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

  #[test]
  fn liburing_smoke() {
    unsafe {
      let mut ring = std::mem::MaybeUninit::zeroed();
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

#[test]
fn lio_uring_smoke() {
  let (mut cq, mut sq) = IoUring::with_capacity(10);
  let uring_operation = Nop;
  unsafe { sq.push(&uring_operation, 5) };

  let hello = "-----\nA message from the uring smoke test\n-----\n";

  let uring_operation2 = Write {
    fd: 1,
    ptr: hello.as_ptr().cast_mut().cast::<_>(),
    len: hello.len() as u32,
    offset: -1i64 as u64,
  };
  unsafe { sq.push(&uring_operation2, 6) };

  sq.submit();

  let completion = cq.wait();

  assert!(completion.user_data == 5);
  let completion = cq.wait();
  assert!(completion.user_data == 6);
}

pub struct IoUring {
  ring: UnsafeCell<bindings::io_uring>,
}

// SAFETY: This struct can ONLY touch completion side of the queue.
pub struct UringCompletion {
  uring: Arc<IoUring>,
}

pub struct UringSubmission {
  uring: Arc<IoUring>,
}

impl UringSubmission {
  /// # Safety
  /// Caller guarrantees that UringOperation has valid data or pointers that
  /// point to valid data.
  pub unsafe fn push(
    &mut self,
    operation: &dyn UringOperation,
    user_data: u64,
  ) {
    let sqe = unsafe { bindings::io_uring_get_sqe(self.uring.ring.get()) };
    assert!(!sqe.is_null());

    unsafe { operation.prep(sqe) };
    unsafe { bindings::io_uring_sqe_set_data(sqe, user_data as _) };
  }
  pub fn submit(&mut self) {
    unsafe { bindings::io_uring_submit(self.uring.ring.get()) };
  }
}

pub struct Completion {
  pub user_data: u64,
  pub res: i32,
  pub flags: u32,
}

impl UringCompletion {
  pub fn wait(&mut self) -> Completion {
    let mut cqe_ptr = ptr::null_mut();
    unsafe {
      bindings::io_uring_wait_cqe(self.uring.ring.get(), &raw mut cqe_ptr)
    };
    let cqe = unsafe { &*cqe_ptr };
    let completion =
      Completion { user_data: cqe.user_data, res: cqe.res, flags: cqe.flags };

    unsafe { bindings::io_uring_cqe_seen(self.uring.ring.get(), cqe_ptr) };

    completion
  }
}

pub trait UringOperation {
  unsafe fn prep(&self, sqe: *mut bindings::io_uring_sqe);
  // unsafe {
  //   match self {
  //     UringOperation::Nop => bindings::io_uring_prep_nop(sqe),
  //     UringOperation::Read { fd, ptr, len, offset } => {
  //       bindings::io_uring_prep_read(sqe, fd, ptr, len, offset)
  //     }
  //     UringOperation::Write { fd, ptr, len, offset } => {
  //       bindings::io_uring_prep_write(sqe, fd, ptr, len, offset)
  //     }
  //   }
  // }
  // Nop,
  // Read { fd: RawFd, ptr: *mut libc::c_void, len: u32, offset: u64 },
  // Write { fd: RawFd, ptr: *mut libc::c_void, len: u32, offset: u64 },
}

pub struct Nop;

impl UringOperation for Nop {
  unsafe fn prep(&self, sqe: *mut bindings::io_uring_sqe) {
    unsafe { bindings::io_uring_prep_nop(sqe) };
  }
}

pub struct Read {
  pub fd: RawFd,
  pub ptr: *mut libc::c_void,
  pub len: u32,
  pub offset: u64,
}

impl UringOperation for Read {
  unsafe fn prep(&self, sqe: *mut bindings::io_uring_sqe) {
    unsafe {
      bindings::io_uring_prep_read(
        sqe,
        self.fd,
        self.ptr,
        self.len,
        self.offset,
      )
    };
  }
}

pub struct Write {
  pub fd: RawFd,
  pub ptr: *mut libc::c_void,
  pub len: u32,
  pub offset: u64,
}

impl UringOperation for Write {
  unsafe fn prep(&self, sqe: *mut bindings::io_uring_sqe) {
    unsafe {
      bindings::io_uring_prep_write(
        sqe,
        self.fd,
        self.ptr,
        self.len,
        self.offset,
      )
    };
  }
}

impl Drop for IoUring {
  fn drop(&mut self) {
    unsafe { bindings::io_uring_queue_exit(self.ring.get()) };
  }
}

impl IoUring {
  pub fn with_capacity(cap: u32) -> (UringCompletion, UringSubmission) {
    let mut ring = MaybeUninit::zeroed();
    unsafe { bindings::io_uring_queue_init(cap, ring.as_mut_ptr(), 0) };

    let this = Self { ring: UnsafeCell::new(unsafe { ring.assume_init() }) };
    this.split()
  }
  fn split(self) -> (UringCompletion, UringSubmission) {
    let uring = Arc::new(self);
    (UringCompletion { uring: uring.clone() }, UringSubmission { uring })
  }
}
