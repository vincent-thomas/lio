use lio::backend::{IoBackend, impls::Iocp};

lio_test::test_io_backend!(lio, Iocp::new());

#[test]
fn notify() {
  let mut backend = Iocp::new();
  backend.init(64).unwrap();
  backend.notify().unwrap();
}
