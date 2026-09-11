//! Actual default-backend submission batches. On Linux this uses io_uring.
//! Run with --allocations for counts, or Criterion arguments for timing.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use lio::{Lio, api};

struct CountingAllocator;
static COUNT: AtomicBool = AtomicBool::new(false);
static ALLOC: AtomicUsize = AtomicUsize::new(0);
static REALLOC: AtomicUsize = AtomicUsize::new(0);
static DEALLOC: AtomicUsize = AtomicUsize::new(0);

// SAFETY: all allocation operations delegate unchanged to System.
unsafe impl GlobalAlloc for CountingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    if COUNT.load(Relaxed) {
      ALLOC.fetch_add(1, Relaxed);
    }
    // SAFETY: caller supplies the allocation contract required by System.
    unsafe { System.alloc(layout) }
  }
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    if COUNT.load(Relaxed) {
      ALLOC.fetch_add(1, Relaxed);
    }
    // SAFETY: caller supplies the allocation contract required by System.
    unsafe { System.alloc_zeroed(layout) }
  }
  unsafe fn realloc(
    &self,
    ptr: *mut u8,
    layout: Layout,
    size: usize,
  ) -> *mut u8 {
    if COUNT.load(Relaxed) {
      REALLOC.fetch_add(1, Relaxed);
    }
    // SAFETY: forwards the caller's valid allocation and requested size.
    unsafe { System.realloc(ptr, layout, size) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    if COUNT.load(Relaxed) {
      DEALLOC.fetch_add(1, Relaxed);
    }
    // SAFETY: forwards the caller's allocation with its original layout.
    unsafe { System.dealloc(ptr, layout) }
  }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct Workload {
  lio: Lio,
  file: api::resource::Resource,
  buffers: Rc<RefCell<Vec<Vec<u8>>>>,
  done: Rc<Cell<usize>>,
  depth: usize,
  read: bool,
}

impl Workload {
  fn new(depth: usize, read: bool, path: &std::path::Path) -> Self {
    let lio = Lio::new(depth).unwrap();
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

  fn batch(&self) {
    self.done.set(0);
    for _ in 0..self.depth {
      let done = Rc::clone(&self.done);
      if self.read {
        let buffer = self.buffers.borrow_mut().pop().unwrap();
        let buffers = Rc::clone(&self.buffers);
        api::read_at(&self.file, buffer, 0).with_lio(&self.lio).when_done(
          move |(result, buffer)| {
            assert_eq!(result.unwrap(), 4096);
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

fn main() {
  // Cached real-file reads, with the fixture and reusable buffers outside timing.
  let path = std::env::temp_dir()
    .join(format!("lio-submission-{}.bin", std::process::id()));
  std::fs::write(&path, vec![7u8; 4096]).unwrap();
  if std::env::args().any(|arg| arg == "--allocations") {
    for read in [false, true] {
      for depth in [1, 32, 256] {
        let workload = Workload::new(depth, read, &path);
        for _ in 0..100 {
          workload.batch();
        }
        ALLOC.store(0, Relaxed);
        REALLOC.store(0, Relaxed);
        DEALLOC.store(0, Relaxed);
        COUNT.store(true, Relaxed);
        for _ in 0..1000 {
          workload.batch();
        }
        COUNT.store(false, Relaxed);
        println!(
          "{} qd={depth} batches=1000 alloc={} realloc={} dealloc={}",
          if read { "read_4k" } else { "nop" },
          ALLOC.load(Relaxed),
          REALLOC.load(Relaxed),
          DEALLOC.load(Relaxed)
        );
      }
    }
  } else {
    let mut criterion = Criterion::default()
      .sample_size(50)
      .warm_up_time(Duration::from_secs(1))
      .measurement_time(Duration::from_secs(3))
      .configure_from_args();
    for read in [false, true] {
      let mut group = criterion.benchmark_group(if read {
        "submission/read_4k"
      } else {
        "submission/nop"
      });
      for depth in [1, 32, 256] {
        let workload = Workload::new(depth, read, &path);
        for _ in 0..100 {
          workload.batch();
        }
        group.throughput(Throughput::Elements(depth as u64));
        group.bench_function(BenchmarkId::from_parameter(depth), |b| {
          b.iter(|| workload.batch())
        });
      }
      group.finish();
    }
    criterion.final_summary();
  }
  std::fs::remove_file(path).unwrap();
}
