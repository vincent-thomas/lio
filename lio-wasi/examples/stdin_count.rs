//! Reads standard input to the end through a [`lio_wasi::Queue`] and prints
//! how many bytes and reads it took.
//!
//! ```text
//! cargo build -p lio-wasi --example stdin_count --target wasm32-wasip3
//! printf 'hello' | wasmtime run -S p3 \
//!   target/wasm32-wasip3/debug/examples/stdin_count.wasm
//! ```

use lio_wasi::{Outcome, Queue};

fn main() -> Result<(), lio_wasi::Error> {
  let (stdin, _result) = wasip3::cli::stdin::read_via_stream();
  let mut queue = Queue::new();
  let stdin = queue.add_reader(stdin);

  let mut buf = [0u8; 4096];
  let mut total = 0;
  let mut reads = 0u64;
  loop {
    // SAFETY: `buf` lives past the completion taken below, and nothing else
    // touches it until then.
    unsafe { queue.push_read(stdin, buf.as_mut_ptr(), buf.len(), reads) }?;
    let Some(done) = queue.wait() else {
      unreachable!("a pushed read always completes")
    };
    assert_eq!(done.user_data(), reads);
    reads += 1;
    total += done.outcome().count();
    if let Outcome::Closed(_) = done.outcome() {
      break;
    }
  }
  println!("{total} bytes in {reads} reads");
  Ok(())
}
