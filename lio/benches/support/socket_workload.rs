use lio::api::resource::Resource;
use lio::{Lio, api};
use std::cell::{Cell, RefCell};
use std::net::UdpSocket;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::rc::Rc;

struct Pair {
  sender: Resource,
  receiver: Resource,
  send_buf: Rc<RefCell<Option<Vec<u8>>>>,
  recv_buf: Rc<RefCell<Option<Vec<u8>>>>,
}

pub(crate) struct Workload {
  lio: Lio,
  pairs: Vec<Pair>,
  done: Rc<Cell<usize>>,
  bytes: usize,
}

impl Workload {
  pub(crate) fn new(lio: Lio, depth: usize, bytes: usize) -> Self {
    let pairs = (0..depth)
      .map(|_| {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();
        receiver.connect(sender.local_addr().unwrap()).unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        // SAFETY: each socket transfers its owned descriptor to exactly one Resource.
        let sender = unsafe { Resource::from_raw_fd(sender.into_raw_fd()) };
        // SAFETY: each socket transfers its owned descriptor to exactly one Resource.
        let receiver = unsafe { Resource::from_raw_fd(receiver.into_raw_fd()) };
        Pair {
          sender,
          receiver,
          send_buf: Rc::new(RefCell::new(Some(vec![7; bytes]))),
          recv_buf: Rc::new(RefCell::new(Some(vec![0; bytes]))),
        }
      })
      .collect();
    Self { lio, pairs, done: Rc::new(Cell::new(0)), bytes }
  }

  pub(crate) fn batch(&self) {
    self.done.set(0);
    for pair in &self.pairs {
      let done = Rc::clone(&self.done);
      let slot = Rc::clone(&pair.recv_buf);
      let bytes = self.bytes;
      let buf = slot.borrow_mut().take().unwrap();
      api::recv(&pair.receiver, buf, None).with_lio(&self.lio).when_done(
        move |(result, buf)| {
          assert_eq!(result.unwrap() as usize, bytes);
          assert!(buf.iter().all(|&byte| byte == 7));
          *slot.borrow_mut() = Some(buf);
          done.set(done.get() + 1);
        },
      );
      let done = Rc::clone(&self.done);
      let slot = Rc::clone(&pair.send_buf);
      let buf = slot.borrow_mut().take().unwrap();
      api::send(&pair.sender, buf, None).with_lio(&self.lio).when_done(
        move |(result, buf)| {
          assert_eq!(result.unwrap() as usize, bytes);
          *slot.borrow_mut() = Some(buf);
          done.set(done.get() + 1);
        },
      );
    }
    while self.done.get() < self.pairs.len() * 2 {
      self.lio.run().unwrap();
    }
  }
}
