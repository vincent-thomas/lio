# Poller socket scheduling: verified improvement

The retained change attempts sends and receives on nonblocking sockets during
`Poller::flush`, before registering readiness. Successful attempts and terminal
errors become immediate completions. `EAGAIN`/`EWOULDBLOCK` use the existing
readiness path. Blocking descriptors keep their prior behavior.

This applies to ordinary `api::send` and `api::recv` (also the corresponding
socket message operations) through Poller: the default backend on macOS, and
the explicitly selected polling backend on Linux. It does **not** change the
default Linux io_uring backend.

## Safety and scheduling behavior

The backend checks `O_NONBLOCK` using `fcntl(F_GETFL)` and adds the per-call
`MSG_DONTWAIT` flag to the immediate attempt. The latter avoids blocking if
another thread changes the descriptor flags after the check. These native
details stay inside the backend; the Op contract is unchanged. See the Linux
[receive](https://man7.org/linux/man-pages/man2/recv.2.html) and
[send](https://man7.org/linux/man-pages/man2/sendmsg.2.html) documentation for
per-call nonblocking behavior.

When immediate completions coexist with registered operations, `wait` also
polls pending readiness with a zero timeout. This prevents continuously ready
sends from starving older receives, without delaying ready results. If that
poll reports an infrastructure error, immediate results are retained for the
next call instead of being lost.

The tradeoff is an extra descriptor-status query and unsuccessful socket call
when a nonblocking socket is not ready. Blocking descriptors incur the status
query and retain the original readiness-first behavior. This is an optimization
for ready socket traffic, not a claim of a universal gain for idle or heavily
backpressured workloads.

## Benchmark

`socket_scheduling.rs` uses real connected loopback UDP socket pairs, ordinary
`api::send`/`api::recv`, and `Lio::new_with_backend(Poller::new(), ...)`.
Each iteration submits one send and receive per pair and drives the driver
until every operation completes. It verifies both byte counts and payload
contents. There is no mock backend, synthetic OpModel, or NOP in this benchmark.

Cases use 64-byte or 4,096-byte datagrams and either 1 or 32 independent socket
pairs. Socket creation and buffer allocation are outside measurement; buffers
are reused. One transfer means one send plus its matching receive, not a
round trip. The batch cases transfer one datagram on each of 32 pairs.

Criterion uses 50 samples, one second warmup, and three seconds measurement
per case. Builds, tests, and measurements were not run concurrently. All
comparisons use the identical benchmark on both implementations. Baseline
Poller source is from commit `3b3547d8bb0061d5cc9defd79659cdea8841b856`.
Earlier unrelated working-tree changes were identical on both sides.

## Linux via OrbStack

Apple M4 Pro host, aarch64 Linux VM, kernel
`7.0.14-orbstack-00380-ga7e0a2dc9535`, Rust 1.98.1, optimized bench profile.

The original executable was saved and run again after the first comparison.
The second comparison rebuilt the final implementation, including preservation
of immediate completions on poll errors. Times below are per batch; reductions
and their 95% confidence intervals are Criterion's change estimates.

| Datagram size | Transfers per batch | Original, repeat | Final, repeat | Time reduction (95% CI) |
| --- | ---: | ---: | ---: | ---: |
| 64 B | 1 | 5.1462 us | 1.7243 us | 66.33% (65.81–66.74%) |
| 64 B | 32 | 101.13 us | 58.185 us | 42.88% (42.58–43.25%) |
| 4 KiB | 1 | 7.6786 us | 4.0423 us | 46.83% (46.38–47.52%) |
| 4 KiB | 32 | 173.74 us | 123.37 us | 29.07% (28.85–29.28%) |

The first comparison reported 67.21%, 44.25%, 47.28%, and 28.73% reductions
respectively. All four cases were significant (p < 0.05) in both comparisons.
Criterion's change estimates need not equal ratios of the displayed time
point estimates.

### Syscall evidence

Separate `strace -c` runs used `--count`: 1,000 verified 64-byte transfers on
one pair. Counts include the same six setup calls in each executable.

| Measured syscall | Before | After |
| --- | ---: | ---: |
| epoll_ctl | 8,006 | 6 |
| epoll_pwait2 | 2,000 | 0 |
| fcntl | 0 | 2,000 |
| sendmsg | 1,000 | 1,000 |
| recvmsg | 1,000 | 1,000 |
| Total of these calls | 12,006 | 4,006 |

This confirms that the improvement removes readiness work while performing
the same socket transfers. The trace preceded the final poll-error handling
addition; this ready-only workload never enters that error path. Trace timings
were not used as performance measurements.

## Native macOS

Apple M4 Pro, Rust 1.94.1, optimized bench profile. The original Poller was
temporarily restored for the baseline, then the final implementation restored
and recompiled. This uses the backend selected by default on macOS.

| Datagram size | Transfers per batch | Original | Final | Time reduction (95% CI) |
| --- | ---: | ---: | ---: | ---: |
| 64 B | 1 | 14.121 us | 11.576 us | 18.52% (18.15–18.90%) |
| 64 B | 32 | 178.57 us | 171.80 us | 4.05% (3.68–4.47%) |
| 4 KiB | 1 | 16.777 us | 13.328 us | 20.80% (20.29–21.30%) |
| 4 KiB | 32 | 242.72 us | 235.80 us | 3.12% (2.87–3.39%) |

All four cases were significant (p < 0.05). Linux and macOS results should
not be directly compared: their readiness implementations, kernels, and Rust
versions differ, and Linux runs in a VM.

## Validation

Six new focused tests passed on both OSes: ready send, ready receive, unavailable
receive completing later, blocking descriptor behavior, backpressured send
retry, and fairness between immediate and previously registered completions.

Full `cargo test --no-fail-fast` passed on macOS, including 150 unit tests,
65 backend contract tests, integration tests, and doctests.

Linux: all 150 unit tests and 130 backend contract tests passed. Integration
tests and doctests ran with `--no-fail-fast`; the same nine pre-existing
bind/listen/shutdown failures remained in the default io_uring ops suite.
These were observed before this Poller change in the preceding investigation.
The overall Linux suite is therefore still not green.

## Reproduce

With the benchmark present and original Poller implementation:

```sh
orb /home/vt/.cargo/bin/cargo bench -p lio --bench socket_scheduling \
  --target-dir /home/vt/lio-target -- --save-baseline socket_before
```

With the change applied, use `--baseline socket_before` instead. The repeat
baseline is named `socket_repeat`. For native macOS omit `orb` and the custom
target directory and use `socket_mac_before` as the baseline name.

The saved Linux executables are `/home/vt/lio-target/socket-before` and
`/home/vt/lio-target/socket-after`. To reproduce the syscall counts:

```sh
orb strace -c -e trace=sendmsg,recvmsg,fcntl,epoll_ctl,epoll_wait,epoll_pwait,epoll_pwait2 \
  /home/vt/lio-target/socket-before --count
orb strace -c -e trace=sendmsg,recvmsg,fcntl,epoll_ctl,epoll_wait,epoll_pwait,epoll_pwait2 \
  /home/vt/lio-target/socket-after --count
```

Criterion data is under `target/criterion/poller_udp_loopback/`.
