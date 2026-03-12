//! Stress tests for high-concurrency scenarios.

mod common;

use common::{TempFile, poll_until_recv, setup_tcp_pair};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ============================================================================
// Concurrent nop tests
// ============================================================================

#[test]
fn test_concurrent_nop_1000() {
  // Capacity smaller than total - tests store cleanup
  let mut lio = Lio::new(256).unwrap();

  let (sender, receiver) = mpsc::channel();
  let total_ops = 1000;
  let max_pending = 128;

  let start = Instant::now();
  let mut submitted = 0;
  let mut completed = 0;

  while completed < total_ops {
    // Submit up to max_pending
    while submitted < total_ops && (submitted - completed) < max_pending {
      api::nop().with_lio(&mut lio).send_with(sender.clone());
      submitted += 1;
    }

    // Run event loop
    lio.run_timeout(Duration::from_micros(100)).unwrap();

    // Drain all available results
    loop {
      match receiver.try_recv() {
        Ok(result) => {
          result.expect("nop should succeed");
          completed += 1;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
  }

  let elapsed = start.elapsed();
  let ops_per_sec = total_ops as f64 / elapsed.as_secs_f64();
  eprintln!(
    "1000 nops completed in {:?} ({:.0} ops/sec)",
    elapsed, ops_per_sec
  );
}

#[test]
fn test_concurrent_nop_10000() {
  // Capacity smaller than total ops - tests store cleanup
  let mut lio = Lio::new(1024).unwrap();

  let (sender, receiver) = mpsc::channel();
  let total_ops = 10000;
  let max_pending = 500;

  let start = Instant::now();
  let mut submitted = 0;
  let mut completed = 0;

  while completed < total_ops {
    // Submit up to max_pending
    while submitted < total_ops && (submitted - completed) < max_pending {
      api::nop().with_lio(&mut lio).send_with(sender.clone());
      submitted += 1;
    }

    // Run event loop
    lio.run_timeout(Duration::from_micros(100)).unwrap();

    // Drain all available results
    loop {
      match receiver.try_recv() {
        Ok(result) => {
          result.expect("nop should succeed");
          completed += 1;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
  }

  let elapsed = start.elapsed();
  let ops_per_sec = total_ops as f64 / elapsed.as_secs_f64();
  eprintln!(
    "10000 nops completed in {:?} ({:.0} ops/sec)",
    elapsed, ops_per_sec
  );
}

// ============================================================================
// Concurrent file writes
// ============================================================================

#[test]
fn test_concurrent_file_writes_256() {
  let mut lio = Lio::new(512).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let (sender_open, receiver_open) = mpsc::channel();
  let (sender_write, receiver_write) = mpsc::channel();

  let num_files = 256;
  let mut temps = Vec::new();

  // Create and open all files
  for i in 0..num_files {
    let temp = TempFile::new(&format!("stress_write_{}", i));
    api::openat(
      &cwd,
      temp.path.clone(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
    )
    .with_lio(&mut lio)
    .send_with(sender_open.clone());
    temps.push(temp);
  }

  // Collect all file descriptors
  let mut fds = Vec::new();
  for i in 0..num_files {
    let fd = poll_until_recv(&mut lio, &receiver_open)
      .expect(&format!("Failed to open file {}", i));
    fds.push(fd);
  }

  // Write to all files concurrently
  let start = Instant::now();
  for (i, fd) in fds.iter().enumerate() {
    let data = format!("Data for file {}\n", i).into_bytes();
    api::write(fd, data).with_lio(&mut lio).send_with(sender_write.clone());
  }

  // Wait for all writes to complete
  for i in 0..num_files {
    let (result, _) = poll_until_recv(&mut lio, &receiver_write);
    result.expect(&format!("Failed to write to file {}", i));
  }

  let elapsed = start.elapsed();
  eprintln!("256 concurrent file writes completed in {:?}", elapsed);

  std::mem::forget(cwd);
}

// ============================================================================
// Network stress tests
// ============================================================================

#[test]
fn test_concurrent_connections_100() {
  let mut lio = Lio::new(1024).unwrap();

  let num_connections = 100;
  let mut pairs = Vec::new();

  let start = Instant::now();

  // Create 100 TCP pairs
  for i in 0..num_connections {
    let pair = setup_tcp_pair(&mut lio);
    pairs.push(pair);
    if (i + 1) % 20 == 0 {
      eprintln!("Created {} connections...", i + 1);
    }
  }

  let connect_elapsed = start.elapsed();
  eprintln!("100 connections established in {:?}", connect_elapsed);

  // Send data on all connections
  let (sender_send, receiver_send) = mpsc::channel();
  for (i, pair) in pairs.iter().enumerate() {
    let data = format!("Message from connection {}", i).into_bytes();
    api::send(&pair.client_sock, data, None)
      .with_lio(&mut lio)
      .send_with(sender_send.clone());
  }

  // Wait for all sends
  for i in 0..num_connections {
    let (result, _) = poll_until_recv(&mut lio, &receiver_send);
    result.expect(&format!("Send {} failed", i));
  }

  let send_elapsed = start.elapsed();
  eprintln!("All sends completed in {:?}", send_elapsed);

  // Receive on all connections
  let (sender_recv, receiver_recv) = mpsc::channel();
  for pair in &pairs {
    api::recv(&pair.accepted_fd, vec![0u8; 128], None)
      .with_lio(&mut lio)
      .send_with(sender_recv.clone());
  }

  // Wait for all receives
  for i in 0..num_connections {
    let (result, buf) = poll_until_recv(&mut lio, &receiver_recv);
    let bytes = result.expect(&format!("Recv {} failed", i)) as usize;
    assert!(bytes > 0, "Should receive data");
    let msg = String::from_utf8_lossy(&buf[..bytes]);
    assert!(msg.starts_with("Message from connection"));
  }

  let total_elapsed = start.elapsed();
  eprintln!("100 connections total time: {:?}", total_elapsed);
}

// ============================================================================
// Sustained load tests
// ============================================================================

#[test]
fn test_sustained_io_5_seconds() {
  let mut lio = Lio::new(1024).unwrap();

  let duration = Duration::from_secs(5);
  let start = Instant::now();
  let mut ops_completed = 0u64;
  let mut pending = 0u64;

  let (sender, receiver) = mpsc::channel();

  // Multiplex ~500 concurrent ops
  let max_pending = 500;

  while start.elapsed() < duration {
    // Submit ops up to max_pending
    while pending < max_pending {
      api::nop().with_lio(&mut lio).send_with(sender.clone());
      pending += 1;
    }

    // Run the event loop briefly
    lio.run_timeout(Duration::from_micros(100)).unwrap();

    // Drain completed ops
    loop {
      match receiver.try_recv() {
        Ok(result) => {
          result.expect("nop should succeed");
          ops_completed += 1;
          pending -= 1;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
  }

  // Drain remaining
  while pending > 0 {
    lio.run_timeout(Duration::from_millis(5)).unwrap();
    loop {
      match receiver.try_recv() {
        Ok(result) => {
          result.expect("nop should succeed");
          ops_completed += 1;
          pending -= 1;
        }
        Err(mpsc::TryRecvError::Empty) => break,
        Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
      }
    }
  }

  let elapsed = start.elapsed();
  let ops_per_sec = ops_completed as f64 / elapsed.as_secs_f64();

  eprintln!(
    "Sustained load: {} ops in {:?} ({:.0} ops/sec)",
    ops_completed, elapsed, ops_per_sec
  );
}

// ============================================================================
// Sequential operations test (baseline)
// ============================================================================

#[test]
fn test_sequential_nop_200() {
  let mut lio = Lio::new(64).unwrap();

  let num_ops = 200;
  let start = Instant::now();

  for i in 0..num_ops {
    let (sender, receiver) = mpsc::channel();
    api::nop().with_lio(&mut lio).send_with(sender);
    let result = poll_until_recv(&mut lio, &receiver);
    result.expect(&format!("Op {} failed", i));
  }

  let elapsed = start.elapsed();
  eprintln!(
    "{} sequential nops in {:?} ({:.0} ops/sec)",
    num_ops,
    elapsed,
    num_ops as f64 / elapsed.as_secs_f64()
  );
}

// ============================================================================
// Mixed operation types test
// ============================================================================

#[test]
fn test_mixed_op_types() {
  // Tests nop + write + fsync on the same file
  // Note: Sequential because kqueue can't have multiple pending ops on same fd
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let temp =
    TempFile::new(&format!("mixed_ops_{:?}", std::thread::current().id()));

  // Create a file to work with
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(
    &cwd,
    temp.path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
  )
  .with_lio(&mut lio)
  .send_with(sender_open);
  let fd =
    poll_until_recv(&mut lio, &receiver_open).expect("Failed to create file");

  let start = Instant::now();
  let mut ops_completed = 0;

  let num_rounds = 50;
  for i in 0..num_rounds {
    // Nop
    let (sender, receiver) = mpsc::channel();
    api::nop().with_lio(&mut lio).send_with(sender);
    poll_until_recv(&mut lio, &receiver).expect("nop failed");
    ops_completed += 1;

    // Write
    let (sender, receiver) = mpsc::channel();
    let data = format!("Round {} data\n", i).into_bytes();
    api::write(&fd, data).with_lio(&mut lio).send_with(sender);
    poll_until_recv(&mut lio, &receiver).0.expect("write failed");
    ops_completed += 1;

    // Fsync every 10 rounds
    if i % 10 == 0 {
      let (sender, receiver) = mpsc::channel();
      api::fsync(&fd).with_lio(&mut lio).send_with(sender);
      poll_until_recv(&mut lio, &receiver).expect("fsync failed");
      ops_completed += 1;
    }
  }

  let elapsed = start.elapsed();
  eprintln!(
    "Mixed op types: {} ops in {:?} ({:.0} ops/sec)",
    ops_completed,
    elapsed,
    ops_completed as f64 / elapsed.as_secs_f64()
  );

  std::mem::forget(cwd);
}
