//! A tour of lio's public API, one section per idea.
//!
//! Run it with `cargo run -p lio --example tour --features high`.
//!
//! 1. The driver: [`Lio`], and installing it for the thread.
//! 2. Four ways to get a result: a callback, a channel, your own channel,
//!    and `.await`.
//! 3. Files, through the operations in [`lio::api`].
//! 4. Files, through [`lio::fs`].
//! 5. Time: `sleep`, `interval`, and a run with a timeout.
//! 6. Sockets, through [`lio::net`].
//! 7. Models: operations combined into bigger ones, with [`OpModel`].
//! 8. Capacity: what happens when the driver is full.

use lio::ready;
use std::ffi::CString;
use std::future::{Future, IntoFuture};
use std::io;
use std::path::Path;
use std::pin::pin;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use lio::api::ops::{Read, Write};
use lio::api::resource::{IntoResource, Resource};
use lio::api::{self, FileMode, OpenFlags, ShutdownHow};
use lio::model::{self, Cx, Join, Next, OpModel, OpStream, Step, StreamExt};
use lio::net::{TcpListener, TcpStream};
use lio::{Lio, fs};

fn main() -> io::Result<()> {
  // ---- 1. The driver -------------------------------------------------------

  // A driver that holds up to 64 operations at once. It does nothing on its
  // own: operations move while `run`, `try_run` or `run_timeout` is called.
  let lio = Lio::new(64)?;

  // An operation is told which driver to use...
  api::nop().with_lio(&lio).when_done(|result| println!("nop: {result:?}"));
  lio.run()?;

  // ...or uses the one installed for this thread, until the guard is
  // dropped. The rest of the tour relies on that.
  let _installed = lio::install_global(lio.clone());

  let dir =
    std::env::temp_dir().join(format!("lio-tour-{}", std::process::id()));
  block_on(&lio, fs::create_dir(c_path(&dir), mode(0o755)))?;

  let result = tour(&lio, &dir);
  // Removes what the tour left, a few files at a time.
  block_on(&lio, fs::remove_dir_all(c_path(&dir)))?;
  result
}

fn tour(lio: &Lio, dir: &Path) -> io::Result<()> {
  results(lio, dir)?;
  files_by_operation(lio, dir)?;
  files_by_type(lio, dir)?;
  time(lio)?;
  sockets(lio)?;
  models(lio, dir)?;
  capacity()
}

// ---- 2. Four ways to get a result ------------------------------------------

fn results(lio: &Lio, dir: &Path) -> io::Result<()> {
  let log = fs::OpenOptions::new().append(true).create(true);
  let log = block_on(lio, log.open(c_path(&dir.join("log.txt"))))?;

  // A callback, called by the driver's thread when the operation is done.
  // A buffer goes in by value and comes back with the result.
  api::write(&log, b"callback\n".to_vec()).when_done(|(result, buf)| {
    println!("callback: wrote {result:?} of {} bytes", buf.len());
  });
  lio.run()?;

  // A channel lio makes for you.
  let mut receiver = api::write(&log, b"channel\n".to_vec()).send();
  let (result, _buf) = loop {
    match receiver.try_recv() {
      Some(done) => break done,
      None => lio.run()?,
    };
  };
  println!("channel: wrote {} bytes", result?);

  // A channel of your own, shared by as many operations as you like.
  let (sender, results) = mpsc::channel();
  api::nop().send_with(sender.clone());
  api::nop().send_with(sender);
  // One run may finish several operations: it returns how many.
  let mut finished = 0;
  while finished < 2 {
    finished += lio.run()?;
  }
  assert_eq!(results.iter().count(), 2);

  // A future. Any executor will do, as long as something runs the driver:
  // `block_on`, at the end of this file, is a ten-line one.
  block_on(lio, async {
    let (result, _buf) = api::write(&log, b"future\n".to_vec()).await;
    println!("future: wrote {} bytes", result?);
    Ok(())
  })
}

// ---- 3. Files, operation by operation --------------------------------------

fn files_by_operation(lio: &Lio, dir: &Path) -> io::Result<()> {
  block_on(lio, async {
    // A `Resource` is a file, socket or directory. It closes itself when
    // its last clone is dropped.
    let dir = api::openat(
      &Resource::cwd(),
      c_path(dir),
      OpenFlags::READ | OpenFlags::DIRECTORY,
      mode(0),
    )
    .await?;

    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
    let file =
      api::openat(&dir, c"raw.txt".to_owned(), flags, mode(0o644)).await?;

    // Several buffers in one operation: a tuple, an array or a `Vec` of
    // them is written, or filled, in order.
    let parts = (b"hello, ".to_vec(), b"lio\n".to_vec());
    let (written, _parts) = api::write_at(&file, parts, 0).await;
    assert_eq!(written?, 11);
    api::fsync(&file).await?;

    // A read fills the buffer's spare capacity and sets its length.
    let (read, buf) =
      api::read_at(&file, Vec::<u8>::with_capacity(64), 0).await;
    assert_eq!(read?, 11);
    assert_eq!(buf, b"hello, lio\n");

    let stat = api::fstat(&file).await?;
    println!("raw.txt: {} bytes, {:?}", stat.size, stat.file_type);

    api::renameat(&dir, c"raw.txt".to_owned(), &dir, c"renamed.txt".to_owned())
      .await?;
    api::unlinkat(&dir, c"renamed.txt".to_owned(), api::UnlinkKind::File).await
  })
}

// ---- 4. Files, through `lio::fs` -------------------------------------------

fn files_by_type(lio: &Lio, dir: &Path) -> io::Result<()> {
  let path = dir.join("notes.txt");
  block_on(lio, async {
    let file = fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .open(c_path(&path))
      .await?;
    let (written, _buf) = file.write_at(b"one\ntwo\n".to_vec(), 0).await;
    written?;
    file.sync_all().await?;
    assert_eq!(file.metadata().await?.size, 8);

    assert_eq!(fs::read_to_string(&path).await?, "one\ntwo\n");
    assert_eq!(fs::metadata(&path).await?.size, 8);

    // The entries were read already: this is an ordinary iterator.
    for entry in fs::read_dir(dir).await? {
      println!("in the directory: {:?}", entry?.file_name());
    }
    Ok(())
  })
}

// ---- 5. Time ---------------------------------------------------------------

fn time(lio: &Lio) -> io::Result<()> {
  // Nothing is in flight: a run with a timeout comes back empty-handed.
  assert_eq!(lio.run_timeout(Duration::from_millis(1))?, 0);

  block_on(lio, async {
    api::sleep(Duration::from_millis(5)).await?;

    // A stream gives its items one at a time, and stops its model when it
    // is dropped.
    let mut ticks = api::interval(Duration::from_millis(2)).start();
    for _ in 0..3 {
      ticks.next().await.expect("an interval never ends")?;
    }
    Ok(())
  })
}

// ---- 6. Sockets ------------------------------------------------------------

fn sockets(lio: &Lio) -> io::Result<()> {
  block_on(lio, async {
    let listener = TcpListener::bind(([127, 0, 0, 1], 0).into()).await?;
    let addr = listener.local_addr()?;

    // Accepting and connecting at the same time: every operation is a
    // model, and `Join` runs two models together until both are done.
    let (accepted, client) =
      Join::new(listener.accept(), TcpStream::connect(addr)).await;
    let ((server, peer), client) = (accepted?, client?);
    println!("accepted a connection from {peer}");

    let (sent, _buf) = client.send(b"ping".to_vec()).await;
    assert_eq!(sent?, 4);
    let (received, buf) = server.recv(Vec::<u8>::with_capacity(16)).await;
    assert_eq!(received?, 4);
    assert_eq!(buf, b"ping");

    client.shutdown(ShutdownHow::Write).await?;
    let (received, _buf) = server.recv(Vec::<u8>::with_capacity(16)).await;
    assert_eq!(received?, 0, "the client is done sending");
    Ok(())
  })
}

// ---- 7. Models -------------------------------------------------------------

/// Writes all of a buffer, however many writes that takes.
///
/// A model is a state machine over operations, the way a future is one over
/// wake-ups. It holds the operation it is waiting on, and `poll` moves it on.
struct WriteAll {
  to: Resource,
  offset: u64,
  write: Write<Vec<u8>>,
}

impl WriteAll {
  fn new(to: &Resource, data: Vec<u8>) -> Self {
    let write = api::write_at(to, data, 0);
    Self { to: to.clone(), offset: 0, write }
  }
}

impl OpModel for WriteAll {
  type Item = io::Result<u64>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    loop {
      // `ready!` returns `Step::Pending` until the write has finished.
      let (result, mut buf) = ready!(self.write.poll(cx));
      let written = match result {
        Ok(written) => written,
        Err(err) => return Step::Done(Err(err)),
      };
      self.offset += written as u64;
      buf.drain(..written);
      if buf.is_empty() {
        return Step::Done(Ok(self.offset));
      }
      self.write = api::write_at(&self.to, buf, self.offset);
    }
  }
}

/// Reads a file a chunk at a time: an item per chunk, until the file ends.
struct Chunks {
  from: Resource,
  offset: u64,
  read: Read<Vec<u8>>,
  failed: bool,
}

impl Chunks {
  const SIZE: usize = 4;

  fn new(from: &Resource) -> Self {
    let read = api::read_at(from, Vec::<u8>::with_capacity(Self::SIZE), 0);
    Self { from: from.clone(), offset: 0, read, failed: false }
  }
}

impl OpStream for Chunks {
  type Item = io::Result<Vec<u8>>;

  fn poll_next(&mut self, cx: &mut Cx<'_>) -> Next<Self::Item> {
    if self.failed {
      return Next::End;
    }
    let (result, chunk) = ready!(self.read.poll(cx));
    match result {
      // The end of the file ends the stream.
      Ok(0) => Next::End,
      Ok(n) => {
        self.offset += n as u64;
        let next = Vec::<u8>::with_capacity(Self::SIZE);
        self.read = api::read_at(&self.from, next, self.offset);
        Next::Item(Ok(chunk))
      }
      // An error is the last item.
      Err(err) => {
        self.failed = true;
        Next::Item(Err(err))
      }
    }
  }
}

fn models(lio: &Lio, dir: &Path) -> io::Result<()> {
  let open = |name: &str| {
    fs::OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .open(c_path(&dir.join(name)))
  };

  block_on(lio, async {
    let (source, copy) = (open("source.txt").await?, open("copy.txt").await?);
    // The operations in `lio::api` take any resource: here, the files'.
    let (source, copy) = (source.into_resource(), copy.into_resource());

    // A model of your own has `with_lio`, `when_done`, `send` and
    // `send_with` like lio's, from `ModelExt`. `api::run` awaits one on
    // the driver installed for the thread.
    let written =
      api::run(WriteAll::new(&source, b"0123456789".to_vec())).await?;
    assert_eq!(written, 10);

    // Models nest: lio's own `copy` is one, and so is a `Join` of two.
    let (copied, stat) =
      Join::new(lio::io::copy(&source, &copy), api::fstat(&source)).await;
    assert_eq!((copied?, stat?.size), (10, 10));

    // Work that produces several items is an `OpStream`. Starting it
    // gives the handle its items are taken from.
    let mut chunks = Chunks::new(&copy).start();
    let mut text = Vec::new();
    while let Some(chunk) = chunks.next().await {
      text.extend(chunk?);
    }
    assert_eq!(text, b"0123456789");
    io::Result::Ok(())
  })?;

  // `spawn_stream` hands every item to a callback, and gives back a handle that
  // can stop the model.
  let ticks = api::interval(Duration::from_millis(1));
  let task = model::spawn_stream(lio, ticks, |tick| println!("tick: {tick:?}"));
  // One tick per run. Dropping the handle would leave the model running:
  // cancelling stops it here.
  lio.run()?;
  lio.run()?;
  task.cancel();
  Ok(())
}

// ---- 8. Capacity -----------------------------------------------------------

fn capacity() -> io::Result<()> {
  let small = Lio::new(1)?;
  let (sender, results) = mpsc::channel();

  api::sleep(Duration::from_millis(1))
    .with_lio(&small)
    .send_with(sender.clone());
  // One operation fills this driver. Check before submitting for strict
  // backpressure...
  assert!(small.is_full());
  // ...or submit anyway: the operation waits its turn.
  api::sleep(Duration::from_millis(1)).with_lio(&small).send_with(sender);

  while results.try_iter().count() == 0 {
    small.run()?;
  }
  while results.try_iter().count() == 0 {
    small.run()?;
  }
  assert!(!small.is_full());
  Ok(())
}

// ---- Helpers ---------------------------------------------------------------

/// Runs `future` to completion, running the driver whenever it is waiting.
fn block_on<F: IntoFuture>(lio: &Lio, future: F) -> F::Output {
  let mut future = pin!(future.into_future());
  let mut cx = Context::from_waker(Waker::noop());
  loop {
    if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
      return output;
    }
    lio.run().expect("the driver failed");
  }
}

fn c_path(path: &Path) -> CString {
  let path = path.to_str().expect("a UTF-8 path");
  CString::new(path).expect("a path without NUL bytes")
}

fn mode(octal: u32) -> FileMode {
  FileMode::from_octal(octal).expect("permission bits only")
}
