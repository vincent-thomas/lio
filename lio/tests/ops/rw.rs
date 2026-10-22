#![allow(
  clippy::duplicate_mod,
  clippy::unnecessary_mut_passed,
  clippy::expect_fun_call
)]

use super::common;

use lio::{
  Lio,
  api::{self, resource::Resource},
  backend::{
    ds::{DSBackend, DSConfig},
    op::Op,
    testing::{Checked, ScriptedBackend},
  },
};
use proptest::{prelude::*, test_runner::TestRunner};
use std::ffi::CString;

fn new_ds_lio() -> Lio {
  Lio::new_with_backend(DSBackend::with_config(DSConfig::default()), 64)
    .unwrap()
}

fn open_file(
  lio: &mut Lio,
  path: CString,
  flags: lio::api::OpenFlags,
  mode: u32,
) -> Result<Resource, std::io::Error> {
  let mut receiver =
    api::openat(&Resource::cwd(), path, flags, mode).with_lio(lio).send();
  common::poll_recv(lio, &mut receiver)
}

fn open_rw_file(lio: &mut Lio, path: CString) -> Resource {
  open_file(
    lio,
    path,
    lio::api::OpenFlags::READ
      | lio::api::OpenFlags::WRITE
      | lio::api::OpenFlags::CREATE
      | lio::api::OpenFlags::TRUNCATE,
    0o644,
  )
  .unwrap()
}

fn open_ro_file(lio: &mut Lio, path: CString) -> Resource {
  open_file(lio, path, lio::api::OpenFlags::READ, 0).unwrap()
}

fn unlink_file(lio: &mut Lio, path: CString) {
  let mut receiver =
    api::unlinkat(&Resource::cwd(), path, api::UnlinkKind::File)
      .with_lio(lio)
      .send();
  common::poll_recv(lio, &mut receiver).unwrap();
}

fn write_all_at(
  lio: &mut Lio,
  fd: &Resource,
  bytes: &[u8],
  offset: u64,
) -> Vec<u8> {
  let expected = bytes.to_vec();
  let mut written_total = 0usize;

  while written_total < expected.len() {
    let chunk = expected[written_total..].to_vec();
    let mut receiver =
      api::write_at(fd, chunk.clone(), offset + written_total as u64)
        .with_lio(lio)
        .send();

    let (result, returned_buf) = common::poll_recv(lio, &mut receiver);
    let bytes_written = result.unwrap();

    assert!(bytes_written <= returned_buf.len());
    assert_eq!(returned_buf, chunk);
    assert!(bytes_written > 0, "write_at returned zero bytes");

    written_total += bytes_written;
  }

  expected
}

fn read_once(lio: &mut Lio, fd: &Resource, len: usize) -> (usize, Vec<u8>) {
  let mut receiver = api::read(fd, vec![0u8; len]).with_lio(lio).send();
  let (result, buf) = common::poll_recv(lio, &mut receiver);
  (result.unwrap(), buf)
}

fn read_once_at(
  lio: &mut Lio,
  fd: &Resource,
  len: usize,
  offset: u64,
) -> (usize, Vec<u8>) {
  let mut receiver =
    api::read_at(fd, vec![0u8; len], offset).with_lio(lio).send();
  let (result, buf) = common::poll_recv(lio, &mut receiver);
  (result.unwrap(), buf)
}

fn read_exact(lio: &mut Lio, fd: &Resource, len: usize) -> Vec<u8> {
  let mut out = Vec::with_capacity(len);

  while out.len() < len {
    let (bytes_read, buf) = read_once(lio, fd, len - out.len());
    assert!(bytes_read <= buf.len());
    assert!(bytes_read > 0, "read returned EOF before reading expected bytes");
    out.extend_from_slice(&buf[..bytes_read]);
  }

  out
}

fn read_exact_at(
  lio: &mut Lio,
  fd: &Resource,
  len: usize,
  offset: u64,
) -> Vec<u8> {
  let mut out = Vec::with_capacity(len);

  while out.len() < len {
    let (bytes_read, buf) =
      read_once_at(lio, fd, len - out.len(), offset + out.len() as u64);
    assert!(bytes_read <= buf.len());
    assert!(
      bytes_read > 0,
      "read_at returned EOF before reading expected bytes"
    );
    out.extend_from_slice(&buf[..bytes_read]);
  }

  out
}

#[test]
fn read_basic() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/lio_test_read_basic.txt").unwrap();
  let test_data = b"Hello, World!";

  let writer = open_rw_file(&mut lio, path.clone());
  let _ = write_all_at(&mut lio, &writer, test_data, 0);

  let reader = open_ro_file(&mut lio, path.clone());
  let bytes = read_exact(&mut lio, &reader, test_data.len());
  assert_eq!(bytes, test_data);

  unlink_file(&mut lio, path);
}

#[test]
fn read_large_buffer() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/lio_test_read_large.txt").unwrap();

  #[cfg(miri)]
  const LARGE_READ_LEN: usize = 16 * 1024;
  #[cfg(not(miri))]
  const LARGE_READ_LEN: usize = 1024 * 1024;

  let large_data: Vec<u8> =
    (0..LARGE_READ_LEN).map(|i| (i % 256) as u8).collect();

  let writer = open_rw_file(&mut lio, path.clone());
  let _ = write_all_at(&mut lio, &writer, &large_data, 0);

  let reader = open_ro_file(&mut lio, path.clone());
  let bytes = read_exact(&mut lio, &reader, large_data.len());
  assert_eq!(bytes, large_data);

  unlink_file(&mut lio, path);
}

#[test]
fn read_concurrent() {
  let mut lio = new_ds_lio();
  let files: Vec<_> = (0..10)
    .map(|i| {
      let path =
        CString::new(format!("/tmp/lio_test_read_concurrent_{}.txt", i))
          .unwrap();
      let data = format!("Data for file {}", i).into_bytes();
      let writer = open_rw_file(&mut lio, path.clone());
      let _ = write_all_at(&mut lio, &writer, &data, 0);
      (path, data)
    })
    .collect();

  for (path, data) in &files {
    let reader = open_ro_file(&mut lio, path.clone());
    let bytes = read_exact(&mut lio, &reader, data.len());
    assert_eq!(bytes, *data);
  }

  for (path, _) in files {
    unlink_file(&mut lio, path);
  }
}

#[test]
fn read_at_basic() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/lio_test_read_at_basic.txt").unwrap();
  let test_data = b"0123456789ABCDEF";

  let writer = open_rw_file(&mut lio, path.clone());
  let _ = write_all_at(&mut lio, &writer, test_data, 0);

  let reader = open_ro_file(&mut lio, path.clone());
  let bytes = read_exact_at(&mut lio, &reader, 5, 5);
  assert_eq!(bytes, b"56789");

  unlink_file(&mut lio, path);
}

#[test]
fn read_at_offset_beyond_file() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/lio_test_read_at_beyond.txt").unwrap();
  let test_data = b"Hello";

  let writer = open_rw_file(&mut lio, path.clone());
  let _ = write_all_at(&mut lio, &writer, test_data, 0);

  let reader = open_ro_file(&mut lio, path.clone());
  let (bytes_read, _buf) = read_once_at(&mut lio, &reader, 10, 100);
  assert_eq!(bytes_read, 0);

  unlink_file(&mut lio, path);
}

/// Offsets past 4 GiB reach the file intact. They used to be `u32`, so the C
/// API wrapped a write at 5 GiB to 1 GiB. The file is sparse, so this takes no
/// disk space on APFS, ext4 or tmpfs.
#[test]
#[cfg_attr(miri, ignore = "needs a real backend")]
#[cfg_attr(
  windows,
  ignore = "NTFS files are not sparse by default: this would write 5 GiB"
)]
fn read_write_at_offset_past_4_gib() {
  const OFFSET: u64 = 5 << 30;
  const WRAPPED: u64 = OFFSET as u32 as u64;
  let data = b"past 4 GiB";

  let mut lio = Lio::new(64).unwrap();
  let temp = common::TempFile::new("lio_rw_offset_past_4_gib");
  let file = open_rw_file(&mut lio, temp.path.clone());

  let _ = write_all_at(&mut lio, &file, data, OFFSET);
  assert_eq!(read_exact_at(&mut lio, &file, data.len(), OFFSET), data);

  let len = std::fs::metadata(temp.path.to_str().unwrap()).unwrap().len();
  assert_eq!(len, OFFSET + data.len() as u64);
  // Nothing landed where the truncated offset points.
  assert_eq!(
    read_exact_at(&mut lio, &file, data.len(), WRAPPED),
    vec![0u8; data.len()]
  );
}

/// A kernel file offset is signed, so an offset past `i64::MAX` fails with
/// `InvalidInput` before any read or write reaches the backend. Cast to
/// `off_t` it would turn negative, and `u64::MAX` would read as `-1`: the
/// current position.
#[test]
fn offset_past_i64_max_is_invalid_input() {
  for offset in [i64::MAX as u64 + 1, u64::MAX] {
    let backend = ScriptedBackend::new();
    let lio = Lio::new_with_backend(Checked::new(backend.clone()), 4).unwrap();
    let file = Resource::stdin();

    let mut read =
      api::read_at(&file, vec![0u8; 4], offset).with_lio(&lio).send();
    let mut write =
      api::write_at(&file, b"data".to_vec(), offset).with_lio(&lio).send();
    lio.try_run().unwrap();

    let pushed = backend.in_flight();
    assert_eq!(pushed.len(), 2);
    for id in pushed {
      let op = backend.op(id).unwrap();
      assert!(matches!(op, Op::Nop), "{offset} reached the backend: {op:?}");
      backend.complete(id, 0);
    }
    lio.try_run().unwrap();

    let (result, buf) = read.try_recv().expect("read completed");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(buf, [0u8; 4]);
    let (result, buf) = write.try_recv().expect("write completed");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(buf, b"data");
  }
}

/// The largest offset the kernel accepts is still submitted as given.
#[test]
fn offset_at_i64_max_is_submitted() {
  let backend = ScriptedBackend::new();
  let lio = Lio::new_with_backend(Checked::new(backend.clone()), 4).unwrap();
  let file = Resource::stdin();

  let _read =
    api::read_at(&file, vec![0u8; 4], i64::MAX as u64).with_lio(&lio).send();
  let _write = api::write_at(&file, b"data".to_vec(), i64::MAX as u64)
    .with_lio(&lio)
    .send();
  lio.try_run().unwrap();

  let ops: Vec<_> =
    backend.in_flight().into_iter().map(|id| backend.op(id).unwrap()).collect();
  assert!(
    matches!(
      ops.as_slice(),
      [
        Op::Read { offset: Some(read), .. },
        Op::Write { offset: Some(write), .. },
      ] if *read == i64::MAX as u64 && *write == i64::MAX as u64
    ),
    "{ops:?}"
  );
  for id in backend.in_flight() {
    backend.complete(id, 0);
  }
  lio.try_run().unwrap();
}

#[test]
fn read_empty_file() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/lio_test_read_empty.txt").unwrap();

  let _writer = open_rw_file(&mut lio, path.clone());
  let reader = open_ro_file(&mut lio, path.clone());
  let (bytes_read, _buf) = read_once_at(&mut lio, &reader, 64, 0);
  assert_eq!(bytes_read, 0);

  unlink_file(&mut lio, path);
}

#[test]
fn read_partial_buffer() {
  let mut lio = new_ds_lio();
  let path = CString::new("/tmp/lio_test_read_partial.txt").unwrap();
  let test_data = b"0123456789";

  let writer = open_rw_file(&mut lio, path.clone());
  let _ = write_all_at(&mut lio, &writer, test_data, 0);

  let reader = open_ro_file(&mut lio, path.clone());
  let bytes = read_exact(&mut lio, &reader, 5);
  assert_eq!(bytes, b"01234");

  unlink_file(&mut lio, path);
}

#[test]
fn read_nonexistent_file() {
  let mut lio = new_ds_lio();
  let path =
    CString::new("/tmp/lio_test_nonexistent_12345_abcdef.txt").unwrap();
  let err =
    open_file(&mut lio, path, lio::api::OpenFlags::READ, 0).unwrap_err();
  assert_eq!(err.raw_os_error(), Some(lio::backend::ds::errno::ENOENT));
}

#[test]
fn prop_test_read_arbitrary_data_and_offsets() {
  let mut runner = TestRunner::new(proptest::test_runner::Config {
    cases: 20,
    ..Default::default()
  });

  runner
    .run(&(1..=4096usize, 0..=2048u64, 1..=2048usize, any::<u64>()), |props| {
      let (data_size, read_offset, buffer_size, seed) = props;
      prop_test_read_arbitrary_data_and_offsets_run(
        data_size,
        read_offset,
        buffer_size,
        seed,
      )
    })
    .unwrap();
}

fn prop_test_read_arbitrary_data_and_offsets_run(
  data_size: usize,
  read_offset: u64,
  buffer_size: usize,
  seed: u64,
) -> Result<(), TestCaseError> {
  let mut lio = new_ds_lio();
  let test_data: Vec<u8> = (0..data_size)
    .map(|i| ((seed.wrapping_add(i as u64)) % 256) as u8)
    .collect();

  let path = common::make_temp_path("read", seed);
  let writer = open_rw_file(&mut lio, path.clone());
  let _ = write_all_at(&mut lio, &writer, &test_data, 0);

  let reader = open_ro_file(&mut lio, path.clone());
  let file_size = test_data.len();
  let offset = read_offset as usize;

  if offset >= file_size {
    let (bytes_read, _buf) =
      read_once_at(&mut lio, &reader, buffer_size, read_offset);
    if bytes_read != 0 {
      return Err(TestCaseError::fail(format!(
        "Reading beyond EOF should return 0 bytes, got {}",
        bytes_read
      )));
    }
  } else {
    let expected_bytes = std::cmp::min(buffer_size, file_size - offset);
    let result_buf =
      read_exact_at(&mut lio, &reader, expected_bytes, read_offset);
    let expected_data = &test_data[offset..offset + expected_bytes];
    if result_buf != expected_data {
      return Err(TestCaseError::fail(
        "Read data should match written data at offset".to_string(),
      ));
    }
  }

  unlink_file(&mut lio, path);
  Ok(())
}

#[test]
fn write_large_buffer() {
  let mut lio = new_ds_lio();

  #[cfg(miri)]
  const LARGE_WRITE_LEN: usize = 16 * 1024;
  #[cfg(not(miri))]
  const LARGE_WRITE_LEN: usize = 16 * 1024;

  let path = CString::new("/tmp/lio_test_write_large.txt").unwrap();
  let fd = open_rw_file(&mut lio, path.clone());
  let data: Vec<u8> = (0..LARGE_WRITE_LEN).map(|i| (i % 256) as u8).collect();

  let returned_buf = write_all_at(&mut lio, &fd, &data, 0);
  assert_eq!(returned_buf, data);

  let read_back = read_exact_at(&mut lio, &fd, data.len(), 0);
  assert_eq!(read_back, data);

  unlink_file(&mut lio, path);
}

#[test]
fn write_concurrent() {
  let mut lio = new_ds_lio();

  for i in 0..10 {
    let path =
      CString::new(format!("/tmp/lio_test_write_concurrent_{}.txt", i))
        .unwrap();
    let data = format!("Task {}", i).into_bytes();

    let fd = open_rw_file(&mut lio, path.clone());
    let returned_buf = write_all_at(&mut lio, &fd, &data, 0);
    assert_eq!(returned_buf, data);

    let read_back = read_exact_at(&mut lio, &fd, data.len(), 0);
    assert_eq!(read_back, data);

    unlink_file(&mut lio, path);
  }
}

#[test]
fn prop_test_write_arbitrary_data_and_offsets() {
  let mut runner = TestRunner::new(ProptestConfig::default());

  runner
    .run(&(0usize..=8192, 0u64..=4096, any::<u64>()), |props| {
      prop_test_write_arbitrary_data_and_offsets_run(props.0, props.1, props.2)
    })
    .unwrap();
}

fn prop_test_write_arbitrary_data_and_offsets_run(
  data_size: usize,
  write_offset: u64,
  seed: u64,
) -> Result<(), TestCaseError> {
  let mut lio = new_ds_lio();
  let test_data: Vec<u8> = (0..data_size)
    .map(|i| ((seed.wrapping_add(i as u64)) % 256) as u8)
    .collect();

  let path = common::make_temp_path("write", seed);
  let resource = open_rw_file(&mut lio, path.clone());

  if write_offset > 0 {
    let zeros = vec![0u8; write_offset as usize];
    let _ = write_all_at(&mut lio, &resource, &zeros, 0);
  }

  let returned_buf =
    write_all_at(&mut lio, &resource, &test_data, write_offset);
  if returned_buf != test_data {
    return Err(TestCaseError::fail(
      "Returned buffer should match original data".to_string(),
    ));
  }

  let read_buf =
    read_exact_at(&mut lio, &resource, test_data.len(), write_offset);
  if read_buf != test_data {
    return Err(TestCaseError::fail(
      "Read data does not match written data".to_string(),
    ));
  }

  unlink_file(&mut lio, path);
  Ok(())
}
