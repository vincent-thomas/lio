//! lio's low-level I/O API.
//!
//! This module contains all the syscall-oriented I/O operations provided by lio.
//! Each function maps directly to a system call and returns a [`Io`] handle
//! representing the in-flight operation.
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
//! Lio is compatible with any synchronisation method, through the [`Io`] type:
//! - Async/await through [`IntoFuture`] on [`Io`]
//!   (runtime-independent).
//! - [Callbacks](io::Io::when_done).
//! - Channels: [get receiver](io::Io::send) *or* [send with your own](io::Io::send_with).
//!
//! # Buffer Ownership
//!
//! Operations that use buffers ([`read`], [`write`](write()), [`recv`], [`send`]) take
//! ownership of the buffer and return it along with the result. This enables
//! zero-copy I/O while ensuring memory safety.
//!
//! # See Also
//!
//! - [`Io`] - Operation handle with multiple completion modes
//! - [`Resource`](crate::api::resource::Resource) - Reference-counted file descriptor wrapper
//! - [`crate::buf`] - Buffer types and pooling

pub mod io;
pub mod op;
pub mod op_contract;
pub mod ops;
pub mod pid;
pub mod resource;

pub use crate::backend::op::{
  DirEntryRef, DirEntryView, FileMode, FileStat, FileType, OpenFlags,
  ReadDirBuf, ReadDirResult, ReadFlags, RecvFlags, SendFlags, ShutdownHow,
  SockDomain, SockProto, SockType, UnlinkKind, WriteFlags,
};
pub use io::{Io, IoStream, Receiver, StreamReceiver};
pub use pid::Pid;

use crate::{IoBufMutVec, IoBufVec};
use resource::AsResource;
use std::net::SocketAddr;
use std::time::Duration;

doc_op! {
    short: "A no-op operation useful for driving and testing the runtime.",

    pub fn nop() -> Io<ops::Nop> {
        Io::from_op(ops::Nop)
    }
}

doc_op! {
    short: "Completes after the specified duration elapses.",

    pub fn sleep(duration: Duration) -> Io<ops::Sleep> {
        Io::from_op(ops::Sleep::new(duration))
    }
}

doc_op! {
    short: "Runs an interval of said duration.",
    pub fn interval(duration: Duration) -> IoStream<ops::Interval> {
      IoStream::from_op(ops::Interval::new(duration))
    }
}

doc_op! {
    short: "Creates a new socket resource.",

    pub fn socket(
        domain: SockDomain,
        ty: SockType,
        proto: SockProto,
    ) -> Io<ops::Socket> {
        Io::from_op(ops::Socket::new(domain, ty, proto))
    }
}

doc_op! {
    short: "Reads into one or more owned buffers.",

    pub fn read<B>(res: &impl AsResource, buf: B) -> Io<ops::Read<B>>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Read::new(res.as_resource().clone(), buf, -1))
    }
}

doc_op! {
    short: "Reads into one or more owned buffers from a fixed offset.",

    pub fn read_at<B>(res: &impl AsResource, buf: B, offset: u32) -> Io<ops::Read<B>>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Read::new(res.as_resource().clone(), buf, offset as i64))
    }
}

doc_op! {
    short: "Writes one or more owned buffers.",
    syscall: "write(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/write.2.html",

    pub fn write<B>(res: &impl AsResource, buf: B) -> Io<ops::Write<B>>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Write::new(res.as_resource().clone(), buf, -1))
    }
}

doc_op! {
    short: "Writes one or more owned buffers.",
    syscall: "write(2)",
    doc_link: "https://man7.org/linux/man-pages/man2/write.2.html",

    pub fn write_at<B>(res: &impl AsResource, buf: B, offset: u32) -> Io<ops::Write<B>>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Write::new(
            res.as_resource().clone(),
            buf,
            offset as i64,
        ))
    }
}

doc_op! {
    short: "Synchronizes a resource to stable storage.",
    syscall: "fsync(2)",

    pub fn fsync(res: &impl AsResource) -> Io<ops::Fsync> {
        Io::from_op(ops::Fsync::new(res.as_resource().clone()))
    }
}

doc_op! {
    short: "Receives into one or more owned buffers from a socket resource.",

    pub fn recv<B>(
        res: &impl AsResource,
        buf: B,
        flags: Option<RecvFlags>,
    ) -> Io<ops::Recv<B>>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Recv::new(res.as_resource().clone(), buf, flags))
    }
}

doc_op! {
    short: "Sends one or more owned buffers through a socket resource.",

    pub fn send<B>(
        res: &impl AsResource,
        buf: B,
        flags: Option<SendFlags>,
    ) -> Io<ops::Send<B>>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Send::new(res.as_resource().clone(), buf, None, flags))
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
    ) -> Io<ops::RecvFrom<B>>
    where
        B: IoBufMutVec + Send + Sync + 'static,
    {
        Io::from_op(ops::RecvFrom::new(res.as_resource().clone(), buf, flags))
    }
}

doc_op! {
    short: "Accepts a single incoming connection from a listening socket.",

    pub fn accept(
        res: &impl AsResource,
    ) -> Io<ops::Accept> {
        Io::from_op(ops::Accept::new(res.as_resource().clone()))
    }
}

doc_op! {
    short: "Connects a socket resource to a remote address.",

    pub fn connect(
        res: &impl AsResource,
        addr: SocketAddr,
    ) -> Io<ops::Connect> {
        Io::from_op(ops::Connect::new(res.as_resource().clone(), addr))
    }
}

doc_op! {
    short: "Binds a socket resource to a local address.",

    pub fn bind(
        res: &impl AsResource,
        addr: SocketAddr,
    ) -> Io<ops::Bind> {
        Io::from_op(ops::Bind::new(res.as_resource().clone(), addr))
    }
}

doc_op! {
    short: "Marks a socket resource as listening.",

    pub fn listen(
        res: &impl AsResource,
        backlog: i32,
    ) -> Io<ops::Listen> {
        Io::from_op(ops::Listen::new(res.as_resource().clone(), backlog))
    }
}

doc_op! {
    short: "Shuts down part or all of a socket connection.",

    pub fn shutdown(
        res: &impl AsResource,
        how: ShutdownHow,
    ) -> Io<ops::Shutdown> {
        Io::from_op(ops::Shutdown::new(res.as_resource().clone(), how))
    }
}

doc_op! {
    short: "Opens a file relative to a directory file descriptor.",

    pub fn openat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        flags: impl Into<OpenFlags>,
        mode: impl Into<FileMode>
    ) -> Io<ops::OpenAt> {
        Io::from_op(ops::OpenAt::new(
            dir_res.as_resource().clone(),
            path,
            flags.into(),
            mode.into()
        ))
    }
}

doc_op! {
    short: "Reads metadata for a path relative to a directory file descriptor.",

    pub fn statat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        follow_symlinks: bool,
    ) -> Io<ops::Stat> {
        Io::from_op(ops::Stat::new_at(
            dir_res.as_resource().clone(),
            path,
            follow_symlinks,
        ))
    }
}

doc_op! {
    short: "Reads metadata for an open file descriptor.",

    pub fn fstat(
        fd: &impl AsResource,
    ) -> Io<ops::Stat> {
        Io::from_op(ops::Stat::new_fd(fd.as_resource().clone()))
    }
}

doc_op! {
    short: "Reads one batch of directory entries into caller-managed buffers.",

    // Repeated calls on the same open directory continue from that directory
    // stream's native OS position until `eof` is reported.

    pub fn readdir(
        fd: &impl AsResource,
        buf: ReadDirBuf,
    ) -> Io<ops::ReadDir> {
        Io::from_op(ops::ReadDir::new(fd.as_resource().clone(), buf))
    }
}

doc_op! {
    short: "Removes a file or directory relative to a directory file descriptor.",

    pub fn unlinkat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        kind: impl Into<UnlinkKind>,
    ) -> Io<ops::UnlinkAt> {
        Io::from_op(ops::UnlinkAt::new(
            dir_res.as_resource().clone(),
            path,
            kind.into(),
        ))
    }
}

doc_op! {
    short: "Renames a file or directory relative to directory file descriptors.",

    pub fn renameat(
        old_dir_res: &impl AsResource,
        old_path: std::ffi::CString,
        new_dir_res: &impl AsResource,
        new_path: std::ffi::CString,
    ) -> Io<ops::RenameAt> {
        Io::from_op(ops::RenameAt::new(
            old_dir_res.as_resource().clone(),
            old_path,
            new_dir_res.as_resource().clone(),
            new_path,
        ))
    }
}

doc_op! {
    short: "Creates a directory relative to a directory file descriptor.",

    pub fn mkdirat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        mode: impl Into<FileMode>,
    ) -> Io<ops::MkdirAt> {
        Io::from_op(ops::MkdirAt::new(
            dir_res.as_resource().clone(),
            path,
            mode.into(),
        ))
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
    ) -> Io<ops::LinkAt> {
        Io::from_op(ops::LinkAt::new(
            source_dir_res.as_resource().clone(),
            source_path,
            new_dir_res.as_resource().clone(),
            new_path,
            kind,
        ))
    }
}

doc_op! {
    short: "Reads the target of a symbolic link relative to a directory file descriptor.",

    pub fn readlinkat(
        dir_res: &impl AsResource,
        path: std::ffi::CString,
        buf: Vec<u8>,
    ) -> Io<ops::ReadlinkAt<Vec<u8>>> {
        Io::from_op(ops::ReadlinkAt::new(
            dir_res.as_resource().clone(),
            path,
            buf,
        ))
    }
}

doc_op! {
    short: "Reads the current working directory into a caller-provided buffer.",

    pub fn getcwd(buf: Vec<u8>) -> Io<ops::GetCwd<Vec<u8>>> {
        Io::from_op(ops::GetCwd::new(buf))
    }
}

doc_op! {
    short: "Spawns a new process using posix_spawn().",

    #[cfg(unix)]
    pub fn spawn(
        path: std::ffi::CString,
        argv: Vec<std::ffi::CString>,
        envp: Option<Vec<std::ffi::CString>>,
    ) -> Io<ops::Spawn> {
        Io::from_op(ops::Spawn::new(path, argv, envp))
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
    pub fn sendto<B>(res: &impl AsResource, buf: B, addr: SocketAddr, flags: Option<SendFlags>) -> Io<ops::Send<B>>
    where
        B: IoBufVec + Send + Sync + 'static,
    {
        Io::from_op(ops::Send::new(
            res.as_resource().clone(),
            buf,
            Some(addr),
            flags,
        ))
    }
}
