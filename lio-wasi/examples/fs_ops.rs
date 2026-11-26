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
  DescriptorType, ErrorCode, GetFlags, GetType, LinkAt, NewTimestamp, OpenAt,
  OpenFlags, PathFlags, ReadlinkAt, RemoveDirectoryAt, RenameAt, SetSize,
  SetTimes, SetTimesAt, Stat, StatAt, StreamStatus, SymlinkAt, Sync, SyncData,
  UnlinkFileAt,
};
use lio_wasi::{Call, Completion, Nop, Outcome, Queue};
use wasip3::clocks::system_clock::Instant;

fn next(queue: &mut Queue) -> Completion {
  let Some(done) = queue.wait() else {
    unreachable!("an operation is outstanding")
  };
  done
}

/// Pushes `call` and waits for it to return.
fn run<C: Call>(queue: &mut Queue, call: &mut C) {
  // SAFETY: `call` is borrowed for this whole function, which takes the
  // completion before returning.
  unsafe { queue.push_call(call, 42) };
  let done = next(queue);
  assert_eq!((done.user_data(), done.outcome()), (42, Outcome::Returned));
}

fn open(
  queue: &mut Queue,
  dir: &Descriptor,
  path: &str,
  open_flags: OpenFlags,
  flags: DescriptorFlags,
) -> Result<Descriptor, ErrorCode> {
  let mut call = OpenAt::new(dir, PathFlags::empty(), path, open_flags, flags);
  run(queue, &mut call);
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
  queue: &mut Queue,
  file: &Descriptor,
  offset: u64,
  data: &[u8],
) -> Result<(), ErrorCode> {
  let (writer, reader) = wasip3::wit_stream::new::<u8>();
  let mut status = StreamStatus::new(file.write_via_stream(reader, offset));
  let writer = queue.add_writer(writer);
  let mut sent = 0;
  while sent < data.len() {
    let rest = &data[sent..];
    // SAFETY: `data` outlives the completion taken on the next line.
    let pushed =
      unsafe { queue.push_write(writer, rest.as_ptr(), rest.len(), 1) };
    assert_eq!(pushed, Ok(()));
    let done = next(queue).outcome();
    if let Outcome::Closed(_) = done {
      break;
    }
    sent += done.count();
  }
  assert_eq!(queue.close_writer(writer), Ok(()));
  run(queue, &mut status);
  let Some(result) = status.result() else {
    unreachable!("a returned status has a result")
  };
  assert!(status.result().is_none(), "a status is taken once");
  result
}

/// Reads from `offset` to the end and returns the bytes and the status.
fn read_from(
  queue: &mut Queue,
  file: &Descriptor,
  offset: u64,
) -> (Vec<u8>, Result<(), ErrorCode>) {
  let (stream, result) = file.read_via_stream(offset);
  let mut status = StreamStatus::new(result);
  let stream = queue.add_reader(stream);
  let mut out = Vec::new();
  let mut buf = [0u8; 64];
  loop {
    // SAFETY: `buf` outlives the completion taken on the next line.
    let pushed =
      unsafe { queue.push_read(stream, buf.as_mut_ptr(), buf.len(), 1) };
    assert_eq!(pushed, Ok(()));
    let done = next(queue).outcome();
    out.extend_from_slice(&buf[..done.count()]);
    if let Outcome::Closed(_) = done {
      break;
    }
  }
  assert_eq!(queue.close_reader(stream), Ok(()));
  run(queue, &mut status);
  let Some(result) = status.result() else {
    unreachable!("a returned status has a result")
  };
  (out, result)
}

fn main() {
  let Some((root, _name)) =
    wasip3::filesystem::preopens::get_directories().into_iter().next()
  else {
    panic!("run with a preopened scratch directory");
  };
  let mut queue = Queue::new();
  let queue = &mut queue;

  run(queue, &mut Nop);
  println!("nop: ok");

  // mkdir, then open it as a directory.
  let mut mkdir = CreateDirectoryAt::new(&root, "work");
  run(queue, &mut mkdir);
  ok!(mkdir);
  run(queue, &mut mkdir);
  assert!(matches!(mkdir.result(), Some(Err(ErrorCode::Exist))));
  let dir = match open(
    queue,
    &root,
    "work",
    OpenFlags::DIRECTORY,
    DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
  ) {
    Ok(dir) => dir,
    Err(error) => panic!("opening the new directory failed: {error:?}"),
  };
  let mut kind = GetType::new(&dir);
  run(queue, &mut kind);
  assert!(matches!(ok!(kind), DescriptorType::Directory));
  println!("create-directory-at, open-at, get-type: ok");

  // Create a file, write it, read it back.
  let file = match open(
    queue,
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
      queue,
      &dir,
      "data.txt",
      OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
      DescriptorFlags::READ,
    ),
    Err(ErrorCode::Exist)
  ));
  assert!(matches!(write_at(queue, &file, 0, b"hello, wasi"), Ok(())));
  assert!(matches!(write_at(queue, &file, 7, b"WASI!"), Ok(())));
  let (bytes, status) = read_from(queue, &file, 0);
  assert_eq!(bytes, b"hello, WASI!");
  assert!(matches!(status, Ok(())));
  let (tail, _) = read_from(queue, &file, 7);
  assert_eq!(tail, b"WASI!");
  println!("write-via-stream, read-via-stream, stream status: ok");

  // A stream that fails reports why through its status.
  let (bytes, status) = read_from(queue, &dir, 0);
  assert!(bytes.is_empty());
  assert!(status.is_err(), "reading a directory as a file succeeded");
  println!("failed stream status: {status:?}");

  let mut flags = GetFlags::new(&file);
  run(queue, &mut flags);
  let flags = ok!(flags);
  assert!(flags.contains(DescriptorFlags::READ | DescriptorFlags::WRITE));
  let mut kind = GetType::new(&file);
  run(queue, &mut kind);
  assert!(matches!(ok!(kind), DescriptorType::RegularFile));
  println!("get-flags, get-type: ok");

  let mut sync = Sync::new(&file);
  run(queue, &mut sync);
  ok!(sync);
  let mut sync_data = SyncData::new(&file);
  run(queue, &mut sync_data);
  ok!(sync_data);
  let mut advise = Advise::new(&file, 0, 12, Advice::Sequential);
  run(queue, &mut advise);
  ok!(advise);
  println!("sync, sync-data, advise: ok");

  // Stat sees the size; set-size changes it both ways.
  let mut stat = Stat::new(&file);
  run(queue, &mut stat);
  let before = ok!(stat);
  assert!(matches!(before.type_, DescriptorType::RegularFile));
  assert_eq!((before.size, before.link_count), (12, 1));
  let mut resize = SetSize::new(&file, 5);
  run(queue, &mut resize);
  ok!(resize);
  assert_eq!(read_from(queue, &file, 0).0, b"hello");
  let mut resize = SetSize::new(&file, 8);
  run(queue, &mut resize);
  ok!(resize);
  assert_eq!(read_from(queue, &file, 0).0, b"hello\0\0\0");
  run(queue, &mut stat);
  assert_eq!(ok!(stat).size, 8);
  println!("stat, set-size: ok");

  // Times, through the descriptor and through a path.
  let at =
    |seconds| NewTimestamp::Timestamp(Instant { seconds, nanoseconds: 250 });
  let mut times = SetTimes::new(&file, at(1_000_000), at(2_000_000));
  run(queue, &mut times);
  ok!(times);
  run(queue, &mut stat);
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
  run(queue, &mut times);
  ok!(times);
  let mut stat_at = StatAt::new(&dir, PathFlags::empty(), "data.txt");
  run(queue, &mut stat_at);
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
  run(queue, &mut missing);
  assert!(matches!(missing.result(), Some(Err(ErrorCode::NoEntry))));
  println!("set-times, set-times-at, stat-at: ok");

  // Links.
  let mut hard =
    LinkAt::new(&dir, PathFlags::empty(), "data.txt", &dir, "hard");
  run(queue, &mut hard);
  ok!(hard);
  run(queue, &mut stat);
  assert_eq!(ok!(stat).link_count, 2);
  let mut soft = SymlinkAt::new(&dir, "data.txt", "soft");
  run(queue, &mut soft);
  ok!(soft);
  let mut target = ReadlinkAt::new(&dir, "soft");
  run(queue, &mut target);
  assert_eq!(ok!(target), "data.txt");
  let mut not_a_link = ReadlinkAt::new(&dir, "data.txt");
  run(queue, &mut not_a_link);
  assert!(matches!(not_a_link.result(), Some(Err(ErrorCode::Invalid))));
  let mut link_stat = StatAt::new(&dir, PathFlags::empty(), "soft");
  run(queue, &mut link_stat);
  assert!(matches!(ok!(link_stat).type_, DescriptorType::SymbolicLink));
  let mut followed = StatAt::new(&dir, PathFlags::SYMLINK_FOLLOW, "soft");
  run(queue, &mut followed);
  assert!(matches!(ok!(followed).type_, DescriptorType::RegularFile));
  println!("link-at, symlink-at, readlink-at: ok");

  // Rename, including across directories, then clean everything up.
  let mut rename = RenameAt::new(&dir, "hard", &root, "moved");
  run(queue, &mut rename);
  ok!(rename);
  let mut moved = StatAt::new(&root, PathFlags::empty(), "moved");
  run(queue, &mut moved);
  assert_eq!(ok!(moved).size, 8);
  let mut gone = StatAt::new(&dir, PathFlags::empty(), "hard");
  run(queue, &mut gone);
  assert!(matches!(gone.result(), Some(Err(ErrorCode::NoEntry))));

  let mut not_empty = RemoveDirectoryAt::new(&root, "work");
  run(queue, &mut not_empty);
  assert!(matches!(not_empty.result(), Some(Err(ErrorCode::NotEmpty))));
  for (parent, name) in [(&dir, "soft"), (&dir, "data.txt"), (&root, "moved")] {
    let mut unlink = UnlinkFileAt::new(parent, name);
    run(queue, &mut unlink);
    ok!(unlink);
  }
  let mut again = UnlinkFileAt::new(&dir, "data.txt");
  run(queue, &mut again);
  assert!(matches!(again.result(), Some(Err(ErrorCode::NoEntry))));
  let mut rmdir = RemoveDirectoryAt::new(&root, "work");
  run(queue, &mut rmdir);
  ok!(rmdir);
  println!("rename-at, unlink-file-at, remove-directory-at: ok");

  // Many calls in flight together, results kept apart by `user_data`.
  let names = ["a", "b", "c", "d", "e"];
  let mut creates: Vec<_> =
    names.iter().map(|name| CreateDirectoryAt::new(&root, name)).collect();
  for (id, call) in creates.iter_mut().enumerate() {
    // SAFETY: `creates` is not touched until every completion is taken.
    unsafe { queue.push_call(call, id as u64) };
  }
  let mut seen = [false; 5];
  for _ in 0..names.len() {
    seen[next(queue).user_data() as usize] = true;
  }
  assert_eq!(seen, [true; 5]);
  for call in &mut creates {
    ok!(call);
  }
  let mut removes: Vec<_> =
    names.iter().map(|name| RemoveDirectoryAt::new(&root, name)).collect();
  for call in &mut removes {
    run(queue, call);
    ok!(call);
  }
  println!("batched calls: ok");
}
