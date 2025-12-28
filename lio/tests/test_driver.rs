// #[test]
// fn test_driver() {
//   lio::init();
//   // Note: Removed lio::exit() to prevent shutting down the shared Driver
//   // during parallel test execution
// }

#[test]
fn test_worker_start_stop() {
  use std::time::Duration;

  let _ = lio::try_init();

  std::thread::sleep(Duration::from_millis(100));

  let thing = "hello".as_bytes().to_vec();

  let send = lio::write_with_buf(2, thing, -1).send();

  let (res, _buf) = send.recv();

  assert_eq!(res.unwrap(), 5);

  lio::exit();
}
