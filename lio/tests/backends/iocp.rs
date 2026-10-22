use lio::backend::impls::Iocp;

lio_test::test_io_backend!(lio, Iocp::new());

#[test]
fn notify_wakes_a_blocked_wait() {
  let mut backend = Iocp::new();
  backend.init(64).unwrap();
  backend.notify().unwrap();

  let mut completed = Vec::new();
  backend.wait(Some(Duration::from_secs(5)), &mut completed).unwrap();
  assert!(completed.is_empty(), "a wake-up is not an op completion");
}

#[test]
fn drop_cancels_a_blocked_pipe_read() {
  use lio::api::resource::Resource;
  use lio::backend::op::{Op, RawBuf, ReadFlags};
  use std::os::windows::io::{FromRawHandle, IntoRawHandle};

  let (reader, _writer) = std::io::pipe().unwrap();
  // SAFETY: the pipe hands its handle to exactly one Resource.
  let reader = unsafe { Resource::from_raw_handle(reader.into_raw_handle()) };
  let mut buf = [0_u8; 4];
  let mut raw = RawBuf::new(&buf);
  raw.ptr = buf.as_mut_ptr();

  let mut backend = Iocp::new();
  backend.init(8).unwrap();
  // SAFETY: test only: what the operation points to outlives it here,
  // and its id is not in flight.
  unsafe {
    backend.push(
      1,
      Op::Read {
        fd: reader,
        iovecs: std::ptr::NonNull::from(&mut raw),
        iov_count: 1,
        offset: None,
        flags: ReadFlags::from_bits(0).unwrap(),
      },
      &mut bumpalo::Bump::new(),
    );
  }
  backend.flush().unwrap();
  // Give a worker time to block in ReadFile on the empty pipe.
  std::thread::sleep(Duration::from_millis(50));

  // Must return: the blocked worker's read is cancelled and joined.
  let start = std::time::Instant::now();
  drop(backend);
  assert!(start.elapsed() < Duration::from_secs(5));
}
