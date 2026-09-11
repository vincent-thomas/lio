# io_uring single-buffer socket submission

## Change and scope

The io_uring backend now lowers a one-buffer `Op::Send` with no destination
address to `IORING_OP_SEND`, and a one-buffer `Op::Recv` with no source-address
output to `IORING_OP_RECV`. It copies only the buffer pointer and checked u32
length into pending state. Previously both cases built and initialized a
16-entry native iovec array, address storage, and an msghdr in the operation's
arena, then submitted SENDMSG/RECVMSG.

This is the path used by ordinary `api::send`/`api::recv` with a `Vec<u8>`,
including the socket wrappers. It does not require a custom OpModel or a
particular future-waker pattern. Multiple buffers, explicit addresses, and
lengths exceeding u32 retain the general message path. Flags are forwarded
unchanged. The public Op contract and platform-independent driver are unchanged.

Initialization probes kernel support for both opcodes. Probe failure or missing
support retains the old path. This costs one probe allocation/free and one
registration syscall per driver initialization, outside the timed loop. There
is no additional thread, spinning, or kernel setup flag. No claim is made that
every application benefits: the measured workload is local UDP, not a deployed
service or remote-network workload.

## Measurement

Linux via `orb`, Apple M4 Pro host, aarch64 Linux kernel
`7.0.14-orbstack-00380-ga7e0a2dc9535`, Rust 1.98.1, optimized bench profile.
`socket_scheduling.rs` uses the public API with `Lio::new` when
`LIO_BENCH_DEFAULT_BACKEND=1`; on Linux this selects io_uring.

Each timed iteration submits one send and receive per connected UDP socket
pair, runs the driver until all complete, and checks byte counts and received
payloads. Buffers are reused; setup is excluded. A transfer is a send plus its
matching receive, not a round trip. No NOP or mock backend is used here.
Criterion uses 50 samples, one second warmup, and three seconds measurement.
No builds or tests run concurrently with benchmarks.

The unchanged original io_uring implementation from commit `1e7a5d1` was built
with the same benchmark and saved as a separate executable. Other dirty-tree
changes were identical on both sides. The saved original was rerun immediately
before the final candidate, which includes kernel probing and copied buffer
metadata. Criterion's repeated-comparison results:

| Payload | Transfers/batch | Original | Candidate | Time reduction (95% CI) |
| --- | ---: | ---: | ---: | ---: |
| 64 B | 1 | 2.0730 us | 1.7843 us | 13.92% (13.71–14.18%) |
| 64 B | 32 | 47.236 us | 43.020 us | 8.99% (8.87–9.10%) |
| 4 KiB | 1 | 4.3048 us | 4.1447 us | 3.49% (3.33–3.65%) |
| 4 KiB | 32 | 113.76 us | 108.36 us | 4.51% (4.26–4.73%) |

All four comparisons have p < 0.05. The 64-byte cases are well beyond
Criterion's 1% noise threshold. The initial prototype also improved all four
cases (8.6%, 10.0%, 4.2%, and 5.1%), but those figures are not substituted for
the final implementation above. Percentage change estimates need not exactly
equal ratios of displayed time point estimates.

A later rerun of the same final executable against `uring_socket_repeat`
again improved all four cases (17.19%, 12.31%, 8.52%, and 5.40%). Because the
baseline was not refreshed for that additional run, the table retains the
more conservative immediately adjacent A/B comparison rather than its peaks.

## Controls and tradeoffs

The unchanged `submission.rs` benchmark was also run against a saved original
binary, with baseline `uring_control_before`:

| Control | Original | Candidate | Criterion time change |
| --- | ---: | ---: | ---: |
| NOP, depth 1 | 62.736 ns | 64.811 ns | +3.09% |
| NOP, depth 32 | 1.2124 us | 1.2311 us | +1.63% |
| NOP, depth 256 | 10.719 us | 9.1414 us | -14.82% |
| Cached 4 KiB read, depth 1 | 28.678 us | 28.239 us | -1.97% |
| Cached 4 KiB read, depth 32 | 45.328 us | 45.144 us | no significant change |
| Cached 4 KiB read, depth 256 | 206.90 us | 214.74 us | +3.48% |

The largest read case was noisy and was repeated immediately in a fresh A/B
comparison (`uring_read_repeat`). It then measured 209.23 us before versus
206.44 us after, reversing the direction. Its outlier-sensitive change CI was
wide (-16.79% to -2.06%). These read results do not establish a consistent gain
or regression; they should not be sold as a file-I/O improvement.

The small NOP regressions are real in this run (about 2 ns per individual NOP
and 19 ns per batch of 32). This is a socket-specific improvement, not a claim
of no costs or a universally faster scheduler. The decision to retain it rests
on the repeated, checked real-I/O results above, not on the favorable NOP case.

## Reproduction

Run the same benchmark file on the original and modified backend, saving the
baseline before applying the implementation changes:

```sh
orb env LIO_BENCH_DEFAULT_BACKEND=1 /home/vt/.cargo/bin/cargo bench \
  -p lio --bench socket_scheduling --target-dir /home/vt/lio-target \
  -- --save-baseline uring_socket_before

orb env LIO_BENCH_DEFAULT_BACKEND=1 /home/vt/.cargo/bin/cargo bench \
  -p lio --bench socket_scheduling --target-dir /home/vt/lio-target \
  -- --baseline uring_socket_before
```

The repeated comparison used saved executables `uring-socket-before` and
`uring-socket-after` in `/home/vt/lio-target`, with `--bench`, and baseline
`uring_socket_repeat`. Raw Criterion samples and confidence estimates are in
`target/criterion/default_udp_loopback/` in this workspace.

## Rejected candidates in this search

- Adding release optimization to liburing C helpers: small, variable read
  gains and regressed NOP controls; reverted.
- Specializing the profiling-disabled io_uring flush: faster NOP bookkeeping,
  no useful file-read gain, and only 1–3% on UDP; reverted.
- Batch completion copying/CQ advancement: about 4.5% on small UDP batches,
  but regressed single-transfer latency; reverted.

Only the single-buffer socket candidate remains from this search.

## Validation

- `orb /home/vt/.cargo/bin/cargo test --no-fail-fast --target-dir /home/vt/lio-target`:
  passed, including 154 unit tests, 130 backend-contract tests, 129 operation
  tests, remaining integration suites, and doctests.
- `orb /home/vt/.cargo/bin/cargo test -p lio-uring --target-dir /home/vt/lio-target`:
  passed: 67 unit tests, 77 integration tests, and one doctest (one ignored).
- Native macOS `cargo test --no-fail-fast`: passed, including 150 unit tests,
  65 backend-contract tests, 119 operation tests (one ignored), remaining
  integration suites, and doctests.
- Focused new tests cover zero arena allocation for single-buffer lowering,
  unsupported-kernel fallback, addressed/vectored fallback, lengths exceeding
  u32, and actual UDP MSG_PEEK/MSG_TRUNC and zero-length behavior compared with
  the general path. The completion helper also checks exactly-once delivery.
  Existing contract tests cover pending receives, EOF, invalid descriptors,
  and vectored transfer byte counts.

Earlier baseline Linux runs in this investigation had nine failures in
bind/listen/shutdown networking tests. Those did not reproduce in either full
Linux run of this candidate. This change does not intentionally fix those
unrelated failures; passing these runs is not evidence that their underlying
cause has been resolved.
