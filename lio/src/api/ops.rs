//! Typed I/O operations.
//!
//! Each type here is one operation as an [`OpModel`]: polling it submits the
//! operation, with the buffers it owns, and produces the result. They are
//! made by the functions in [`api`](crate::api), and are opaque: what an
//! operation submits and how its result is read is fixed inside lio.
//!
//! Run one with [`Io`](crate::api::Io), or take it out with
//! [`Io::into_inner`](crate::api::Io::into_inner) and poll it from a model
//! of your own.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crate::api::resource::Resource;
use crate::backend::op::{FileStat, ReadDirBuf};
use crate::model::{Cx, OpModel, Step};
use crate::{BufResult, IoBufMutVec, IoBufVec, ready};

pub(crate) mod raw;

pub use raw::{LinkKind, UnixAddr};

leaf!(
  /// Does nothing, through the driver.
  [] pub struct Nop(raw::Nop) => io::Result<()>
);
leaf!(
  /// Completes after a duration.
  [] pub struct Sleep(raw::Sleep) => io::Result<()>
);
leaf!(
  /// Creates a socket.
  [] pub struct Socket(raw::Socket) => io::Result<Resource>
);
leaf!(
  /// Accepts a connection and its peer address.
  [] pub struct Accept(raw::Accept) => io::Result<(Resource, SocketAddr)>
);
leaf!(
  /// Accepts a connection on a Unix socket.
  [] pub struct AcceptUnix(raw::AcceptUnix) => io::Result<(Resource, UnixAddr)>
);
leaf!(
  /// Connects a socket.
  [] pub struct Connect(raw::Connect) => io::Result<()>
);
leaf!(
  /// Binds a socket to an address.
  [] pub struct Bind(raw::Bind) => io::Result<()>
);
leaf!(
  /// Marks a socket as listening.
  [] pub struct Listen(raw::Listen) => io::Result<()>
);
leaf!(
  /// Shuts down part of a connection.
  [] pub struct Shutdown(raw::Shutdown) => io::Result<()>
);
leaf!(
  /// Flushes a file to disk.
  [] pub struct Fsync(raw::Fsync) => io::Result<()>
);
leaf!(
  /// Reads into an owned buffer. Produces the byte count and the buffer.
  [B: IoBufMutVec + 'static] pub struct Read<B>(raw::Read<B>) => BufResult<usize, B>
);
leaf!(
  /// Writes an owned buffer. Produces the byte count and the buffer.
  [B: IoBufVec + std::marker::Send + Sync + 'static] pub struct Write<B>(raw::Write<B>) => BufResult<usize, B>
);
leaf!(
  /// Receives from a socket into an owned buffer.
  [B: IoBufMutVec + std::marker::Send + Sync + 'static] pub struct Recv<B>(raw::Recv<B>) => BufResult<usize, B>
);
leaf!(
  /// Receives a datagram and the address it came from.
  [B: IoBufMutVec + std::marker::Send + Sync + 'static] pub struct RecvFrom<B>(raw::RecvFrom<B>)
    => (io::Result<usize>, B, Option<SocketAddr>)
);
leaf!(
  /// Sends an owned buffer on a socket.
  [B: IoBufVec + std::marker::Send + Sync + 'static] pub struct Send<B>(raw::Send<B>) => BufResult<usize, B>
);
leaf!(
  /// Opens a file relative to a directory.
  [] pub struct OpenAt(raw::OpenAt) => io::Result<Resource>
);
leaf!(
  /// Reads a file's metadata.
  [] pub struct Stat(raw::Stat) => io::Result<FileStat>
);
leaf!(
  /// Reads directory entries into a buffer.
  [] pub struct ReadDir(raw::ReadDir) => io::Result<ReadDirBuf>
);
leaf!(
  /// Removes a file or an empty directory.
  [] pub struct UnlinkAt(raw::UnlinkAt) => io::Result<()>
);
leaf!(
  /// Renames a file.
  [] pub struct RenameAt(raw::RenameAt) => io::Result<()>
);
leaf!(
  /// Creates a directory.
  [] pub struct MkdirAt(raw::MkdirAt) => io::Result<()>
);
leaf!(
  /// Creates a hard or symbolic link.
  [] pub struct LinkAt(raw::LinkAt) => io::Result<()>
);
leaf!(
  /// Reads the target of a symbolic link into an owned buffer.
  [B: IoBufMutVec + std::marker::Send + Sync + 'static] pub struct ReadlinkAt<B>(raw::ReadlinkAt<B>) => BufResult<usize, B>
);
leaf!(
  /// Reads the current working directory into an owned buffer.
  [B: IoBufMutVec + std::marker::Send + Sync + 'static] pub struct GetCwd<B>(raw::GetCwd<B>) => BufResult<usize, B>
);
leaf!(
  /// Starts a process.
  [] pub struct Spawn(raw::Spawn) => io::Result<crate::api::Pid>
);

/// Yields every time a period has passed, until it is dropped.
pub struct Interval {
  period: Duration,
  sleep: Sleep,
}

impl Interval {
  pub(crate) fn new(period: Duration) -> Self {
    Self { period, sleep: raw::Sleep::new(period).into() }
  }

  pub fn period(&self) -> Duration {
    self.period
  }
}

impl OpModel for Interval {
  type Item = io::Result<()>;

  fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
    let tick = ready!(self.sleep.poll(cx));
    // The next period starts when this item has been delivered.
    self.sleep = raw::Sleep::new(self.period).into();
    Step::Yield(tick)
  }
}
