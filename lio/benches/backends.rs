//! The same UDP loopback transfer on every real backend this host can run.
//!
//! One iteration is `qd` transfers. A transfer is one 64-byte datagram: lio
//! posts the recv, posts the send, and runs until both have completed. The
//! sockets are created before timing. Throughput is transfers per second.
//! Each transfer is two operations, and the driver is sized for both.
//!
//! The time includes the kernel. Bookkeeping without the kernel is
//! `cargo bench -p lio --bench bookkeeping`.
//!
//! - `backend/io_uring/udp_64` — Linux. The send and the recv are SQEs.
//! - `backend/poller/udp_64` — Linux (epoll), macOS, and the BSDs (kqueue).
//!   The recv waits for readiness. A nonblocking send often finishes inside
//!   `flush`.
//! - `backend/iocp/udp_64` — Windows. The send and the recv are overlapped.
//!
//! A backend this operating system does not implement is left out of the
//! binary. One that fails to initialize is skipped, and the others still run.
//! io_uring needs Linux. The poller needs Linux, macOS, or a BSD. IOCP needs
//! Windows.
//!
//! ```text
//! cargo bench -p lio --bench backends
//! mise run bench
//! ```
//!
//! `mise run bench` runs the bookkeeping bench first. Criterion flags go
//! after `--`.

use std::cell::{Cell, RefCell};
use std::net::UdpSocket;
use std::rc::Rc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use lio::api::resource::Resource;
use lio::backend::IoBackend;
use lio::{Lio, api};

const PAYLOAD: usize = 64;
const PAYLOAD_BYTE: u8 = 7;
const QUEUE_DEPTHS: [usize; 3] = [1, 32, 256];

struct Pair {
  sender: Resource,
  receiver: Resource,
  send_buf: Rc<RefCell<Option<Vec<u8>>>>,
  recv_buf: Rc<RefCell<Option<Vec<u8>>>>,
}

struct Transfer {
  lio: Lio,
  pairs: Vec<Pair>,
  done: Rc<Cell<usize>>,
}

impl Transfer {
  fn new(lio: Lio, depth: usize) -> Self {
    let pairs = (0..depth)
      .map(|_| {
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();
        receiver.connect(sender.local_addr().unwrap()).unwrap();
        sender.set_nonblocking(true).unwrap();
        receiver.set_nonblocking(true).unwrap();
        Pair {
          sender: into_resource(sender),
          receiver: into_resource(receiver),
          send_buf: Rc::new(RefCell::new(Some(vec![PAYLOAD_BYTE; PAYLOAD]))),
          recv_buf: Rc::new(RefCell::new(Some(vec![0; PAYLOAD]))),
        }
      })
      .collect();
    Self { lio, pairs, done: Rc::new(Cell::new(0)) }
  }

  fn batch(&self) {
    self.done.set(0);
    for pair in &self.pairs {
      let done = Rc::clone(&self.done);
      let slot = Rc::clone(&pair.recv_buf);
      let buf = slot.borrow_mut().take().unwrap();
      api::recv(&pair.receiver, buf, None).with_lio(&self.lio).when_done(
        move |(result, buf)| {
          assert_eq!(result.unwrap(), PAYLOAD);
          assert_eq!(buf[0], PAYLOAD_BYTE);
          assert_eq!(buf[PAYLOAD - 1], PAYLOAD_BYTE);
          *slot.borrow_mut() = Some(buf);
          done.set(done.get() + 1);
        },
      );

      let done = Rc::clone(&self.done);
      let slot = Rc::clone(&pair.send_buf);
      let buf = slot.borrow_mut().take().unwrap();
      api::send(&pair.sender, buf, None).with_lio(&self.lio).when_done(
        move |(result, buf)| {
          assert_eq!(result.unwrap(), PAYLOAD);
          *slot.borrow_mut() = Some(buf);
          done.set(done.get() + 1);
        },
      );
    }
    let pending = self.pairs.len() * 2;
    while self.done.get() < pending {
      self.lio.run().unwrap();
    }
  }
}

fn bench_udp<B, F>(criterion: &mut Criterion, name: &str, make: F) -> bool
where
  B: IoBackend + 'static,
  F: Fn() -> B,
{
  if let Err(err) = Lio::new_with_backend(make(), 2) {
    eprintln!("backends: skipping {name}: {err}");
    return false;
  }

  let mut group =
    criterion.benchmark_group(format!("backend/{name}/udp_{PAYLOAD}"));
  for depth in QUEUE_DEPTHS {
    let lio = Lio::new_with_backend(make(), depth * 2).unwrap();
    let transfer = Transfer::new(lio, depth);
    for _ in 0..8 {
      transfer.batch();
    }
    group.throughput(Throughput::Elements(depth as u64));
    group.bench_function(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      |bencher| bencher.iter(|| transfer.batch()),
    );
  }
  group.finish();
  true
}

fn main() {
  let mut criterion = Criterion::default()
    .sample_size(50)
    .warm_up_time(Duration::from_secs(1))
    .measurement_time(Duration::from_secs(3))
    .configure_from_args();

  let ran = run_available(&mut criterion);
  if !ran {
    eprintln!(
      "backends: no IoBackend ran. io_uring is Linux, the poller is Linux, \
       macOS, and the BSDs, and IOCP is Windows."
    );
  }
  criterion.final_summary();
}

fn run_available(criterion: &mut Criterion) -> bool {
  let mut ran = false;
  #[cfg(all(feature = "backend_impls", target_os = "linux"))]
  {
    ran |= bench_udp(criterion, "io_uring", lio::backend::impls::IoUring::new);
  }
  #[cfg(all(
    feature = "backend_impls",
    any(
      target_os = "linux",
      target_os = "macos",
      target_os = "ios",
      target_os = "tvos",
      target_os = "watchos",
      target_os = "freebsd",
      target_os = "dragonfly",
      target_os = "openbsd",
      target_os = "netbsd"
    )
  ))]
  {
    ran |= bench_udp(criterion, "poller", lio::backend::impls::Poller::new);
  }
  #[cfg(all(feature = "backend_impls", windows))]
  {
    ran |= bench_udp(criterion, "iocp", lio::backend::impls::Iocp::new);
  }
  ran
}

fn into_resource(socket: UdpSocket) -> Resource {
  #[cfg(unix)]
  {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    // SAFETY: the socket hands its owned descriptor to exactly one Resource.
    Resource::from(unsafe { OwnedFd::from_raw_fd(socket.into_raw_fd()) })
  }
  #[cfg(windows)]
  {
    use std::os::windows::io::{FromRawHandle, IntoRawSocket, OwnedHandle};
    // SAFETY: the socket hands its owned handle to exactly one Resource.
    Resource::from(unsafe {
      OwnedHandle::from_raw_handle(socket.into_raw_socket() as _)
    })
  }
}
