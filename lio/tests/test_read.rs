#![cfg(feature = "buf")]

mod common;

use lio::read;
use proptest::{prelude::*, test_runner::TestRunner};
use std::{ffi::CString, sync::mpsc};

#[test]
fn test_read_large_buffer() {
  lio::init();

  let path = CString::new("/tmp/lio_test_read_large.txt").unwrap();

  // Create large data (1MB)
  let large_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
  let fd = unsafe {
    let fd = libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    );
    libc::write(
      fd,
      large_data.as_ptr() as *const libc::c_void,
      large_data.len(),
    );
    fd
  };

  // Read it back
  let buf = vec![0u8; 1024 * 1024];
  let mut recv_read = lio::read_with_buf(fd, buf, 0).send();

  lio::tick();

  let (bytes_read_result, result) =
    recv_read.try_recv().expect("recv didn't finish at time of call");
  let bytes_read =
    bytes_read_result.expect("Failed to read large buffer") as usize;

  assert_eq!(bytes_read, large_data.len());
  assert_eq!(&result[..bytes_read], large_data.as_slice());

  // Cleanup
  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_read_concurrent() {
  lio::init();

  // Test multiple concurrent read operations on different files
  let test_data: Vec<_> = (0..10)
    .map(|i| {
      let path =
        CString::new(format!("/tmp/lio_test_read_concurrent_{}.txt", i))
          .unwrap();
      let data = format!("Data for file {}", i);

      let fd = unsafe {
        let fd = libc::open(
          path.as_ptr(),
          libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
          0o644,
        );
        libc::write(fd, data.as_ptr() as *const libc::c_void, data.len());
        fd
      };

      (fd, path, data)
    })
    .collect();

  let (sender, receiver) = mpsc::channel();

  for (fd, _, data) in &test_data {
    let buf = vec![0u8; 100];
    lio::read_with_buf(*fd, buf, 0).send_with(sender.clone());
  }

  lio::tick();

  for _ in 0..10 {
    let (bytes_read_result, expected_data) = receiver.try_recv().unwrap();
    let bytes_read = bytes_read_result.expect("Failed to read") as usize;

    assert_eq!(bytes_read, expected_data.len());
  }

  for (fd, path, _) in test_data {
    unsafe {
      libc::close(fd);
      libc::unlink(path.as_ptr());
    }
  }
}

#[test]
fn prop_test_read_arbitrary_data_and_offsets() {
  let mut runner = TestRunner::new(proptest::test_runner::Config::default());

  lio::init();

  let result = runner
    .run(&(0..=8192usize, 0..=4096i64, 0..=4096usize, any::<u64>()), |props| {
      let (_0, _1, _2, _3) = props;
      prop_test_read_arbitrary_data_and_offsets_run(_0, _1, _2, _3)
    })
    .unwrap();
}

fn prop_test_read_arbitrary_data_and_offsets_run(
  data_size: usize,   // in 0usize..=8192,
  read_offset: i64,   // in 0i64..=4096,
  buffer_size: usize, // in 0usize..=4096,
  seed: u64,          // in any::<u64>(),
) -> Result<(), TestCaseError> {
  // lio::init();

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
    assert!(fd >= 0, "Failed to create test file");

    let written = libc::write(
      fd,
      test_data.as_ptr() as *const libc::c_void,
      test_data.len(),
    );
    assert_eq!(written as usize, test_data.len(), "Failed to write all data");
    fd
  };

  // Perform the read operation
  let buf = vec![0u8; buffer_size];
  let mut receiver = lio::read_with_buf(fd, buf, read_offset).send();

  lio::tick();

  let (bytes_read_result, result_buf) =
    receiver.try_recv().expect("read wasn't ready");

  let test_result = (|| -> Result<(), TestCaseError> {
    let bytes_read = bytes_read_result.map_err(|e| {
      TestCaseError::fail(format!("Read operation failed unexpectedly: {}", e))
    })?;
    let bytes_read = bytes_read as usize;

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
  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }

  test_result?;

  Ok(())
}
