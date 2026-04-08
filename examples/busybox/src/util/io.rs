use std::{ffi::CString, io};

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
  let mut written = 0usize;
  while written < buf.len() {
    let (result, _) = write_once(lio, fd, buf[written..].to_vec());
    let n = result? as usize;
    if n == 0 {
      return Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "write returned zero before completing output",
      ));
    }
    written += n;
  }
  Ok(())
}

pub fn write_all_many(
  lio: &Lio,
  bufs: Vec<(Resource, Vec<u8>)>,
) -> io::Result<()> {
  let mut pending: Vec<(Resource, Vec<u8>, usize)> =
    bufs.into_iter().map(|(fd, buf)| (fd, buf, 0usize)).collect();

  while pending.iter().any(|(_, buf, written)| *written < buf.len()) {
    let mut rxs = Vec::new();
    let mut active = Vec::new();

    for (idx, (fd, buf, written)) in pending.iter().enumerate() {
      if *written >= buf.len() {
        continue;
      }
      rxs.push(api::write(fd, buf[*written..].to_vec()).with_lio(lio).send());
      active.push(idx);
    }

    for (idx, (result, _)) in active.into_iter().zip(run_all(lio, rxs)) {
      let n = result? as usize;
      if n == 0 {
        return Err(io::Error::new(
          io::ErrorKind::WriteZero,
          "write returned zero before completing output",
        ));
      }
      pending[idx].2 += n;
    }
  }

  Ok(())
}

pub fn read_to_string(lio: &Lio, path: Option<&str>) -> io::Result<String> {
  String::from_utf8(read_to_bytes(lio, path)?)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub fn read_to_string_fd(lio: &Lio, input: &Resource) -> io::Result<String> {
  String::from_utf8(read_to_bytes_fd(lio, input)?)
    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub fn read_to_bytes(lio: &Lio, path: Option<&str>) -> io::Result<Vec<u8>> {
  match path {
    Some(path) => {
      let cpath = CString::new(path)?;
      let fd = run_all(
        lio,
        vec![
          api::openat(&Resource::cwd(), cpath, libc::O_RDONLY)
            .with_lio(lio)
            .send(),
        ],
      )
      .into_iter()
      .next()
      .expect("missing open result")?;
      read_to_bytes_fd(lio, &fd)
    }
    None => read_to_bytes_fd(lio, &Resource::stdin()),
  }
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
