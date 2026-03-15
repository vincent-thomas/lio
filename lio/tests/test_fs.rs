//! Tests for the high-level fs module.

mod common;

use common::{poll_recv, poll_until_recv, TempFile};
use lio::fs::{File, OpenOptions};
use lio::Lio;
use std::sync::mpsc;

// ============================================================================
// File::open tests
// ============================================================================

#[test]
fn test_file_open_existing() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_open");

  // Create file first using libc
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    assert!(fd >= 0, "Failed to create test file");
    libc::write(fd, b"test data".as_ptr() as *const _, 9);
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();

  let mut recv = File::open(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to open file");

  // Verify we can read from it
  let buffer = vec![0u8; 100];
  let (sender, receiver) = mpsc::channel();
  file.read(buffer).with_lio(&mut lio).send_with(sender);
  let (result, buffer) = poll_until_recv(&mut lio, &receiver);
  let bytes_read = result.expect("read should succeed") as usize;

  assert_eq!(bytes_read, 9);
  assert_eq!(&buffer[..bytes_read], b"test data");
}

#[test]
fn test_file_open_nonexistent() {
  let mut lio = Lio::new(64).unwrap();

  let mut recv = File::open("/tmp/nonexistent_lio_test_12345.txt")
    .with_lio(&mut lio)
    .send();
  let result = poll_recv(&mut lio, &mut recv);

  assert!(result.is_err(), "Opening nonexistent file should fail");
  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
}

// ============================================================================
// File::create tests
// ============================================================================

#[test]
fn test_file_create() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_create");
  let path = temp.path.to_str().unwrap();

  let mut recv = File::create(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to create file");

  // Write some data
  let data = b"Hello, World!".to_vec();
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _data) = poll_until_recv(&mut lio, &receiver);
  let bytes_written = result.expect("write should succeed");
  assert_eq!(bytes_written, 13);

  // Verify file contents using libc
  unsafe {
    let fd = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    assert!(fd >= 0, "Failed to open file for verification");
    let mut buf = [0u8; 100];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 100);
    libc::close(fd);
    assert_eq!(n, 13);
    assert_eq!(&buf[..13], b"Hello, World!");
  }
}

#[test]
fn test_file_create_truncates_existing() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_create_trunc");

  // Create file with some data
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(
      fd,
      b"This is a long existing content".as_ptr() as *const _,
      31,
    );
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();

  // Create (should truncate)
  let mut recv = File::create(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to create file");

  // Write shorter data
  let data = b"Short".to_vec();
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _data) = poll_until_recv(&mut lio, &receiver);
  result.expect("write should succeed");

  // Verify file was truncated (only contains "Short")
  unsafe {
    let fd = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    let mut buf = [0u8; 100];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 100);
    libc::close(fd);
    assert_eq!(n, 5, "File should only contain 'Short'");
    assert_eq!(&buf[..5], b"Short");
  }
}

// ============================================================================
// File::create_new tests
// ============================================================================

#[test]
fn test_file_create_new_success() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_create_new");
  let path = temp.path.to_str().unwrap();

  // File shouldn't exist yet (TempFile just generates a path)
  let mut recv = File::create_new(path).with_lio(&mut lio).send();
  let _file = poll_recv(&mut lio, &mut recv).expect("create_new should succeed");
}

#[test]
fn test_file_create_new_fails_if_exists() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_create_new_exists");

  // Create the file first
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();

  // create_new should fail
  let mut recv = File::create_new(path).with_lio(&mut lio).send();
  let result = poll_recv(&mut lio, &mut recv);

  assert!(result.is_err(), "create_new should fail for existing file");
  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
}

// ============================================================================
// read_at / write_at tests
// ============================================================================

#[test]
fn test_file_read_at() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_read_at");

  // Create file with known content
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"0123456789ABCDEF".as_ptr() as *const _, 16);
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();
  let mut recv = File::open(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to open file");

  // Read from offset 5
  let buffer = vec![0u8; 5];
  let (sender, receiver) = mpsc::channel();
  file.read_at(buffer, 5).with_lio(&mut lio).send_with(sender);
  let (result, buffer) = poll_until_recv(&mut lio, &receiver);
  let bytes_read = result.expect("read_at should succeed") as usize;

  assert_eq!(bytes_read, 5);
  assert_eq!(&buffer[..bytes_read], b"56789");
}

#[test]
fn test_file_write_at() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_write_at");

  // Create file with initial content
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"AAAAAAAAAA".as_ptr() as *const _, 10);
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();

  // Open for read/write
  let mut recv = OpenOptions::new()
    .read(true)
    .write(true)
    .open(path)
    .with_lio(&mut lio)
    .send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to open file");

  // Write at offset 3
  let data = b"XXX".to_vec();
  let (sender, receiver) = mpsc::channel();
  file
    .write_at(data, 3)
    .with_lio(&mut lio)
    .send_with(sender);
  let (result, _data) = poll_until_recv(&mut lio, &receiver);
  result.expect("write_at should succeed");

  // Verify
  unsafe {
    let fd = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    let mut buf = [0u8; 20];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 20);
    libc::close(fd);
    assert_eq!(n, 10);
    assert_eq!(&buf[..10], b"AAAXXXAAAA");
  }
}

// ============================================================================
// sync_all tests
// ============================================================================

#[test]
fn test_file_sync_all() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_sync");
  let path = temp.path.to_str().unwrap();

  let mut recv = File::create(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to create file");

  // Write some data
  let data = b"data to sync".to_vec();
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _) = poll_until_recv(&mut lio, &receiver);
  result.expect("write should succeed");

  // Sync
  let (sender, receiver) = mpsc::channel();
  file.sync_all().with_lio(&mut lio).send_with(sender);
  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("sync_all should succeed");
}

// ============================================================================
// set_len (truncate) tests
// ============================================================================

#[test]
fn test_file_set_len_truncate() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_truncate");
  let path = temp.path.to_str().unwrap();

  let mut recv = File::create(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to create file");

  // Write 100 bytes
  let data = vec![b'X'; 100];
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _) = poll_until_recv(&mut lio, &receiver);
  result.expect("write should succeed");

  // Truncate to 50 bytes
  let (sender, receiver) = mpsc::channel();
  file.set_len(50).with_lio(&mut lio).send_with(sender);
  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("set_len should succeed");

  // Verify size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::stat(temp.path.as_ptr(), &mut stat);
    assert_eq!(stat.st_size, 50);
  }
}

#[test]
fn test_file_set_len_extend() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_extend");
  let path = temp.path.to_str().unwrap();

  let mut recv = File::create(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to create file");

  // Write 10 bytes
  let data = vec![b'X'; 10];
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _) = poll_until_recv(&mut lio, &receiver);
  result.expect("write should succeed");

  // Extend to 100 bytes
  let (sender, receiver) = mpsc::channel();
  file.set_len(100).with_lio(&mut lio).send_with(sender);
  let result = poll_until_recv(&mut lio, &receiver);
  result.expect("set_len should succeed");

  // Verify size
  unsafe {
    let mut stat: libc::stat = std::mem::zeroed();
    libc::stat(temp.path.as_ptr(), &mut stat);
    assert_eq!(stat.st_size, 100);
  }
}

// ============================================================================
// metadata tests
// ============================================================================

#[test]
fn test_file_metadata() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_metadata");

  // Create file with known size
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"12345".as_ptr() as *const _, 5);
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();
  let mut recv = File::open(path).with_lio(&mut lio).send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to open file");

  let metadata = file.metadata().expect("metadata should succeed");
  assert_eq!(metadata.len(), 5);
  assert!(metadata.is_file());
  assert!(!metadata.is_dir());
  assert!(!metadata.is_symlink());
}

// ============================================================================
// OpenOptions tests
// ============================================================================

#[test]
fn test_open_options_append() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_append");

  // Create file with initial content
  unsafe {
    let fd = libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    );
    libc::write(fd, b"Hello".as_ptr() as *const _, 5);
    libc::close(fd);
  }

  let path = temp.path.to_str().unwrap();

  // Open in append mode
  let mut recv = OpenOptions::new()
    .append(true)
    .open(path)
    .with_lio(&mut lio)
    .send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to open file");

  // Write more data (should be appended)
  let data = b" World".to_vec();
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _) = poll_until_recv(&mut lio, &receiver);
  result.expect("write should succeed");

  // Verify
  unsafe {
    let fd = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    let mut buf = [0u8; 20];
    let n = libc::read(fd, buf.as_mut_ptr() as *mut _, 20);
    libc::close(fd);
    assert_eq!(n, 11);
    assert_eq!(&buf[..11], b"Hello World");
  }
}

#[test]
fn test_open_options_no_access_mode_fails() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_no_mode");
  let path = temp.path.to_str().unwrap();

  // Neither read, write, nor append set - should fail
  let mut recv = OpenOptions::new().open(path).with_lio(&mut lio).send();
  let result = poll_recv(&mut lio, &mut recv);

  assert!(result.is_err());
  let err = result.unwrap_err();
  assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn test_open_options_read_write() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("fs_rw");
  let path = temp.path.to_str().unwrap();

  // Create file
  let mut recv = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .open(path)
    .with_lio(&mut lio)
    .send();
  let file = poll_recv(&mut lio, &mut recv).expect("Failed to open file");

  // Write
  let data = b"test".to_vec();
  let (sender, receiver) = mpsc::channel();
  file.write(data).with_lio(&mut lio).send_with(sender);
  let (result, _) = poll_until_recv(&mut lio, &receiver);
  result.expect("write should succeed");

  // Read back (need to re-open to reset cursor, or use read_at)
  let buffer = vec![0u8; 10];
  let (sender, receiver) = mpsc::channel();
  file.read_at(buffer, 0).with_lio(&mut lio).send_with(sender);
  let (result, buffer) = poll_until_recv(&mut lio, &receiver);
  let n = result.expect("read_at should succeed") as usize;

  assert_eq!(n, 4);
  assert_eq!(&buffer[..n], b"test");
}
