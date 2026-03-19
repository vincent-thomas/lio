//! Tests for readv and writev (vectored I/O) operations.

mod common;

use common::{TempFile, poll_recv};
use lio::api::resource::Resource;
use lio::{Lio, api};
use std::os::fd::FromRawFd;

#[test]
fn test_writev_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_basic");

  // Create empty file
  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    ))
  };

  // Write multiple buffers
  let buf1 = b"Hello, ".to_vec();
  let buf2 = b"World!".to_vec();
  let mut recv = api::writev(&fd, (buf1, buf2)).with_lio(&mut lio).send();

  let (result, (buf1, buf2)) = poll_recv(&mut lio, &mut recv);
  let bytes_written = result.expect("Failed to writev") as usize;

  assert_eq!(bytes_written, 13);
  assert_eq!(buf1, b"Hello, ");
  assert_eq!(buf2, b"World!");

  // Verify the data was written correctly
  drop(fd);
  let written_data = std::fs::read(temp.path.to_str().unwrap()).unwrap();
  assert_eq!(written_data, b"Hello, World!");
}

#[test]
fn test_readv_basic() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_basic");

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
  let fd = unsafe {
    Resource::from_raw_fd(libc::open(temp.path.as_ptr(), libc::O_RDONLY))
  };

  // Read into multiple buffers
  let buf1 = vec![0u8; 7]; // "Hello, "
  let buf2 = vec![0u8; 6]; // "World!"
  let mut recv = api::readv(&fd, (buf1, buf2)).with_lio(&mut lio).send();

  let (result, (buf1, buf2)) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to readv") as usize;

  assert_eq!(bytes_read, 13);
  assert_eq!(&buf1[..], b"Hello, ");
  assert_eq!(&buf2[..], b"World!");
}

#[test]
fn test_writev_multiple_buffers() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_multi");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    ))
  };

  // Write with 4 buffers
  let buf1 = b"Part1-".to_vec();
  let buf2 = b"Part2-".to_vec();
  let buf3 = b"Part3-".to_vec();
  let buf4 = b"Part4".to_vec();
  let mut recv =
    api::writev(&fd, (buf1, buf2, buf3, buf4)).with_lio(&mut lio).send();

  let (result, _bufs) = poll_recv(&mut lio, &mut recv);
  let bytes_written = result.expect("Failed to writev") as usize;

  assert_eq!(bytes_written, 23);

  // Verify
  drop(fd);
  let written_data = std::fs::read(temp.path.to_str().unwrap()).unwrap();
  assert_eq!(written_data, b"Part1-Part2-Part3-Part4");
}

#[test]
fn test_readv_multiple_buffers() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_multi");

  // Create file
  let test_data = b"AAAAABBBBBCCCCCDDDDD";
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

  // Read into 4 buffers
  let buf1 = vec![0u8; 5];
  let buf2 = vec![0u8; 5];
  let buf3 = vec![0u8; 5];
  let buf4 = vec![0u8; 5];
  let mut recv =
    api::readv(&fd, (buf1, buf2, buf3, buf4)).with_lio(&mut lio).send();

  let (result, (buf1, buf2, buf3, buf4)) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to readv") as usize;

  assert_eq!(bytes_read, 20);
  assert_eq!(&buf1[..], b"AAAAA");
  assert_eq!(&buf2[..], b"BBBBB");
  assert_eq!(&buf3[..], b"CCCCC");
  assert_eq!(&buf4[..], b"DDDDD");
}

#[test]
fn test_writev_array() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_array");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    ))
  };

  // Write with array of buffers
  let bufs: [Vec<u8>; 3] =
    [b"One-".to_vec(), b"Two-".to_vec(), b"Three".to_vec()];
  let mut recv = api::writev(&fd, bufs).with_lio(&mut lio).send();

  let (result, _bufs) = poll_recv(&mut lio, &mut recv);
  let bytes_written = result.expect("Failed to writev") as usize;

  assert_eq!(bytes_written, 13);

  // Verify
  drop(fd);
  let written_data = std::fs::read(temp.path.to_str().unwrap()).unwrap();
  assert_eq!(written_data, b"One-Two-Three");
}

#[test]
fn test_readv_array() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_array");

  // Create file
  let test_data = b"123456789012";
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

  // Read into array of buffers
  let bufs: [Vec<u8>; 3] = [vec![0u8; 4], vec![0u8; 4], vec![0u8; 4]];
  let mut recv = api::readv(&fd, bufs).with_lio(&mut lio).send();

  let (result, bufs) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to readv") as usize;

  assert_eq!(bytes_read, 12);
  assert_eq!(&bufs[0][..], b"1234");
  assert_eq!(&bufs[1][..], b"5678");
  assert_eq!(&bufs[2][..], b"9012");
}

#[test]
fn test_readv_partial_fill() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_partial");

  // Create file with less data than buffer capacity
  let test_data = b"Short";
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

  // Read into buffers with more capacity than file size
  let buf1 = vec![0u8; 10];
  let buf2 = vec![0u8; 10];
  let mut recv = api::readv(&fd, (buf1, buf2)).with_lio(&mut lio).send();

  let (result, (buf1, buf2)) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to readv") as usize;

  assert_eq!(bytes_read, 5);
  // First buffer should have all 5 bytes
  assert_eq!(buf1.len(), 5);
  assert_eq!(&buf1[..], b"Short");
  // Second buffer should be empty
  assert_eq!(buf2.len(), 0);
}

#[test]
fn test_writev_large_buffers() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_large");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    ))
  };

  // Write large buffers (100KB each)
  let buf1: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
  let buf2: Vec<u8> = (0..100_000).map(|i| ((i + 128) % 256) as u8).collect();
  let expected_total = buf1.len() + buf2.len();

  let mut recv =
    api::writev(&fd, (buf1.clone(), buf2.clone())).with_lio(&mut lio).send();

  let (result, _bufs) = poll_recv(&mut lio, &mut recv);
  let bytes_written = result.expect("Failed to writev large") as usize;

  assert_eq!(bytes_written, expected_total);

  // Verify
  drop(fd);
  let written_data = std::fs::read(temp.path.to_str().unwrap()).unwrap();
  assert_eq!(&written_data[..100_000], buf1.as_slice());
  assert_eq!(&written_data[100_000..], buf2.as_slice());
}

#[test]
fn test_readv_large_buffers() {
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_large");

  // Create large file
  let data1: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
  let data2: Vec<u8> = (0..100_000).map(|i| ((i + 128) % 256) as u8).collect();
  let test_data: Vec<u8> = data1.iter().chain(data2.iter()).copied().collect();

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

  // Read into large buffers
  let buf1 = vec![0u8; 100_000];
  let buf2 = vec![0u8; 100_000];
  let mut recv = api::readv(&fd, (buf1, buf2)).with_lio(&mut lio).send();

  let (result, (buf1, buf2)) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to readv large") as usize;

  assert_eq!(bytes_read, 200_000);
  assert_eq!(buf1, data1);
  assert_eq!(buf2, data2);
}

#[test]
fn test_writev_http_response_pattern() {
  // Common use case: writing HTTP response with separate header and body
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_http");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    ))
  };

  let header = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n".to_vec();
  let body = b"Hello, World!".to_vec();
  let expected_total = header.len() + body.len();

  let mut recv = api::writev(&fd, (header, body)).with_lio(&mut lio).send();

  let (result, _bufs) = poll_recv(&mut lio, &mut recv);
  let bytes_written = result.expect("Failed to writev HTTP") as usize;

  assert_eq!(bytes_written, expected_total);

  // Verify
  drop(fd);
  let written_data = std::fs::read(temp.path.to_str().unwrap()).unwrap();
  assert_eq!(
    written_data,
    b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\nHello, World!"
  );
}

#[test]
fn test_writev_vec_dynamic() {
  // Test Vec<B> for dynamic buffer count
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("writev_vec");

  let fd = unsafe {
    Resource::from_raw_fd(libc::open(
      temp.path.as_ptr(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o644,
    ))
  };

  // Dynamic number of buffers
  let buffers: Vec<Vec<u8>> = vec![
    b"One-".to_vec(),
    b"Two-".to_vec(),
    b"Three-".to_vec(),
    b"Four-".to_vec(),
    b"Five".to_vec(),
  ];
  let expected_total: usize = buffers.iter().map(|b| b.len()).sum();

  let mut recv = api::writev(&fd, buffers).with_lio(&mut lio).send();

  let (result, _bufs) = poll_recv(&mut lio, &mut recv);
  let bytes_written = result.expect("Failed to writev Vec") as usize;

  assert_eq!(bytes_written, expected_total);

  // Verify
  drop(fd);
  let written_data = std::fs::read(temp.path.to_str().unwrap()).unwrap();
  assert_eq!(written_data, b"One-Two-Three-Four-Five");
}

#[test]
fn test_readv_vec_dynamic() {
  // Test Vec<B> for dynamic buffer count
  let mut lio = Lio::new(64).unwrap();
  let temp = TempFile::new("readv_vec");

  // Create file with data
  let test_data = b"AAABBBCCCDDD";
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

  // Dynamic number of buffers
  let buffers: Vec<Vec<u8>> =
    vec![vec![0u8; 3], vec![0u8; 3], vec![0u8; 3], vec![0u8; 3]];

  let mut recv = api::readv(&fd, buffers).with_lio(&mut lio).send();

  let (result, buffers) = poll_recv(&mut lio, &mut recv);
  let bytes_read = result.expect("Failed to readv Vec") as usize;

  assert_eq!(bytes_read, 12);
  assert_eq!(&buffers[0][..], b"AAA");
  assert_eq!(&buffers[1][..], b"BBB");
  assert_eq!(&buffers[2][..], b"CCC");
  assert_eq!(&buffers[3][..], b"DDD");
}
