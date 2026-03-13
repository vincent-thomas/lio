//! Integration tests for LioUring core functionality.

use lio_uring::operation::*;
use lio_uring::{LioUring, Params, SqeFlags};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

// ============================================================================
// Ring Creation Tests
// ============================================================================

#[test]
fn test_new_basic() {
  let ring = LioUring::new(8);
  assert!(ring.is_ok(), "failed to create ring: {:?}", ring.err());
}

#[test]
fn test_new_various_capacities() {
  for capacity in [1, 2, 4, 8, 16, 32, 64, 128, 256] {
    let ring = LioUring::new(capacity);
    assert!(ring.is_ok(), "failed to create ring with capacity {}", capacity);
  }
}

#[test]
fn test_new_power_of_two_rounding() {
  // Kernel rounds up to power of 2
  let ring = LioUring::new(5).unwrap();
  // Should have at least 5 slots (kernel rounds to 8)
  assert!(ring.sq_space_left() >= 5);
}

#[test]
fn test_with_params_default() {
  let ring = LioUring::with_params(Params::default());
  assert!(ring.is_ok());
}

#[test]
fn test_is_sqpoll_false_by_default() {
  let ring = LioUring::new(8).unwrap();
  assert!(!ring.is_sqpoll());
}

// ============================================================================
// Submission Queue Tests
// ============================================================================

#[test]
fn test_sq_space_left_initial() {
  let ring = LioUring::new(8).unwrap();
  assert!(ring.sq_space_left() >= 8);
}

#[test]
fn test_sq_space_decreases_on_push() {
  let mut ring = LioUring::new(8).unwrap();
  let initial = ring.sq_space_left();

  let op = Nop::new().build();
  unsafe { ring.push(op, 1) }.unwrap();

  assert_eq!(ring.sq_space_left(), initial - 1);
}

#[test]
fn test_sq_full_error() {
  let mut ring = LioUring::new(1).unwrap();
  let capacity = ring.sq_space_left();

  // Fill up the queue
  for i in 0..capacity {
    let op = Nop::new().build();
    unsafe { ring.push(op, i as u64) }.unwrap();
  }

  // Next push should fail
  let op = Nop::new().build();
  let result = unsafe { ring.push(op, 999) };
  assert!(result.is_err());
  assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn test_sq_space_restored_after_submit() {
  let mut ring = LioUring::new(4).unwrap();
  let initial = ring.sq_space_left();

  // Push some ops
  for i in 0..2 {
    let op = Nop::new().build();
    unsafe { ring.push(op, i) }.unwrap();
  }

  assert_eq!(ring.sq_space_left(), initial - 2);

  // Submit and wait for completions
  ring.submit().unwrap();
  ring.wait().unwrap();
  ring.wait().unwrap();

  // Space should be restored
  assert_eq!(ring.sq_space_left(), initial);
}

// ============================================================================
// Completion Queue Tests
// ============================================================================

#[test]
fn test_cq_ready_initially_zero() {
  let ring = LioUring::new(8).unwrap();
  assert_eq!(ring.cq_ready(), 0);
}

#[test]
fn test_cq_ready_after_completion() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Nop::new().build();
  unsafe { ring.push(op, 1) }.unwrap();
  ring.submit().unwrap();

  // Wait a bit for completion
  std::thread::sleep(Duration::from_millis(10));

  assert!(ring.cq_ready() >= 1);
}

#[test]
fn test_try_wait_empty() {
  let mut ring = LioUring::new(8).unwrap();
  let result = ring.try_wait().unwrap();
  assert!(result.is_none());
}

#[test]
fn test_try_wait_with_completion() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Nop::new().build();
  unsafe { ring.push(op, 42) }.unwrap();
  ring.submit().unwrap();

  // Wait for completion to be ready
  ring.wait().unwrap();

  // Push another and check try_wait
  let op = Nop::new().build();
  unsafe { ring.push(op, 43) }.unwrap();
  ring.submit().unwrap();

  std::thread::sleep(Duration::from_millis(10));

  let result = ring.try_wait().unwrap();
  assert!(result.is_some());
  assert_eq!(result.unwrap().user_data(), 43);
}

#[test]
fn test_wait_timeout_expires() {
  let mut ring = LioUring::new(8).unwrap();

  let start = std::time::Instant::now();
  let result = ring.wait_timeout(Duration::from_millis(50)).unwrap();
  let elapsed = start.elapsed();

  assert!(result.is_none());
  assert!(elapsed >= Duration::from_millis(40)); // Allow some slack
}

#[test]
fn test_wait_timeout_immediate_completion() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Nop::new().build();
  unsafe { ring.push(op, 123) }.unwrap();
  ring.submit().unwrap();

  let result = ring.wait_timeout(Duration::from_secs(5)).unwrap();
  assert!(result.is_some());
  assert_eq!(result.unwrap().user_data(), 123);
}

#[test]
fn test_peek_does_not_consume() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Nop::new().build();
  unsafe { ring.push(op, 99) }.unwrap();
  ring.submit().unwrap();
  ring.wait().unwrap(); // Ensure completion is ready

  // Push another
  let op = Nop::new().build();
  unsafe { ring.push(op, 100) }.unwrap();
  ring.submit().unwrap();

  std::thread::sleep(Duration::from_millis(10));

  // Peek should see it
  let peeked = ring.peek().unwrap();
  assert!(peeked.is_some());

  // Peek again should see same completion
  let peeked2 = ring.peek().unwrap();
  assert!(peeked2.is_some());
  assert_eq!(peeked.unwrap().user_data(), peeked2.unwrap().user_data());

  // try_wait should consume it
  let consumed = ring.try_wait().unwrap();
  assert!(consumed.is_some());
}

// ============================================================================
// Completion Struct Tests
// ============================================================================

#[test]
fn test_completion_user_data() {
  let mut ring = LioUring::new(8).unwrap();

  for user_data in [0u64, 1, 42, u64::MAX / 2, u64::MAX] {
    let op = Nop::new().build();
    unsafe { ring.push(op, user_data) }.unwrap();
    ring.submit().unwrap();

    let completion = ring.wait().unwrap();
    assert_eq!(completion.user_data(), user_data);
  }
}

#[test]
fn test_completion_is_ok_success() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Nop::new().build();
  unsafe { ring.push(op, 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0);
}

#[test]
fn test_completion_is_ok_failure() {
  let mut ring = LioUring::new(8).unwrap();

  // Read from invalid fd should fail
  let mut buf = [0u8; 64];
  let op =
    lio_uring::operation::Read::new(-1, buf.as_mut_ptr(), buf.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(!completion.is_ok());
  assert!(completion.result() < 0);
}

// ============================================================================
// SqeFlags Tests
// ============================================================================

#[test]
fn test_sqe_flags_none() {
  assert_eq!(SqeFlags::NONE.bits(), 0);
}

#[test]
fn test_sqe_flags_or() {
  let combined = SqeFlags::ASYNC.or(SqeFlags::IO_LINK);
  assert!(combined.contains(SqeFlags::ASYNC));
  assert!(combined.contains(SqeFlags::IO_LINK));
  assert!(!combined.contains(SqeFlags::FIXED_FILE));
}

#[test]
fn test_sqe_flags_bitor_operator() {
  let combined = SqeFlags::ASYNC | SqeFlags::IO_DRAIN;
  assert!(combined.contains(SqeFlags::ASYNC));
  assert!(combined.contains(SqeFlags::IO_DRAIN));
}

#[test]
fn test_sqe_flags_contains() {
  let flags = SqeFlags::ASYNC | SqeFlags::IO_LINK | SqeFlags::FIXED_FILE;
  assert!(flags.contains(SqeFlags::ASYNC));
  assert!(flags.contains(SqeFlags::IO_LINK));
  assert!(flags.contains(SqeFlags::FIXED_FILE));
  assert!(!flags.contains(SqeFlags::IO_DRAIN));
}

// ============================================================================
// Linked Operations Tests
// ============================================================================

#[test]
fn test_linked_operations() {
  let mut ring = LioUring::new(8).unwrap();

  // Create a temp file
  let path = "/tmp/lio_uring_test_linked";
  let mut file = File::create(path).unwrap();
  file.write_all(b"hello").unwrap();
  drop(file);

  let file = File::open(path).unwrap();
  let fd = file.as_raw_fd();

  // Link read -> nop
  let mut buf = [0u8; 64];
  let read_op =
    lio_uring::operation::Read::new(fd, buf.as_mut_ptr(), buf.len() as u32);
  unsafe { ring.push_with_flags(read_op.build(), 1, SqeFlags::IO_LINK) }
    .unwrap();

  let nop_op = Nop::new();
  unsafe { ring.push(nop_op.build(), 2) }.unwrap();

  ring.submit().unwrap();

  // Should get both completions
  let c1 = ring.wait().unwrap();
  let c2 = ring.wait().unwrap();

  // Order might vary, but both should complete
  let user_datas: Vec<u64> = vec![c1.user_data(), c2.user_data()];
  assert!(user_datas.contains(&1));
  assert!(user_datas.contains(&2));

  std::fs::remove_file(path).ok();
}

// ============================================================================
// File Registration Tests
// ============================================================================

#[test]
fn test_register_files() {
  let mut ring = LioUring::new(8).unwrap();

  let file1 = File::open("/dev/null").unwrap();
  let file2 = File::open("/dev/zero").unwrap();

  let fds = [file1.as_raw_fd(), file2.as_raw_fd()];
  let result = ring.register_files(&fds);
  assert!(result.is_ok());

  let result = ring.unregister_files();
  assert!(result.is_ok());
}

#[test]
fn test_register_files_update() {
  let mut ring = LioUring::new(8).unwrap();

  let file1 = File::open("/dev/null").unwrap();
  let file2 = File::open("/dev/zero").unwrap();

  let fds = [file1.as_raw_fd(), -1]; // Second slot empty
  ring.register_files(&fds).unwrap();

  // Update second slot
  let update_fds = [file2.as_raw_fd()];
  let result = ring.register_files_update(1, &update_fds);
  assert!(result.is_ok());

  ring.unregister_files().unwrap();
}

#[test]
fn test_unregister_files_without_register() {
  let mut ring = LioUring::new(8).unwrap();
  let result = ring.unregister_files();
  // Should fail - nothing registered
  assert!(result.is_err());
}

// ============================================================================
// Buffer Registration Tests
// ============================================================================

#[test]
fn test_register_buffers() {
  let mut ring = LioUring::new(8).unwrap();

  let buf1 = vec![0u8; 4096];
  let buf2 = vec![0u8; 4096];

  let slices = [std::io::IoSlice::new(&buf1), std::io::IoSlice::new(&buf2)];

  let result = unsafe { ring.register_buffers(&slices) };
  assert!(result.is_ok());

  let result = ring.unregister_buffers();
  assert!(result.is_ok());
}

#[test]
fn test_unregister_buffers_without_register() {
  let mut ring = LioUring::new(8).unwrap();
  let result = ring.unregister_buffers();
  // Should fail - nothing registered
  assert!(result.is_err());
}

// ============================================================================
// Multiple Operations Tests
// ============================================================================

#[test]
fn test_batch_submit() {
  let mut ring = LioUring::new(32).unwrap();

  // Submit multiple nops at once
  for i in 0..10 {
    let op = Nop::new().build();
    unsafe { ring.push(op, i) }.unwrap();
  }

  let submitted = ring.submit().unwrap();
  assert_eq!(submitted, 10);

  // Wait for all completions
  for _ in 0..10 {
    let completion = ring.wait().unwrap();
    assert!(completion.is_ok());
  }
}

#[test]
fn test_interleaved_submit_wait() {
  let mut ring = LioUring::new(8).unwrap();

  for i in 0..5 {
    let op = Nop::new().build();
    unsafe { ring.push(op, i) }.unwrap();
    ring.submit().unwrap();

    let completion = ring.wait().unwrap();
    assert_eq!(completion.user_data(), i);
  }
}

// ============================================================================
// Stress Tests
// ============================================================================

#[test]
fn test_many_operations() {
  let mut ring = LioUring::new(256).unwrap();
  let count = 1000;

  for batch_start in (0..count).step_by(100) {
    let batch_end = (batch_start + 100).min(count);

    for i in batch_start..batch_end {
      let op = Nop::new().build();
      unsafe { ring.push(op, i as u64) }.unwrap();
    }

    ring.submit().unwrap();

    for _ in batch_start..batch_end {
      let completion = ring.wait().unwrap();
      assert!(completion.is_ok());
    }
  }
}

#[test]
fn test_rapid_submit_cycles() {
  let mut ring = LioUring::new(8).unwrap();

  for _ in 0..100 {
    let op = Nop::new().build();
    unsafe { ring.push(op, 1) }.unwrap();
    ring.submit().unwrap();
    ring.wait().unwrap();
  }
}
