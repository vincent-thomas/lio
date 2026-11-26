//! lio on WebAssembly: the WASI 0.3 backend driven through lio's own API.
//!
//! Everything here asserts what it did, so the run doubles as a test of the
//! backend. It is its own crate because `lio`'s dev-dependencies do not
//! build for `wasm32-wasip3`.
//!
//! It expects an empty directory preopened as `/`, and network access for
//! the loopback sockets:
//!
//! ```text
//! cargo build -p lio-wasi-example --target wasm32-wasip3
//! wasmtime run -S p3 -S inherit-network --dir <empty directory>::/ \
//!   target/wasm32-wasip3/debug/lio-wasi-example.wasm
//! ```

use std::ffi::CString;
use std::future::{Future, IntoFuture};
use std::io;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use lio::api::ops::LinkKind;
use lio::api::resource::Resource;
use lio::api::{self, FileMode, OpenFlags, ShutdownHow, UnlinkKind};
use lio::net::{TcpListener, TcpStream, UdpSocket};
use lio::{Lio, fs};

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

fn c(path: &str) -> CString {
  CString::new(path).expect("a path without NUL bytes")
}

fn mode(octal: u32) -> FileMode {
  FileMode::from_octal(octal).expect("permission bits only")
}

fn main() -> io::Result<()> {
  let lio = Lio::new(64)?;
  let _installed = lio::install_global(lio.clone());
  let cwd = Resource::cwd();

  block_on(&lio, async {
    api::nop().await?;
    println!("nop: ok");

    // Directories, as anchors for what follows.
    api::mkdirat(&cwd, c("work"), mode(0o755)).await?;
    let exists = api::mkdirat(&cwd, c("work"), mode(0o755)).await;
    assert_eq!(exists.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    let dir = api::openat(
      &cwd,
      c("work"),
      OpenFlags::READ | OpenFlags::DIRECTORY,
      mode(0),
    )
    .await?;
    println!("mkdirat, openat a directory: ok");

    // Positional I/O, several buffers per operation.
    let flags = OpenFlags::READ | OpenFlags::WRITE | OpenFlags::CREATE;
    let file = api::openat(&dir, c("raw.txt"), flags, mode(0o644)).await?;
    let parts = (b"hello, ".to_vec(), b"lio\n".to_vec());
    let (written, _parts) = api::write_at(&file, parts, 0).await;
    assert_eq!(written?, 11);
    api::fsync(&file).await?;
    let (read, buf) =
      api::read_at(&file, Vec::<u8>::with_capacity(64), 0).await;
    assert_eq!(read?, 11);
    assert_eq!(buf, b"hello, lio\n");
    let (read, buf) = api::read_at(&file, Vec::<u8>::with_capacity(3), 7).await;
    assert_eq!((read?, buf.as_slice()), (3, &b"lio"[..]));
    let (read, buf) =
      api::read_at(&file, Vec::<u8>::with_capacity(8), 11).await;
    assert_eq!((read?, buf.len()), (0, 0), "a read at the end is empty");
    println!("write_at, read_at, fsync: ok");

    // The cursor: reads and writes without an offset follow each other.
    let (written, _) = api::write(&file, b"HELLO".to_vec()).await;
    assert_eq!(written?, 5);
    let (read, buf) = api::read(&file, Vec::<u8>::with_capacity(2)).await;
    assert_eq!((read?, buf.as_slice()), (2, &b", "[..]));
    let (read, buf) = api::read(&file, Vec::<u8>::with_capacity(64)).await;
    assert_eq!((read?, buf.as_slice()), (4, &b"lio\n"[..]));
    let (_, buf) = api::read_at(&file, Vec::<u8>::with_capacity(64), 0).await;
    assert_eq!(buf, b"HELLO, lio\n");
    println!("write, read with a cursor: ok");

    // Metadata.
    let stat = api::fstat(&file).await?;
    assert!(stat.is_file());
    assert_eq!(stat.size, 11);
    assert!(api::fstat(&dir).await?.is_dir());
    let missing = api::statat(&dir, c("absent"), true).await;
    assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    println!("fstat, statat: ok");

    // Links.
    api::linkat(&dir, c("raw.txt"), &dir, c("hard.txt"), LinkKind::Hard)
      .await?;
    assert_eq!(api::fstat(&file).await?.nlink, 2);
    api::linkat(&dir, c("raw.txt"), &dir, c("soft.txt"), LinkKind::Soft)
      .await?;
    let (len, target) =
      api::readlinkat(&dir, c("soft.txt"), Vec::<u8>::with_capacity(64)).await;
    assert_eq!((len?, target.as_slice()), (7, &b"raw.txt"[..]));
    assert!(api::statat(&dir, c("soft.txt"), false).await?.is_symlink());
    assert!(api::statat(&dir, c("soft.txt"), true).await?.is_file());
    println!("linkat, readlinkat: ok");

    // Append mode ignores the cursor.
    let log = api::openat(
      &dir,
      c("log.txt"),
      OpenFlags::APPEND | OpenFlags::CREATE,
      mode(0o644),
    )
    .await?;
    for line in ["one\n", "two\n"] {
      let (written, _) = api::write(&log, line.as_bytes().to_vec()).await;
      assert_eq!(written?, 4);
    }
    drop(log);
    println!("append: ok");

    // `lio::fs`, by absolute path.
    assert_eq!(fs::read_to_string("/work/log.txt").await?, "one\ntwo\n");
    assert_eq!(fs::metadata("/work/raw.txt").await?.size, 11);
    let notes = fs::OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .open(c("/work/notes.txt"))
      .await?;
    let (written, _) = notes.write_at(b"notes".to_vec(), 0).await;
    assert_eq!(written?, 5);
    notes.sync_all().await?;
    assert_eq!(notes.metadata().await?.size, 5);
    drop(notes);
    println!("lio::fs: ok");

    // Listing: by batches small enough to need several, then all at once.
    let listed = fs::Directory::open(c("/work")).await?;
    let mut names = Vec::new();
    let mut batches = 0;
    loop {
      let batch = listed.readdir(api::ReadDirBuf::with_capacity(64, 2)).await?;
      batches += 1;
      assert!(batch.result.entries <= 2);
      names.extend(
        batch.iter().map(|e| String::from_utf8_lossy(e.name).into_owned()),
      );
      if batch.result.eof {
        break;
      }
    }
    names.sort();
    assert_eq!(
      names,
      ["hard.txt", "log.txt", "notes.txt", "raw.txt", "soft.txt"]
    );
    assert!(batches >= 3, "five entries came in {batches} batches of two");
    let again = listed.readdir(api::ReadDirBuf::with_capacity(64, 2)).await?;
    assert_eq!((again.result.entries, again.result.eof), (0, true));
    // A name that fits no buffer is an error, not a silent stall.
    let cramped = fs::Directory::open(c("/work")).await?;
    let tiny = cramped.readdir(api::ReadDirBuf::with_capacity(2, 2)).await;
    assert!(tiny.is_err(), "a name was squeezed into two bytes");
    drop((listed, cramped));

    let mut seen = Vec::new();
    for entry in fs::read_dir("/work").await? {
      let entry = entry?;
      let kind = entry.file_type();
      seen.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].0, "hard.txt");
    assert_eq!(seen[4], ("soft.txt".into(), Some(api::FileType::Symlink)));
    println!("readdir, read_dir: ok");

    // Rename and clean up.
    api::renameat(&dir, c("hard.txt"), &cwd, c("moved.txt")).await?;
    assert_eq!(api::statat(&cwd, c("moved.txt"), true).await?.size, 11);
    let busy = api::unlinkat(&cwd, c("work"), UnlinkKind::Directory).await;
    assert!(busy.is_err(), "removed a directory that is not empty");
    for name in ["raw.txt", "soft.txt", "log.txt", "notes.txt"] {
      api::unlinkat(&dir, c(name), UnlinkKind::File).await?;
    }
    api::unlinkat(&cwd, c("moved.txt"), UnlinkKind::File).await?;
    drop((file, dir));
    api::unlinkat(&cwd, c("work"), UnlinkKind::Directory).await?;
    println!("renameat, unlinkat: ok");

    // A whole tree removed at once, which lists every level of it.
    api::mkdirat(&cwd, c("tree"), mode(0o755)).await?;
    api::mkdirat(&cwd, c("tree/a"), mode(0o755)).await?;
    api::mkdirat(&cwd, c("tree/a/b"), mode(0o755)).await?;
    for path in ["tree/top.txt", "tree/a/mid.txt", "tree/a/b/deep.txt"] {
      let flags = OpenFlags::WRITE | OpenFlags::CREATE;
      drop(api::openat(&cwd, c(path), flags, mode(0o644)).await?);
    }
    fs::remove_dir_all(c("/tree")).await?;
    let gone = api::statat(&cwd, c("tree"), true).await;
    assert_eq!(gone.unwrap_err().kind(), io::ErrorKind::NotFound);
    println!("remove_dir_all: ok");

    // Standard output, and two writes to it at once.
    let out = Resource::stdout();
    let (written, _) = api::write(&out, b"written by lio\n".to_vec()).await;
    assert_eq!(written?, 15);
    let first = api::write(&out, b"first, ".to_vec()).into_future();
    let second = api::write(&out, b"second\n".to_vec()).into_future();
    let (first, second) = (pin!(first), pin!(second));
    let ((a, _), (b, _)) = join(first, second).await;
    assert_eq!((a?, b?), (7, 7));

    sockets().await?;

    // Time.
    let start = Instant::now();
    api::sleep(Duration::from_millis(50)).await?;
    assert!(start.elapsed() >= Duration::from_millis(50));
    println!("sleep: ok after {:?}", start.elapsed());

    io::Result::Ok(())
  })?;

  // A bounded run with nothing in flight comes back on its own.
  let start = Instant::now();
  lio.run_timeout(Duration::from_millis(30))?;
  println!("run_timeout: back after {:?}", start.elapsed());
  Ok(())
}

/// TCP and UDP over loopback, with both ends in this program.
async fn sockets() -> io::Result<()> {
  let listener = TcpListener::bind(([127, 0, 0, 1], 0).into()).await?;
  let addr = listener.local_addr()?;
  assert_ne!(addr.port(), 0, "the listener was given a port");

  // Accepting and connecting at the same time.
  let (accepted, client) =
    api::join(listener.accept(), TcpStream::connect(addr)).await;
  let ((server, peer), client) = (accepted?, client?);
  assert!(peer.ip().is_loopback());
  assert_ne!(peer.port(), addr.port(), "the peer is the client's end");
  println!("tcp accept, connect: ok");

  let (sent, _) = client.send(b"ping".to_vec()).await;
  assert_eq!(sent?, 4);
  let (received, buf) = server.recv(Vec::<u8>::with_capacity(16)).await;
  assert_eq!((received?, buf.as_slice()), (4, &b"ping"[..]));
  let (sent, _) = server.send(b"pong".to_vec()).await;
  assert_eq!(sent?, 4);
  let (received, buf) = client.recv(Vec::<u8>::with_capacity(16)).await;
  assert_eq!((received?, buf.as_slice()), (4, &b"pong"[..]));

  // More than one receive holds, and several buffers in one send.
  let big: Vec<u8> = (0..20_000u32).map(|n| (n % 251) as u8).collect();
  let halves = (big[..10_000].to_vec(), big[10_000..].to_vec());
  let (sent, _) = client.send(halves).await;
  assert_eq!(sent?, big.len());
  let mut got = Vec::new();
  while got.len() < big.len() {
    let (received, buf) = server.recv(Vec::<u8>::with_capacity(4096)).await;
    assert_ne!(received?, 0, "the stream ended early");
    got.extend_from_slice(&buf);
  }
  assert_eq!(got, big);
  println!("tcp send, recv: ok");

  // Two receives on one socket wait their turn, and each gets data.
  let first = server.recv(Vec::<u8>::with_capacity(1)).into_future();
  let second = server.recv(Vec::<u8>::with_capacity(1)).into_future();
  let (sent, _) = client.send(b"ab".to_vec()).await;
  assert_eq!(sent?, 2);
  let ((a, a_buf), (b, b_buf)) = join(pin!(first), pin!(second)).await;
  assert_eq!((a?, b?), (1, 1));
  let mut both = [a_buf[0], b_buf[0]];
  both.sort_unstable();
  assert_eq!(&both, b"ab");
  println!("tcp queued receives: ok");

  // Shutting down the write side ends the peer's stream.
  client.shutdown(ShutdownHow::Write).await?;
  let (received, _) = server.recv(Vec::<u8>::with_capacity(16)).await;
  assert_eq!(received?, 0, "the client is done sending");
  let (sent, _) = client.send(b"late".to_vec()).await;
  assert_eq!(sent.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
  // The other direction still works.
  let (sent, _) = server.send(b"bye".to_vec()).await;
  assert_eq!(sent?, 3);
  let (received, buf) = client.recv(Vec::<u8>::with_capacity(16)).await;
  assert_eq!((received?, buf.as_slice()), (3, &b"bye"[..]));
  println!("tcp shutdown: ok");

  // Nobody listens on a closed listener's port.
  drop((server, client, listener));
  let refused = TcpStream::connect(addr).await;
  assert_eq!(
    refused.err().map(|error| error.kind()),
    Some(io::ErrorKind::ConnectionRefused)
  );
  println!("tcp connect to a closed port: refused");

  // UDP, addressed and connected.
  let a = UdpSocket::bind(([127, 0, 0, 1], 0).into()).await?;
  let b = UdpSocket::bind(([127, 0, 0, 1], 0).into()).await?;
  let (a_addr, b_addr) = (a.local_addr()?, b.local_addr()?);
  let (sent, _) = a.sendto(b"datagram".to_vec(), b_addr).await;
  assert_eq!(sent?, 8);
  let (received, buf, from) = b.recvfrom(Vec::<u8>::with_capacity(64)).await;
  assert_eq!((received?, buf.as_slice()), (8, &b"datagram"[..]));
  assert_eq!(from, Some(a_addr));
  assert!(a.peer_addr().is_err(), "an unconnected socket has no peer");
  // A datagram longer than the buffer is cut to fit.
  let (sent, _) = a.sendto(b"too long".to_vec(), b_addr).await;
  assert_eq!(sent?, 8);
  let (received, buf) = b.recv(Vec::<u8>::with_capacity(3)).await;
  assert_eq!((received?, buf.as_slice()), (3, &b"too"[..]));
  println!("udp sendto, recvfrom: ok");
  Ok(())
}

/// Polls two futures until both are done.
async fn join<A: Future + Unpin, B: Future + Unpin>(
  mut a: A,
  mut b: B,
) -> (A::Output, B::Output) {
  let (mut left, mut right) = (None, None);
  std::future::poll_fn(move |cx| {
    if left.is_none()
      && let Poll::Ready(output) = std::pin::Pin::new(&mut a).poll(cx)
    {
      left = Some(output);
    }
    if right.is_none()
      && let Poll::Ready(output) = std::pin::Pin::new(&mut b).poll(cx)
    {
      right = Some(output);
    }
    if left.is_some() && right.is_some() {
      Poll::Ready((left.take().expect("set"), right.take().expect("set")))
    } else {
      Poll::Pending
    }
  })
  .await
}
