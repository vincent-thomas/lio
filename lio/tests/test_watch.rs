//! Tests for the file watch functionality.

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lio::api::ops::WatchMask;
use lio::{api, Lio};

fn temp_file() -> PathBuf {
  let id = std::process::id();
  let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap()
    .as_nanos();
  PathBuf::from(format!("/tmp/lio_watch_test_{}_{}", id, ts))
}

#[test]
fn test_watch_modify() {
  let path = temp_file();

  // Create the file first
  fs::write(&path, b"initial content").unwrap();

  let mut lio = Lio::new(64).unwrap();
  let (sender, receiver) = mpsc::channel();

  // Start watching for modifications
  api::watch(&path, WatchMask::MODIFY)
    .with_lio(&mut lio)
    .send_with(sender);

  // Modify the file in a separate thread after a short delay
  let path_clone = path.clone();
  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    let mut f = fs::OpenOptions::new()
      .write(true)
      .open(&path_clone)
      .unwrap();
    f.write_all(b"modified!").unwrap();
  });

  let start = Instant::now();

  // Wait for the watch to complete
  loop {
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    match receiver.try_recv() {
      Ok(result) => {
        let events = result.expect("watch should succeed");
        assert!(
          events.contains(WatchMask::MODIFY),
          "Expected MODIFY event, got {:?}",
          events
        );
        break;
      }
      Err(mpsc::TryRecvError::Empty) => {
        if start.elapsed() > Duration::from_secs(5) {
          panic!("Timed out waiting for watch event");
        }
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        panic!("Channel disconnected");
      }
    }
  }

  // Cleanup
  let _ = fs::remove_file(&path);
}

#[test]
fn test_watch_delete() {
  let path = temp_file();

  // Create the file first
  fs::write(&path, b"to be deleted").unwrap();

  let mut lio = Lio::new(64).unwrap();
  let (sender, receiver) = mpsc::channel();

  // Start watching for deletions
  api::watch(&path, WatchMask::DELETE)
    .with_lio(&mut lio)
    .send_with(sender);

  // Delete the file in a separate thread after a short delay
  let path_clone = path.clone();
  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    fs::remove_file(&path_clone).unwrap();
  });

  let start = Instant::now();

  // Wait for the watch to complete
  loop {
    lio.run_timeout(Duration::from_millis(10)).unwrap();
    match receiver.try_recv() {
      Ok(result) => {
        let events = result.expect("watch should succeed");
        assert!(
          events.contains(WatchMask::DELETE),
          "Expected DELETE event, got {:?}",
          events
        );
        break;
      }
      Err(mpsc::TryRecvError::Empty) => {
        if start.elapsed() > Duration::from_secs(5) {
          panic!("Timed out waiting for watch event");
        }
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        panic!("Channel disconnected");
      }
    }
  }
}

#[test]
fn test_watch_nonexistent_file() {
  let path = temp_file();
  // Don't create the file - it doesn't exist

  let mut lio = Lio::new(64).unwrap();
  let (sender, receiver) = mpsc::channel();

  // Try to watch a nonexistent file - should fail
  api::watch(&path, WatchMask::MODIFY)
    .with_lio(&mut lio)
    .send_with(sender);

  // Poll once to get the immediate error
  lio.run_timeout(Duration::from_millis(10)).unwrap();

  let result = receiver.try_recv().expect("Should receive result immediately");
  assert!(result.is_err(), "Watching nonexistent file should fail");
}
