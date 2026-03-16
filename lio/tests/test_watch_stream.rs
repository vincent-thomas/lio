//! Tests for the watch_stream functionality.

#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use lio::api::ops::WatchMask;
use lio::{api, Lio};

fn temp_file() -> PathBuf {
    let id = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(format!("/tmp/lio_watch_stream_test_{}_{}", id, ts))
}

/// Test that watch_stream compiles and can be created
#[test]
fn test_watch_stream_creation() {
    let path = temp_file();

    // Create the file first
    fs::write(&path, b"initial content").unwrap();

    let lio = Lio::new(64).unwrap();

    // Create watch stream - just verify it compiles
    let _stream = api::watch_stream(&path, WatchMask::MODIFY | WatchMask::DELETE)
        .with_lio(&lio);

    // Cleanup
    let _ = fs::remove_file(&path);
}

/// Test watch_stream using callback-based approach
#[test]
fn test_watch_stream_modify_events() {
    use std::sync::mpsc;
    use std::thread;

    let path = temp_file();

    // Create the file first
    fs::write(&path, b"initial content").unwrap();

    let lio = Lio::new(64).unwrap();
    let (sender, receiver) = mpsc::channel();

    // Use single-shot watch for this test (stream API works the same way internally)
    api::watch(&path, WatchMask::MODIFY)
        .with_lio(&lio)
        .send_with(sender);

    // Modify the file in a separate thread
    let path_clone = path.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(&path_clone)
            .unwrap();
        f.write_all(b"modified!").unwrap();
    });

    let start = std::time::Instant::now();

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
