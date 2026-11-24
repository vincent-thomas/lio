---
name: lio-add-op
description: Checklist for adding a new I/O operation to lio, or changing the fields/semantics of an existing `Op` variant, across the contract, every backend, the op model, tests and the C API. Use when touching `lio/src/backend/op.rs`, `lio/src/api/ops.rs`, or adding a function to `lio::api`.
---

# Adding or changing an operation

An op crosses every layer. The `match op { .. }` sites are exhaustive, so
after step 1 the compiler finds most of the rest; use this list for what it
cannot find (tests, docs, the FFI, error contracts).

Trace an existing op of the same shape first. For a path op:
`grep -rn 'LinkAt' --include='*.rs' lio lio-test`.

## 1. Contract: `lio/src/backend/op.rs`

- Add or change the `Op` variant. Pure data only: no `libc`, no
  `cfg(target_os)`. Use portable newtypes (`SockDomain`, the
  `portable_flags!` sets, `OsString` paths, `Resource`) and let backends
  translate.
- Document what the backend must return: the success value and the exact
  negated errnos for each failure.

## 2. Backends: `lio/src/backend/impls/`

Implement it in every backend, or complete with `-ENOTSUP` where the
platform truly cannot, and say so in the contract docs.

- `io_uring.rs`: path/state preparation, SQE construction, and the
  fallback to a blocking syscall for ops io_uring lacks.
- `pollingv2/mod.rs`: the interest mapping, the "runs immediately" check,
  and the syscall (via `syscall!`).
- `iocp.rs`: the dispatch and the Win32 implementation.
- If the op can block indefinitely (accept, recv), make sure `cancel`
  reaches it.

Also the non-OS backends in `lio/src/backend/`:

- `ds.rs` (`DSBackend`): the errnos it may inject and the simulated effect.
- `testing.rs` (`ScriptedBackend`): if the op fills a buffer, poison it on
  completion like the others.

## 3. Op model: `lio/src/api/ops.rs` and `lio/src/api/mod.rs`

- A struct holding owned inputs (buffers move in and come back out).
- `impl OpModel`: `action()` builds the `Op`; `complete()` maps
  `Completion` to `OpResult`. Use `completion.os_error()` for negative
  results and `Resource::from_completion` for new descriptors.
- `impl OneshotOpModel` or `StreamOpModel`.
- `#[cfg(test)] impl OpModelContract` (with
  `impl_op_model_contract_runtime!()`) plus a
  `lio_test::test_op_model_contract!(YourOp)` module beside the others.
- Public constructor in `api/mod.rs`, wrapped in `doc_op!` with the
  syscall name and man-page link, returning `Io<ops::YourOp>`.
- `high` wrappers in `fs.rs` / `net/` if it belongs there.

## 4. Backend contract tests: `lio-test/src/lib.rs`

- Add the guarantees to the `test_io_backend!` doc comment (exact results
  and errnos) and a `mod your_op { .. }` with tests that check them. These
  run against every real backend via `lio/tests/backends/*.rs`.

## 5. Integration, simulation, FFI

- `lio/tests/ops/your_op.rs` (register it in `lio/tests/ops.rs`): end-to-end
  through the default backend. Keep it portable; gate single tests only.
- `lio/tests/ds_runtime.rs`: add a `ScriptOp` / `Event` arm if the op fits
  the simulated world.
- `lio/src/ffi.rs`: a `lio_your_op` C function if the op belongs in the C
  API, a C test in `lio/tests/ffi/`, and the regenerated
  `lio/include/lio.h` (build with `--features unstable_ffi`).

## 6. Verify

Run the `lio-check` skill. Changing `Op` means the full suite including
doctests (`cargo test --doc`), not just `-p lio --lib`, and each backend's
contract suite on the platforms you can reach (`cargo test -p lio --test
backends <io_uring|poller|iocp>`). Say which platforms you could not run.
