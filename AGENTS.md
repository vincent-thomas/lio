# lio — notes for coding agents

`lio` is a manually driven, syscall-shaped async I/O library with pluggable
backends (io_uring on Linux, epoll/kqueue polling on Unix, IOCP on Windows).
The bar is **production-ready**: correctness, memory safety and tested
contracts come before features or speed.

## Repository map

| Path | What it is |
| --- | --- |
| `lio/` | The main crate. Default member; `cargo test` with no `-p` hits it. |
| `lio/src/api/` | Public ops: constructors (`mod.rs`), op state machines (`ops.rs`), the `Io<T>` handle (`io.rs`), `OpModel` traits (`op.rs`), `Resource` (`resource.rs`). |
| `lio/src/lio.rs` | The `Lio` driver (mounted as `mod driver`). |
| `lio/src/{registration,slab,backend/store}.rs` | In-flight op bookkeeping: slots, generations, per-slot bump arenas. |
| `lio/src/time/` | Hierarchical timer wheel (`clock.rs`). |
| `lio/src/backend/op.rs` | `Op`: the backend contract, **pure data**. |
| `lio/src/backend/impls/` | Real backends: `io_uring.rs`, `pollingv2/`, `iocp.rs`, plus shared OS helpers. |
| `lio/src/backend/testing.rs` | `ScriptedBackend` and `Checked`: drive the core with no I/O. |
| `lio/src/backend/ds.rs` | `DSBackend`: seeded deterministic simulation (synthetic fs/sockets). |
| `lio/src/{fs,net}/` | Higher-level wrappers, behind the `high` feature. |
| `lio/src/ffi.rs` | C API behind `unstable_ffi`; `build.rs` regenerates `lio/include/lio.h`. |
| `lio-test/` | Contract macros: `test_io_backend!`, `test_op_model_contract!`. |
| `lio-uring/` | Safe io_uring bindings (Linux only, bindgen over the `liburing` submodule). |
| `lio-tls/` | rustls on top of `lio::net`. |
| `examples/busybox/` | Large Unix coreutils clone used as a dogfooding app (not built on Windows). |

## How an operation flows

```
api::read(..)            -> Io<ops::Read<B>>             (user handle)
  .with_lio(&lio) / global, then .await | .when_done | .send
OpModel::action(&mut self) -> Action::Io(BorrowedOp) | Action::Sleep(d)
Lio driver               -> OpStore slot + Registration, IoBackend::push(InFlight)/flush
IoBackend::wait()        -> OpCompleted { registration_id, result: isize }
OpModel::complete(Completion) -> OpResult::{Again, Yield(item), Done(item)}
```

Negative `result` is a raw negated OS error code. Buffers move into the op
model and come back with the result. `action()` returns a `BorrowedOp` tied
to the model's borrow (built with the `action_io!` macro); the driver keeps
the model pinned until the backend reports the op finished.

`IoBackend` is an **`unsafe trait`** (contract in `lio/src/backend/mod.rs`):
`push` only queues; `flush` submits; store the `Keep` from each `InFlight`
for as long as the backend, kernel or a worker thread may use an address
from that `Op`; report each pushed id **at most once** from `wait`, and only
when nothing outside the backend can still use its addresses; a cancelled
op still completes once; `drain` returns `true` only when the kernel and
every worker are done with every pushed address.

## Hard rules

1. **The core is bookkeeping only.** Syscalls, kernel submission and
   polling live in `backend/impls/`. No `libc`, `windows-sys` or
   `cfg(target_os)` outside `backend/` and `ffi.rs`. The only accepted
   exceptions: `Resource`'s std raw fd/handle trait impls, `OsString`
   byte conversions under `cfg(unix)`, and `libc` in unit-test fixtures.
   `backend/op.rs` stays platform-free data.
2. **No `debug_assert!` / `debug_assert_eq!` / `debug_assert_ne!`.** CI rejects
   them (`mise run no-debug-assert`). Use `assert!`.
3. **No `unwrap`/`expect` in library code** (denied; `clippy.toml` allows
   them in tests). Return an error, or restructure. A deliberate panic is
   spelled with `let ... else`: `unreachable!` for a broken internal
   invariant, `panic!` for caller misuse, documented under `# Panics`.
   Never silence these lints with `#[allow]`/`#[expect]`.
4. **Every `unsafe` block gets a `// SAFETY:` comment** (denied by
   `clippy::undocumented_unsafe_blocks`).
5. **Platform gates go on individual tests, never on whole test crates or
   modules** (`#![cfg(unix)]` on a test file is rejected). Port the test with
   helpers in `lio/tests/common.rs`; gate only what is truly
   platform-specific, with the reason in a comment.
6. **Build scripts must not rerun when nothing changed.** On Linux, CI
   fails if a second `cargo check` compiles anything (`mise run check-clean`).
7. Warnings are errors: the `check`, `clippy` and `test` tasks set
   `RUSTFLAGS=-Dwarnings`.

## Commands

Toolchains and tools come from `mise.toml` (`mise install`; it also
initialises the `liburing` submodule). Rust is pinned to 1.100.0; nightly is
only for Miri. CI runs these same tasks, and each one already picks the
right packages and features for the OS (no `lio-uring` off Linux, no
`busybox` on Windows), so prefer them over hand-written cargo commands.

```bash
mise run ci                # PR CI job: fmt, check, check-clean, clippy, test
mise run no-debug-assert   # the other PR gate
BASE=main mise run mutants-diff  # PR mutants job, on your diff
mise run test              # lib, integration (this OS's backends), doctests
mise run lint              # fmt, check, clippy, cargo deny
mise run miri              # core tests under Miri (nightly CI only)
mise run search            # model/lifecycle tests at nightly sizes
mise run ffi               # C ABI build + FFI tests (not in CI)
mise run bench             # bookkeeping, then each real backend
```

For a fast inner loop: `cargo test -p lio --lib` (core bookkeeping) or
`cargo test -p lio --features high --test <name>`.

This repository uses `cargo test` only, locally and in CI. Don't use
`cargo nextest`, even if your own instructions prefer it.

Run each suite once per change. Don't loop test runs to probe flakiness on
macOS (it triggers OS permission prompts); ask first.

## Testing contract

- **Core logic** (driver, store, slab, registration, timers, `Io`) is tested
  without I/O through `ScriptedBackend` + `Checked`. Prefer this for any core
  change. Tests that need a real backend carry
  `#[cfg_attr(miri, ignore = "needs a real backend")]`.
- **Model tests** (`lio/src/lio_model_tests.rs`, proptest, size via
  `PROPTEST_CASES`) and **lifecycle enumeration**
  (`lio/src/lio_lifecycle_tests.rs`, depth via `LIO_LIFECYCLE_DEPTH`,
  default 6) cover driver orderings. PRs run them small (`mise run test`);
  `deep.yaml` runs them large nightly (`mise run search`), along with Miri
  and every core mutant.
- **Op models**: each one in `api/ops.rs` implements `OpModelContract` and
  instantiates `lio_test::test_op_model_contract!`.
- **Backends**: `lio_test::test_io_backend!` in `lio/tests/backends/*.rs`.
  Its doc comment lists the exact per-op guarantees (results, errnos).
- **Changing `Op` fields or semantic wrapper types** means updating the
  `lio-test` contract macro, every backend, `DSBackend`, `ScriptedBackend`,
  and running the full suite including doctests: `-p lio --lib` alone misses
  integration tests and doctests that build raw `Op` values.
- **Mutation testing**: CI mutates every changed line in the files listed in
  `.cargo/mutants.toml`. A surviving mutant fails the PR
  (`mise run mutants-diff`); add a test that kills it. Add to `exclude_re`
  only for a genuinely equivalent mutant, with a comment proving why.

## Conventions

- `rustfmt.toml`: 2-space indent, 80 columns. Run `cargo fmt`.
- Doc comments explain *why* and state contracts; keep them true when code
  changes. Public ops use `doc_op!` (`lio/src/macros.rs`) to link the
  equivalent syscall.
- Building with `unstable_ffi` regenerates `lio/include/lio.h`; commit it
  with the `ffi.rs` change.
- Commit subjects: short, imperative, sentence case, ending with a period
  (`Fail a Miri run that selects nothing.`).
