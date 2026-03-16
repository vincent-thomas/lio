//! Tests for the timeout wrapper functionality.
//!
//! These tests verify that operations can be wrapped with timeouts,
//! and that the timeout correctly cancels operations that take too long.

#![cfg(unix)]

use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lio::api::ops::TimedOut;
use lio::api::resource::Resource;
use lio::{api, Lio};

/// Helper to poll until we receive a result with a max wait time
fn poll_recv<T>(
  lio: &mut Lio,
  recv: &mut api::io::Receiver<T>,
  max_wait: Duration,
) -> Option<T> {
  let start = Instant::now();
  loop {
    if let Some(result) = recv.try_recv() {
      return Some(result);
    }
    if start.elapsed() > max_wait {
      return None;
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  }
}

#[test]
fn test_timeout_recv_times_out() {
  let mut lio = Lio::new(64).unwrap();

  // Create a Unix socketpair - we control both ends
  let mut fds = [0i32; 2];
  let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
  assert_eq!(ret, 0, "socketpair failed");

  // Wrap the receiving end in a Resource
  let recv_sock = unsafe { Resource::from_raw_fd(fds[0]) };
  // Keep the sending end but don't send anything
  let _send_sock = fds[1];

  // Try to recv with a short timeout - should timeout since no data is sent
  let buf = vec![0u8; 1024];
  let timeout_duration = Duration::from_millis(100);

  let start = Instant::now();
  let mut recv = api::timeout(timeout_duration, api::recv(&recv_sock, buf, None))
    .with_lio(&mut lio)
    .send();

  let result = poll_recv(&mut lio, &mut recv, Duration::from_secs(5))
    .expect("Should receive a result");

  let elapsed = start.elapsed();

  // Should have timed out
  assert!(
    matches!(result, Err(TimedOut)),
    "Expected TimedOut error, got {:?}",
    result.map(|(r, _)| r)
  );

  // Should have taken approximately the timeout duration
  assert!(
    elapsed >= timeout_duration,
    "Should wait at least the timeout duration: {:?} >= {:?}",
    elapsed,
    timeout_duration
  );
  assert!(
    elapsed < timeout_duration + Duration::from_millis(200),
    "Should not wait too much longer than timeout: {:?}",
    elapsed
  );

  // Clean up the send socket
  unsafe { libc::close(_send_sock) };
}

#[test]
fn test_timeout_nop_completes() {
  let mut lio = Lio::new(64).unwrap();

  // Nop should complete immediately, well before the timeout
  let timeout_duration = Duration::from_secs(1);

  let start = Instant::now();
  let mut recv = api::timeout(timeout_duration, api::nop())
    .with_lio(&mut lio)
    .send();

  let result = poll_recv(&mut lio, &mut recv, Duration::from_secs(5))
    .expect("Should receive a result");

  let elapsed = start.elapsed();

  // Should have completed successfully (not timed out)
  assert!(
    matches!(result, Ok(Ok(()))),
    "Expected Ok(Ok(())), got {:?}",
    result
  );

  // Should have completed quickly
  assert!(
    elapsed < Duration::from_millis(100),
    "Nop should complete quickly: {:?}",
    elapsed
  );
}

#[test]
fn test_timeout_sleep_shorter_than_timeout() {
  let mut lio = Lio::new(64).unwrap();

  // Sleep for 50ms with a 500ms timeout - should complete before timeout
  let sleep_duration = Duration::from_millis(50);
  let timeout_duration = Duration::from_millis(500);

  let start = Instant::now();
  let mut recv = api::timeout(timeout_duration, api::sleep(sleep_duration))
    .with_lio(&mut lio)
    .send();

  let result = poll_recv(&mut lio, &mut recv, Duration::from_secs(5))
    .expect("Should receive a result");

  let elapsed = start.elapsed();

  // Should have completed successfully (not timed out)
  assert!(
    matches!(result, Ok(Ok(()))),
    "Expected Ok(Ok(())), got {:?}",
    result
  );

  // Should have taken approximately the sleep duration
  assert!(
    elapsed >= sleep_duration,
    "Should wait at least the sleep duration: {:?} >= {:?}",
    elapsed,
    sleep_duration
  );
  assert!(
    elapsed < timeout_duration,
    "Should complete before timeout: {:?} < {:?}",
    elapsed,
    timeout_duration
  );
}

#[test]
fn test_timeout_sleep_longer_than_timeout() {
  let mut lio = Lio::new(64).unwrap();

  // Sleep for 500ms with a 50ms timeout - should timeout
  let sleep_duration = Duration::from_millis(500);
  let timeout_duration = Duration::from_millis(50);

  let start = Instant::now();
  let mut recv = api::timeout(timeout_duration, api::sleep(sleep_duration))
    .with_lio(&mut lio)
    .send();

  let result = poll_recv(&mut lio, &mut recv, Duration::from_secs(5))
    .expect("Should receive a result");

  let elapsed = start.elapsed();

  // Should have timed out
  assert!(
    matches!(result, Err(TimedOut)),
    "Expected TimedOut error, got {:?}",
    result
  );

  // Should have taken approximately the timeout duration (not the sleep duration)
  assert!(
    elapsed >= timeout_duration,
    "Should wait at least the timeout duration: {:?} >= {:?}",
    elapsed,
    timeout_duration
  );
  assert!(
    elapsed < sleep_duration,
    "Should timeout before sleep completes: {:?} < {:?}",
    elapsed,
    sleep_duration
  );
}

#[test]
fn test_timeout_multiple_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();

  // Start 3 nop operations with timeout - all should complete quickly
  for _ in 0..3 {
    api::timeout(Duration::from_secs(1), api::nop())
      .with_lio(&mut lio)
      .send_with(sender.clone());
  }

  let start = Instant::now();

  // Wait for all to complete
  for i in 0..3 {
    loop {
      lio.run_timeout(Duration::from_millis(10)).unwrap();
      match receiver.try_recv() {
        Ok(result) => {
          assert!(
            matches!(result, Ok(Ok(()))),
            "Operation {} should succeed, got {:?}",
            i,
            result
          );
          break;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
  }

  let elapsed = start.elapsed();
  assert!(
    elapsed < Duration::from_millis(500),
    "All nops should complete quickly: {:?}",
    elapsed
  );
}
