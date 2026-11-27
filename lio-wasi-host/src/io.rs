//! lio operations as futures wasmtime accepts, and errno translation.

use std::future::{Future, IntoFuture};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::local::Local;
use lio::api::Io;
use lio::model::OpModel;

use crate::bindings::wasi::filesystem::types::ErrorCode as FsError;
use crate::bindings::wasi::sockets::types::ErrorCode as SocketError;

/// A lio operation in flight.
///
/// lio's futures belong to the driver's thread, and wasmtime wants `Send`
/// ones. The whole host runs on that one thread ([`crate::block_on`]), so
/// the wrapper never has to refuse: it panics if the future is touched
/// from any other thread.
pub(crate) struct Op<M: OpModel + Unpin + 'static> {
  run: Local<<Io<M> as IntoFuture>::IntoFuture>,
  /// Expected to finish as soon as it is submitted.
  likely: bool,
  /// The driver was already given its one chance to finish this at once.
  driven: bool,
}

/// Starts `io`, which usually finishes at once, on the driver installed for
/// the thread.
pub(crate) fn op<M: OpModel + Unpin + 'static>(io: Io<M>) -> Op<M> {
  Op { run: Local::new(io.into_future()), likely: true, driven: false }
}

/// Starts `io`, which usually has to wait (for a peer, or for time to
/// pass): its submission is left to share a kernel entry.
pub(crate) fn op_waits<M: OpModel + Unpin + 'static>(io: Io<M>) -> Op<M> {
  Op { run: Local::new(io.into_future()), likely: false, driven: false }
}

impl<M: OpModel + Unpin + 'static> Op<M> {
  /// Says whether this one is expected to finish at once.
  pub(crate) fn likely(mut self, likely: bool) -> Self {
    self.likely = likely;
    self
  }
}

impl<M: OpModel + Unpin + 'static> Future for Op<M> {
  type Output = M::Item;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let me = self.get_mut();
    if let Poll::Ready(item) = Pin::new(&mut me.run).poll(cx) {
      return Poll::Ready(item);
    }
    if me.driven {
      return Poll::Pending;
    }
    me.driven = true;
    if !crate::exec::drive(me.likely) {
      return Poll::Pending;
    }
    Pin::new(&mut me.run).poll(cx)
  }
}

/// Whether a stream's next operation is worth submitting at once.
///
/// It is while they keep finishing at once. After one that had to wait, the
/// next few are left to share a kernel entry, and then one is tried again:
/// twice as many are left each time, up to 63.
#[derive(Default)]
pub(crate) struct Guess {
  /// Operations in a row that had to wait when submitted at once.
  misses: u8,
  /// Operations left before the next is submitted at once.
  skip: u8,
}

impl Guess {
  /// A stream whose first operation is expected to wait.
  pub(crate) fn waits() -> Self {
    Self { misses: 1, skip: 1 }
  }

  /// Whether to submit the operation starting now at once.
  pub(crate) fn likely(&mut self) -> bool {
    if self.skip > 0 {
      self.skip -= 1;
      return false;
    }
    true
  }

  /// Records how an operation that was submitted at once went.
  pub(crate) fn record(&mut self, finished_at_once: bool) {
    if finished_at_once {
      self.misses = 0;
    } else {
      self.misses = (self.misses + 1).min(6);
      self.skip = (1u8 << self.misses) - 1;
    }
  }
}

/// The WASI filesystem error closest to an OS error.
pub(crate) fn fs_error(error: &io::Error) -> FsError {
  let Some(errno) = error.raw_os_error() else {
    return match error.kind() {
      io::ErrorKind::NotFound => FsError::NoEntry,
      io::ErrorKind::PermissionDenied => FsError::NotPermitted,
      io::ErrorKind::AlreadyExists => FsError::Exist,
      io::ErrorKind::InvalidInput => FsError::Invalid,
      _ => FsError::Io,
    };
  };
  match errno {
    libc::EACCES => FsError::Access,
    libc::EALREADY => FsError::Already,
    libc::EBADF => FsError::BadDescriptor,
    libc::EBUSY => FsError::Busy,
    libc::EDEADLK => FsError::Deadlock,
    libc::EDQUOT => FsError::Quota,
    libc::EEXIST => FsError::Exist,
    libc::EFBIG => FsError::FileTooLarge,
    libc::EILSEQ => FsError::IllegalByteSequence,
    libc::EINPROGRESS => FsError::InProgress,
    libc::EINTR => FsError::Interrupted,
    libc::EINVAL => FsError::Invalid,
    libc::EIO => FsError::Io,
    libc::EISDIR => FsError::IsDirectory,
    libc::ELOOP => FsError::Loop,
    libc::EMLINK => FsError::TooManyLinks,
    libc::EMSGSIZE => FsError::MessageSize,
    libc::ENAMETOOLONG => FsError::NameTooLong,
    libc::ENODEV => FsError::NoDevice,
    libc::ENOENT => FsError::NoEntry,
    libc::ENOLCK => FsError::NoLock,
    libc::ENOMEM => FsError::InsufficientMemory,
    libc::ENOSPC => FsError::InsufficientSpace,
    libc::ENOTDIR => FsError::NotDirectory,
    libc::ENOTEMPTY => FsError::NotEmpty,
    libc::ENOTRECOVERABLE => FsError::NotRecoverable,
    libc::ENOTSUP => FsError::Unsupported,
    libc::ENOTTY => FsError::NoTty,
    libc::ENXIO => FsError::NoSuchDevice,
    libc::EOVERFLOW => FsError::Overflow,
    libc::EPERM => FsError::NotPermitted,
    libc::EPIPE => FsError::Pipe,
    libc::EROFS => FsError::ReadOnly,
    libc::ESPIPE => FsError::InvalidSeek,
    libc::ETXTBSY => FsError::TextFileBusy,
    libc::EXDEV => FsError::CrossDevice,
    _ => FsError::Io,
  }
}

/// The WASI socket error closest to an OS error.
pub(crate) fn socket_error(error: &io::Error) -> SocketError {
  let Some(errno) = error.raw_os_error() else {
    return match error.kind() {
      io::ErrorKind::InvalidInput => SocketError::InvalidArgument,
      io::ErrorKind::TimedOut => SocketError::Timeout,
      _ => SocketError::Other(Some(error.to_string())),
    };
  };
  match errno {
    libc::EACCES | libc::EPERM => SocketError::AccessDenied,
    libc::EOPNOTSUPP | libc::EAFNOSUPPORT | libc::EPROTONOSUPPORT => {
      SocketError::NotSupported
    }
    libc::EINVAL | libc::EDESTADDRREQ => SocketError::InvalidArgument,
    libc::ENOMEM | libc::ENOBUFS => SocketError::OutOfMemory,
    libc::ETIMEDOUT => SocketError::Timeout,
    libc::EISCONN | libc::ENOTCONN | libc::EALREADY => {
      SocketError::InvalidState
    }
    libc::EADDRNOTAVAIL => SocketError::AddressNotBindable,
    libc::EADDRINUSE => SocketError::AddressInUse,
    libc::EHOSTUNREACH
    | libc::ENETUNREACH
    | libc::ENETDOWN
    | libc::EHOSTDOWN => SocketError::RemoteUnreachable,
    libc::ECONNREFUSED => SocketError::ConnectionRefused,
    libc::EPIPE => SocketError::ConnectionBroken,
    libc::ECONNRESET => SocketError::ConnectionReset,
    libc::ECONNABORTED => SocketError::ConnectionAborted,
    libc::EMSGSIZE => SocketError::DatagramTooLarge,
    _ => SocketError::Other(Some(error.to_string())),
  }
}
