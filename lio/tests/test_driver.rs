use lio::Lio;
use lio::api::resource::Resource;

#[test]
fn test_worker_start_stop() {
  let mut lio = Lio::new(64).unwrap();

  let thing = "hello".as_bytes().to_vec();

  let fd = Resource::stdout();

  let (sender, receiver) = std::sync::mpsc::channel();

  lio::api::write(&fd, thing).with_lio(&mut lio).send_with(sender);

  // Poll until done
  loop {
    lio.try_run().unwrap();
    if let Ok((res, _buf)) = receiver.try_recv() {
      res.unwrap();
      break;
    }
    std::thread::sleep(std::time::Duration::from_micros(100));
  }
}
