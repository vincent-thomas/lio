use std::io;

use lio::{
  Lio,
  api::{self, io::Receiver, resource::Resource},
};

pub fn run<T>(lio: &Lio, mut rx: api::io::Receiver<T>) -> T {
  loop {
    if let Some(result) = rx.try_recv() {
      return result;
    }
    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }
}

pub fn run_recv<T>(lio: &Lio, receiver: &mut Receiver<T>) -> T {
  loop {
    if let Some(result) = receiver.try_recv() {
      return result;
    }
    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }
}

pub fn run_all<T>(lio: &Lio, mut rxs: Vec<api::io::Receiver<T>>) -> Vec<T> {
  let mut results: Vec<Option<T>> = Vec::with_capacity(rxs.len());
  results.resize_with(rxs.len(), || None);
  let mut remaining = rxs.len();

  while remaining > 0 {
    for (idx, rx) in rxs.iter_mut().enumerate() {
      if results[idx].is_some() {
        continue;
      }
      if let Some(result) = rx.try_recv() {
        results[idx] = Some(result);
        remaining -= 1;
      }
    }

    if remaining == 0 {
      break;
    }

    if lio.try_run().expect("lio.try_run()") == 0 {
      lio.run().expect("lio.run()");
    }
  }

  results.into_iter().map(|item| item.expect("missing result")).collect()
}

pub fn write_once(
  lio: &Lio,
  fd: &Resource,
  buf: Vec<u8>,
) -> (std::io::Result<i32>, Vec<u8>) {
  let rx = api::write(fd, buf).with_lio(lio).send();
  run(lio, rx)
}

pub fn write_all(lio: &Lio, fd: &Resource, buf: Vec<u8>) -> io::Result<()> {
  let _ = write_all_reusing_buffer(lio, fd, buf)?;
  Ok(())
}

pub fn write_all_reusing_buffer(
  lio: &Lio,
  fd: &Resource,
  mut buf: Vec<u8>,
) -> io::Result<Vec<u8>> {
  while !buf.is_empty() {
    let (result, mut returned_buf) = write_once(lio, fd, buf);
    let n = result? as usize;
    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "write returned zero before completing output",
      ));
    }
    if n >= returned_buf.len() {
      return Ok(returned_buf);
    }

    let remaining = returned_buf.len() - n;
    returned_buf.copy_within(n.., 0);
    returned_buf.truncate(remaining);
    buf = returned_buf;
  }

  Ok(buf)
}

pub fn read_to_string(lio: &Lio, path: Option<&str>) -> io::Result<String> {
  match path {
    Some(path) => run(lio, lio::fs::read_to_string(path).with_lio(lio).send()),
    None => String::from_utf8(read_to_bytes_fd(lio, &Resource::stdin())?)
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
  }
}

pub fn read_to_string_fd(lio: &Lio, input: &Resource) -> io::Result<String> {
  String::from_utf8(read_to_bytes_fd(lio, input)?)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub fn read_to_bytes_fd(lio: &Lio, input: &Resource) -> io::Result<Vec<u8>> {
  let mut data = Vec::new();
  let mut buf = vec![0u8; 8192];
  loop {
    let rx = api::read(input, buf).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break;
    }
    data.extend_from_slice(&buf[..n]);
  }

  Ok(data)
}
