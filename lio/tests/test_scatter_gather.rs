//! Tests for scatter-gather readv and writev operations.

mod common;

use common::{TempFile, poll_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::os::fd::FromRawFd;

/// Write known data to a file using the OS directly, then read it back
/// with `readv` into multiple buffers.
#[test]
fn test_readv_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_basic");

  let test_data = b"Hello, World! Scatter-gather!";
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

  // Two buffers: first gets 14 bytes, second gets the rest.
  let bufs = vec![vec![0u8; 14], vec![0u8; 32]];
  let mut recv = api::readv(&fd, bufs).with_lio(&mut lio).send();

  let (result, bufs) = poll_recv(&mut lio, &mut recv);
  let total = result.expect("readv failed") as usize;

  assert_eq!(total, test_data.len());
  let combined: Vec<u8> = bufs.into_iter().flatten().collect();
  assert_eq!(&combined[..total], test_data);
}

/// Write data with `writev` from multiple source buffers, then verify
/// the file contains the expected concatenated content.
#[test]
fn test_writev_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_basic");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    ))
  };

  let bufs = vec![b"Hello, ".to_vec(), b"world!\n".to_vec()];
  let expected: Vec<u8> = bufs.iter().flat_map(|b| b.iter().copied()).collect();
  let expected_len = expected.len();

  let mut recv = api::writev(&fd, bufs).with_lio(&mut lio).send();

  let (result, _bufs) = poll_recv(&mut lio, &mut recv);
  let total = result.expect("writev failed") as usize;

  assert_eq!(total, expected_len);

  // Verify the written data
  let mut written = vec![0u8; expected_len];
  unsafe {
    let fd2 =
      libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    libc::read(fd2, written.as_mut_ptr() as *mut libc::c_void, expected_len);
    libc::close(fd2);
  }
  assert_eq!(written, expected);
}

/// Round-trip test: write multiple buffers with writev, read them back
/// with readv, and verify they match.
#[test]
fn test_readv_writev_roundtrip() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_writev_roundtrip");

  // Write: three distinct segments
  let write_bufs = vec![
    b"segment_one__".to_vec(),
    b"segment_two__".to_vec(),
    b"segment_three".to_vec(),
  ];
  let full_data: Vec<u8> =
    write_bufs.iter().flat_map(|b| b.iter().copied()).collect();

  // Open for writing
  let fd_w = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    ))
  };

  let mut recv_w =
    api::writev(&fd_w, write_bufs.clone()).with_lio(&mut lio).send();
  let (result_w, _) = poll_recv(&mut lio, &mut recv_w);
  result_w.expect("writev round-trip write failed");

  // Open for reading
  let fd_r = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read back into three buffers of same sizes
  let read_bufs = write_bufs.iter().map(|b| vec![0u8; b.len()]).collect();
  let mut recv_r = api::readv(&fd_r, read_bufs).with_lio(&mut lio).send();
  let (result_r, read_bufs) = poll_recv(&mut lio, &mut recv_r);
  let total = result_r.expect("readv round-trip read failed") as usize;

  assert_eq!(total, full_data.len());
  let combined: Vec<u8> = read_bufs.into_iter().flatten().collect();
  assert_eq!(&combined[..total], full_data.as_slice());
}

/// Ensure that buffers beyond the last filled one have length 0.
/// This covers the case where the file is smaller than the combined capacity.
#[test]
fn test_readv_partial_fill_clears_unused_buffers() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_partial_fill");

  // Write only 10 bytes
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

  // Three buffers: first gets 8 bytes, second gets 2 bytes, third gets nothing
  let bufs = vec![vec![0u8; 8], vec![0u8; 16], vec![0u8; 16]];
  let mut recv = api::readv(&fd, bufs).with_lio(&mut lio).send();
  let (result, bufs) = poll_recv(&mut lio, &mut recv);
  let total = result.expect("readv partial fill failed") as usize;

  assert_eq!(total, test_data.len());
  assert_eq!(bufs[0], b"01234567");
  assert_eq!(bufs[1], b"89");
  // Third buffer received nothing — its length must be 0.
  assert_eq!(bufs[2].len(), 0);
}

/// Verify that readv with a single buffer matches a plain read.
#[test]
fn test_readv_single_buffer() {  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_single");

  let test_data = b"single buffer test data";
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

  let bufs = vec![vec![0u8; 64]];
  let mut recv = api::readv(&fd, bufs).with_lio(&mut lio).send();
  let (result, bufs) = poll_recv(&mut lio, &mut recv);
  let n = result.expect("readv single buffer failed") as usize;

  assert_eq!(n, test_data.len());
  assert_eq!(&bufs[0][..n], test_data);
}

/// Verify that writev with a single buffer behaves like a plain write.
#[test]
fn test_writev_single_buffer() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_single");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
      0o644,
    ))
  };

  let data = b"single buffer write test";
  let bufs = vec![data.to_vec()];
  let mut recv = api::writev(&fd, bufs).with_lio(&mut lio).send();
  let (result, _) = poll_recv(&mut lio, &mut recv);
  let n = result.expect("writev single buffer failed") as usize;
  assert_eq!(n, data.len());

  // Verify written content
  let mut out = vec![0u8; data.len()];
  unsafe {
    let fd2 = libc::open(temp.path.as_ptr(), libc::O_RDONLY);
    libc::read(fd2, out.as_mut_ptr() as *mut libc::c_void, out.len());
    libc::close(fd2);
  }
  assert_eq!(out, data);
}
