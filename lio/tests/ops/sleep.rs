#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use super::common;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lio::{
  Lio, api,
  api::resource::Resource,
  backend::ds::{DSBackend, DSConfig},
};

fn new_ds_lio() -> Lio {
  Lio::new_with_backend(
    DSBackend::with_config(DSConfig { fault_every: 0, ..DSConfig::default() }),
    64,
  )
  .unwrap()
}

fn open_rw_file(lio: &mut Lio, path: std::ffi::CString) -> Resource {
  let mut receiver = api::openat(
    &Resource::cwd(),
    path,
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    0o644,
  )
  .with_lio(lio)
  .send();
  common::poll_recv(lio, &mut receiver).unwrap()
}

fn write_all_at(lio: &mut Lio, fd: &Resource, bytes: &[u8], offset: u32) {
  let mut written_total = 0usize;
  while written_total < bytes.len() {
    let chunk = bytes[written_total..].to_vec();
    let mut receiver =
      api::write_at(fd, chunk.clone(), offset + written_total as u32)
        .with_lio(lio)
        .send();
    let (result, returned_buf) = common::poll_recv(lio, &mut receiver);
    let bytes_written = result.unwrap() as usize;
    assert!(bytes_written > 0);
    assert!(bytes_written <= returned_buf.len());
    written_total += bytes_written;
  }
}

fn unlink_file(lio: &mut Lio, path: std::ffi::CString) {
  let mut receiver =
    api::unlinkat(&Resource::cwd(), path, 0).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

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

fn run_sleep_and_measure(lio: &mut Lio, duration: Duration) -> Duration {
  let start = Instant::now();
  let mut recv = api::sleep(duration).with_lio(lio).send();

  let result =
    poll_recv_timeout(lio, &mut recv, duration + Duration::from_secs(1))
      .expect("sleep should complete within the polling budget");
  assert!(result.is_ok(), "sleep should complete successfully: {result:?}");

  start.elapsed()
}

#[test]
fn basic() {
  let mut lio = new_ds_lio();

  let start = Instant::now();
  let timeout_duration = Duration::from_millis(500);

  let mut recv = api::sleep(timeout_duration).with_lio(&mut lio).send();

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
fn multiple() {
  let mut lio = new_ds_lio();

  let start = Instant::now();

  // Start 3 timeouts with different durations
  let mut recv1 =
    api::sleep(Duration::from_millis(50)).with_lio(&mut lio).send();
  let mut recv2 =
    api::sleep(Duration::from_millis(100)).with_lio(&mut lio).send();
  let mut recv3 =
    api::sleep(Duration::from_millis(150)).with_lio(&mut lio).send();

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
fn zero_duration() {
  let mut lio = new_ds_lio();

  let start = Instant::now();

  // Zero duration timeout should complete almost immediately
  let mut recv = api::sleep(Duration::from_millis(0)).with_lio(&mut lio).send();

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
fn short_duration() {
  let mut lio = new_ds_lio();

  let start = Instant::now();
  let timeout_duration = Duration::from_millis(10);

  let mut recv = api::sleep(timeout_duration).with_lio(&mut lio).send();

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
fn concurrent_same_duration() {
  let mut lio = new_ds_lio();

  let (sender, receiver) = mpsc::channel();
  let timeout_duration = Duration::from_millis(100);

  // Start 5 timeouts with the same duration
  for _ in 0..5 {
    api::sleep(timeout_duration).with_lio(&mut lio).send_with(sender.clone());
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
fn interleaved_with_io() {
  let mut lio = new_ds_lio();

  let path =
    std::ffi::CString::new("/tmp/lio_sleep_interleaved_with_io.txt").unwrap();
  let file = open_rw_file(&mut lio, path.clone());
  write_all_at(&mut lio, &file, b"abc", 0);

  // Start a timeout
  let start = Instant::now();
  let timeout_duration = Duration::from_millis(100);
  let mut timeout_recv = api::sleep(timeout_duration).with_lio(&mut lio).send();

  // Do some real I/O operations while waiting.
  let (io_sender, io_receiver) = mpsc::channel();
  for _ in 0..3 {
    api::read_at(&file, vec![0u8; 1], 0)
      .with_lio(&mut lio)
      .send_with(io_sender.clone());
  }

  // Collect I/O results
  let mut io_done = 0;
  while io_done < 3 {
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    while let Ok((result, _buf)) = io_receiver.try_recv() {
      assert_eq!(result.expect("read should succeed"), 1);
      io_done += 1;
    }
  }

  // Wait for timeout
  let result =
    poll_recv_timeout(&mut lio, &mut timeout_recv, Duration::from_secs(1))
      .expect("Timeout should complete");

  assert!(result.is_ok(), "Timeout should complete successfully");
  assert!(
    start.elapsed() < Duration::from_secs(1),
    "Timeout should complete within the polling budget"
  );

  drop(file);
  unlink_file(&mut lio, path);
}

#[test]
fn ordering() {
  let mut lio = new_ds_lio();

  // Start timeouts in reverse order of duration
  let mut recv_long =
    api::sleep(Duration::from_millis(200)).with_lio(&mut lio).send();
  let mut recv_medium =
    api::sleep(Duration::from_millis(100)).with_lio(&mut lio).send();
  let mut recv_short =
    api::sleep(Duration::from_millis(50)).with_lio(&mut lio).send();

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
fn many_concurrent() {
  let mut lio = Lio::new_with_backend(
    DSBackend::with_config(DSConfig { fault_every: 0, ..DSConfig::default() }),
    256,
  )
  .unwrap();

  let (sender, receiver) = mpsc::channel();

  // Start 50 timeouts with varying durations
  for i in 0..50 {
    let duration = Duration::from_millis(50 + (i % 10) * 10);
    api::sleep(duration).with_lio(&mut lio).send_with(sender.clone());
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

#[test]
fn pause_resume_preserves_remaining_duration() {
  let mut lio = new_ds_lio();
  let mut recv =
    api::sleep(Duration::from_millis(200)).with_lio(&mut lio).send();

  std::thread::sleep(Duration::from_millis(100));
  lio::time::pause(&lio);

  std::thread::sleep(Duration::from_millis(250));
  lio.run_timeout(Duration::from_millis(10)).unwrap();
  assert!(
    recv.try_recv().is_none(),
    "sleep should not complete while lio time is paused"
  );

  lio::time::resume(&lio);
  lio.run_timeout(Duration::from_millis(10)).unwrap();
  assert!(
    recv.try_recv().is_none(),
    "sleep should still have remaining duration immediately after resume"
  );

  let result = poll_recv_timeout(&mut lio, &mut recv, Duration::from_secs(1))
    .expect("sleep should complete after resume and remaining duration");
  assert!(result.is_ok(), "sleep should succeed after resume: {result:?}");
}

#[test]
fn accuracy_within_five_milliseconds() {
  let mut lio = new_ds_lio();
  let sleep_duration = Duration::from_millis(100);
  let threshold = Duration::from_millis(5);

  for _ in 0..5 {
    let _ = run_sleep_and_measure(&mut lio, Duration::from_millis(10));
  }

  for sample in 0..10 {
    let elapsed = run_sleep_and_measure(&mut lio, sleep_duration);
    let overshoot = elapsed.saturating_sub(sleep_duration);

    assert!(
      overshoot <= threshold,
      "sleep sample {sample} exceeded the 5ms threshold: requested {sleep_duration:?}, elapsed {elapsed:?}, overshoot {overshoot:?}",
    );
  }
}
