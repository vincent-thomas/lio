---
name: lio-check
description: Run lio's CI gates locally through the same mise tasks CI uses (fmt, check, check-clean, clippy, tests, debug_assert scan, mutants on the diff) and report what passed and what could not run here. Use before declaring a change done, committing, or opening a PR in the lio repository.
---

# Local CI check for lio

CI runs mise tasks, so run the same ones. Each task already picks the
packages and features for this OS. Run each once; don't loop runs. Use
`cargo test`, never `cargo nextest`.

## Always (what every PR must pass)

```bash
mise run no-debug-assert
mise run ci        # fmt, check, check-clean, clippy, test
```

`mise run ci` stops at the first failing step; fix it and rerun.

Also read the diff against the hard rules in `AGENTS.md`: no platform code
outside `backend/`, no `unwrap`/`expect` or `#[allow]` of those lints in
library code, a `// SAFETY:` on every `unsafe` block, no crate-wide
`#![cfg(..)]` in tests.

## Depending on what changed

| Changed | Also run |
| --- | --- |
| a file listed in `.cargo/mutants.toml` | `BASE=main mise run mutants-diff` (see the `lio-kill-mutant` skill) |
| `unsafe`, the driver, store, slab, registration, timers | `mise run miri` (CI runs it nightly only, so a PR will not catch it) |
| driver ordering or op lifecycle logic | `mise run search` |
| `ffi.rs` | `mise run ffi`; commit the regenerated `lio/include/lio.h` |
| `Cargo.toml` dependencies | `mise run lint` (adds `cargo deny`) |

## Report

List each gate as passed, failed, or not run, and why it was not run. This
host runs only its own OS's backends: the io_uring suite needs Linux and
IOCP needs Windows. Never report a gate you did not run as passing.
