---
name: lio-check
description: Run lio's CI gates locally (fmt, clippy with warnings denied, debug_assert scan, rebuild check, unit/integration/doc tests, Miri, mutants) and report what passed and what could not run here. Use before declaring a change done, committing, or opening a PR in the lio repository.
---

# Local CI check for lio

Run the steps that match what changed, in this order, and stop at the first
failure to fix it. Run each suite once; don't loop runs.

## Always

```bash
cargo fmt --check
RUSTFLAGS=-Dwarnings cargo clippy --workspace --all-targets --all-features
git grep -nE 'debug_assert(_eq|_ne)?!' -- '*.rs'    # must print nothing
cargo nextest run -p lio --lib
```

On macOS add `--exclude lio-uring` to workspace commands.

Also check the diff against the hard rules in `AGENTS.md`: no platform code
outside `backend/`, no `unwrap`/`expect` or `#[allow]` of those lints in
library code, a `// SAFETY:` on every `unsafe` block, no crate-wide
`#![cfg(..)]` in tests.

## When the change touches more than the core

| Changed | Also run |
| --- | --- |
| `api/`, `fs.rs`, `net/`, `Op` | `cargo nextest run -p lio --features high` and `cargo test --doc -p lio -p lio-test --all-features` |
| a backend in `backend/impls/` | `cargo test -p lio --test backends <io_uring\|poller\|iocp>` for each one this host can run |
| `lio-tls`, `busybox`, `lio-uring` | `cargo nextest run -p <crate>` |
| `ffi.rs` | `scripts/test-ffi.sh`; commit the regenerated `lio/include/lio.h` |
| a `build.rs` | run `cargo check` twice; the second must compile nothing |
| `unsafe`, the driver, store, slab, registration, timers | `scripts/test-miri.sh` (needs nightly from mise) |
| a file listed in `.cargo/mutants.toml` | the `lio-kill-mutant` skill, on the diff |
| `Cargo.toml` dependencies | `cargo deny check` |

## Report

List each gate as passed, failed, or not run, and why it was not run (for
example: io_uring and IOCP suites need Linux and Windows). Never report a
gate you did not run as passing.
