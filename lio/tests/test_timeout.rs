#![cfg(linux)]
/// write in append mode is not tested since `pwrite` doesn't support it.
use std::{future::Future, pin::Pin, task::Context, time::Duration};

use futures_task::noop_waker;

#[test]
#[ignore]
fn test_timeout() {
  lio::init();

  println!("testing");

  let mut _test = lio::timeout(Duration::from_millis(1000)).send();

  std::thread::sleep(Duration::from_secs(1010));
  lio::tick();

  _test.try_recv().unwrap().unwrap();
}
