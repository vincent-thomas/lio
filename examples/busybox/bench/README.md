## Startup Profiling

These scripts measure fixed startup cost for the `busybox` binary and selected applets.

### Requirements

- `hyperfine`
- optional: Linux `perf`

### Quick Start

```bash
./examples/busybox/bench/startup.sh
```

This builds `busybox` in release mode and compares:

- `busybox true-ish applet startup` via `busybox pwd`
- direct applet invocation where available via `busybox rg <pattern>`
- stock tool comparisons for `rg` and `jq` when those tools are installed

### Notes

- The harness uses `hyperfine -N` to avoid shell startup noise.
- For sub-millisecond commands, variance still matters. Run on a quiet system.
- A "perf harness" here means a repeatable script plus stable command set for benchmarking, not just a one-off manual timing command.
