//! Runs every `lio_wasi::fs` call against a scratch directory and checks
//! what each one did.
//!
//! ```text
//! cargo build -p lio-wasi --example fs_ops --target wasm32-wasip3
//! wasmtime run -S p3 --dir <empty directory>::/ \
//!   target/wasm32-wasip3/debug/examples/fs_ops.wasm
//! ```

use lio_wasi::fs::{
  Advice, Advise, CreateDirectoryAt, Descriptor, DescriptorFlags,
  DescriptorType, DirEntryBuf, ErrorCode, GetFlags, GetType, Instant, LinkAt,
  NewTimestamp, OpenAt, OpenFlags, PathFlags, ReadlinkAt, RemoveDirectoryAt,
  RenameAt, SetSize, SetTimes, SetTimesAt, Stat, StatAt, StreamStatus,
  SymlinkAt, Sync, SyncData, UnlinkFileAt,
};
use lio_wasi::{
  Call, Event, ReadStream, ReadableFuture, Started, StreamResult, Subtask,
  WaitableSet, WriteStream,
};

mod common;

/// Makes `call` and waits for it to return.
fn run<C: Call>(set: &mut WaitableSet, call: &mut C) {
  // SAFETY: `call` is borrowed for this whole function, which waits for
  // the call to return.
  if let Some(subtask) = unsafe { set.start(call) } {
    assert_eq!(set.wait(), Some(Event::Returned(subtask)));
  }
}

/// Reads a stream's status and waits for it to be delivered.
fn read_status<F: ReadableFuture>(set: &mut WaitableSet, status: &mut F) {
  // SAFETY: `status` is borrowed for this whole function, which waits for
  // the value to be delivered.
  if let Some(future) = unsafe { set.read_future(status) } {
    assert_eq!(set.wait(), Some(Event::FutureRead(future)));
  }
}

/// Reads into `buf` and waits for the read to end.
fn read(
  set: &mut WaitableSet,
  stream: ReadStream,
  buf: &mut [u8],
) -> StreamResult {
  // SAFETY: `buf` is borrowed for this whole function, which waits for the
  // read to end.
  match unsafe { set.read(stream, buf.as_mut_ptr(), buf.len()) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.wait() {
      Some(Event::Read(ended, result)) if ended == stream => result,
      other => unreachable!("only the read is in flight: {other:?}"),
    },
    Err(error) => panic!("the read was refused: {error}"),
  }
}

/// Writes from `data` and waits for the write to end.
fn write(
  set: &mut WaitableSet,
  stream: WriteStream,
  data: &[u8],
) -> StreamResult {
  // SAFETY: `data` is borrowed for this whole function, which waits for
  // the write to end.
  match unsafe { set.write(stream, data.as_ptr(), data.len()) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.wait() {
      Some(Event::Write(ended, result)) if ended == stream => result,
      other => unreachable!("only the write is in flight: {other:?}"),
    },
    Err(error) => panic!("the write was refused: {error}"),
  }
}

fn open(
  set: &mut WaitableSet,
  dir: &Descriptor,
  path: &str,
  open_flags: OpenFlags,
  flags: DescriptorFlags,
) -> Result<Descriptor, ErrorCode> {
  let mut call = OpenAt::new(dir, PathFlags::empty(), path, open_flags, flags);
  run(set, &mut call);
  let Some(result) = call.result() else {
    unreachable!("a returned call has a result")
  };
  result
}

/// Takes a result that must be there and must be ok.
macro_rules! ok {
  ($call:expr) => {
    match $call.result() {
      Some(Ok(value)) => value,
      Some(Err(error)) => panic!("{} failed: {error:?}", stringify!($call)),
      None => panic!("{} has no result", stringify!($call)),
    }
  };
}

/// Writes `data` at `offset` and returns the stream's status.
fn write_at(
  set: &mut WaitableSet,
  file: &Descriptor,
  offset: u64,
  data: &[u8],
) -> Result<(), ErrorCode> {
  let (writer, reader) = lio_wasi::stream();
  let mut status = StreamStatus::new(file.write_via_stream(reader, offset));
  let writer = set.add_writer(writer);
  let mut sent = 0;
  while sent < data.len() {
    let wrote = write(set, writer, &data[sent..]);
    if let StreamResult::Dropped(_) = wrote {
      break;
    }
    sent += wrote.count();
  }
  assert_eq!(set.close_writer(writer), Ok(None));
  read_status(set, &mut status);
  let Some(result) = status.result() else {
    unreachable!("a returned status has a result")
  };
  assert!(status.result().is_none(), "a status is taken once");
  result
}

/// Reads from `offset` to the end and returns the bytes and the status.
fn read_from(
  set: &mut WaitableSet,
  file: &Descriptor,
  offset: u64,
) -> (Vec<u8>, Result<(), ErrorCode>) {
  let (stream, result) = file.read_via_stream(offset);
  let mut status = StreamStatus::new(result);
  let stream = set.add_reader(stream);
  let mut out = Vec::new();
  let mut buf = [0u8; 64];
  loop {
    let done = read(set, stream, &mut buf);
    out.extend_from_slice(&buf[..done.count()]);
    if let StreamResult::Dropped(_) = done {
      break;
    }
  }
  assert_eq!(set.close_reader(stream), Ok(None));
  read_status(set, &mut status);
  let Some(result) = status.result() else {
    unreachable!("a returned status has a result")
  };
  (out, result)
}

fn main() {
  let mut out = common::Out::stdout();
  let Some((root, _name)) = lio_wasi::fs::preopens().into_iter().next() else {
    panic!("run with a preopened scratch directory");
  };
  let mut set = WaitableSet::new();
  let set = &mut set;

  // mkdir, then open it as a directory.
  let mut mkdir = CreateDirectoryAt::new(&root, "work");
  run(set, &mut mkdir);
  ok!(mkdir);
  run(set, &mut mkdir);
  assert!(matches!(mkdir.result(), Some(Err(ErrorCode::Exist))));
  let dir = match open(
    set,
    &root,
    "work",
    OpenFlags::DIRECTORY,
    DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
  ) {
    Ok(dir) => dir,
    Err(error) => panic!("opening the new directory failed: {error:?}"),
  };
  let mut kind = GetType::new(&dir);
  run(set, &mut kind);
  assert!(matches!(ok!(kind), DescriptorType::Directory));
  say!(out, "create-directory-at, open-at, get-type: ok");

  // Create a file, write it, read it back.
  let file = match open(
    set,
    &dir,
    "data.txt",
    OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
    DescriptorFlags::READ | DescriptorFlags::WRITE,
  ) {
    Ok(file) => file,
    Err(error) => panic!("creating the file failed: {error:?}"),
  };
  assert!(matches!(
    open(
      set,
      &dir,
      "data.txt",
      OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
      DescriptorFlags::READ,
    ),
    Err(ErrorCode::Exist)
  ));
  assert!(matches!(write_at(set, &file, 0, b"hello, wasi"), Ok(())));
  assert!(matches!(write_at(set, &file, 7, b"WASI!"), Ok(())));
  let (bytes, status) = read_from(set, &file, 0);
  assert_eq!(bytes, b"hello, WASI!");
  assert!(matches!(status, Ok(())));
  let (tail, _) = read_from(set, &file, 7);
  assert_eq!(tail, b"WASI!");
  say!(out, "write-via-stream, read-via-stream, stream status: ok");

  // A stream that fails reports why through its status.
  let (bytes, status) = read_from(set, &dir, 0);
  assert!(bytes.is_empty());
  assert!(status.is_err(), "reading a directory as a file succeeded");
  say!(out, "failed stream status: {status:?}");

  let mut flags = GetFlags::new(&file);
  run(set, &mut flags);
  let flags = ok!(flags);
  assert!(flags.contains(DescriptorFlags::READ | DescriptorFlags::WRITE));
  let mut kind = GetType::new(&file);
  run(set, &mut kind);
  assert!(matches!(ok!(kind), DescriptorType::RegularFile));
  say!(out, "get-flags, get-type: ok");

  let mut sync = Sync::new(&file);
  run(set, &mut sync);
  ok!(sync);
  let mut sync_data = SyncData::new(&file);
  run(set, &mut sync_data);
  ok!(sync_data);
  let mut advise = Advise::new(&file, 0, 12, Advice::Sequential);
  run(set, &mut advise);
  ok!(advise);
  say!(out, "sync, sync-data, advise: ok");

  // Stat sees the size; set-size changes it both ways.
  let mut stat = Stat::new(&file);
  run(set, &mut stat);
  let before = ok!(stat);
  assert!(matches!(before.type_, DescriptorType::RegularFile));
  assert_eq!((before.size, before.link_count), (12, 1));
  let mut resize = SetSize::new(&file, 5);
  run(set, &mut resize);
  ok!(resize);
  assert_eq!(read_from(set, &file, 0).0, b"hello");
  let mut resize = SetSize::new(&file, 8);
  run(set, &mut resize);
  ok!(resize);
  assert_eq!(read_from(set, &file, 0).0, b"hello\0\0\0");
  run(set, &mut stat);
  assert_eq!(ok!(stat).size, 8);
  say!(out, "stat, set-size: ok");

  // Times, through the descriptor and through a path.
  let at =
    |seconds| NewTimestamp::Timestamp(Instant { seconds, nanoseconds: 250 });
  let mut times = SetTimes::new(&file, at(1_000_000), at(2_000_000));
  run(set, &mut times);
  ok!(times);
  run(set, &mut stat);
  let stamped = ok!(stat);
  let Some(modified) = stamped.data_modification_timestamp else {
    panic!("the host reported no modification time");
  };
  assert_eq!((modified.seconds, modified.nanoseconds), (2_000_000, 250));
  let Some(accessed) = stamped.data_access_timestamp else {
    panic!("the host reported no access time");
  };
  assert_eq!(accessed.seconds, 1_000_000);
  let mut times = SetTimesAt::new(
    &dir,
    PathFlags::empty(),
    "data.txt",
    NewTimestamp::NoChange,
    at(3_000_000),
  );
  run(set, &mut times);
  ok!(times);
  let mut stat_at = StatAt::new(&dir, PathFlags::empty(), "data.txt");
  run(set, &mut stat_at);
  let by_path = ok!(stat_at);
  assert_eq!(by_path.size, 8);
  let Some(modified) = by_path.data_modification_timestamp else {
    panic!("the host reported no modification time");
  };
  assert_eq!(modified.seconds, 3_000_000);
  let Some(accessed) = by_path.data_access_timestamp else {
    panic!("the host reported no access time");
  };
  assert_eq!(accessed.seconds, 1_000_000, "no-change left the access time");
  let mut missing = StatAt::new(&dir, PathFlags::empty(), "absent");
  run(set, &mut missing);
  assert!(matches!(missing.result(), Some(Err(ErrorCode::NoEntry))));
  say!(out, "set-times, set-times-at, stat-at: ok");

  // Links.
  let mut hard =
    LinkAt::new(&dir, PathFlags::empty(), "data.txt", &dir, "hard");
  run(set, &mut hard);
  ok!(hard);
  run(set, &mut stat);
  assert_eq!(ok!(stat).link_count, 2);
  let mut soft = SymlinkAt::new(&dir, "data.txt", "soft");
  run(set, &mut soft);
  ok!(soft);
  let mut target = ReadlinkAt::new(&dir, "soft");
  run(set, &mut target);
  assert_eq!(ok!(target), "data.txt");
  let mut not_a_link = ReadlinkAt::new(&dir, "data.txt");
  run(set, &mut not_a_link);
  assert!(matches!(not_a_link.result(), Some(Err(ErrorCode::Invalid))));
  let mut link_stat = StatAt::new(&dir, PathFlags::empty(), "soft");
  run(set, &mut link_stat);
  assert!(matches!(ok!(link_stat).type_, DescriptorType::SymbolicLink));
  let mut followed = StatAt::new(&dir, PathFlags::SYMLINK_FOLLOW, "soft");
  run(set, &mut followed);
  assert!(matches!(ok!(followed).type_, DescriptorType::RegularFile));
  say!(out, "link-at, symlink-at, readlink-at: ok");

  // Rename, including across directories, then clean everything up.
  let mut rename = RenameAt::new(&dir, "hard", &root, "moved");
  run(set, &mut rename);
  ok!(rename);
  let mut moved = StatAt::new(&root, PathFlags::empty(), "moved");
  run(set, &mut moved);
  assert_eq!(ok!(moved).size, 8);
  let mut gone = StatAt::new(&dir, PathFlags::empty(), "hard");
  run(set, &mut gone);
  assert!(matches!(gone.result(), Some(Err(ErrorCode::NoEntry))));

  let mut not_empty = RemoveDirectoryAt::new(&root, "work");
  run(set, &mut not_empty);
  assert!(matches!(not_empty.result(), Some(Err(ErrorCode::NotEmpty))));
  for (parent, name) in [(&dir, "soft"), (&dir, "data.txt"), (&root, "moved")] {
    let mut unlink = UnlinkFileAt::new(parent, name);
    run(set, &mut unlink);
    ok!(unlink);
  }
  let mut again = UnlinkFileAt::new(&dir, "data.txt");
  run(set, &mut again);
  assert!(matches!(again.result(), Some(Err(ErrorCode::NoEntry))));
  let mut rmdir = RemoveDirectoryAt::new(&root, "work");
  run(set, &mut rmdir);
  ok!(rmdir);
  say!(out, "rename-at, unlink-file-at, remove-directory-at: ok");

  // Many calls started together, told apart by their subtasks.
  let names = ["a", "b", "c", "d", "e"];
  let mut creates: Vec<_> =
    names.iter().map(|name| CreateDirectoryAt::new(&root, name)).collect();
  // SAFETY: `creates` is not touched until every call has returned.
  let mut waiting: Vec<Option<Subtask>> =
    creates.iter_mut().map(|call| unsafe { set.start(call) }).collect();
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
  for call in &mut creates {
    ok!(call);
  }
  let mut removes: Vec<_> =
    names.iter().map(|name| RemoveDirectoryAt::new(&root, name)).collect();
  for call in &mut removes {
    run(set, call);
    ok!(call);
  }
  say!(out, "batched calls: ok");

  // List a directory with a buffer smaller than its contents, so the
  // listing takes several reads.
  let mut mkdir = CreateDirectoryAt::new(&root, "ls");
  run(set, &mut mkdir);
  ok!(mkdir);
  let listed = match open(
    set,
    &root,
    "ls",
    OpenFlags::DIRECTORY,
    DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
  ) {
    Ok(dir) => dir,
    Err(error) => panic!("opening the listed directory failed: {error:?}"),
  };
  for name in ["one.txt", "two.txt", "three.txt"] {
    let created =
      open(set, &listed, name, OpenFlags::CREATE, DescriptorFlags::WRITE);
    assert!(created.is_ok());
  }
  let mut sub = CreateDirectoryAt::new(&listed, "sub");
  run(set, &mut sub);
  ok!(sub);

  let (entries, status) = listed.read_directory();
  let mut status = StreamStatus::new(status);
  let entries = set.add_dir_reader(entries);
  let mut buf = DirEntryBuf::with_capacity(2);
  let mut found = Vec::new();
  let mut reads = 0;
  loop {
    // SAFETY: `buf` outlives the read, which has ended once this statement
    // has.
    let done = match unsafe { set.read_dir(entries, &mut buf) } {
      Ok(Started::Done(result)) => result,
      Ok(Started::Blocked) => match set.wait() {
        Some(Event::ReadDir(ended, result)) if ended == entries => result,
        other => unreachable!("only the listing is in flight: {other:?}"),
      },
      Err(error) => panic!("the listing read was refused: {error}"),
    };
    reads += 1;
    let before = found.len();
    found.extend(buf.drain().map(|entry| {
      (entry.name, matches!(entry.type_, DescriptorType::Directory))
    }));
    assert_eq!(found.len() - before, done.count());
    assert!(done.count() <= 2, "a read delivered more than the buffer holds");
    if let StreamResult::Dropped(_) = done {
      break;
    }
  }
  read_status(set, &mut status);
  assert!(matches!(status.result(), Some(Ok(()))));
  assert_eq!(set.close_dir_reader(entries), Ok(None));
  found.sort();
  assert_eq!(
    found,
    [
      ("one.txt".to_string(), false),
      ("sub".to_string(), true),
      ("three.txt".to_string(), false),
      ("two.txt".to_string(), false),
    ]
  );
  assert!(reads >= 2, "four entries fit a two-entry buffer in one read");

  // A cancelled listing read leaves nothing behind.
  let (entries, _status) = listed.read_directory();
  let entries = set.add_dir_reader(entries);
  // SAFETY: `buf` outlives the read, which the cancel below ends.
  let cancelled = match unsafe { set.read_dir(entries, &mut buf) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.cancel_read_dir(entries) {
      Some(result) => result,
      None => unreachable!("the blocked read is in flight"),
    },
    Err(error) => panic!("the listing read was refused: {error}"),
  };
  assert_eq!(buf.drain().count(), cancelled.count());
  assert_eq!(set.cancel_read_dir(entries), None);
  assert_eq!(set.close_dir_reader(entries), Ok(None));

  for name in ["one.txt", "two.txt", "three.txt"] {
    let mut unlink = UnlinkFileAt::new(&listed, name);
    run(set, &mut unlink);
    ok!(unlink);
  }
  let mut rmdir = RemoveDirectoryAt::new(&listed, "sub");
  run(set, &mut rmdir);
  ok!(rmdir);
  let mut rmdir = RemoveDirectoryAt::new(&root, "ls");
  run(set, &mut rmdir);
  ok!(rmdir);
  say!(out, "read-directory: ok ({reads} reads for 4 entries)");
}
