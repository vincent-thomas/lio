//! Opens files with [`lio_wasi::fs::OpenAt`] and reads one through a
//! [`lio_wasi::WaitableSet`].
//!
//! ```text
//! cargo build -p lio-wasi --example open_read --target wasm32-wasip3
//! wasmtime run -S p3 --dir . \
//!   target/wasm32-wasip3/debug/examples/open_read.wasm Cargo.toml
//! ```

use lio_wasi::fs::{
  Descriptor, DescriptorFlags, ErrorCode, OpenAt, OpenFlags, PathFlags,
};
use lio_wasi::{Cancel, Event, Started, StreamResult, Subtask, WaitableSet};

mod common;

fn open(
  set: &mut WaitableSet,
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
  // SAFETY: `call`, `dir` and `path` stay put until the call has returned,
  // which the wait below sees.
  if let Some(subtask) = unsafe { set.start(&mut call) } {
    assert_eq!(set.wait(), Some(Event::Returned(subtask)));
  }
  let Some(result) = call.result() else {
    unreachable!("a returned call has a result")
  };
  assert!(call.result().is_none(), "a result is taken once");
  result
}

/// Reads `file` to the end and returns how many bytes it held.
fn read_all(set: &mut WaitableSet, file: &Descriptor) -> usize {
  let (stream, _result) = file.read_via_stream(0);
  let stream = set.add_reader(stream);
  let mut buf = [0u8; 4096];
  let mut total = 0;
  loop {
    // SAFETY: `buf` outlives the read, which has ended once this statement
    // has.
    let read = match unsafe { set.read(stream, buf.as_mut_ptr(), buf.len()) } {
      Ok(Started::Done(read)) => read,
      Ok(Started::Blocked) => match set.wait() {
        Some(Event::Read(ended, read)) if ended == stream => read,
        other => unreachable!("only the read is in flight: {other:?}"),
      },
      Err(error) => panic!("the read was refused: {error}"),
    };
    total += read.count();
    if let StreamResult::Dropped(_) = read {
      break;
    }
  }
  assert_eq!(set.close_reader(stream), Ok(None));
  total
}

fn main() {
  let mut out = common::Out::stdout();
  let path = std::env::args().nth(1).unwrap_or_else(|| "Cargo.toml".into());
  let Some((dir, _name)) = lio_wasi::fs::preopens().into_iter().next() else {
    panic!("run with a preopened directory: wasmtime run --dir . ...");
  };
  let mut set = WaitableSet::new();

  match open(&mut set, &dir, &path) {
    Ok(file) => {
      say!(out, "{path}: {} bytes", read_all(&mut set, &file));
    }
    Err(error) => say!(out, "{path}: {error:?}"),
  }

  match open(&mut set, &dir, "no/such/file") {
    Ok(_) => panic!("opened a file that does not exist"),
    Err(error) => say!(out, "no/such/file: {error:?}"),
  }

  // Many opens started together. Each is told apart by its subtask.
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
  // SAFETY: `calls` is not touched again until every call has returned.
  let mut waiting: Vec<Option<Subtask>> =
    calls.iter_mut().map(|call| unsafe { set.start(call) }).collect();
  while let Some(event) = set.wait() {
    let Event::Returned(subtask) = event else {
      unreachable!("only calls are in flight: {event:?}")
    };
    let Some(slot) = waiting.iter_mut().find(|slot| **slot == Some(subtask))
    else {
      unreachable!("a call returned that was not started here")
    };
    *slot = None;
  }
  assert!(waiting.iter().all(Option::is_none), "a call never returned");
  let opened: Vec<bool> =
    calls.iter_mut().map(|call| matches!(call.result(), Some(Ok(_)))).collect();
  say!(out, "batch of four opened: {opened:?}");

  // Cancel right after starting. The call either beat the cancel or not;
  // either way the cancel says which, no event follows, and the result
  // agrees.
  let mut call = OpenAt::new(
    &dir,
    PathFlags::empty(),
    &path,
    OpenFlags::empty(),
    DescriptorFlags::READ,
  );
  // SAFETY: `call` stays put until it has returned or been cancelled.
  match unsafe { set.start(&mut call) } {
    None => {
      assert!(call.result().is_some());
      say!(out, "the open returned before it could be cancelled");
    }
    Some(subtask) => {
      let found = set.cancel(subtask);
      assert_eq!(set.cancel(subtask), None, "a call is cancelled once");
      assert!(set.wait().is_none());
      match found {
        Some(Cancel::Completed) => assert!(call.result().is_some()),
        Some(Cancel::Cancelled) => assert!(call.result().is_none()),
        None => panic!("the cancel did not find the call it was given"),
      }
      say!(out, "cancelling an open in flight found: {found:?}");
    }
  }
}
