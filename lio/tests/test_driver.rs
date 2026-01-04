#[test]
fn test_worker_start_stop() {
  use lio::resource::Resource;
  use std::{os::fd::FromRawFd, time::Duration};

  let _ = lio::try_init_with_driver::<lio::backends::Poller>();

  let thing = "hello".as_bytes().to_vec();

  let (res, _buf) = lio::write(unsafe { Resource::from_raw_fd(2) }, thing)
    .send()
    .recv_timeout(Duration::from_millis(1))
    .unwrap();

  // panic!();

  assert_eq!(res.unwrap(), 5);
  lio::exit();
}
