//! Opens files with [`lio_wasi::fs::OpenAt`] and reads one through a
//! [`lio_wasi::Queue`].
//!
//! ```text
//! cargo build -p lio-wasi --example open_read --target wasm32-wasip3
//! wasmtime run -S p3 --dir . \
//!   target/wasm32-wasip3/debug/examples/open_read.wasm Cargo.toml
//! ```

use lio_wasi::fs::{
  Descriptor, DescriptorFlags, ErrorCode, OpenAt, OpenFlags, PathFlags,
};
use lio_wasi::{Completion, Outcome, Queue};

const OPEN: u64 = 1;
const READ: u64 = 2;

fn next(queue: &mut Queue) -> Completion {
  let Some(done) = queue.wait() else {
    unreachable!("an operation is outstanding")
  };
  done
}

fn open(
  queue: &mut Queue,
  dir: &Descriptor,
  path: &str,
) -> Result<Descriptor, ErrorCode> {
  let mut call = OpenAt::new(
    dir,
    PathFlags::empty(),
    path,
    OpenFlags::empty(),
    DescriptorFlags::READ,
  );
  // SAFETY: `call`, `dir` and `path` stay put until the completion below.
  unsafe { queue.push_call(&mut call, OPEN) };
  let done = next(queue);
  assert_eq!(done.user_data(), OPEN);
  assert_eq!(done.outcome(), Outcome::Returned);
  let Some(result) = call.result() else {
    unreachable!("a returned call has a result")
  };
  assert!(call.result().is_none(), "a result is taken once");
  result
}

/// Reads `file` to the end and returns how many bytes it held.
fn read_all(queue: &mut Queue, file: &Descriptor) -> usize {
  let (stream, _result) = file.read_via_stream(0);
  let stream = queue.add_reader(stream);
  let mut buf = [0u8; 4096];
  let mut total = 0;
  loop {
    // SAFETY: `buf` outlives the completion taken on the next line.
    let pushed =
      unsafe { queue.push_read(stream, buf.as_mut_ptr(), buf.len(), READ) };
    assert_eq!(pushed, Ok(()));
    let done = next(queue).outcome();
    total += done.count();
    if let Outcome::Closed(_) = done {
      break;
    }
  }
  assert_eq!(queue.close_reader(stream), Ok(()));
  total
}

fn main() {
  let path = std::env::args().nth(1).unwrap_or_else(|| "Cargo.toml".into());
  let Some((dir, _name)) =
    wasip3::filesystem::preopens::get_directories().into_iter().next()
  else {
    panic!("run with a preopened directory: wasmtime run --dir . ...");
  };
  let mut queue = Queue::new();

  match open(&mut queue, &dir, &path) {
    Ok(file) => {
      println!("{path}: {} bytes", read_all(&mut queue, &file));
    }
    Err(error) => println!("{path}: {error:?}"),
  }

  match open(&mut queue, &dir, "no/such/file") {
    Ok(_) => panic!("opened a file that does not exist"),
    Err(error) => println!("no/such/file: {error:?}"),
  }

  // Many opens in flight at once, each with its own `user_data`.
  let names = [path.as_str(), "no/such/file", path.as_str(), "."];
  let mut calls: Vec<OpenAt<'_>> = names
    .iter()
    .map(|name| {
      OpenAt::new(
        &dir,
        PathFlags::empty(),
        name,
        OpenFlags::empty(),
        DescriptorFlags::READ,
      )
    })
    .collect();
  for (id, call) in calls.iter_mut().enumerate() {
    // SAFETY: `calls` is not touched again until every completion is taken.
    unsafe { queue.push_call(call, id as u64) };
  }
  let mut returned = [false; 4];
  for _ in 0..calls.len() {
    let done = next(&mut queue);
    assert_eq!(done.outcome(), Outcome::Returned);
    returned[done.user_data() as usize] = true;
  }
  assert_eq!(returned, [true; 4]);
  let opened: Vec<bool> =
    calls.iter_mut().map(|call| matches!(call.result(), Some(Ok(_)))).collect();
  println!("batch of four opened: {opened:?}");

  // Cancel right after starting. The call either beat the cancel or not;
  // either way there is exactly one completion and the result agrees.
  let mut call = OpenAt::new(
    &dir,
    PathFlags::empty(),
    &path,
    OpenFlags::empty(),
    DescriptorFlags::READ,
  );
  // SAFETY: `call` stays put until its completion is taken below.
  unsafe { queue.push_call(&mut call, OPEN) };
  let cancelled = queue.cancel(OPEN);
  let done = next(&mut queue);
  assert!(queue.wait().is_none());
  match done.outcome() {
    Outcome::Returned => assert!(call.result().is_some()),
    Outcome::Cancelled(0) => assert!(call.result().is_none()),
    other => panic!("unexpected outcome {other:?}"),
  }
  println!(
    "cancel found it in flight: {cancelled}, ended {:?}",
    done.outcome()
  );
}
