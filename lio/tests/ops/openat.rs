#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

#[path = "../common.rs"]
mod common;

use lio::{
  Lio,
  api::{self, resource::Resource},
  backend::ds::{DSBackend, DSConfig},
};
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd};

fn new_ds_lio() -> Lio {
  Lio::new_with_backend(
    DSBackend::with_config(DSConfig { fault_every: 0, ..DSConfig::default() }),
    64,
  )
  .unwrap()
}

fn open_file(
  lio: &mut Lio,
  dir: &impl lio::api::resource::AsResource,
  path: CString,
  flags: i32,
  mode: u32,
) -> Result<Resource, std::io::Error> {
  let mut receiver = api::openat(dir, path, flags, mode).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver)
}

fn mkdir(lio: &mut Lio, path: CString) {
  let mut receiver =
    api::mkdirat(&Resource::cwd(), path, 0o755).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

fn unlink_file(lio: &mut Lio, path: CString) {
  let mut receiver =
    api::unlinkat(&Resource::cwd(), path, 0).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

fn write_all_at(lio: &mut Lio, fd: &Resource, bytes: &[u8], offset: u32) {
  let mut written_total = 0usize;
  while written_total < bytes.len() {
    let chunk = bytes[written_total..].to_vec();
    let mut receiver =
      api::write_at(fd, chunk.clone(), offset + written_total as u32)
        .with_lio(lio)
        .send();
    let (result, returned_buf) = common::poll_recv(lio, &mut receiver);
    let bytes_written = result.unwrap() as usize;
    assert!(bytes_written > 0);
    assert!(bytes_written <= returned_buf.len());
    written_total += bytes_written;
  }
}

fn read_exact_at(
  lio: &mut Lio,
  fd: &Resource,
  len: usize,
  offset: u32,
) -> Vec<u8> {
  let mut out = Vec::with_capacity(len);
  while out.len() < len {
    let mut receiver =
      api::read_at(fd, vec![0u8; len - out.len()], offset + out.len() as u32)
        .with_lio(lio)
        .send();
    let (result, buf) = common::poll_recv(lio, &mut receiver);
    let bytes_read = result.unwrap() as usize;
    assert!(bytes_read > 0, "read_at returned EOF before expected bytes");
    out.extend_from_slice(&buf[..bytes_read]);
  }
  out
}

#[test]
fn test_openat_with_directory_fd() {
  let mut lio = new_ds_lio();

  let dir_path =
    CString::new(format!("/tmp/openat-dirfd-{}", std::process::id())).unwrap();
  mkdir(&mut lio, dir_path.clone());

  let dir_res = open_file(
    &mut lio,
    &Resource::cwd(),
    dir_path.clone(),
    libc::O_RDONLY | libc::O_DIRECTORY,
    0,
  )
  .expect("Failed to open synthetic directory");

  let file_path = CString::new("child.txt").unwrap();
  let fd = open_file(
    &mut lio,
    &dir_res,
    file_path,
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    0o666,
  )
  .expect("Failed to open file with directory fd");

  assert!(fd.as_fd().as_raw_fd() >= 0);

  let child_path =
    CString::new(format!("{}/child.txt", dir_path.to_string_lossy())).unwrap();
  unlink_file(&mut lio, child_path);
}

#[test]
fn test_openat_concurrent() {
  let mut lio = new_ds_lio();
  let mut fds = Vec::new();

  for i in 0..10 {
    let path =
      CString::new(format!("/tmp/openat-concurrent-{}.txt", i)).unwrap();
    let fd = open_file(
      &mut lio,
      &Resource::cwd(),
      path.clone(),
      libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
      0o666,
    )
    .expect("Failed to open file");
    assert!(fd.as_fd().as_raw_fd() >= 0);
    fds.push(fd);
    unlink_file(&mut lio, path);
  }

  let raw_fds: Vec<_> = fds.iter().map(|f| f.as_fd().as_raw_fd()).collect();
  for i in 0..raw_fds.len() {
    for j in i + 1..raw_fds.len() {
      assert_ne!(raw_fds[i], raw_fds[j]);
    }
  }
}

#[test]
fn test_openat_append_mode() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/openat-append.txt").unwrap();

  let writer = open_file(
    &mut lio,
    &Resource::cwd(),
    path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    0o644,
  )
  .unwrap();
  write_all_at(&mut lio, &writer, b"Hello", 0);

  let append_fd = open_file(
    &mut lio,
    &Resource::cwd(),
    path.clone(),
    libc::O_WRONLY | libc::O_APPEND,
    0,
  )
  .expect("Failed to open append mode file");
  write_all_at(&mut lio, &append_fd, b"World", 0);

  let reader =
    open_file(&mut lio, &Resource::cwd(), path.clone(), libc::O_RDONLY, 0)
      .unwrap();
  let read_back = read_exact_at(&mut lio, &reader, 10, 0);
  assert_eq!(read_back, b"HelloWorld");

  unlink_file(&mut lio, path);
}

#[test]
fn test_openat_excl_flag() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/openat-excl.txt").unwrap();

  let fd = open_file(
    &mut lio,
    &Resource::cwd(),
    path.clone(),
    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
    0o666,
  )
  .expect("First O_EXCL create should succeed");
  drop(fd);

  let result = open_file(
    &mut lio,
    &Resource::cwd(),
    path.clone(),
    libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
    0o666,
  );
  assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EEXIST));

  unlink_file(&mut lio, path);
}

#[test]
fn test_openat_directory() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp").unwrap();

  let fd = open_file(
    &mut lio,
    &Resource::cwd(),
    path,
    libc::O_RDONLY | libc::O_DIRECTORY,
    0,
  )
  .expect("Failed to open directory");
  assert!(fd.as_fd().as_raw_fd() >= 0);
}

#[test]
fn test_openat_directory_flag_on_file() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/openat-dir-flag.txt").unwrap();
  let _fd = open_file(
    &mut lio,
    &Resource::cwd(),
    path.clone(),
    libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
    0o644,
  )
  .unwrap();

  let result = open_file(
    &mut lio,
    &Resource::cwd(),
    path.clone(),
    libc::O_RDONLY | libc::O_DIRECTORY,
    0,
  );
  assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOTDIR));

  unlink_file(&mut lio, path);
}
