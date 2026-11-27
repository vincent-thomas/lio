//! Reads standard input to the end through a [`lio_wasi::WaitableSet`] and
//! prints how many bytes and reads it took.
//!
//! ```text
//! cargo build -p lio-wasi --example stdin_count --target wasm32-wasip3
//! printf 'hello' | wasmtime run -S p3 \
//!   target/wasm32-wasip3/debug/examples/stdin_count.wasm
//! ```

use lio_wasi::{Event, Started, StreamResult, WaitableSet};

fn main() -> Result<(), lio_wasi::Error> {
  let (stdin, _result) = wasip3::cli::stdin::read_via_stream();
  let mut set = WaitableSet::new();
  let stdin = set.add_reader(stdin);

  let mut buf = [0u8; 4096];
  let mut total = 0;
  let mut reads = 0u64;
  loop {
    // SAFETY: `buf` lives past the end of the read, which this statement
    // waits for, and nothing else touches it until then.
    let read = match unsafe { set.read(stdin, buf.as_mut_ptr(), buf.len()) }? {
      Started::Done(read) => read,
      Started::Blocked => match set.wait() {
        Some(Event::Read(stream, read)) if stream == stdin => read,
        other => unreachable!("only the read is in flight: {other:?}"),
      },
    };
    reads += 1;
    total += read.count();
    if let StreamResult::Dropped(_) = read {
      break;
    }
  }
  println!("{total} bytes in {reads} reads");
  Ok(())
}
