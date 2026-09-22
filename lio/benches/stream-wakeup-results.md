# Stream wakeup benchmark

Measured on Apple M4 Pro, aarch64-apple-darwin, rustc 1.94.1,
using Cargo's optimized bench profile. Original registration implementation:
`3b3547d8bb0061d5cc9defd79659cdea8841b856`.

The change preserves the stream consumer's waker on `OpResult::Again` and
wakes only after `Yield` or `Done`. The decision lives inside the typed result
handler; no extra field is added to `ProcessResult`.

## Applicability audit

A subsequent audit of the repository found **no built-in API path that combines
`Again` with the waker-based stream registration changed here**. The benchmark
therefore does not establish a practical speedup for the built-in workloads.

- `api::interval` is the only built-in API constructing an `IoStream`.
  `Interval::complete` always returns `Yield`, never `Again`.
- `IoStream::next` is the only production call site for
  `Registration::new_waker_in`. The other call sites are tests.
- Multi-step file and network models (including copy, reading a whole file,
  and socket setup) can return `Again`, but their built-in APIs return `Io`.
  Awaiting `Io` creates a callback registration, whose `Again` branch already
  advances without invoking the result callback or waking the consumer.
  Channel and callback consumption also use callback registrations.
- Repository TLS code awaits ordinary `Io` operations. The busybox example
  uses channel/callback consumption; neither supplies a custom `IoStream`.
- References to `accept_stream` in `api/io.rs` are documentation examples,
  not an implemented API.

The measured optimization applies to downstream/custom models consumed through
`IoStream::from_op(...).next()` that return `Again`, including a built-in model
explicitly wrapped that way by a caller. No such non-test workload was found
in this repository. External application usage has not been measured.

Consequently, the unnecessary-wakeup pattern occurs zero times through the
normal built-in API paths by construction; this is a source-level reachability
finding, not a sampled runtime frequency. The synthetic results below remain
valid, but should not be described as a demonstrated improvement to current
built-in I/O workloads.

## Workload

`bookkeeping/stream_executor` uses one persistent stream and the existing
zero-syscall `ImmediateBackend`. Each item requires 0, 1, or 8 internal `Again`
completions, followed by `Yield`. A minimal executor uses an atomic ready flag
and polls the consumer only when woken, between driver turns. Each iteration
measures delivery of one item, including driver turns, wakeups, channel access,
and consumer polls. Stream creation is outside steady-state timing.

Both versions use exactly the same benchmark. Each case uses 60 samples,
one second of warmup, and four seconds of measurement. No tests or other builds
were run concurrently with benchmark measurement.

## Results

Times are Criterion point estimates in ns per delivered item. Reduction and
95% confidence intervals are Criterion's reported time-change statistics
(which need not equal the ratio of its time point estimates).

| Internal steps | Original, run 1 | Updated, run 1 | Time reduction (95% CI) |
| --- | ---: | ---: | ---: |
| 0 | 46.350 | 44.777 | 3.01% (2.10–3.70%) |
| 1 | 77.478 | 66.894 | 14.45% (14.00–14.85%) |
| 8 | 292.12 | 208.81 | 28.44% (28.21–28.70%) |

The exact original source was then restored temporarily, recompiled, and
remeasured, followed by another build and measurement of the final version:

| Internal steps | Original, run 2 | Updated, run 2 | Time reduction (95% CI) |
| --- | ---: | ---: | ---: |
| 0 | 46.006 | 44.470 | 1.26% (0.74–1.77%); within noise threshold |
| 1 | 79.412 | 67.164 | 15.84% (15.17–16.54%) |
| 8 | 324.36 | 207.53 | 36.18% (36.01–36.35%) |

Criterion reported p < 0.05 for both internal-step cases in both comparisons.
Between-run variation is visible, particularly in the eight-step original
case; the gain persisted across both comparisons. The zero-step control
showed no regression in the final implementation.

With N internal steps and an executor poll after each wake, the change removes
N wakeups and N empty consumer polls per item. Driver turns and backend
submissions remain identical. Unit tests verify that repeated `Again` results
leave the waker registered and that both `Yield` and `Done` still wake it.

These are scheduling measurements for streams with internal steps, not
end-to-end disk/network throughput claims. Actual I/O latency and executor
wakeup coalescing can reduce the relative gain. Ordinary single-result futures
use a different callback path.

## Reproduction

With this benchmark file present, run against the original registration code:

```sh
cargo bench -p lio --bench bookkeeping -- stream_executor --save-baseline wake_before
```

Then apply the registration change and run:

```sh
cargo bench -p lio --bench bookkeeping -- stream_executor --baseline wake_before
```

The repeat used `wake_before_repeat` as its baseline name. Criterion's raw
samples and reports are under `target/criterion/bookkeeping_stream_executor/`.

Validation: full `cargo test` passed, including integration tests and doctests.
