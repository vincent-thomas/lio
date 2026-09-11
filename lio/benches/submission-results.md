# Linux submission queue investigation

The allocation churn is confirmed on the real io_uring backend. A net
performance improvement from the attempted queue-reuse changes is **not**
established. The experimental backend changes were removed because larger
NOP batches regressed. Only the benchmark and this report are retained.

## Environment and method

- OrbStack Linux VM on Apple M4 Pro, aarch64.
- Kernel: `7.0.14-orbstack-00380-ga7e0a2dc9535`.
- Rust/Cargo: 1.98.1, optimized bench profile.
- Default `Lio::new` backend on Linux: io_uring.
- Baseline backend source: commit `3b3547d8bb0061d5cc9defd79659cdea8841b856`.
  The earlier stream-waker change was present and identical in all builds;
  these workloads use callbacks and do not exercise that change.
- One batch submits 1, 32, or 256 operations, then drives `Lio::run` until
  every callback completes. NOPs isolate bookkeeping; ordinary
  `api::read_at` operations read 4 KiB from a real file in the VM's `/tmp`
  (tmpfs), through the kernel. Buffers are reused and setup is outside timing.
- Allocation counts use 100 warmup batches followed by 1,000 counted batches.
  Timing runs disable counting, but retain the same allocator wrapper.
- Criterion: 50 samples, 1 second warmup, 3 seconds measurement per case.
  Measurements were sequential, with no concurrent tests or builds.

## Allocation proof

Counts below were identical for NOPs and real reads:

| Queue depth | Baseline allocations | Reallocations | Frees |
| --- | ---: | ---: | ---: |
| 1 | 1,000 | 0 | 1,000 |
| 32 | 1,000 | 3,000 | 1,000 |
| 256 | 1,000 | 6,000 | 1,000 |

Temporarily changing `flush` to drain and restore its detached vector made
all three counts **zero**, for both workloads at all depths. The rest of the
workload and driver were unchanged. This confirms that queue reuse can remove
the observed steady-state allocator traffic, including on ordinary file I/O.

## Timing results and rejected implementations

These are Criterion time point estimates **per batch**, not per operation:

| Workload | Original | Vec drain/reuse | VecDeque FIFO reuse |
| --- | ---: | ---: | ---: |
| NOP, depth 1 | 62.482 ns | 57.774 ns | 51.344 ns |
| NOP, depth 32 | 1.2023 us | 1.7282 us | 1.6726 us |
| NOP, depth 256 | 10.641 us | 13.282 us | 12.720 us |
| Read 4 KiB, depth 1 | 28.299 us | 27.673 us | 27.774 us |
| Read 4 KiB, depth 32 | 43.944 us | 44.491 us | 44.428 us |
| Read 4 KiB, depth 256 | 191.22 us | 199.91 us | 198.05 us |

The drain implementation regressed NOP batches by about 44% at depth 32 and
25% at depth 256 (Criterion p < 0.05). The corresponding VecDeque regressions
were about 39% and 20%. Neither is a suitable performance fix.

A further VecDeque variant handled NOPs at the front before moving a full
entry. It measured 45.515 ns / 1.5177 us / 11.656 us at depths 1 / 32 / 256;
the larger batches still regressed. This variant was also removed.

For variability checking, the saved original executable was run again:
62.680 ns / 1.2387 us / 9.1512 us for NOPs, and
27.760 us / 44.183 us / 197.37 us for reads. This variation further limits
claims about small read-throughput changes in this VM. It does not explain
away the large NOP regressions.

Conclusion: the original queue has demonstrable avoidable allocation churn,
but fewer allocations alone did not produce a broadly faster implementation
in these experiments. No backend optimization from this investigation remains
in the working tree.

## Reproduction

The VM needed Clang and libclang development packages for lio-uring's build.

```sh
orb /home/vt/.cargo/bin/cargo bench -p lio --bench submission \
  --target-dir /home/vt/lio-target -- --allocations

orb /home/vt/.cargo/bin/cargo bench -p lio --bench submission \
  --target-dir /home/vt/lio-target -- --save-baseline backlog_before
```

To evaluate a future candidate, apply it and rerun with
`--baseline backlog_before` instead of `--save-baseline backlog_before`.
Criterion data is under `target/criterion/submission_nop` and
`target/criterion/submission_read_4k`. The unmodified baseline executable is
also saved in the VM at `/home/vt/lio-target/submission-before`.

## Validation

The benchmark checked completion counts and 4,096-byte read results throughout.
After restoring the original backend, full Linux `cargo test` was attempted.
The `ops` integration suite reported 120 passed and 9 failed, involving bind,
listen, and shutdown (address-family errors and incorrect bound addresses).
Those failures occurred with the backend restored; this investigation did not
diagnose or modify the affected networking code. The full suite is not green.
