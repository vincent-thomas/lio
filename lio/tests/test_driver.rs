use lio::Lio;
use lio::api::resource::Resource;
use std::os::fd::FromRawFd;

#[test]
fn test_worker_start_stop() {
  let mut lio = Lio::new(64).unwrap();

  let thing = "hello".as_bytes().to_vec();

  let fd = unsafe { Resource::from_raw_fd(1) };

  let (sender, receiver) = std::sync::mpsc::channel();

  lio::api::write(&fd, thing).with_lio(&mut lio).send_with(sender);

  // Poll until done
  loop {
    lio.try_run().unwrap();
    if let Ok((res, _buf)) = receiver.try_recv() {
      dbg!(res);
      break;
    }
    std::thread::sleep(std::time::Duration::from_micros(100));
  }
}
