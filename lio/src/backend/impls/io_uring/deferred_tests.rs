use super::*;
use crate::api::resource::Resource;
use crate::backend::op::{RawBuf, ReadFlags};
use std::os::fd::{FromRawFd, IntoRawFd};

mod contract {
  lio_test::test_io_backend!(
    lio,
    lio::backend::impls::IoUring::with_deferred_completions()
  );
}

#[path = "../../../../benches/support/file_workload.rs"]
mod file_workload;
#[path = "../../../../benches/support/socket_workload.rs"]
mod socket_workload;

fn driver(deferred: bool, capacity: usize) -> crate::Lio {
  let backend = if deferred {
    IoUring::with_deferred_completions()
  } else {
    IoUring::new()
  };
  crate::Lio::new_with_backend(backend, capacity).unwrap()
}

fn compare(name: &str, mut baseline: impl FnMut(), mut deferred: impl FnMut()) {
  let filter = std::env::var("LIO_BENCH_FILTER").unwrap_or_default();
  if !name.contains(&filter) {
    return;
  }
  let warm_until = Instant::now() + Duration::from_secs(1);
  while Instant::now() < warm_until {
    baseline();
    deferred();
  }
  let start = Instant::now();
  for _ in 0..100 {
    baseline();
  }
  let per_batch = start.elapsed().as_nanos() / 100;
  let iterations =
    (100_000_000 / per_batch.max(1)).clamp(100, 2_000_000) as usize;
  let timed = |work: &mut dyn FnMut()| {
    let start = Instant::now();
    for _ in 0..iterations {
      work();
    }
    start.elapsed().as_nanos() as f64 / iterations as f64
  };
  // Alternate AB/BA order. Both drivers use the same executable, payload,
  // public API and validation, rather than comparing unrelated code layouts.
  for sample in 0..20 {
    let (before, after) = if sample % 2 == 0 {
      let before = timed(&mut baseline);
      (before, timed(&mut deferred))
    } else {
      let after = timed(&mut deferred);
      (timed(&mut baseline), after)
    };
    println!("PAIR,{name},{sample},{iterations},{before:.3},{after:.3}");
  }
}

fn workloads<T>(mut build: impl FnMut(bool) -> T) -> (T, T) {
  if std::env::var_os("LIO_BENCH_REVERSE").is_some() {
    let deferred = build(true);
    (build(false), deferred)
  } else {
    let baseline = build(false);
    (baseline, build(true))
  }
}

#[test]
#[ignore = "isolated diagnostic workload for tracing; not a performance assertion"]
fn trace_read_batches() {
  use std::cell::RefCell;
  use std::rc::Rc;
  #[derive(Default)]
  struct Stats {
    waits: usize,
    wait_ns: u128,
    flush_ns: u128,
    chunks: Vec<usize>,
  }
  struct Observed {
    inner: IoUring,
    stats: Rc<RefCell<Stats>>,
  }
  impl IoBackend for Observed {
    fn init(&mut self, cap: usize) -> io::Result<()> {
      self.inner.init(cap)
    }
    fn push(&mut self, id: u64, op: Op, bump: &mut Bump) {
      self.inner.push(id, op, bump)
    }
    fn flush(&mut self) -> io::Result<()> {
      let t = Instant::now();
      let result = self.inner.flush();
      self.stats.borrow_mut().flush_ns += t.elapsed().as_nanos();
      result
    }
    fn wait(
      &mut self,
      timeout: Option<Duration>,
      completed: &mut Vec<OpCompleted>,
    ) -> io::Result<()> {
      let t = Instant::now();
      let result = self.inner.wait(timeout, completed);
      let elapsed = t.elapsed().as_nanos();
      let mut stats = self.stats.borrow_mut();
      stats.waits += 1;
      stats.wait_ns += elapsed;
      stats.chunks[completed.len()] += 1;
      result
    }
  }
  let depth: usize =
    std::env::var("LIO_DIAG_DEPTH").unwrap_or("32".into()).parse().unwrap();
  let batches: usize =
    std::env::var("LIO_DIAG_BATCHES").unwrap_or("1000".into()).parse().unwrap();
  let deferred = std::env::var_os("LIO_DIAG_DEFERRED").is_some();
  let stats = Rc::new(RefCell::new(Stats {
    chunks: vec![0; depth + 1],
    ..Default::default()
  }));
  let backend = if deferred {
    IoUring::with_deferred_completions()
  } else {
    IoUring::new()
  };
  let lio = crate::Lio::new_with_backend(
    Observed { inner: backend, stats: stats.clone() },
    depth,
  )
  .unwrap();
  let path =
    std::env::temp_dir().join(format!("lio-trace-{}", std::process::id()));
  std::fs::write(&path, vec![7u8; 4096]).unwrap();
  let workload = file_workload::Workload::new(lio, depth, true, &path);
  for _ in 0..2000 {
    workload.batch();
  }
  *stats.borrow_mut() =
    Stats { chunks: vec![0; depth + 1], ..Default::default() };
  let marker = std::env::var_os("LIO_TRACE_MARKER");
  if let Some(marker) = &marker {
    std::fs::write(marker, "lio_start\n").unwrap();
  }
  let start = Instant::now();
  for _ in 0..batches {
    workload.batch();
  }
  let elapsed = start.elapsed().as_nanos();
  if let Some(marker) = &marker {
    std::fs::write(marker, "lio_end\n").unwrap();
  }
  let stats = stats.borrow();
  println!(
    "DIAG deferred={deferred} depth={depth} batches={batches} total_ns={elapsed} flush_ns={} wait_ns={} waits={} chunks={:?}",
    stats.flush_ns, stats.wait_ns, stats.waits, stats.chunks
  );
  std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "manual release-mode paired real-I/O performance measurement"]
fn paired_performance() {
  let path =
    std::env::temp_dir().join(format!("lio-defer-perf-{}", std::process::id()));
  std::fs::write(&path, vec![7u8; 4096]).unwrap();
  for read in [false, true] {
    for depth in [1, 32, 256] {
      let (before, after) = workloads(|deferred| {
        file_workload::Workload::new(
          driver(deferred, depth),
          depth,
          read,
          &path,
        )
      });
      compare(
        &format!("{}/{depth}", if read { "read_4k" } else { "nop" }),
        || before.batch(),
        || after.batch(),
      );
    }
  }
  std::fs::remove_file(path).unwrap();
  for bytes in [64, 4096] {
    for depth in [1, 32] {
      let (before, after) = workloads(|deferred| {
        socket_workload::Workload::new(
          driver(deferred, depth * 2),
          depth,
          bytes,
        )
      });
      compare(
        &format!("udp_{bytes}/{depth}"),
        || before.batch(),
        || after.batch(),
      );
    }
  }
}

#[test]
fn unsupported_setup_flags_retry_without_flags() {
  let mut attempts = Vec::new();
  let ring = IoUring::create_ring_with(16, |params| {
    attempts.push((params.sq_entries, params.flags));
    if attempts.len() == 1 {
      Err(io::Error::from_raw_os_error(libc::EINVAL))
    } else {
      LioUring::with_params(params)
    }
  })
  .unwrap();
  assert_eq!(
    attempts,
    vec![(16, lio_uring::Params::default().deferred_taskrun().flags), (16, 0)]
  );
  drop(ring);
}

#[test]
fn other_setup_errors_are_not_retried() {
  let mut attempts = 0;
  let result = IoUring::create_ring_with(16, |_| {
    attempts += 1;
    Err(io::Error::from_raw_os_error(libc::ENOMEM))
  });
  assert_eq!(attempts, 1);
  assert_eq!(result.err().unwrap().raw_os_error(), Some(libc::ENOMEM));
}

#[test]
fn nonblocking_waits_make_progress_on_file_completions() {
  const DEPTH: usize = 256;
  let path = std::env::temp_dir()
    .join(format!("lio-deferred-read-{}", std::process::id()));
  std::fs::write(&path, vec![7u8; 4096]).unwrap();
  let file = std::fs::File::open(&path).unwrap();
  std::fs::remove_file(&path).unwrap();
  // SAFETY: transfers the file descriptor to its sole Resource owner.
  let file = unsafe { Resource::from_raw_fd(file.into_raw_fd()) };
  let mut backend = IoUring::with_deferred_completions();
  backend.init(DEPTH).unwrap();
  let mut arena = Bump::new();
  let mut buffers = vec![vec![0u8; 4096]; DEPTH];
  let raws: Vec<_> = buffers
    .iter_mut()
    .map(|buffer| {
      // SAFETY: buffers remain live and exclusively owned until all reads finish.
      unsafe { RawBuf::from_raw_parts(buffer.as_mut_ptr(), buffer.len()) }
    })
    .collect();
  for (id, raw) in raws.iter().enumerate() {
    backend.push(
      id as u64,
      Op::Read {
        fd: file.clone(),
        iovecs: NonNull::from(raw),
        iov_count: 1,
        offset: 0,
        flags: ReadFlags::EMPTY,
      },
      &mut arena,
    );
  }
  backend.flush().unwrap();
  let mut seen = [false; DEPTH];
  let mut count = 0;
  let mut completed = Vec::new();
  let deadline = Instant::now() + Duration::from_secs(5);
  while count < DEPTH {
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    for completion in &completed {
      let id = completion.registration_id() as usize;
      assert!(!seen[id], "duplicate completion");
      seen[id] = true;
      assert_eq!(completion.result(), 4096);
      count += 1;
    }
    assert!(
      Instant::now() < deadline,
      "nonblocking waits failed to make progress"
    );
  }
  assert!(buffers.iter().flatten().all(|&byte| byte == 7));
  backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
  assert!(completed.is_empty());
}
