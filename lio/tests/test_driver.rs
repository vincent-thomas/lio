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
  let driver = lio::driver::Driver::get();

  driver.spawn_worker().expect("Failed to spawn worker");

  std::thread::sleep(Duration::from_millis(100));

  let thing = "hello".as_bytes().to_vec();

  let send = lio::write(2, thing, -1).send();

  let (res, _buf) = send.recv();

  assert_eq!(res.unwrap(), 5);

  driver.stop_worker();

  driver.spawn_worker().expect("Should be able to restart worker");
  driver.stop_worker();
}
