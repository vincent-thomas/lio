use std::time::{Duration, Instant};

#[test]
fn test_timeout_basic() {
  lio::init();
  lio::start().unwrap();

  let start = Instant::now();
  let timeout_duration = Duration::from_millis(500);

  let recv = lio::timeout(timeout_duration).send();
  let result = recv.recv();

  let elapsed = start.elapsed();

  assert!(result.is_ok(), "Timeout should complete successfully: {:?}", result);
  assert!(
    elapsed >= timeout_duration,
    "Timeout should wait at least the specified duration: {:?} >= {:?}",
    elapsed,
    timeout_duration
  );
  assert!(
    elapsed < timeout_duration + Duration::from_millis(20),
    "Timeout should not wait too much longer: {:?} < {:?}",
    elapsed,
    timeout_duration + Duration::from_millis(50)
  );

  lio::stop();
  lio::exit();
}

#[test]
fn test_timeout_multiple() {
  lio::init();
  lio::start().unwrap();

  let start = Instant::now();

  // Start 3 timeouts with different durations
  let recv1 = lio::timeout(Duration::from_millis(50)).send();
  let recv2 = lio::timeout(Duration::from_millis(100)).send();
  let recv3 = lio::timeout(Duration::from_millis(150)).send();

  // They should complete in order
  let result1 = recv1.recv();
  let elapsed1 = start.elapsed();

  let result2 = recv2.recv();
  let elapsed2 = start.elapsed();

  let result3 = recv3.recv();
  let elapsed3 = start.elapsed();

  assert!(result1.is_ok());
  assert!(result2.is_ok());
  assert!(result3.is_ok());

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

  lio::stop();
  lio::exit();
}
