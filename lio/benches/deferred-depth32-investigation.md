# Deferred completion depth-32 investigation

Linux Orb VM, 2026-09-11. Diagnostic changes only; production scheduling and
the opt-in/default selection were not changed during this investigation.

## Reproduction without tracing

Same-executable paired benchmark, reversed driver construction, alternating
AB/BA timing, 20 pairs of 3,750 batches per mode. Every batch reads and verifies
32 x 4 KiB using the public lio API.

- Default: 48.244 us/batch.
- Deferred: 50.017 us/batch.
- Deferred is 3.67% slower; paired bootstrap 95% interval 1.86–5.46% slower.
- Deferred wins 2/20 pairs; two-sided sign-test p=0.000402.
- Raw data: `deferred-depth32-investigation.csv`.

Reproduce using `orb env LIO_BENCH_FILTER=read_4k/32 LIO_BENCH_REVERSE=1
/home/vt/.cargo/bin/cargo test -p lio --lib --release paired_performance
--target-dir /home/vt/lio-target -- --ignored --nocapture` (one shell line).
Analyze with `python3 lio/benches/analyze_deferred.py
lio/benches/deferred-depth32-investigation.csv`.

## Confirmed mechanism: nonblocking checks can enter the kernel

The backend's `wait` drains completions through repeated `LioUring::try_wait`.
That calls vendored liburing's `io_uring_peek_cqe`. If its initial CQ lookup is
empty, it calls `io_uring_wait_cqe_nr(..., 0)`. In `src/queue.c`, pending
`IORING_SQ_TASKRUN` makes this issue `io_uring_enter(GETEVENTS)` despite a zero
minimum completion count. Consequently one backend wait can contain multiple
kernel entries. Counting backend waits alone misses this cost.

An isolated syscall-only ftrace instance recorded the following in the marked
100-batch measurement interval (warmup excluded):

| Depth / mode | Submit-and-wait entries | Zero-minimum completion entries | Other blocking entries |
| --- | ---: | ---: | ---: |
| 32 default | 100 | 0 | 0 |
| 32 deferred | 100 | 245 | 2 |
| 256 default | 100 | 0 | 1 |
| 256 deferred | 100 | 6,157 | 4 |

At depth 32, the 245 nonblocking entries spent an aggregate 188 us inside the
traced syscall boundaries, or 1.88 us/batch. This is an observed cost, **not** a
causal estimate of how much faster removing them would make the workload.
The submitting syscalls were also 1.54 us/batch shorter with deferred mode.
These numbers do not account for the entire regression. Timestamps have
microsecond resolution, and tracing affects execution.

A separate detailed trace confirmed all 3,200 reads in each mode were submitted,
queued to io-wq workers, and completed with result 4096. The fixture is cached
on Linux `/tmp` (tmpfs), not physical-disk I/O. In that trace, default CQ completion
events ran on an io-wq worker, while deferred CQ completion events ran on the
application thread. Default had 572 task-work-run events; deferred had 299
local-work-run events. Thus deferred mode changes the execution context and
grouping of completion work, not the number of requested reads.

## What this does and does not establish

The depth-32 slowdown reproduces independently of tracing. An extra completion
processing/kernel-entry cost is real and its call path is identified. The earlier
explanation that deferred delivery simply makes the application wait longer is
not established. Nor is the claim that it needs substantially more outer driver
waits: syscall-only tracing found 100 versus 102 such waits.

Extra entries cannot by themselves predict which batch size wins: depth 256
incurs many more, yet still wins. The remaining balance includes worker/application
overlap, completion execution context, scheduling and shared-ring interaction.
No controlled ablation has assigned the whole slowdown to any one of these.
In particular, there is no evidence here for a special threshold at exactly 32.

Detailed tracing actually reversed the depth-32 result (55.02 us default versus
38.31 us deferred), so its wall times must not be used as performance evidence.
Syscall-only tracing retained the direction (48.66 versus 51.70 us), but it too
is diagnostic, not the source of the reported untraced confidence interval.

## Diagnostic tools and artifacts

`trace_read_batches` is an ignored test wrapping the actual backend and shared
file fixture, with per-wait counters and optional start/end trace markers.
`trace_deferred.py` creates and removes its own ftrace instance; it never changes
the global tracing session. `analyze_deferred_trace.py` counts marked events and
matches syscall entry/exit intervals.

Example (run from the repo; choose a fresh output filename):

```sh
orb sudo env LIO_TRACE_SYSCALLS_ONLY=1 python3 lio/benches/trace_deferred.py /home/vt/lio-target/example.trace /home/vt/lio-target/release/deps/lio-055db758af0edc4e --deferred 32
orb python3 lio/benches/analyze_deferred_trace.py /home/vt/lio-target/example.trace
```

Omit `LIO_TRACE_SYSCALLS_ONLY` for detailed events. The executable hash may change
after rebuilding. Existing VM traces are under `/home/vt/lio-target/`:
`depth32-{default,deferred}.trace` and
`depth{32,256}-{default,deferred}-syscalls.trace`.

The diagnostic test compiled and passed in Linux release mode, including full
payload validation. No production code was changed by this diagnosis; it does
not constitute a new optimization or justify enabling deferred mode by default.
