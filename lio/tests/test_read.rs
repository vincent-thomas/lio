//! Tests for read and read_at operations.

mod common;

use common::{TempFile, poll_recv, poll_until_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use proptest::{prelude::*, test_runner::TestRunner};
use std::ffi::CString;
use std::os::fd::FromRawFd;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn test_read_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("read_basic");

  // Create file with data
  let test_data = b"Hello, World!";
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const libc::c_void, test_data.len());
    libc::close(fd);
  }

  // Open for reading
  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let (sender_open, receiver_open) = mpsc::channel();
  api::openat(&cwd, temp.path.clone(), libc::O_RDONLY)
    .with_lio(&mut lio)
    .send_with(sender_open);

  let fd =
    poll_until_recv(&mut lio, &receiver_open).expect("Failed to open file");

  // Read the data
  let buf = vec![0u8; 64];
  let mut recv = api::read(&fd, buf).with_lio(&mut lio).send();

  let (result, buf) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to read") as usize;

  assert_eq!(bytes_read, test_data.len());
  assert_eq!(&buf[..bytes_read], test_data);

  std::mem::forget(cwd);
}

#[test]
fn test_read_large_buffer() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("read_large");

  // Create large data (1MB)
  let large_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(
      fd,
      large_data.as_ptr() as *const libc::c_void,
      large_data.len(),
    );
    libc::close(fd);
  }

  // Open for reading
  let fd = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read it back
  let buf = vec![0u8; 1024 * 1024];
  let mut recv = api::read(&fd, buf).with_lio(&mut lio).send();

  let (result, buf) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to read large buffer") as usize;

  assert_eq!(bytes_read, large_data.len());
  assert_eq!(&buf[..bytes_read], large_data.as_slice());
}

#[test]
fn test_read_concurrent() {
  let mut lio = Lio::new(64).unwrap();

  // Test multiple concurrent read operations on different files
  let test_data: Vec<_> = (0..10)
    .map(|i| {
      let temp = TempFile::new(&format!("read_concurrent_{}", i));
      let data = format!("Data for file {}", i);

      unsafe {
        let fd = libc::open(
          temp.path.as_ptr(),
          libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
          0o644,
        );
        libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
        libc::close(fd);
      }

      (temp, data)
    })
    .collect();

  let (sender, receiver) = mpsc::channel();

  for (temp, _) in &test_data {
    let fd = unsafe {
      Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
    };
    let buf = vec![0u8; 100];
    api::read(&fd, buf).with_lio(&mut lio).send_with(sender.clone());
  }

  // Collect all results
  for _ in 0..10 {
    let (result, _buf) = poll_until_recv(&mut lio, &receiver);
    let bytes_read = result.expect("Failed to read") as usize;
    assert!(bytes_read > 0);
  }
}

#[test]
fn test_read_at_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("read_at_basic");

  // Create file with data
  let test_data = b"0123456789ABCDEF";
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const libc::c_void, test_data.len());
    libc::close(fd);
  }

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read from offset 5
  let buf = vec![0u8; 5];
  let mut recv = api::read_at(&fd, buf, 5).with_lio(&mut lio).send();

  let (result, buf) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to read_at") as usize;

  assert_eq!(bytes_read, 5);
  assert_eq!(&buf[..bytes_read], b"56789");
}

#[test]
fn test_read_at_offset_beyond_file() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("read_at_beyond");

  // Create small file
  let test_data = b"Hello";
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const libc::c_void, test_data.len());
    libc::close(fd);
  }

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read beyond EOF
  let buf = vec![0u8; 10];
  let mut recv = api::read_at(&fd, buf, 100).with_lio(&mut lio).send();

  let (result, _buf) = poll_recv(&mut lio, &mut recv);
  let bytes_read =
    result.expect("read_at beyond EOF should succeed with 0 bytes") as usize;

  assert_eq!(bytes_read, 0, "Reading beyond EOF should return 0 bytes");
}

/// Test reading from empty file.
/// Note: This test is Linux-only because the polling backend on macOS
/// doesn't generate readiness events for reading from empty files.
#[cfg(target_os = "linux")]
#[test]
fn test_read_empty_file() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("read_empty");

  // Create empty file
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read from empty file using read_at with explicit offset 0
  // This works better across backends than read() on empty files
  let buf = vec![0u8; 64];
  let mut recv = api::read_at(&fd, buf, 0).with_lio(&mut lio).send();

  let (result, _buf) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Reading empty file should succeed") as usize;

  assert_eq!(bytes_read, 0, "Reading empty file should return 0 bytes");
}

#[test]
fn test_read_partial_buffer() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("read_partial");

  // Create file with data
  let test_data = b"0123456789";
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, test_data.as_ptr() as *const libc::c_void, test_data.len());
    libc::close(fd);
  }

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read with buffer smaller than file
  let buf = vec![0u8; 5];
  let mut recv = api::read(&fd, buf).with_lio(&mut lio).send();

  let (result, buf) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to read") as usize;

  assert_eq!(bytes_read, 5);
  assert_eq!(&buf[..bytes_read], b"01234");
}

#[test]
fn test_read_nonexistent_file() {
  let mut lio = Lio::new(64).unwrap();

  let cwd = unsafe { Resource::from_raw_fd(libc::AT_FDCWD) };
  let path =
    CString::new("/tmp/lio_test_nonexistent_12345_abcdef.txt").unwrap();

  // Try to open non-existent file
  let (sender, receiver) = mpsc::channel();
  api::openat(&cwd, path, libc::O_RDONLY).with_lio(&mut lio).send_with(sender);

  let result = poll_until_recv(&mut lio, &receiver);
  assert!(result.is_err(), "Opening non-existent file should fail");

  std::mem::forget(cwd);
}

/// Property-based test for read operations with various data sizes and offsets.
/// Uses a limited number of cases to avoid long test times.
/// Note: Excludes edge cases where buffer_size=0 or (data_size=0 and offset=0) because
/// these don't generate completion events on the polling backend.
#[test]
fn prop_test_read_arbitrary_data_and_offsets() {
  let mut runner = TestRunner::new(proptest::test_runner::Config {
    cases: 20, // Limit to 20 cases to avoid long test times
    ..Default::default()
  });

  runner
    .run(
      // Use 1..= to avoid 0-byte buffers/files which cause polling issues
      &(0..=4096usize, 0..=2048i64, 0..=2048usize, any::<u64>()),
      |props| {
        let (data_size, read_offset, buffer_size, seed) = props;
        prop_test_read_arbitrary_data_and_offsets_run(
          data_size,
          read_offset,
          buffer_size,
          seed,
        )
      },
    )
    .unwrap();
}

fn prop_test_read_arbitrary_data_and_offsets_run(
  data_size: usize,
  read_offset: i64,
  buffer_size: usize,
  seed: u64,
) -> Result<(), TestCaseError> {
  let mut lio = Lio::new(64)
    .map_err(|e| TestCaseError::fail(format!("Failed to create Lio: {}", e)))?;

  // Generate deterministic random data based on seed
  let test_data: Vec<u8> = (0..data_size)
    .map(|i| ((seed.wrapping_add(i as u64)) % 256) as u8)
    .collect();

  // Create unique test file path
  let path = common::make_temp_path("read", seed);

  // Write test data to file
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    if fd < 0 {
      return Err(TestCaseError::fail(
        "Failed to create test file".to_string(),
      ));
    }

    let written = libc::write(
      fd,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
    if written as usize != test_data.len() {
      libc::close(fd);
      libc::unlink(path.as_ptr());
      return Err(TestCaseError::fail("Failed to write all data".to_string()));
    }
    fd
  };

  // Perform the read operation
  let resource = unsafe { Resource::from_raw_fd(fd) };
  let buf = vec![0u8; buffer_size];
  let mut receiver =
    api::read_at(&resource, buf, read_offset).with_lio(&mut lio).send();

  // Poll until done - use same pattern as write proptest
  let (result, result_buf) = {
    let mut attempts = 0;
    loop {
      lio.try_run().map_err(|e| {
        TestCaseError::fail(format!("Lio try_run failed: {}", e))
      })?;
      match receiver.try_recv() {
        Some(result) => break result,
        None => {
          attempts += 1;
          if attempts > 100 {
            unsafe {
              libc::unlink(path.as_ptr());
            }
            return Err(TestCaseError::fail(
              "Read operation did not complete after 100 ticks".to_string(),
            ));
          }
          std::thread::sleep(Duration::from_micros(100));
        }
      }
    }
  };

  let test_result = (|| -> Result<(), TestCaseError> {
    let bytes_read = result.map_err(|e| {
      TestCaseError::fail(format!("Read operation failed: {}", e))
    })? as usize;

    // Calculate expected bytes to read
    let file_size = test_data.len();
    let offset = read_offset as usize;

    if offset >= file_size {
      // Reading beyond EOF should return 0 bytes
      if bytes_read != 0 {
        return Err(TestCaseError::fail(format!(
          "Reading beyond EOF should return 0 bytes, got {}",
          bytes_read
        )));
      }
    } else {
      // Calculate how many bytes we expect to read
      let available_bytes = file_size - offset;
      let expected_bytes = std::cmp::min(buffer_size, available_bytes);

      if bytes_read != expected_bytes {
        return Err(TestCaseError::fail(format!(
          "Read should return min(buffer_size={}, available_bytes={})={}, got {}",
          buffer_size, available_bytes, expected_bytes, bytes_read
        )));
      }

      // Verify the data matches what we wrote
      let expected_data = &test_data[offset..offset + bytes_read];
      if &result_buf[..bytes_read] != expected_data {
        return Err(TestCaseError::fail(
          "Read data should match written data at offset".to_string(),
        ));
      }
    }
    Ok(())
  })();

  // Cleanup
  drop(resource);
  unsafe {
    libc::unlink(path.as_ptr());
  }

  test_result
}
