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
