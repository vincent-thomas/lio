//! One operation, every real [`IoBackend`](lio::backend::IoBackend) this host
//! can run.
//!
//! Each group is a batch of `qd` cached 4 KiB reads of the same temp file,
//! through `Lio` on that backend. The file is written and opened before
//! timing, and a few batches run first so the page cache is warm. Throughput
//! is reads per second. The time includes the kernel: submission, the read
//! itself, and completion. It is not the bookkeeping-only number from
//! `cargo bench -p lio --bench bookkeeping`.
//!
//! The operation is the same. The kernel path is the one that backend uses:
//!
//! - `backend/io_uring/read_4kib` — Linux. A read SQE on io_uring.
//! - `backend/poller/read_4kib` — Linux (epoll), macOS, and the BSDs
//!   (kqueue). A regular-file read is a `pread` inside `flush`; the poller
//!   does not wait on the descriptor.
//! - `backend/iocp/read_4kib` — Windows. The read is offloaded to a thread
//!   and completed through IOCP.
//!
//! A backend the host OS does not implement is left out of the binary. One
//! this OS implements but cannot initialize (for example io_uring blocked by
//! the sandbox) is skipped with a message, and the others still run.
//!
//! ```text
//! cargo bench -p lio --bench backends
//! mise run bench
//! ```
//!
//! `mise run bench` runs bookkeeping first, then this file. Criterion flags
//! go after `--`. To run a backend this machine cannot: io_uring on Linux,
//! the poller on Linux, macOS, or a BSD, and IOCP on Windows.

use std::path::Path;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput};
use lio::Lio;
use lio::backend::IoBackend;

#[path = "support/file_workload.rs"]
mod file_workload;
use file_workload::Workload;

const QUEUE_DEPTHS: [usize; 3] = [1, 32, 256];

fn bench_read<B, F>(
  criterion: &mut Criterion,
  name: &str,
  make: F,
  path: &Path,
) -> bool
where
  B: IoBackend + 'static,
  F: Fn() -> B,
{
  if let Err(err) = Lio::new_with_backend(make(), 1) {
    eprintln!("backends: skipping {name}: {err}");
    return false;
  }

  let mut group =
    criterion.benchmark_group(format!("backend/{name}/read_4kib"));
  for depth in QUEUE_DEPTHS {
    let lio = Lio::new_with_backend(make(), depth).unwrap();
    let workload = Workload::new(lio, depth, true, path);
    for _ in 0..8 {
      workload.batch();
    }
    group.throughput(Throughput::Elements(depth as u64));
    group.bench_function(
      BenchmarkId::from_parameter(format!("qd_{depth}")),
      |bencher| bencher.iter(|| workload.batch()),
    );
  }
  group.finish();
  true
}

fn main() {
  let path = std::env::temp_dir()
    .join(format!("lio-backend-bench-{}.bin", std::process::id()));
  std::fs::write(&path, vec![7u8; 4096]).unwrap();

  let mut criterion = Criterion::default()
    .sample_size(50)
    .warm_up_time(Duration::from_secs(1))
    .measurement_time(Duration::from_secs(3))
    .configure_from_args();

  let ran = run_available(&mut criterion, &path);
  if !ran {
    eprintln!(
      "backends: no IoBackend ran. io_uring is Linux, the poller is Linux, \
       macOS, and the BSDs, and IOCP is Windows."
    );
  }
  criterion.final_summary();
  std::fs::remove_file(&path).unwrap();
}

fn run_available(criterion: &mut Criterion, path: &Path) -> bool {
  let mut ran = false;
  #[cfg(all(feature = "backend_impls", target_os = "linux"))]
  {
    ran |= bench_read(
      criterion,
      "io_uring",
      lio::backend::impls::IoUring::new,
      path,
    );
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
    ran |=
      bench_read(criterion, "poller", lio::backend::impls::Poller::new, path);
  }
  #[cfg(all(feature = "backend_impls", windows))]
  {
    ran |= bench_read(criterion, "iocp", lio::backend::impls::Iocp::new, path);
  }
  ran
}
