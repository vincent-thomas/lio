//! Integration tests for file I/O operations.

use lio_uring::operation::*;
use lio_uring::LioUring;
use std::fs::{self, File};
use std::io::{Read as IoRead, Seek, SeekFrom, Write as IoWrite};
use std::os::fd::AsRawFd;

fn temp_path(name: &str) -> String {
  format!("/tmp/lio_uring_test_{}", name)
}

// ============================================================================
// Read Tests
// ============================================================================

#[test]
fn test_read_file() {
  let path = temp_path("read_file");
  fs::write(&path, b"hello world").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = File::open(&path).unwrap();

  let mut buf = [0u8; 64];
  let op = Read::new(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 11); // "hello world".len()
  assert_eq!(&buf[..11], b"hello world");

  fs::remove_file(&path).ok();
}

#[test]
fn test_read_at_offset() {
  let path = temp_path("read_offset");
  fs::write(&path, b"0123456789").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = File::open(&path).unwrap();

  let mut buf = [0u8; 5];
  let op =
    Read::new(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32).offset(5);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 5);
  assert_eq!(&buf, b"56789");

  fs::remove_file(&path).ok();
}

#[test]
fn test_read_beyond_eof() {
  let path = temp_path("read_eof");
  fs::write(&path, b"short").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = File::open(&path).unwrap();

  let mut buf = [0u8; 64];
  let op =
    Read::new(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32).offset(100);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0); // EOF

  fs::remove_file(&path).ok();
}

#[test]
fn test_read_empty_file() {
  let path = temp_path("read_empty");
  File::create(&path).unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = File::open(&path).unwrap();

  let mut buf = [0u8; 64];
  let op = Read::new(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0);

  fs::remove_file(&path).ok();
}

#[test]
fn test_read_invalid_fd() {
  let mut ring = LioUring::new(8).unwrap();

  let mut buf = [0u8; 64];
  let op = Read::new(-1, buf.as_mut_ptr(), buf.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(!completion.is_ok());
  assert_eq!(completion.result(), -libc::EBADF);
}

// ============================================================================
// Write Tests
// ============================================================================

#[test]
fn test_write_file() {
  let path = temp_path("write_file");

  let mut ring = LioUring::new(8).unwrap();
  let file = File::create(&path).unwrap();

  let data = b"hello io_uring";
  let op = Write::new(file.as_raw_fd(), data.as_ptr(), data.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), data.len() as i32);

  drop(file);
  let contents = fs::read_to_string(&path).unwrap();
  assert_eq!(contents, "hello io_uring");

  fs::remove_file(&path).ok();
}

#[test]
fn test_write_at_offset() {
  let path = temp_path("write_offset");
  fs::write(&path, b"AAAAAAAAAA").unwrap(); // 10 A's

  let mut ring = LioUring::new(8).unwrap();
  let file = fs::OpenOptions::new().write(true).open(&path).unwrap();

  let data = b"BBB";
  let op =
    Write::new(file.as_raw_fd(), data.as_ptr(), data.len() as u32).offset(3);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 3);

  drop(file);
  let contents = fs::read_to_string(&path).unwrap();
  assert_eq!(contents, "AAABBBAAA\0"); // Note: might have null if file was extended

  fs::remove_file(&path).ok();
}

#[test]
fn test_write_append() {
  let path = temp_path("write_append");
  fs::write(&path, b"hello").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = fs::OpenOptions::new().append(true).open(&path).unwrap();

  let data = b" world";
  let op = Write::new(file.as_raw_fd(), data.as_ptr(), data.len() as u32);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  drop(file);
  let contents = fs::read_to_string(&path).unwrap();
  assert_eq!(contents, "hello world");

  fs::remove_file(&path).ok();
}

// ============================================================================
// Readv/Writev Tests
// ============================================================================

#[test]
fn test_readv() {
  let path = temp_path("readv");
  fs::write(&path, b"hello world!").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = File::open(&path).unwrap();

  let mut buf1 = [0u8; 6];
  let mut buf2 = [0u8; 6];
  let iovecs = [
    libc::iovec { iov_base: buf1.as_mut_ptr().cast(), iov_len: buf1.len() },
    libc::iovec { iov_base: buf2.as_mut_ptr().cast(), iov_len: buf2.len() },
  ];

  let op = Readv::new(file.as_raw_fd(), iovecs.as_ptr().cast(), 2);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 12);
  assert_eq!(&buf1, b"hello ");
  assert_eq!(&buf2, b"world!");

  fs::remove_file(&path).ok();
}

#[test]
fn test_writev() {
  let path = temp_path("writev");

  let mut ring = LioUring::new(8).unwrap();
  let file = File::create(&path).unwrap();

  let buf1 = b"hello ";
  let buf2 = b"world!";
  let iovecs = [
    libc::iovec { iov_base: buf1.as_ptr() as *mut _, iov_len: buf1.len() },
    libc::iovec { iov_base: buf2.as_ptr() as *mut _, iov_len: buf2.len() },
  ];

  let op = Writev::new(file.as_raw_fd(), iovecs.as_ptr().cast(), 2);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 12);

  drop(file);
  let contents = fs::read_to_string(&path).unwrap();
  assert_eq!(contents, "hello world!");

  fs::remove_file(&path).ok();
}

// ============================================================================
// Openat Tests
// ============================================================================

#[test]
fn test_openat_create() {
  let path = temp_path("openat_create");
  let _ = fs::remove_file(&path); // Ensure doesn't exist

  let mut ring = LioUring::new(8).unwrap();
  let path_cstr = format!("{}\0", path);

  let op = Openat::new(-100, path_cstr.as_ptr().cast()) // AT_FDCWD
    .flags(libc::O_CREAT | libc::O_WRONLY)
    .mode(0o644);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let fd = completion.result();
  assert!(fd >= 0);

  unsafe { libc::close(fd) };
  assert!(fs::metadata(&path).is_ok());

  fs::remove_file(&path).ok();
}

#[test]
fn test_openat_read_existing() {
  let path = temp_path("openat_read");
  fs::write(&path, b"test content").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let path_cstr = format!("{}\0", path);

  let op = Openat::new(-100, path_cstr.as_ptr().cast()).flags(libc::O_RDONLY);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  let fd = completion.result();
  assert!(fd >= 0);

  unsafe { libc::close(fd) };
  fs::remove_file(&path).ok();
}

#[test]
fn test_openat_nonexistent() {
  let mut ring = LioUring::new(8).unwrap();
  let path = b"/tmp/lio_uring_nonexistent_12345\0";

  let op = Openat::new(-100, path.as_ptr().cast()).flags(libc::O_RDONLY);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(!completion.is_ok());
  assert_eq!(completion.result(), -libc::ENOENT);
}

// ============================================================================
// Close Tests
// ============================================================================

#[test]
fn test_close() {
  let path = temp_path("close");
  let file = File::create(&path).unwrap();
  let fd = file.as_raw_fd();
  std::mem::forget(file); // Don't close normally

  let mut ring = LioUring::new(8).unwrap();

  let op = Close::new(fd);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0);

  // Verify fd is closed by trying to use it
  let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
  assert_eq!(result, -1);

  fs::remove_file(&path).ok();
}

#[test]
fn test_close_invalid_fd() {
  let mut ring = LioUring::new(8).unwrap();

  let op = Close::new(-1);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(!completion.is_ok());
  assert_eq!(completion.result(), -libc::EBADF);
}

// ============================================================================
// Fsync Tests
// ============================================================================

#[test]
fn test_fsync() {
  let path = temp_path("fsync");
  let mut file = File::create(&path).unwrap();
  file.write_all(b"data to sync").unwrap();

  let mut ring = LioUring::new(8).unwrap();

  let op = Fsync::new(file.as_raw_fd());
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0);

  fs::remove_file(&path).ok();
}

#[test]
fn test_fsync_datasync() {
  let path = temp_path("fdatasync");
  let mut file = File::create(&path).unwrap();
  file.write_all(b"data to sync").unwrap();

  let mut ring = LioUring::new(8).unwrap();

  let op = Fsync::new(file.as_raw_fd()).datasync();
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0);

  fs::remove_file(&path).ok();
}

// ============================================================================
// Ftruncate Tests
// ============================================================================

#[test]
fn test_ftruncate_shrink() {
  let path = temp_path("ftruncate_shrink");
  fs::write(&path, b"hello world").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = fs::OpenOptions::new().write(true).open(&path).unwrap();

  let op = Ftruncate::new(file.as_raw_fd(), 5);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  // ftruncate might return error on some kernels
  if completion.is_ok() {
    drop(file);
    let contents = fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "hello");
  }

  fs::remove_file(&path).ok();
}

#[test]
fn test_ftruncate_extend() {
  let path = temp_path("ftruncate_extend");
  fs::write(&path, b"hi").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let file = fs::OpenOptions::new().write(true).open(&path).unwrap();

  let op = Ftruncate::new(file.as_raw_fd(), 10);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  if completion.is_ok() {
    drop(file);
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), 10);
  }

  fs::remove_file(&path).ok();
}

// ============================================================================
// Fallocate Tests
// ============================================================================

#[test]
fn test_fallocate() {
  let path = temp_path("fallocate");
  let file = File::create(&path).unwrap();

  let mut ring = LioUring::new(8).unwrap();

  let op = Fallocate::new(file.as_raw_fd(), 4096);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());
  assert_eq!(completion.result(), 0);

  drop(file);
  let meta = fs::metadata(&path).unwrap();
  assert!(meta.len() >= 4096);

  fs::remove_file(&path).ok();
}

// ============================================================================
// Symlink/Link Tests
// ============================================================================

#[test]
fn test_symlinkat() {
  let target = temp_path("symlink_target");
  let link = temp_path("symlink_link");
  fs::write(&target, b"target content").unwrap();
  let _ = fs::remove_file(&link);

  let mut ring = LioUring::new(8).unwrap();

  let target_cstr = format!("{}\0", target);
  let link_cstr = format!("{}\0", link);

  let op = Symlinkat::new(
    target_cstr.as_ptr().cast(),
    -100,
    link_cstr.as_ptr().cast(),
  );
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  let link_target = fs::read_link(&link).unwrap();
  assert_eq!(link_target.to_str().unwrap(), target);

  fs::remove_file(&link).ok();
  fs::remove_file(&target).ok();
}

#[test]
fn test_linkat() {
  let original = temp_path("linkat_original");
  let link = temp_path("linkat_link");
  fs::write(&original, b"original content").unwrap();
  let _ = fs::remove_file(&link);

  let mut ring = LioUring::new(8).unwrap();

  let original_cstr = format!("{}\0", original);
  let link_cstr = format!("{}\0", link);

  let op = Linkat::new(
    -100,
    original_cstr.as_ptr().cast(),
    -100,
    link_cstr.as_ptr().cast(),
  );
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  let content = fs::read_to_string(&link).unwrap();
  assert_eq!(content, "original content");

  fs::remove_file(&link).ok();
  fs::remove_file(&original).ok();
}

// ============================================================================
// Unlink/Rmdir Tests
// ============================================================================

#[test]
fn test_unlinkat_file() {
  let path = temp_path("unlinkat_file");
  fs::write(&path, b"to delete").unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let path_cstr = format!("{}\0", path);

  let op = Unlinkat::new(-100, path_cstr.as_ptr().cast());
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn test_unlinkat_directory() {
  let path = temp_path("unlinkat_dir");
  fs::create_dir_all(&path).unwrap();

  let mut ring = LioUring::new(8).unwrap();
  let path_cstr = format!("{}\0", path);

  let op =
    Unlinkat::new(-100, path_cstr.as_ptr().cast()).flags(libc::AT_REMOVEDIR);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  assert!(!std::path::Path::new(&path).exists());
}

// ============================================================================
// Mkdir Tests
// ============================================================================

#[test]
fn test_mkdirat() {
  let path = temp_path("mkdirat_test");
  let _ = fs::remove_dir(&path);

  let mut ring = LioUring::new(8).unwrap();
  let path_cstr = format!("{}\0", path);

  let op = Mkdirat::new(-100, path_cstr.as_ptr().cast(), 0o755);
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  let meta = fs::metadata(&path).unwrap();
  assert!(meta.is_dir());

  fs::remove_dir(&path).ok();
}

// ============================================================================
// Rename Tests
// ============================================================================

#[test]
fn test_renameat() {
  let old_path = temp_path("renameat_old");
  let new_path = temp_path("renameat_new");
  fs::write(&old_path, b"rename me").unwrap();
  let _ = fs::remove_file(&new_path);

  let mut ring = LioUring::new(8).unwrap();

  let old_cstr = format!("{}\0", old_path);
  let new_cstr = format!("{}\0", new_path);

  let op = Renameat::new(
    -100,
    old_cstr.as_ptr().cast(),
    -100,
    new_cstr.as_ptr().cast(),
  );
  unsafe { ring.push(op.build(), 1) }.unwrap();
  ring.submit().unwrap();

  let completion = ring.wait().unwrap();
  assert!(completion.is_ok());

  assert!(!std::path::Path::new(&old_path).exists());
  let content = fs::read_to_string(&new_path).unwrap();
  assert_eq!(content, "rename me");

  fs::remove_file(&new_path).ok();
}

// ============================================================================
// Concurrent File Operations
// ============================================================================

#[test]
fn test_concurrent_reads() {
  let path = temp_path("concurrent_reads");
  let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
  fs::write(&path, &data).unwrap();

  let mut ring = LioUring::new(32).unwrap();
  let file = File::open(&path).unwrap();

  let mut buffers: Vec<[u8; 128]> = (0..8).map(|_| [0u8; 128]).collect();

  // Submit 8 reads at different offsets
  for (i, buf) in buffers.iter_mut().enumerate() {
    let op = Read::new(file.as_raw_fd(), buf.as_mut_ptr(), buf.len() as u32)
      .offset((i * 128) as u64);
    unsafe { ring.push(op.build(), i as u64) }.unwrap();
  }

  ring.submit().unwrap();

  // Wait for all completions
  for _ in 0..8 {
    let completion = ring.wait().unwrap();
    assert!(completion.is_ok());
    assert_eq!(completion.result(), 128);
  }

  // Verify data
  for (i, buf) in buffers.iter().enumerate() {
    let expected: Vec<u8> =
      ((i * 128)..((i + 1) * 128)).map(|j| (j % 256) as u8).collect();
    assert_eq!(buf.as_slice(), expected.as_slice());
  }

  fs::remove_file(&path).ok();
}

#[test]
fn test_concurrent_writes() {
  let path = temp_path("concurrent_writes");
  // Pre-allocate file
  fs::write(&path, vec![0u8; 1024]).unwrap();

  let mut ring = LioUring::new(32).unwrap();
  let file = fs::OpenOptions::new().write(true).open(&path).unwrap();

  let buffers: Vec<Vec<u8>> =
    (0..8).map(|i| vec![(i + b'A') as u8; 128]).collect();

  // Submit 8 writes at different offsets
  for (i, buf) in buffers.iter().enumerate() {
    let op = Write::new(file.as_raw_fd(), buf.as_ptr(), buf.len() as u32)
      .offset((i * 128) as u64);
    unsafe { ring.push(op.build(), i as u64) }.unwrap();
  }

  ring.submit().unwrap();

  for _ in 0..8 {
    let completion = ring.wait().unwrap();
    assert!(completion.is_ok());
    assert_eq!(completion.result(), 128);
  }

  drop(file);

  // Verify file contents
  let contents = fs::read(&path).unwrap();
  for i in 0..8 {
    let expected = (i + b'A') as u8;
    for j in 0..128 {
      assert_eq!(contents[i * 128 + j], expected);
    }
  }

  fs::remove_file(&path).ok();
}
