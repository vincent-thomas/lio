//! lio on WebAssembly: the WASI 0.3 backend driven through lio's own API.
//!
//! Everything here asserts what it did, so the run doubles as a test of the
//! backend. It is its own crate because `lio`'s dev-dependencies do not
//! build for `wasm32-wasip3`. Nothing in it is specific to WASI: built for
//! the host, it runs against the native backend from inside an empty
//! directory.
//!
//! ```text
//! cargo build -p lio-wasi-example --target wasm32-wasip3
//! wasmtime run -S p3 --dir <empty directory>::/ \
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
use lio::api::{self, FileMode, OpenFlags, UnlinkKind};
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

    // Standard output, and two writes to it at once.
    let out = Resource::stdout();
    let (written, _) = api::write(&out, b"written by lio\n".to_vec()).await;
    assert_eq!(written?, 15);
    let first = api::write(&out, b"first, ".to_vec()).into_future();
    let second = api::write(&out, b"second\n".to_vec()).into_future();
    let (first, second) = (pin!(first), pin!(second));
    let ((a, _), (b, _)) = join(first, second).await;
    assert_eq!((a?, b?), (7, 7));

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
