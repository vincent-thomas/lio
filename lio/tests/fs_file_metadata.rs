#![cfg(feature = "high")]

use std::ffi::CString;

use lio::{Lio, fs::OpenOptions};

#[path = "common.rs"]
mod common;

#[test]
fn file_metadata_reads_open_file_metadata() {
  let mut lio = Lio::new(64).unwrap();
  let temp = common::TempFile::new("fs_file_metadata_reads_open_file_metadata");
  std::fs::write(temp.path.to_str().unwrap(), b"hello metadata").unwrap();

  let mut file_rx = OpenOptions::new()
    .read(true)
    .open(CString::new(temp.path.as_bytes()).unwrap())
    .with_lio(&lio)
    .send();
  let file = common::poll_recv(&mut lio, &mut file_rx).unwrap();

  let mut stat_rx = file.metadata().with_lio(&lio).send();
  let stat = common::poll_recv(&mut lio, &mut stat_rx).unwrap();

  assert!(stat.is_file());
  assert_eq!(stat.len(), b"hello metadata".len() as u64);
}
