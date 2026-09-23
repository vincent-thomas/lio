//! Focused, bounded Poller exercise for the Linux Valgrind job.
#![cfg(target_os = "linux")]

use std::{
  os::fd::{FromRawFd, IntoRawFd},
  os::unix::net::UnixStream,
  time::Duration,
};

use lio::{
  Lio, api, api::io::Receiver, api::resource::Resource, backend::impls::Poller,
};

fn complete<T>(lio: &Lio, receiver: &mut Receiver<T>) -> T {
  for _ in 0..100 {
    if let Some(result) = receiver.try_recv() {
      return result;
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  }
  panic!("Poller operation did not complete within 1 second");
}

#[test]
fn leak_poller_roundtrip() {
  let (writer, reader) = UnixStream::pair().unwrap();
  let writer = unsafe { Resource::from_raw_fd(writer.into_raw_fd()) };
  let reader = unsafe { Resource::from_raw_fd(reader.into_raw_fd()) };
  let lio = Lio::new_with_backend(Poller::new(), 32).unwrap();

  for i in 0..16u8 {
    let payload = vec![i; 32];
    let mut write = api::write(&writer, payload.clone()).with_lio(&lio).send();
    let (result, returned) = complete(&lio, &mut write);
    assert_eq!(result.unwrap(), payload.len() as _);
    assert_eq!(returned, payload);

    let mut read = api::read(&reader, vec![0u8; 32]).with_lio(&lio).send();
    let (result, returned) = complete(&lio, &mut read);
    assert_eq!(result.unwrap(), payload.len() as _);
    assert_eq!(returned, payload);
  }
}
