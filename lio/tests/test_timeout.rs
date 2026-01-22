/// write in append mode is not tested since `pwrite` doesn't support it.
use lio::Lio;
use std::time::Duration;

#[test]
fn test_timeout() {
  let mut lio = Lio::new(64).unwrap();

  let mut recv =
    lio::api::timeout(Duration::from_millis(300)).with_lio(&mut lio).send();

  assert!(recv.try_recv().is_none());

  lio.try_run().unwrap();
  assert!(recv.try_recv().is_none());
  std::thread::sleep(Duration::from_millis(320));
  assert!(recv.try_recv().is_none());
  lio.try_run().unwrap();

  let _ = recv.try_recv().unwrap();
}
