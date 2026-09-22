# Opt-in deferred io_uring completions

## Result

On Linux via `orb`, batches of 256 real, cached 4 KiB reads took **31–33% less
time** with deferred completion processing. This is an opt-in backend mode;
`Lio::new()` and `IoUring::new()` retain their original kernel setup.

| Paired experiment | Default per batch | Deferred per batch | Time reduction, paired-bootstrap 95% CI |
| --- | ---: | ---: | ---: |
| First run | 242.681 us | 162.896 us | 32.88% (31.71–33.67%) |
| Repeat, reversed construction order | 232.307 us | 160.508 us | 30.91% (29.91–31.47%) |

Deferred mode won all 20 paired samples in each run (40/40 total). The two-sided
sign-test p-value is 0.00000191 within each run. These are substantial effects,
not merely tiny differences detected by many measurements. The repeat implies
about 45% greater read throughput for this workload.

This is **not** a universal improvement. At depth 32, reads were 3.07% slower
in the first run and 7.92% slower in the repeat. That tradeoff is why the default
is unchanged. No claim is made about cold storage, remote filesystems, production
workload prevalence, or applications that never sustain this concurrency.

## Use

```rust
use lio::{Lio, backend::impls::IoUring};

let lio = Lio::new_with_backend(IoUring::with_deferred_completions(), 256)?;
```

The opt-in requests COOP_TASKRUN, TASKRUN_FLAG, SINGLE_ISSUER and DEFER_TASKRUN.
The kernel can finish submission batches without repeatedly running completion
work in the middle; lio explicitly processes that work when checking the ring.
TASKRUN_FLAG preserves progress through liburing's nonblocking peek path.
The backend is thread-local and cannot be transferred between threads.
See the upstream [liburing setup documentation](https://github.com/axboe/liburing/blob/master/man/io_uring_setup.2).

Linux 6.1+ is needed for these flags. If setup returns EINVAL (as on older
kernels), initialization retries with the original flag-free setup. Other
errors are propagated. No polling thread, busy-wait loop, new syscall in the
platform-independent driver, or Op contract change was introduced.

## Workload and method

Apple M4 Pro host, aarch64 Linux VM, kernel
`7.0.14-orbstack-00380-ga7e0a2dc9535`, Rust 1.98.1, optimized release build.
The file is on Linux `/tmp` (tmpfs), **not** the macOS shared filesystem.

The ignored `paired_performance` test constructs a default and a deferred
backend in the same executable. Both run the same public `api::read_at`
workload, reusable buffers, completion callbacks, byte-count checks, and
full-payload comparisons. Every measured read verifies all 4,096 bytes.
The high-depth cases alone performed over eight million verified reads across
the two experiments. There is no mock backend or custom OpModel.

There are 20 paired samples per case, alternating AB/BA execution order.
Iterations per sample are calibrated toward 100 ms using the default mode and
then held equal for both modes. Allocation of fixtures and drivers is outside
timing; scheduling, kernel I/O, callbacks, and validation are inside timing.
No builds, other tests, or other benchmarks ran concurrently.

The first paired run warmed up with 100 batches per mode. The final harness
uses one second of warmup per case; the repeat uses this version and constructs
the deferred workload first to check allocation/construction-order sensitivity.
Both modes in each comparison share one executable and code path. Do not
compare absolute times between different binaries or harness versions.

The file/socket fixtures are shared with the standalone Criterion benchmarks
under `benches/support/`. The existing submission benchmark now validates whole
read payloads as well as byte counts; its historical results measured byte
counts only and should not be directly compared with the new fixture.

## Controls

Positive values below mean less time; negative values mean a slowdown.

| Workload | First run | Repeat |
| --- | ---: | ---: |
| NOP, depth 1 | -0.77% | -0.11% |
| NOP, depth 32 | +0.57% | +0.76% |
| NOP, depth 256 | +0.46% | -1.04% |
| 4 KiB read, depth 1 | +1.06% | +1.42% |
| 4 KiB read, depth 32 | -3.07% | -7.92% |
| 4 KiB read, depth 256 | +32.88% | +30.91% |
| 64 B UDP, one transfer | +2.34% | +5.82% |
| 64 B UDP, 32 transfers | +0.49% | -1.06% |
| 4 KiB UDP, one transfer | +2.04% | +1.82% |
| 4 KiB UDP, 32 transfers | +0.76% | -1.26% |

The large NOP and UDP regressions from initial separate-executable trials did
not reproduce at that magnitude in the paired design. Small controls remain
sensitive to layout and machine conditions; the retained result is the large,
repeatable high-depth read benefit, not a universal speedup.

## Reproduction and raw evidence

```sh
orb /home/vt/.cargo/bin/cargo test -p lio --lib --release \
  paired_performance --target-dir /home/vt/lio-target \
  -- --ignored --nocapture

orb env LIO_BENCH_REVERSE=1 /home/vt/.cargo/bin/cargo test -p lio --lib --release \
  paired_performance --target-dir /home/vt/lio-target \
  -- --ignored --nocapture

python3 lio/benches/analyze_deferred.py \
  lio/benches/deferred-first.csv lio/benches/deferred-repeat.csv
```

Optional `LIO_BENCH_FILTER=read_4k` selects just the read cases. Output lines
beginning `PAIR,` contain workload, sample, iterations, default ns/batch and
deferred ns/batch. The supplied CSVs preserve all 200 pairs from each run.
The analysis script uses 10,000 bootstrap resamples of whole pairs, seed 1337,
and an exact two-sided sign test. No outliers were removed.

## Other candidates tested

- COOP_TASKRUN + TASKRUN_FLAG without deferral: no useful read improvement.
- Direct single-buffer READ/WRITE opcodes: about 2% at depth 256, negligible
  at depth 1; reverted.
- Unconditional deferred mode: rejected as the default because gains depend
  materially on workload and queue depth. The opt-in is the retained change.

## Validation

- Full Linux `cargo test --no-fail-fast`: passed, including 222 unit tests
  (one manual performance test ignored), 130 external backend-contract tests,
  129 operation tests, other integration suites and doctests.
- The unit tests include all 65 backend-contract cases instantiated with the
  deferred constructor, in addition to the existing default-mode suites.
- Focused tests verify EINVAL fallback, propagation of other setup errors, and
  256 real file completions driven exclusively by zero-timeout waits, with
  full payload checks and no duplicate completions.
- `cargo test -p lio-uring`: 68 unit tests, 77 integration tests and one doctest
  passed (one doctest ignored). Setup-flag composition is tested explicitly.
- Full native macOS `cargo test --no-fail-fast`: passed, including 150 unit
  tests, 65 backend-contract tests, 119 operation tests (one ignored), other
  integration suites and doctests.
- The new usage doctest and compile-fail check preventing cross-thread backend
  transfer both passed on Linux.

Earlier baseline investigation encountered unrelated intermittent Linux
bind/listen/shutdown failures. They did not reproduce in these final runs;
this change does not claim to fix them.
