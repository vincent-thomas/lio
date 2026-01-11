#[test]
fn test_worker_start_stop() {
  use lio::api::resource::Resource;
  use std::{os::fd::FromRawFd, time::Duration};

  let _ = lio::try_init_with_driver::<lio::backends::pollingv2::Poller>();

  let thing = "hello".as_bytes().to_vec();

  let fd = unsafe { Resource::from_raw_fd(1) };

  let (res, _buf) = lio::api::write(fd, thing).send().recv();

  dbg!(res);

  // assert_eq!(res.unwrap(), 5);
  lio::exit();
}
