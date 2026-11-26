//! lio's low-level I/O API.
//!
//! This module contains all the syscall-oriented I/O operations provided by lio.
//! Each function maps directly to a system call and returns the operation as
//! a value, one of the types in [`ops`]. Nothing happens until it is run, or
//! polled by a model it was put in.
//!
//! # Design Philosophy
//!
//! The API is designed to be:
//! - **Explicit**: Direct syscall mapping with minimal abstraction.
//! - **Zero-copy**: Buffers are moved into operations and returned on completions.
//! - **Flexible**: Multiple completion modes (async, blocking, channels, callbacks).
//! - **Type-safe**: Resources are reference-counted and automatically cleaned up.
//!
//! ## Synchronisation
//!
//! Every operation can be run in any of these ways:
//! - Async/await: `.await` it (runtime-independent).
//! - A callback: `when_done`.
//! - Channels: `send` for a receiver, *or* `send_with` for a sender of your
//!   own.
//!
//! They use the driver installed for the thread. `with_lio` names another,
//! and gives an [`Io`] with the same four ways.
//!
//! # Composing
//!
//! An operation is an [`OpModel`](crate::model::OpModel), so it goes
//! straight into a [`Join`](crate::model::Join),
//! [`model::spawn`](crate::model::spawn), or a model of your own. A model
//! of your own is run the same way: see [`ModelExt`](crate::model::ModelExt).
//!
//! # Buffer Ownership
//!
//! Operations that use buffers ([`read`], [`write`](write()), [`recv`], [`send`]) take
//! ownership of the buffer and return it along with the result. This enables
//! zero-copy I/O while ensuring memory safety.
//!
//! # See Also
//!
//! - [`Io`] - A model and the driver it runs on
//! - [`Resource`](crate::api::resource::Resource) - Reference-counted file descriptor wrapper
//! - [`crate::buf`] - Buffer types and pooling

pub mod io;
pub(crate) mod op;
pub mod ops;
pub mod pid;
pub mod resource;

pub use crate::backend::op::{
  DirEntryRef, DirEntryView, FileMode, FileStat, FileType, OpenFlags,
  ReadDirBuf, ReadDirResult, ReadFlags, RecvFlags, SendFlags, ShutdownHow,
  SockDomain, SockProto, SockType, UnlinkKind, WriteFlags,
};
pub use io::{Io, IoStream, Items, Receiver, StreamReceiver};
pub use ops::UnixAddr;
pub use pid::Pid;

use crate::backend::op::socket_addr_into_buf;
use crate::{IoBufMutVec, IoBufVec};
use resource::AsResource;
use std::net::SocketAddr;
use std::time::Duration;

/// Runs a model of your own, the way lio's operations are run.
///
/// Nothing happens until the returned [`Io`] is consumed: `.await` it, or
/// use [`when_done`](Io::when_done) or [`send`](Io::send).
/// [`ModelExt`](crate::model::ModelExt) has the same methods for every model:
/// this is for `.await` on the driver installed for the thread.
///
/// ```
/// use lio::api::{self, ops};
/// use lio::backend::testing::ScriptedBackend;
/// use lio::model::{Cx, OpModel, Step};
/// use lio::ready;
///
/// /// Two nops, one after the other.
/// struct TwoNops(u8, ops::Nop);
///
/// impl OpModel for TwoNops {
///   type Item = std::io::Result<()>;
///
///   fn poll(&mut self, cx: &mut Cx<'_>) -> Step<Self::Item> {
///     loop {
///       if let Err(err) = ready!(self.1.poll(cx)) {
///         return Step::Done(Err(err));
///       }
///       self.0 += 1;
///       if self.0 == 2 {
///         return Step::Done(Ok(()));
///       }
///       self.1 = api::nop();
///     }
///   }
/// }
///
/// let lio = lio::Lio::new_with_backend(ScriptedBackend::auto(0), 4).unwrap();
/// api::run(TwoNops(0, api::nop()))
///   .with_lio(&lio)
///   .when_done(|result| result.unwrap());
/// while lio.try_run().unwrap() > 0 {}
/// ```
pub fn run<M: crate::model::OpModel>(model: M) -> Io<M> {
  Io::from_op(model)
}

doc_op! {
    short: "A no-op operation useful for driving and testing the runtime.",

    pub fn nop() -> ops::Nop {
        ops::Nop::new()
    }
}

doc_op! {
    short: "Completes after the specified duration elapses.",

    pub fn sleep(duration: Duration) -> ops::Sleep {
        ops::Sleep::new(duration)
    }
}

doc_op! {
    short: "Runs an interval of said duration.",
    pub fn interval(duration: Duration) -> ops::Interval {
      ops::Interval::new(duration)
    }
}

doc_op! {
    short: "Creates a new socket resource.",

    pub fn socket(
        domain: SockDomain,
        ty: SockType,
        proto: SockProto,
    ) -> ops::Socket {
        ops::Socket::new(domain, ty, proto)
    }
}

doc_op! {
    short: "Reads into one or more owned buffers.",

    pub fn read<B>(res: &impl AsResource, buf: B) -> ops::Read<B>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        ops::Read::new(res.as_resource().clone(), buf, None)
    }
}

doc_op! {
    short: "Reads into one or more owned buffers from a fixed offset.",

    ///
    /// Operating systems address files with signed 64-bit offsets, so an
    /// `offset` above `i64::MAX` completes with
    /// [`InvalidInput`](std::io::ErrorKind::InvalidInput) without reading.
    pub fn read_at<B>(res: &impl AsResource, buf: B, offset: u64) -> ops::Read<B>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        ops::Read::new(res.as_resource().clone(), buf, Some(offset))
    }
}

doc_op! {
    short: "Writes one or more owned buffers.",
    syscall: "write(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/write.2.html",

    pub fn write<B>(res: &impl AsResource, buf: B) -> ops::Write<B>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        ops::Write::new(res.as_resource().clone(), buf, None)
    }
}

doc_op! {
    short: "Writes one or more owned buffers at a fixed offset.",
    syscall: "pwrite(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/pwrite.2.html",

    ///
    /// Operating systems address files with signed 64-bit offsets, so an
    /// `offset` above `i64::MAX` completes with
    /// [`InvalidInput`](std::io::ErrorKind::InvalidInput) without writing.
    pub fn write_at<B>(res: &impl AsResource, buf: B, offset: u64) -> ops::Write<B>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        ops::Write::new(
            res.as_resource().clone(),
            buf,
            Some(offset),
        )
    }
}

doc_op! {
    short: "Synchronizes a resource to stable storage.",
    syscall: "fsync(2)",

    pub fn fsync(res: &impl AsResource) -> ops::Fsync {
        ops::Fsync::new(res.as_resource().clone())
    }
}

doc_op! {
    short: "Receives into one or more owned buffers from a socket resource.",

    pub fn recv<B>(
        res: &impl AsResource,
        buf: B,
        flags: Option<RecvFlags>,
    ) -> ops::Recv<B>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        ops::Recv::new(
            res.as_resource().clone(),
            buf,
            flags.unwrap_or(RecvFlags::empty()),
        )
    }
}

doc_op! {
    short: "Sends one or more owned buffers through a socket resource.",

    pub fn send<B>(
        res: &impl AsResource,
        buf: B,
        flags: Option<SendFlags>,
    ) -> ops::Send<B>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        ops::Send::new(
            res.as_resource().clone(),
            buf,
            None,
            flags.unwrap_or(SendFlags::empty()),
        )
    }
}

doc_op! {
    short: "Receives data and the sender's address from a socket.",
    syscall: "recvfrom(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/recvfrom.2.html",

    pub fn recvfrom<B>(
        res: &impl AsResource,
        buf: B,
        flags: Option<RecvFlags>,
    ) -> ops::RecvFrom<B>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        ops::RecvFrom::new(
            res.as_resource().clone(),
            buf,
            flags.unwrap_or(RecvFlags::empty()),
        )
    }
}

doc_op! {
    short: "Accepts a single incoming connection from a listening socket.",

    pub fn accept(
        res: &impl AsResource,
    ) -> ops::Accept {
        ops::Accept::new(res.as_resource().clone())
    }
}

doc_op! {
    short: "Accepts a single incoming connection from a UNIX-domain listening socket, with the peer's address.",

    pub fn accept_unix(
        res: &impl AsResource,
    ) -> ops::AcceptUnix {
        ops::AcceptUnix::new(res.as_resource().clone())
    }
}

doc_op! {
    short: "Connects a socket resource to a remote address.",

    pub fn connect(
        res: &impl AsResource,
        addr: SocketAddr,
    ) -> ops::Connect {
        ops::Connect::new(
            res.as_resource().clone(),
            socket_addr_into_buf(addr),
        )
    }
}

doc_op! {
    short: "Binds a socket resource to a local address.",

    pub fn bind(
        res: &impl AsResource,
        addr: SocketAddr,
    ) -> ops::Bind {
        ops::Bind::new(
            res.as_resource().clone(),
            socket_addr_into_buf(addr),
        )
    }
}

doc_op! {
    short: "Marks a socket resource as listening.",

    pub fn listen(
        res: &impl AsResource,
        backlog: i32,
    ) -> ops::Listen {
        ops::Listen::new(res.as_resource().clone(), backlog)
    }
}

doc_op! {
    short: "Shuts down part or all of a socket connection.",

    pub fn shutdown(
        res: &impl AsResource,
        how: ShutdownHow,
    ) -> ops::Shutdown {
        ops::Shutdown::new(res.as_resource().clone(), how)
    }
}

doc_op! {
    short: "Opens a file relative to a directory file descriptor.",

    pub fn openat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        flags: OpenFlags,
        mode: FileMode,
    ) -> ops::OpenAt {
        ops::OpenAt::new(
            dir_res.as_resource().clone(),
            path,
            flags,
            mode,
        )
    }
}

doc_op! {
    short: "Reads metadata for a path relative to a directory file descriptor.",

    pub fn statat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        follow_symlinks: bool,
    ) -> ops::Stat {
        ops::Stat::new(ops::StatOf::Path {
            dir: dir_res.as_resource().clone(),
            path,
            follow_symlinks,
        })
    }
}

doc_op! {
    short: "Reads metadata for an open file descriptor.",

    pub fn fstat(
        fd: &impl AsResource,
    ) -> ops::Stat {
        ops::Stat::new(ops::StatOf::File(fd.as_resource().clone()))
    }
}

doc_op! {
    short: "Reads one batch of directory entries into caller-managed buffers.",

    // Repeated calls on the same open directory continue from that directory
    // stream's native OS position until `eof` is reported.

    pub fn readdir(
        fd: &impl AsResource,
        buf: ReadDirBuf,
    ) -> ops::ReadDir {
        ops::ReadDir::new(fd.as_resource().clone(), buf)
    }
}

doc_op! {
    short: "Removes a file or directory relative to a directory file descriptor.",

    pub fn unlinkat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        kind: UnlinkKind,
    ) -> ops::UnlinkAt {
        ops::UnlinkAt::new(
            dir_res.as_resource().clone(),
            path,
            kind,
        )
    }
}

doc_op! {
    short: "Renames a file or directory relative to directory file descriptors.",

    pub fn renameat(
        old_dir_res: &impl AsResource,
        old_path: std::ffi::CString,
        new_dir_res: &impl AsResource,
        new_path: std::ffi::CString,
    ) -> ops::RenameAt {
        ops::RenameAt::new(
            old_dir_res.as_resource().clone(),
            old_path,
            new_dir_res.as_resource().clone(),
            new_path,
        )
    }
}

doc_op! {
    short: "Creates a directory relative to a directory file descriptor.",

    pub fn mkdirat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        mode: FileMode,
    ) -> ops::MkdirAt {
        ops::MkdirAt::new(
            dir_res.as_resource().clone(),
            path,
            mode,
        )
    }
}

doc_op! {
    short: "Creates a hard or symbolic link relative to directory file descriptors.",

    pub fn linkat(
        source_dir_res: &impl AsResource,
        source_path: std::ffi::CString,
        new_dir_res: &impl AsResource,
        new_path: std::ffi::CString,
        kind: ops::LinkKind,
    ) -> ops::LinkAt {
        ops::LinkAt::new(
            source_dir_res.as_resource().clone(),
            source_path,
            new_dir_res.as_resource().clone(),
            new_path,
            kind,
        )
    }
}

doc_op! {
    short: "Reads the target of a symbolic link relative to a directory file descriptor.",

    pub fn readlinkat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        buf: Vec<u8>,
    ) -> ops::ReadlinkAt<Vec<u8>> {
        ops::ReadlinkAt::new(
            dir_res.as_resource().clone(),
            path,
            buf,
        )
    }
}

doc_op! {
    short: "Reads the current working directory into a caller-provided buffer.",

    pub fn getcwd(buf: Vec<u8>) -> ops::GetCwd<Vec<u8>> {
        ops::GetCwd::new(buf)
    }
}

doc_op! {
    short: "Spawns a new process; `argv[0]` is the conventional program name.",

    pub fn spawn(
        path: std::ffi::CString,
        argv: Vec<std::ffi::CString>,
        envp: Option<Vec<std::ffi::CString>>,
    ) -> ops::Spawn {
        ops::Spawn::new(path, argv, envp)
    }
}

doc_op! {
    short: "Sends data to a specific address over an unconnected socket.",
    syscall: "sendto(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/sendto.2.html",

    ///
    /// Unlike [`send`], this function allows sending data to a specific destination
    /// address without first connecting the socket. This is commonly used with UDP sockets.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::net::SocketAddr;
    ///
    /// async fn sendto_example() -> std::io::Result<()> {
    ///     # use lio::api::resource::Resource;
    ///     # let fd = Resource::stdin();
    ///     let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    ///     let data = b"Hello, server!".to_vec();
    ///     let (bytes_sent, _buf) = lio::api::sendto(&fd, data, addr, None).await;
    ///     println!("Sent {} bytes", bytes_sent?);
    ///     Ok(())
    /// }
    /// ```
    pub fn sendto<B>(res: &impl AsResource, buf: B, addr: SocketAddr, flags: Option<SendFlags>) -> ops::Send<B>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        ops::Send::new(
            res.as_resource().clone(),
            buf,
            Some(addr),
            flags.unwrap_or(SendFlags::empty()),
        )
    }
}
