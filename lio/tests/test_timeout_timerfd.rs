use std::time::{Duration, Instant};

use lio::{api, Lio};

#[test]
fn test_timeout_basic() {
  let mut lio = Lio::new(64).unwrap();

  let start = Instant::now();
  let timeout_duration = Duration::from_millis(500);

  let mut recv = api::timeout(timeout_duration).with_lio(&mut lio).send();

  let result = loop {
    if let Some(result) = recv.try_recv() {
      break result;
    }
    lio.run_timeout(Duration::from_millis(100)).unwrap();
  };

  let elapsed = start.elapsed();

  assert!(result.is_ok(), "Timeout should complete successfully: {:?}", result);
  assert!(
    elapsed >= timeout_duration,
    "Timeout should wait at least the specified duration: {:?} >= {:?}",
    elapsed,
    timeout_duration
  );
  assert!(
    elapsed < timeout_duration + Duration::from_millis(50),
    "Timeout should not wait too much longer: time waited {elapsed:?}, time set waited: {timeout_duration:?}, wiggle: 50ms",
  );
}

#[test]
fn test_timeout_multiple() {
  let mut lio = Lio::new(64).unwrap();

  let start = Instant::now();

  // Start 3 timeouts with different durations
  let mut recv1 =
    api::timeout(Duration::from_millis(50)).with_lio(&mut lio).send();
  let mut recv2 =
    api::timeout(Duration::from_millis(100)).with_lio(&mut lio).send();
  let mut recv3 =
    api::timeout(Duration::from_millis(150)).with_lio(&mut lio).send();

  // They should complete in order
  let result1 = loop {
    if let Some(result) = recv1.try_recv() {
      break result;
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  };
  let elapsed1 = start.elapsed();

  let result2 = loop {
    if let Some(result) = recv2.try_recv() {
      break result;
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  };
  let elapsed2 = start.elapsed();

  let result3 = loop {
    if let Some(result) = recv3.try_recv() {
      break result;
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  };
  let elapsed3 = start.elapsed();

  assert!(result1.is_ok(), "err: {result1:?}");
  assert!(result2.is_ok(), "err: {result2:?}");
  assert!(result3.is_ok(), "err: {result3:?}");

  assert!(
    elapsed1 >= Duration::from_millis(50),
    "First timeout elapsed: {:?}",
    elapsed1
  );
  assert!(
    elapsed2 >= Duration::from_millis(100),
    "Second timeout elapsed: {:?}",
    elapsed2
  );
  assert!(
    elapsed3 >= Duration::from_millis(150),
    "Third timeout elapsed: {:?}",
    elapsed3
  );
}
