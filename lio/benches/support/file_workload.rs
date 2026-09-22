use lio::{Lio, api};
use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::rc::Rc;
const READ_PAYLOAD: [u8; 4096] = [7; 4096];

pub(crate) struct Workload {
  lio: Lio,
  file: api::resource::Resource,
  buffers: Rc<RefCell<Vec<Vec<u8>>>>,
  done: Rc<Cell<usize>>,
  depth: usize,
  read: bool,
}

impl Workload {
  pub(crate) fn new(
    lio: Lio,
    depth: usize,
    read: bool,
    path: &std::path::Path,
  ) -> Self {
    let mut receiver = api::openat(
      &api::resource::Resource::cwd(),
      CString::new(path.to_str().unwrap()).unwrap(),
      api::OpenFlags::EMPTY,
      api::FileMode::default(),
    )
    .with_lio(&lio)
    .send();
    let file = loop {
      if let Some(result) = receiver.try_recv() {
        break result.unwrap();
      }
      lio.run().unwrap();
    };
    Self {
      lio,
      file,
      depth,
      read,
      buffers: Rc::new(RefCell::new(
        (0..depth).map(|_| vec![0; 4096]).collect(),
      )),
      done: Rc::new(Cell::new(0)),
    }
  }

  pub(crate) fn batch(&self) {
    self.done.set(0);
    for _ in 0..self.depth {
      let done = Rc::clone(&self.done);
      if self.read {
        let buffer = self.buffers.borrow_mut().pop().unwrap();
        let buffers = Rc::clone(&self.buffers);
        api::read_at(&self.file, buffer, 0).with_lio(&self.lio).when_done(
          move |(result, buffer)| {
            assert_eq!(result.unwrap(), 4096);
            assert_eq!(buffer.as_slice(), READ_PAYLOAD.as_slice());
            buffers.borrow_mut().push(buffer);
            done.set(done.get() + 1);
          },
        );
      } else {
        api::nop().with_lio(&self.lio).when_done(move |result| {
          result.unwrap();
          done.set(done.get() + 1);
        });
      }
    }
    while self.done.get() < self.depth {
      self.lio.run().unwrap();
    }
  }
}
