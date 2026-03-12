use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lio::{Lio, api};

/// Helper to poll until we receive a result with timeout
fn poll_recv_timeout<T>(
  lio: &mut Lio,
  recv: &mut api::io::Receiver<T>,
  timeout: Duration,
) -> Option<T> {
  let start = Instant::now();
  loop {
    if let Some(result) = recv.try_recv() {
      return Some(result);
    }
    if start.elapsed() > timeout {
      return None;
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  }
}

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
    timeout_duration,
  );
  assert!(
    elapsed < timeout_duration + Duration::from_millis(200),
    "Timeout should not wait too much longer: time waited {elapsed:?}, time set waited: {timeout_duration:?}, wiggle: 200ms",
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

#[test]
fn test_timeout_zero_duration() {
  let mut lio = Lio::new(64).unwrap();

  let start = Instant::now();

  // Zero duration timeout should complete almost immediately
  let mut recv =
    api::timeout(Duration::from_millis(0)).with_lio(&mut lio).send();

  let result = poll_recv_timeout(&mut lio, &mut recv, Duration::from_secs(1))
    .expect("Zero timeout should complete");

  let elapsed = start.elapsed();

  assert!(result.is_ok(), "Zero timeout should complete successfully");
  assert!(
    elapsed < Duration::from_millis(100),
    "Zero timeout should complete quickly: {:?}",
    elapsed
  );
}

#[test]
fn test_timeout_short_duration() {
  let mut lio = Lio::new(64).unwrap();

  let start = Instant::now();
  let timeout_duration = Duration::from_millis(10);

  let mut recv = api::timeout(timeout_duration).with_lio(&mut lio).send();

  let result = poll_recv_timeout(&mut lio, &mut recv, Duration::from_secs(1))
    .expect("Short timeout should complete");

  let elapsed = start.elapsed();

  assert!(result.is_ok(), "Short timeout should complete successfully");
  assert!(
    elapsed >= timeout_duration,
    "Should wait at least {:?}, waited {:?}",
    timeout_duration,
    elapsed
  );
}

#[test]
fn test_timeout_concurrent_same_duration() {
  let mut lio = Lio::new(64).unwrap();

  let (sender, receiver) = mpsc::channel();
  let timeout_duration = Duration::from_millis(100);

  // Start 5 timeouts with the same duration
  for _ in 0..5 {
    api::timeout(timeout_duration).with_lio(&mut lio).send_with(sender.clone());
  }

  let start = Instant::now();

  // All should complete around the same time
  for i in 0..5 {
    loop {
      lio.run_timeout(Duration::from_millis(10)).unwrap();
      match receiver.try_recv() {
        Ok(result) => {
          assert!(result.is_ok(), "Timeout {} should succeed", i);
          break;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
  }

  let elapsed = start.elapsed();
  assert!(
    elapsed >= timeout_duration,
    "All timeouts should wait at least {:?}",
    timeout_duration
  );
  // Allow some slack for processing all 5
  assert!(
    elapsed < timeout_duration + Duration::from_millis(200),
    "All timeouts with same duration should complete close together: {:?}",
    elapsed
  );
}

#[test]
fn test_timeout_interleaved_with_io() {
  let mut lio = Lio::new(64).unwrap();

  // Create a socket for some I/O
  let sock_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  assert!(sock_fd >= 0, "Failed to create socket");
  let sock = unsafe { lio::api::resource::Resource::from_raw_fd(sock_fd) };

  // Start a timeout
  let start = Instant::now();
  let timeout_duration = Duration::from_millis(100);
  let mut timeout_recv =
    api::timeout(timeout_duration).with_lio(&mut lio).send();

  // Do some nop operations while waiting
  let (nop_sender, nop_receiver) = mpsc::channel();
  for _ in 0..3 {
    api::nop().with_lio(&mut lio).send_with(nop_sender.clone());
  }

  // Collect nop results
  let mut nops_done = 0;
  while nops_done < 3 {
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    while let Ok(result) = nop_receiver.try_recv() {
      result.expect("nop should succeed");
      nops_done += 1;
    }
  }

  // Wait for timeout
  let result =
    poll_recv_timeout(&mut lio, &mut timeout_recv, Duration::from_secs(1))
      .expect("Timeout should complete");

  let elapsed = start.elapsed();

  assert!(result.is_ok(), "Timeout should complete successfully");
  assert!(
    elapsed >= timeout_duration,
    "Timeout should wait at least {:?}",
    timeout_duration
  );

  // sock is automatically dropped/closed
  drop(sock);
}

#[test]
fn test_timeout_ordering() {
  let mut lio = Lio::new(64).unwrap();

  // Start timeouts in reverse order of duration
  let mut recv_long =
    api::timeout(Duration::from_millis(200)).with_lio(&mut lio).send();
  let mut recv_medium =
    api::timeout(Duration::from_millis(100)).with_lio(&mut lio).send();
  let mut recv_short =
    api::timeout(Duration::from_millis(50)).with_lio(&mut lio).send();

  let start = Instant::now();

  // Short should complete first
  let result_short =
    poll_recv_timeout(&mut lio, &mut recv_short, Duration::from_secs(1))
      .expect("Short timeout should complete");
  let elapsed_short = start.elapsed();

  // Medium should complete second
  let result_medium =
    poll_recv_timeout(&mut lio, &mut recv_medium, Duration::from_secs(1))
      .expect("Medium timeout should complete");
  let elapsed_medium = start.elapsed();

  // Long should complete last
  let result_long =
    poll_recv_timeout(&mut lio, &mut recv_long, Duration::from_secs(1))
      .expect("Long timeout should complete");
  let elapsed_long = start.elapsed();

  assert!(result_short.is_ok());
  assert!(result_medium.is_ok());
  assert!(result_long.is_ok());

  assert!(elapsed_short < elapsed_medium, "Short should finish before medium");
  assert!(elapsed_medium < elapsed_long, "Medium should finish before long");
}

#[test]
fn test_timeout_many_concurrent() {
  let mut lio = Lio::new(256).unwrap();

  let (sender, receiver) = mpsc::channel();

  // Start 50 timeouts with varying durations
  for i in 0..50 {
    let duration = Duration::from_millis(50 + (i % 10) * 10);
    api::timeout(duration).with_lio(&mut lio).send_with(sender.clone());
  }

  let start = Instant::now();

  // Wait for all to complete
  let mut completed = 0;
  while completed < 50 {
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    while let Ok(result) = receiver.try_recv() {
      assert!(result.is_ok(), "Timeout {} should succeed", completed);
      completed += 1;
    }
    if start.elapsed() > Duration::from_secs(5) {
      panic!(
        "Timed out waiting for all timeouts to complete: only {} of 50",
        completed
      );
    }
  }

  let elapsed = start.elapsed();
  // All timeouts should complete within reasonable time
  // Longest is 50 + 9*10 = 140ms, plus some slack
  assert!(
    elapsed < Duration::from_millis(500),
    "All timeouts should complete in reasonable time: {:?}",
    elapsed
  );
}
