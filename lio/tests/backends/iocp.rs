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
  use lio::backend::op::{RawBufMut, ReadFlags};
  use std::os::windows::io::{FromRawHandle, IntoRawHandle};

  let (reader, _writer) = std::io::pipe().unwrap();
  use std::os::windows::io::OwnedHandle;
  // SAFETY: the pipe hands its handle to exactly one Resource.
  let reader = Resource::from(unsafe {
    OwnedHandle::from_raw_handle(reader.into_raw_handle())
  });
  let mut buf = [0_u8; 4];
  let raw = RawBufMut::from_mut(&mut buf);

  let mut backend = Iocp::new();
  backend.init(8).unwrap();
  // SAFETY: `buf` stays alive until this backend is dropped.
  unsafe {
    backend.push(
      1,
      lio::backend::InFlight::attest(lio::backend::op::BorrowedOp::read(
        reader,
        std::slice::from_ref(&raw),
        None,
        ReadFlags::empty(),
      )),
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
