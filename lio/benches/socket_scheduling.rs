//! Real loopback UDP transfers through the public I/O API.
//! Uses Poller unless LIO_BENCH_DEFAULT_BACKEND is set (io_uring on Linux).
use std::cell::{Cell, RefCell};
use std::net::UdpSocket;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::rc::Rc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use lio::api::resource::Resource;
use lio::backend::impls::Poller;
use lio::{Lio, api};

struct Pair {
  sender: Resource,
  receiver: Resource,
  send_buf: Rc<RefCell<Option<Vec<u8>>>>,
  recv_buf: Rc<RefCell<Option<Vec<u8>>>>,
}

struct Workload {
  lio: Lio,
  pairs: Vec<Pair>,
  done: Rc<Cell<usize>>,
  bytes: usize,
}

impl Workload {
  fn new(depth: usize, bytes: usize) -> Self {
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
    Self {
      lio: if std::env::var_os("LIO_BENCH_DEFAULT_BACKEND").is_some() {
        Lio::new(depth * 2).unwrap()
      } else {
        Lio::new_with_backend(Poller::new(), depth * 2).unwrap()
      },
      pairs,
      done: Rc::new(Cell::new(0)),
      bytes,
    }
  }

  fn batch(&self) {
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

fn main() {
  if std::env::args().any(|arg| arg == "--count") {
    let workload = Workload::new(1, 64);
    for _ in 0..1000 {
      workload.batch();
    }
    println!("1000 loopback UDP transfers completed and verified");
    return;
  }
  let mut criterion = Criterion::default()
    .sample_size(50)
    .warm_up_time(Duration::from_secs(1))
    .measurement_time(Duration::from_secs(3))
    .configure_from_args();
  let mut group = criterion.benchmark_group(
    if std::env::var_os("LIO_BENCH_DEFAULT_BACKEND").is_some() {
      "default/udp_loopback"
    } else {
      "poller/udp_loopback"
    },
  );
  for bytes in [64, 4096] {
    for depth in [1, 32] {
      let workload = Workload::new(depth, bytes);
      group.throughput(Throughput::Elements(depth as u64));
      group.bench_function(
        BenchmarkId::new(format!("bytes_{bytes}"), depth),
        |b| b.iter(|| workload.batch()),
      );
    }
  }
  group.finish();
  criterion.final_summary();
}
