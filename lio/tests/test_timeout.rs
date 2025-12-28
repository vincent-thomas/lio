/// write in append mode is not tested since `pwrite` doesn't support it.
use std::time::Duration;

#[test]
fn test_timeout() {
  lio::init();

  let mut recv = lio::timeout(Duration::from_millis(300)).send();

  assert!(recv.try_recv().is_none());

  lio::tick();
  assert!(recv.try_recv().is_none());
  std::thread::sleep(Duration::from_millis(500));
  assert!(recv.try_recv().is_some());

  lio::exit();
}
