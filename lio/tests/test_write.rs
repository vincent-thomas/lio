/// write in append mode is not tested since `pwrite` doesn't support it.
mod common;

use lio::write;
use proptest::{prelude::*, test_runner::TestRunner};
use std::{ffi::CString, sync::mpsc, thread, time::Duration};

#[test]
fn test_write_large_buffer() {
  lio::init();

  let path = CString::new("/tmp/lio_test_write_large.txt").unwrap();

  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    )
  };

  // Write large buffer (1MB)
  let large_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();

  let (sender, receiver) = mpsc::channel();
  let sender1 = sender.clone();
  let large_data_clone = large_data.clone();

  write(fd, large_data.clone(), 0).when_done(move |res| {
    sender1.send(res).unwrap();
  });

  // assert_eq!(
  //   receiver.try_recv().map(|nice| nice.0).unwrap_err(),
  //   TryRecvError::Empty
  // );

  // Poll until the write completes (may take multiple ticks on some backends)
  let (bytes_written, returned_buf) = {
    let mut attempts = 0;
    loop {
      lio::tick();
      match receiver.try_recv() {
        Ok(result) => break result,
        Err(_) => {
          attempts += 1;
          if attempts > 100 {
            panic!("Write operation did not complete after 100 ticks");
          }
          thread::sleep(Duration::from_micros(100));
        }
      }
    }
  };
  let bytes_written =
    bytes_written.expect("Failed to write large buffer") as usize;

  assert_eq!(bytes_written, large_data_clone.len());
  assert_eq!(returned_buf, large_data_clone);

  // Verify file size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::fstat(fd, &mut stat);
    assert_eq!(stat.st_size as usize, large_data_clone.len());
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }
}

#[test]
fn test_write_concurrent() {
  lio::init();

  // Test multiple concurrent write operations on different files
  for i in 0..10 {
    let path =
      CString::new(format!("/tmp/lio_test_write_concurrent_{}.txt", i))
        .unwrap();
    let data = format!("Task {}", i).into_bytes();

    let fd = unsafe {
      libc::open(
        path.as_ptr(),
        libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
        0o644,
      )
    };

    let (sender, receiver) = mpsc::channel();
    let sender1 = sender.clone();
    let data_clone = data.clone();

    write(fd, data.clone(), 0).when_done(move |res| {
      sender1.send(res).unwrap();
    });

    // assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);

    // Poll until the write completes (may take multiple ticks on some backends)
    let (bytes_written, returned_buf) = {
      let mut attempts = 0;
      loop {
        lio::tick();
        match receiver.try_recv() {
          Ok(result) => break result,
          Err(_) => {
            attempts += 1;
            if attempts > 100 {
              panic!("Write operation did not complete after 100 ticks");
            }
            thread::sleep(Duration::from_micros(100));
          }
        }
      }
    };
    let bytes_written = bytes_written.expect("Failed to write") as usize;

    assert_eq!(bytes_written, data_clone.len());
    assert_eq!(returned_buf, data_clone);

    unsafe {
      libc::close(fd);
      libc::unlink(path.as_ptr());
    }
  }
}

#[test]
fn prop_test_write_arbitrary_data_and_offsets() {
  lio::init();
  let mut runner = TestRunner::new(ProptestConfig::default());

  runner
    .run(&(0usize..=8192, 0i64..=4096, any::<u64>()), |props| {
      prop_test_write_arbitrary_data_and_offsets_run(props.0, props.1, props.2)
    })
    .unwrap();
}

// proptest! {
fn prop_test_write_arbitrary_data_and_offsets_run(
  data_size: usize,
  write_offset: i64,
  seed: u64,
) -> Result<(), TestCaseError> {
  // Generate deterministic random data based on seed
  let test_data: Vec<u8> = (0..data_size)
    .map(|i| ((seed.wrapping_add(i as u64)) % 256) as u8)
    .collect();

  // Create unique test file path
  let path = common::make_temp_path("write", seed);

  // Create file for writing
  let fd = unsafe {
    libc::open(
      path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    )
  };

  if fd < 0 {
    return Err(TestCaseError::fail("Failed to create test file".to_string()));
  }

  // If writing at an offset, we need to create a file with enough space
  // Fill the file with zeros up to write_offset + data_size
  if write_offset > 0 {
    let zeros = vec![0u8; write_offset as usize];
    unsafe {
      let written =
        libc::write(fd, zeros.as_ptr() as *const libc::c_void, zeros.len());
      if written < 0 || written as usize != zeros.len() {
        return Err(TestCaseError::fail(
          "Failed to write initial zeros".to_string(),
        ));
      }
    }
  }

  // Perform the write operation with channel pattern
  let test_data_clone = test_data.clone();

  let mut receiver = write(fd, test_data.clone(), write_offset).send();

  // Poll until the write completes (may take multiple ticks on some backends)
  let (write_result, returned_buf) = {
    let mut attempts = 0;
    loop {
      lio::tick();
      match receiver.try_recv() {
        Some(result) => break result,
        None => {
          attempts += 1;
          if attempts > 100 {
            return Err(TestCaseError::fail(
              "Write operation did not complete after 100 ticks".to_string(),
            ));
          }
          thread::sleep(Duration::from_micros(100));
        }
      }
    }
  };

  let bytes_written = write_result.map_err(|e| {
    TestCaseError::fail(format!("Write operation failed: {}", e))
  })?;

  // Verify bytes written
  if bytes_written as usize != test_data_clone.len() {
    return Err(TestCaseError::fail(format!(
      "Write should return data_size={}, got {}",
      test_data_clone.len(),
      bytes_written
    )));
  }

  // Verify returned buffer matches original
  if returned_buf != test_data_clone {
    return Err(TestCaseError::fail(
      "Returned buffer should match original data".to_string(),
    ));
  }

  // Read back and verify the data was written correctly
  let mut read_buf = vec![0u8; test_data_clone.len()];
  unsafe {
    let read_bytes = libc::pread(
      fd,
      read_buf.as_mut_ptr() as *mut libc::c_void,
      test_data_clone.len(),
      write_offset,
    );

    if read_bytes < 0 {
      return Err(TestCaseError::fail("Failed to read back data".to_string()));
    }

    if read_bytes as usize != test_data_clone.len() {
      return Err(TestCaseError::fail(format!(
        "Read back {} bytes, expected {}",
        read_bytes,
        test_data_clone.len()
      )));
    }

    if read_buf != test_data_clone {
      return Err(TestCaseError::fail(
        "Read data does not match written data".to_string(),
      ));
    }
  }

  // Cleanup
  unsafe {
    libc::close(fd);
    libc::unlink(path.as_ptr());
  }

  Ok(())
}
