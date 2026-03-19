//! Tests for file locking (flock).

mod common;

use common::{TempFile, poll_until_recv};
use lio::api::ops::lock;
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::os::fd::FromRawFd;
use std::sync::mpsc;

fn cwd_resource() -> Resource {
  // AT_FDCWD is -100 on most systems
  unsafe { Resource::from_raw_fd(libc::AT_FDCWD) }
}

#[test]
fn test_flock_exclusive() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("flock_exclusive");

  // Create file
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd_resource(), temp.path.clone(), libc::O_CREAT | libc::O_RDWR)
    .with_lio(&mut lio)
    .send_with(sender_open);
  let resource = poll_until_recv(&mut lio, &receiver_open).unwrap();

  // Acquire exclusive lock
  let (sender_flock, receiver_flock) = mpsc::channel();
  api::flock(&resource, lock::LOCK_EX)
    .with_lio(&mut lio)
    .send_with(sender_flock.clone());
  poll_until_recv(&mut lio, &receiver_flock)
    .expect("Failed to acquire exclusive lock");

  // Unlock
  api::flock(&resource, lock::LOCK_UN)
    .with_lio(&mut lio)
    .send_with(sender_flock.clone());
  poll_until_recv(&mut lio, &receiver_flock).expect("Failed to unlock");
}

#[test]
fn test_flock_shared() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("flock_shared");

  // Create file first
  unsafe {
    let fd = libc::creat(temp.path.as_ptr(), 0o644);
    libc::close(fd);
  }

  // Open file
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd_resource(), temp.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_open);
  let resource = poll_until_recv(&mut lio, &receiver_open).unwrap();

  // Acquire shared lock
  let (sender_flock, receiver_flock) = mpsc::channel();
  api::flock(&resource, lock::LOCK_SH)
    .with_lio(&mut lio)
    .send_with(sender_flock.clone());
  poll_until_recv(&mut lio, &receiver_flock)
    .expect("Failed to acquire shared lock");

  // Unlock
  api::flock(&resource, lock::LOCK_UN)
    .with_lio(&mut lio)
    .send_with(sender_flock.clone());
  poll_until_recv(&mut lio, &receiver_flock).expect("Failed to unlock");
}

#[test]
fn test_flock_nonblocking() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("flock_nb");

  // Create file
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd_resource(), temp.path.clone(), libc::O_CREAT | libc::O_RDWR)
    .with_lio(&mut lio)
    .send_with(sender_open);
  let resource = poll_until_recv(&mut lio, &receiver_open).unwrap();

  // Try non-blocking exclusive lock
  let (sender_flock, receiver_flock) = mpsc::channel();
  api::flock(&resource, lock::LOCK_EX | lock::LOCK_NB)
    .with_lio(&mut lio)
    .send_with(sender_flock.clone());
  poll_until_recv(&mut lio, &receiver_flock)
    .expect("Failed to acquire non-blocking lock");

  // Unlock
  api::flock(&resource, lock::LOCK_UN)
    .with_lio(&mut lio)
    .send_with(sender_flock.clone());
  poll_until_recv(&mut lio, &receiver_flock).expect("Failed to unlock");
}
