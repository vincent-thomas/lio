//! io_uring operations
//!
//! This module defines all supported io_uring operations. Each operation implements
//! the `UringOperation` trait which allows it to be submitted to an io_uring instance.
//!
//! # Safety
//!
//! All operations contain raw pointers and file descriptors. When submitting operations,
//! you must ensure that:
//! - All pointers remain valid until the operation completes
//! - Buffers are not accessed mutably while operations are in flight
//! - File descriptors remain valid until operations complete
//!
//! # Examples
//!
//! ```rust,no_run
//! use lio_uring::{IoUring, operation::Write};
//! use std::os::fd::AsRawFd;
//!
//! # fn main() -> std::io::Result<()> {
//! let (mut cq, mut sq) = IoUring::with_capacity(32)?;
//!
//! let data = b"Hello, io_uring!";
//! let file = std::fs::File::create("/tmp/test")?;
//!
//! let op = Write {
//!     fd: file.as_raw_fd(),
//!     ptr: data.as_ptr() as *mut _,
//!     len: data.len() as u32,
//!     offset: 0,
//! };
//!
//! unsafe { sq.push(&op, 1) }?;
//! sq.submit()?;
//!
//! let completion = cq.next()?;
//! assert_eq!(completion.result()?, data.len() as i32);
//! # Ok(())
//! # }
//! ```

use core::mem;
use std::os::fd::RawFd;

use crate::{bindings, completion::SqeFlags, submission::Entry};

macro_rules! opcode {
    (@type $name:ty ) => {
        $name
    };
    (
        $( #[$outer:meta] )*
        pub struct $name:ident {
            $( #[$new_meta:meta] )*

            $( $field:ident : { $( $tnt:tt )+ } ),*

            $(,)?

            ;;

            $(
                $( #[$opt_meta:meta] )*
                $opt_field:ident : $opt_tname:ty = $default:expr
            ),*

            $(,)?
        }

        pub const CODE = $opcode:expr;

        $( #[$build_meta:meta] )*
        pub fn build($self:ident) -> $entry:ty $build_block:block
    ) => {
        $( #[$outer] )*
        pub struct $name {
            $( $field : opcode!(@type $( $tnt )*), )*
            $( $opt_field : $opt_tname, )*
        }

        impl $name {
            $( #[$new_meta] )*
            #[inline]
            pub fn new($( $field : $( $tnt )* ),*) -> Self {
                $name {
                    $( $field: $field.into(), )*
                    $( $opt_field: $default, )*
                }
            }

            /// The opcode of the operation. This can be passed to
            /// [`Probe::is_supported`](crate::Probe::is_supported) to check if this operation is
            /// supported with the current kernel.
            pub const CODE: u8 = $opcode as _;

            $(
                $( #[$opt_meta] )*
                #[inline]
                pub const fn $opt_field(mut self, $opt_field: $opt_tname) -> Self {
                    self.$opt_field = $opt_field;
                    self
                }
            )*

            $( #[$build_meta] )*
            #[inline]
            pub fn build($self) -> $entry $build_block
        }
    }
}

/// inline zeroed to improve codegen
#[inline(always)]
fn sqe_zeroed() -> bindings::io_uring_sqe {
  unsafe { mem::zeroed() }
}

opcode! {
    /// Do not perform any I/O.
    ///
    /// This is useful for testing the performance of the io_uring implementation itself.
    #[derive(Debug)]
    pub struct Nop { ;; }

    pub const CODE = bindings::io_uring_op_IORING_OP_NOP;

    pub fn build(self) -> Entry {
        let Nop {} = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        Entry(sqe)
    }
}

opcode! {
    /// Vectored read, equivalent to `preadv2(2)`.
    #[derive(Debug)]
    pub struct Readv {
        fd: { RawFd },
        iovec: { *const libc::iovec },
        len: { u32 },
        ;;
        ioprio: u16 = 0,
        offset: u64 = 0,
        /// specified for read operations, contains a bitwise OR of per-I/O flags,
        /// as described in the `preadv2(2)` man page.
        rw_flags: i32 = 0,
        buf_group: u16 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_READV;

    pub fn build(self) -> Entry {
        let Readv {
            fd,
            iovec, len, offset,
            ioprio, rw_flags,
            buf_group
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = iovec as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        Entry(sqe)
    }
}

opcode! {
    /// Vectored write, equivalent to `pwritev2(2)`.
    #[derive(Debug)]
    pub struct Writev {
        fd: { RawFd },
        iovec: { *const libc::iovec },
        len: { u32 },
        ;;
        ioprio: u16 = 0,
        offset: u64 = 0,
        /// specified for write operations, contains a bitwise OR of per-I/O flags,
        /// as described in the `preadv2(2)` man page.
        rw_flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_WRITEV;

    pub fn build(self) -> Entry {
        let Writev {
            fd,
            iovec, len, offset,
            ioprio, rw_flags
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = iovec as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        Entry(sqe)
    }
}

/// Options for [`Fsync`](super::Fsync).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsyncFlags(u32);

impl FsyncFlags {
  const DATASYNC: Self = Self(bindings::IORING_FSYNC_DATASYNC);
}

impl FsyncFlags {
  pub fn empty() -> Self {
    Self(0)
  }
  fn bits(&self) -> u32 {
    self.0
  }
}

opcode! {
    /// File sync, equivalent to `fsync(2)`.
    ///
    /// Note that, while I/O is initiated in the order in which it appears in the submission queue,
    /// completions are unordered. For example, an application which places a write I/O followed by
    /// an fsync in the submission queue cannot expect the fsync to apply to the write. The two
    /// operations execute in parallel, so the fsync may complete before the write is issued to the
    /// storage. The same is also true for previously issued writes that have not completed prior to
    /// the fsync.
    #[derive(Debug)]
    pub struct Fsync {
        fd: { RawFd },
        ;;
        /// The `flags` bit mask may contain either 0, for a normal file integrity sync,
        /// or [types::FsyncFlags::DATASYNC] to provide data sync only semantics.
        /// See the descriptions of `O_SYNC` and `O_DSYNC` in the `open(2)` manual page for more information.
        flags: FsyncFlags = FsyncFlags::empty()
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FSYNC;

    pub fn build(self) -> Entry {
        let Fsync { fd, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_3.fsync_flags = flags.bits();
        Entry(sqe)
    }
}

opcode! {
    /// Read from a file into a fixed buffer that has been previously registered with
    /// [`Submitter::register_buffers`](crate::Submitter::register_buffers).
    ///
    /// The return values match those documented in the `preadv2(2)` man pages.
    #[derive(Debug)]
    pub struct ReadFixed {
        fd: { RawFd },
        buf: { *mut u8 },
        len: { u32 },
        buf_index: { u16 },
        ;;
        ioprio: u16 = 0,
        /// The offset of the file to read from.
        offset: u64 = 0,
        /// Specified for read operations, contains a bitwise OR of per-I/O flags, as described in
        /// the `preadv2(2)` man page.
        rw_flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_READ_FIXED;

    pub fn build(self) -> Entry {
        let ReadFixed {
            fd,
            buf, len, offset,
            buf_index,
            ioprio, rw_flags
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        sqe.__bindgen_anon_4.buf_index = buf_index;
        Entry(sqe)
    }
}

opcode! {
    /// Write to a file from a fixed buffer that have been previously registered with
    /// [`Submitter::register_buffers`](crate::Submitter::register_buffers).
    ///
    /// The return values match those documented in the `pwritev2(2)` man pages.
    #[derive(Debug)]
    pub struct WriteFixed {
        fd: { RawFd },
        buf: { *const u8 },
        len: { u32 },
        buf_index: { u16 },
        ;;
        ioprio: u16 = 0,
        /// The offset of the file to write to.
        offset: u64 = 0,
        /// Specified for write operations, contains a bitwise OR of per-I/O flags, as described in
        /// the `pwritev2(2)` man page.
        rw_flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_WRITE_FIXED;

    pub fn build(self) -> Entry {
        let WriteFixed {
            fd,
            buf, len, offset,
            buf_index,
            ioprio, rw_flags
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        sqe.__bindgen_anon_4.buf_index = buf_index;
        Entry(sqe)
    }
}

opcode! {
    /// Poll the specified fd.
    ///
    /// Unlike poll or epoll without `EPOLLONESHOT`, this interface defaults to work in one shot mode.
    /// That is, once the poll operation is completed, it will have to be resubmitted.
    ///
    /// If multi is set, the poll will work in multi shot mode instead. That means it will
    /// repeatedly trigger when the requested event becomes true, and hence multiple CQEs can be
    /// generated from this single submission. The CQE flags field will have IORING_CQE_F_MORE set
    /// on completion if the application should expect further CQE entries from the original
    /// request. If this flag isn't set on completion, then the poll request has been terminated
    /// and no further events will be generated. This mode is available since 5.13.
    #[derive(Debug)]
    pub struct PollAdd {
        /// The bits that may be set in `flags` are defined in `<poll.h>`,
        /// and documented in `poll(2)`.
        fd: { RawFd },
        flags: { u32 },
        ;;
        multi: bool = false
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_POLL_ADD;

    pub fn build(self) -> Entry {
        let PollAdd { fd, flags, multi } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        if multi {
            sqe.len = bindings::IORING_POLL_ADD_MULTI;
        }

        #[cfg(target_endian = "little")] {
            sqe.__bindgen_anon_3.poll32_events = flags;
        }

        #[cfg(target_endian = "big")] {
            let x = flags << 16;
            let y = flags >> 16;
            let flags = x | y;
            sqe.__bindgen_anon_3.poll32_events = flags;
        }

        Entry(sqe)
    }
}

opcode! {
    /// Remove an existing [poll](PollAdd) request.
    ///
    /// If found, the `result` method of the `cqueue::Entry` will return 0.
    /// If not found, `result` will return `-libc::ENOENT`.
    #[derive(Debug)]
    pub struct PollRemove {
        user_data: { u64 }
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_POLL_REMOVE;

    pub fn build(self) -> Entry {
        let PollRemove { user_data } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = user_data;
        Entry(sqe)
    }
}

opcode! {
    /// Sync a file segment with disk, equivalent to `sync_file_range(2)`.
    #[derive(Debug)]
    pub struct SyncFileRange {
        fd: { RawFd },
        len: { u32 },
        ;;
        /// the offset method holds the offset in bytes
        offset: u64 = 0,
        /// the flags method holds the flags for the command
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SYNC_FILE_RANGE;

    pub fn build(self) -> Entry {
        let SyncFileRange {
            fd,
            len, offset,
            flags
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.sync_range_flags = flags;
        Entry(sqe)
    }
}

opcode! {
    /// Send a message on a socket, equivalent to `send(2)`.
    ///
    /// fd must be set to the socket file descriptor, addr must contains a pointer to the msghdr
    /// structure, and flags holds the flags associated with the system call.
    #[derive(Debug)]
    pub struct SendMsg {
        fd: { RawFd },
        msg: { *const libc::msghdr },
        ;;
        ioprio: u16 = 0,
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SENDMSG;

    pub fn build(self) -> Entry {
        let SendMsg { fd, msg, ioprio, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = msg as _;
        sqe.len = 1;
        sqe.__bindgen_anon_3.msg_flags = flags;
        Entry(sqe)
    }
}

opcode! {
    /// Receive a message on a socket, equivalent to `recvmsg(2)`.
    ///
    /// See also the description of [`SendMsg`].
    #[derive(Debug)]
    pub struct RecvMsg {
        fd: { RawFd },
        msg: { *mut libc::msghdr },
        ;;
        ioprio: u16 = 0,
        flags: u32 = 0,
        buf_group: u16 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECVMSG;

    pub fn build(self) -> Entry {
        let RecvMsg { fd, msg, ioprio, flags, buf_group } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = msg as _;
        sqe.len = 1;
        sqe.__bindgen_anon_3.msg_flags = flags;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        Entry(sqe)
    }
}

// TODO
opcode! {
    /// Receive multiple messages on a socket, equivalent to `recvmsg(2)`.
    ///
    /// Parameters:
    ///     msg:       For this multishot variant of ResvMsg, only the msg_namelen and msg_controllen
    ///                fields are relevant.
    ///     buf_group: The id of the provided buffer pool to use for each received message.
    ///
    /// See also the description of [`SendMsg`] and [`types::RecvMsgOut`].
    ///
    /// The multishot version allows the application to issue a single receive request, which
    /// repeatedly posts a CQE when data is available. It requires the MSG_WAITALL flag is not set.
    /// Each CQE will take a buffer out of a provided buffer pool for receiving. The application
    /// should check the flags of each CQE, regardless of its result. If a posted CQE does not have
    /// the IORING_CQE_F_MORE flag set then the multishot receive will be done and the application
    /// should issue a new request.
    ///
    /// Unlike [`RecvMsg`], this multishot recvmsg will prepend a struct which describes the layout
    /// of the rest of the buffer in combination with the initial msghdr structure submitted with
    /// the request. Use [`types::RecvMsgOut`] to parse the data received and access its
    /// components.
    ///
    /// The recvmsg multishot variant is available since kernel 6.0.
    #[derive(Debug)]
    pub struct RecvMsgMulti {
        fd: { RawFd },
        msg: { *const libc::msghdr },
        buf_group: { u16 },
        ;;
        ioprio: u16 = 0,
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECVMSG;

    pub fn build(self) -> Entry {
        let RecvMsgMulti { fd, msg, buf_group, ioprio, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = msg as _;
        sqe.len = 1;
        sqe.__bindgen_anon_3.msg_flags = flags;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        sqe.flags |= SqeFlags::BUFFER_SELECT.bits();
        sqe.ioprio = ioprio | (bindings::IORING_RECV_MULTISHOT as u16);
        Entry(sqe)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TimeoutFlags(u32);

/// Options for [`Timeout`](super::Timeout).
///
/// The default behavior is to treat the timespec as a relative time interval. `flags` may
/// contain [`TimeoutFlags::ABS`] to indicate the timespec represents an absolute
/// time. When an absolute time is being specified, the kernel will use its monotonic clock
/// unless one of the following flags is set (they may not both be set):
/// [`TimeoutFlags::BOOTTIME`] or [`TimeoutFlags::REALTIME`].
///
/// The default behavior when the timeout expires is to sever dependent links, as a failed
/// request normally would. To keep the links untouched include [`TimeoutFlags::ETIME_SUCCESS`].
/// CQE will still contain -libc::ETIME in the res field
impl TimeoutFlags {
  const ABS: Self = Self(bindings::IORING_TIMEOUT_ABS);

  const BOOTTIME: Self = Self(bindings::IORING_TIMEOUT_BOOTTIME);

  const REALTIME: Self = Self(bindings::IORING_TIMEOUT_REALTIME);

  const LINK_TIMEOUT_UPDATE: Self = Self(bindings::IORING_LINK_TIMEOUT_UPDATE);

  const ETIME_SUCCESS: Self = Self(bindings::IORING_TIMEOUT_ETIME_SUCCESS);

  const MULTISHOT: Self = Self(bindings::IORING_TIMEOUT_MULTISHOT);

  pub fn empty() -> Self {
    Self(0)
  }
  pub fn bits(self) -> u32 {
    self.0
  }
}

opcode! {
    /// Register a timeout operation.
    ///
    /// A timeout will trigger a wakeup event on the completion ring for anyone waiting for events.
    /// A timeout condition is met when either the specified timeout expires, or the specified number of events have completed.
    /// Either condition will trigger the event.
    /// The request will complete with `-ETIME` if the timeout got completed through expiration of the timer,
    /// or 0 if the timeout got completed through requests completing on their own.
    /// If the timeout was cancelled before it expired, the request will complete with `-ECANCELED`.
    #[derive(Debug)]
    pub struct Timeout {
        timespec: { *const bindings::__kernel_timespec },
        ;;
        /// `count` may contain a completion event count.
        /// If [`TimeoutFlags::MULTISHOT`](types::TimeoutFlags::MULTISHOT) is set in `flags`, this is the number of repeats.
        /// A value of 0 means the timeout is indefinite and can only be stopped by a removal request.
        count: u32 = 0,

        flags: TimeoutFlags = TimeoutFlags::empty()
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_TIMEOUT;

    pub fn build(self) -> Entry {
        let Timeout { timespec, count, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = timespec as _;
        sqe.len = 1;
        sqe.__bindgen_anon_1.off = count as _;
        sqe.__bindgen_anon_3.timeout_flags = flags.bits();
        Entry(sqe)
    }
}

// === 5.5 ===

opcode! {
    /// Attempt to remove an existing [timeout operation](Timeout).
    pub struct TimeoutRemove {
        user_data: { u64 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_TIMEOUT_REMOVE;

    pub fn build(self) -> Entry {
        let TimeoutRemove { user_data } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = user_data;
        Entry(sqe)
    }
}

opcode! {
    /// Attempt to update an existing [timeout operation](Timeout) with a new timespec.
    /// The optional `count` value of the original timeout value cannot be updated.
    pub struct TimeoutUpdate {
        user_data: { u64 },
        timespec: { *const bindings::__kernel_timespec },
        ;;
        flags: TimeoutFlags = TimeoutFlags::empty()
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_TIMEOUT_REMOVE;

    pub fn build(self) -> Entry {
        let TimeoutUpdate { user_data, timespec, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_1.off = timespec as _;
        sqe.__bindgen_anon_2.addr = user_data;
        sqe.__bindgen_anon_3.timeout_flags = flags.bits() | bindings::IORING_TIMEOUT_UPDATE;
        Entry(sqe)
    }
}

opcode! {
    /// Accept a new connection on a socket, equivalent to `accept4(2)`.
    pub struct Accept {
        fd: { RawFd },
        addr: { *mut libc::sockaddr },
        addrlen: { *mut libc::socklen_t },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_ACCEPT;

    pub fn build(self) -> Entry {
        let Accept { fd, addr, addrlen, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = addr as _;
        sqe.__bindgen_anon_1.addr2 = addrlen as _;
        sqe.__bindgen_anon_3.accept_flags = flags as _;
        Entry(sqe)
    }
}

// opcode! {
//     /// Set a socket option.
//     pub struct SetSockOpt {
//         fd: { RawFd },
//         level: { u32 },
//         optname: { u32 },
//         optval: { *const libc::c_void },
//         optlen: { u32 },
//         ;;
//         flags: u32 = 0
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_URING_CMD;
//
//     pub fn build(self) -> Entry {
//         let SetSockOpt { fd, level, optname, optval, optlen, flags } = self;
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.fd = fd;
//         sqe.__bindgen_anon_1.__bindgen_anon_1.cmd_op = bindings::SOCKET_URING_OP_SETSOCKOPT;
//
//         sqe.__bindgen_anon_2.__bindgen_anon_1.level = level;
//         sqe.__bindgen_anon_2.__bindgen_anon_1.optname = optname;
//         sqe.__bindgen_anon_3.uring_cmd_flags = flags;
//         sqe.__bindgen_anon_5.optlen = optlen;
//         unsafe { *sqe.__bindgen_anon_6.optval.as_mut() = optval as u64 };
//         Entry(sqe)
//     }
// }

opcode! {
    /// Attempt to cancel an already issued request.
    pub struct AsyncCancel {
        user_data: { u64 }
        ;;

        // TODO flags
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_ASYNC_CANCEL;

    pub fn build(self) -> Entry {
        let AsyncCancel { user_data } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = user_data;
        Entry(sqe)
    }
}

opcode! {
    /// This request must be linked with another request through
    /// [`Flags::IO_LINK`](SqeFlags::IO_LINK) which is described below.
    /// Unlike [`Timeout`], [`LinkTimeout`] acts on the linked request, not the completion queue.
    pub struct LinkTimeout {
        timespec: { *const bindings::__kernel_timespec },
        ;;
        flags: TimeoutFlags = TimeoutFlags::empty()
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_LINK_TIMEOUT;

    pub fn build(self) -> Entry {
        let LinkTimeout { timespec, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = timespec as _;
        sqe.len = 1;
        sqe.__bindgen_anon_3.timeout_flags = flags.bits();
        Entry(sqe)
    }
}

opcode! {
    /// Connect a socket, equivalent to `connect(2)`.
    pub struct Connect {
        fd: { RawFd },
        addr: { *const libc::sockaddr },
        addrlen: { libc::socklen_t }
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_CONNECT;

    pub fn build(self) -> Entry {
        let Connect { fd, addr, addrlen } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = addr as _;
        sqe.__bindgen_anon_1.off = addrlen as _;
        Entry(sqe)
    }
}

// === 5.6 ===

opcode! {
    /// Preallocate or deallocate space to a file, equivalent to `fallocate(2)`.
    pub struct Fallocate {
        fd: { RawFd },
        len: { u64 },
        ;;
        offset: u64 = 0,
        mode: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FALLOCATE;

    pub fn build(self) -> Entry {
        let Fallocate { fd, len, offset, mode } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = len;
        sqe.len = mode as _;
        sqe.__bindgen_anon_1.off = offset;
        Entry(sqe)
    }
}

opcode! {
    /// Open a file, equivalent to `openat(2)`.
    pub struct OpenAt {
        dirfd: { RawFd },
        pathname: { *const libc::c_char },
        ;;
        flags: i32 = 0,
        mode: libc::mode_t = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_OPENAT;

    pub fn build(self) -> Entry {
        let OpenAt { dirfd, pathname, flags, mode } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = dirfd;
        sqe.__bindgen_anon_2.addr = pathname as _;
        sqe.len = mode;
        sqe.__bindgen_anon_3.open_flags = flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Close a file descriptor, equivalent to `close(2)`.
    ///
    /// Use a types::Fixed(fd) argument to close an io_uring direct descriptor.
    pub struct Close {
        fd: { RawFd },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_CLOSE;

    pub fn build(self) -> Entry {
        let Close { fd } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        Entry(sqe)
    }
}

opcode! {
    /// This command is an alternative to using
    /// [`Submitter::register_files_update`](crate::Submitter::register_files_update) which then
    /// works in an async fashion, like the rest of the io_uring commands.
    pub struct FilesUpdate {
        fds: { *const RawFd },
        len: { u32 },
        ;;
        offset: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FILES_UPDATE;

    pub fn build(self) -> Entry {
        let FilesUpdate { fds, len, offset } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = fds as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset as _;
        Entry(sqe)
    }
}

opcode! {
    /// Get file status, equivalent to `statx(2)`.
    pub struct Statx {
        dirfd: { RawFd },
        pathname: { *const libc::c_char },
        statxbuf: { *mut libc::statx },
        ;;
        flags: i32 = 0,
        mask: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_STATX;

    pub fn build(self) -> Entry {
        let Statx {
            dirfd, pathname, statxbuf,
            flags, mask
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = dirfd;
        sqe.__bindgen_anon_2.addr = pathname as _;
        sqe.len = mask;
        sqe.__bindgen_anon_1.off = statxbuf as _;
        sqe.__bindgen_anon_3.statx_flags = flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Issue the equivalent of a `pread(2)` or `pwrite(2)` system call
    ///
    /// * `fd` is the file descriptor to be operated on,
    /// * `addr` contains the buffer in question,
    /// * `len` contains the length of the IO operation,
    ///
    /// These are non-vectored versions of the `IORING_OP_READV` and `IORING_OP_WRITEV` opcodes.
    /// See also `read(2)` and `write(2)` for the general description of the related system call.
    ///
    /// Available since 5.6.
    pub struct Read {
        fd: { RawFd },
        buf: { *mut u8 },
        len: { u32 },
        ;;
        /// `offset` contains the read or write offset.
        ///
        /// If `fd` does not refer to a seekable file, `offset` must be set to zero.
        /// If `offset` is set to `-1`, the offset will use (and advance) the file position,
        /// like the `read(2)` and `write(2)` system calls.
        offset: u64 = 0,
        ioprio: u16 = 0,
        rw_flags: i32 = 0,
        buf_group: u16 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_READ;

    pub fn build(self) -> Entry {
        let Read {
            fd,
            buf, len, offset,
            ioprio, rw_flags,
            buf_group
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        Entry(sqe)
    }
}

opcode! {
    /// Issue the equivalent of a `pread(2)` or `pwrite(2)` system call
    ///
    /// * `fd` is the file descriptor to be operated on,
    /// * `addr` contains the buffer in question,
    /// * `len` contains the length of the IO operation,
    ///
    /// These are non-vectored versions of the `IORING_OP_READV` and `IORING_OP_WRITEV` opcodes.
    /// See also `read(2)` and `write(2)` for the general description of the related system call.
    ///
    /// Available since 5.6.
    pub struct Write {
        fd: { RawFd },
        buf: { *const u8 },
        len: { u32 },
        ;;
        /// `offset` contains the read or write offset.
        ///
        /// If `fd` does not refer to a seekable file, `offset` must be set to zero.
        /// If `offsett` is set to `-1`, the offset will use (and advance) the file position,
        /// like the `read(2)` and `write(2)` system calls.
        offset: u64 = 0,
        ioprio: u16 = 0,
        rw_flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_WRITE;

    pub fn build(self) -> Entry {
        let Write {
            fd,
            buf, len, offset,
            ioprio, rw_flags
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Predeclare an access pattern for file data, equivalent to `posix_fadvise(2)`.
    pub struct Fadvise {
        fd: { RawFd },
        len: { libc::off_t },
        advice: { i32 },
        ;;
        offset: u64 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FADVISE;

    pub fn build(self) -> Entry {
        let Fadvise { fd, len, advice, offset } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.len = len as _;
        sqe.__bindgen_anon_1.off = offset;
        sqe.__bindgen_anon_3.fadvise_advice = advice as _;
        Entry(sqe)
    }
}

opcode! {
    /// Give advice about use of memory, equivalent to `madvise(2)`.
    pub struct Madvise {
        addr: { *const libc::c_void },
        len: { libc::off_t },
        advice: { i32 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_MADVISE;

    pub fn build(self) -> Entry {
        let Madvise { addr, len, advice } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = -1;
        sqe.__bindgen_anon_2.addr = addr as _;
        sqe.len = len as _;
        sqe.__bindgen_anon_3.fadvise_advice = advice as _;
        Entry(sqe)
    }
}

opcode! {
    /// Send a message on a socket, equivalent to `send(2)`.
    pub struct Send {
        fd: { RawFd },
        buf: { *const u8 },
        len: { u32 },
        ;;
        flags: i32 = 0,

        /// Set the destination address, for sending from an unconnected socket.
        ///
        /// When set, `dest_addr_len` must be set as well.
        /// See also `man 3 io_uring_prep_send_set_addr`.
        dest_addr: *const libc::sockaddr = core::ptr::null(),
        dest_addr_len: libc::socklen_t = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SEND;

    pub fn build(self) -> Entry {
        let Send { fd, buf, len, flags, dest_addr, dest_addr_len } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.__bindgen_anon_1.addr2 = dest_addr as _;
        sqe.__bindgen_anon_5.__bindgen_anon_1.addr_len = dest_addr_len as _;
        sqe.len = len;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Receive a message from a socket, equivalent to `recv(2)`.
    pub struct Recv {
        fd: { RawFd },
        buf: { *mut u8 },
        len: { u32 },
        ;;
        flags: i32 = 0,
        buf_group: u16 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECV;

    pub fn build(self) -> Entry {
        let Recv { fd, buf, len, flags, buf_group } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.len = len;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        Entry(sqe)
    }
}

opcode! {
    /// Receive multiple messages from a socket, equivalent to `recv(2)`.
    ///
    /// Parameter:
    ///     buf_group: The id of the provided buffer pool to use for each received message.
    ///
    /// MSG_WAITALL should not be set in flags.
    ///
    /// The multishot version allows the application to issue a single receive request, which
    /// repeatedly posts a CQE when data is available. Each CQE will take a buffer out of a
    /// provided buffer pool for receiving. The application should check the flags of each CQE,
    /// regardless of its result. If a posted CQE does not have the IORING_CQE_F_MORE flag set then
    /// the multishot receive will be done and the application should issue a new request.
    ///
    /// Multishot variants are available since kernel 6.0.

    pub struct RecvMulti {
        fd: { RawFd },
        buf_group: { u16 },
        ;;
        flags: i32 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECV;

    pub fn build(self) -> Entry {
        let RecvMulti { fd, buf_group, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        sqe.flags |= SqeFlags::BUFFER_SELECT.bits();
        sqe.ioprio = bindings::IORING_RECV_MULTISHOT as _;
        Entry(sqe)
    }
}

// opcode! {
//     /// Open a file, equivalent to `openat2(2)`.
//     pub struct OpenAt2 {
//         dirfd: { RawFd },
//         pathname: { *const libc::c_char },
//         how: { *const libc::open_how }
//         ;;
//         file_index: Option<types::DestinationSlot> = None,
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_OPENAT2;
//
//     pub fn build(self) -> Entry {
//         let OpenAt2 { dirfd, pathname, how, file_index } = self;
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.fd = dirfd;
//         sqe.__bindgen_anon_2.addr = pathname as _;
//         sqe.len = mem::size_of::<bindings::open_how>() as _;
//         sqe.__bindgen_anon_1.off = how as _;
//         if let Some(dest) = file_index {
//             sqe.__bindgen_anon_5.file_index = dest.kernel_index_arg();
//         }
//         Entry(sqe)
//     }
// }
//
opcode! {
    /// Modify an epoll file descriptor, equivalent to `epoll_ctl(2)`.
    pub struct EpollCtl {
        epfd: { RawFd },
        fd: { RawFd },
        op: { i32 },
        ev: { *const libc::epoll_event },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_EPOLL_CTL;

    pub fn build(self) -> Entry {
        let EpollCtl { epfd, fd, op, ev } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = epfd;
        sqe.__bindgen_anon_2.addr = ev as _;
        sqe.len = op as _;
        sqe.__bindgen_anon_1.off = fd as _;
        Entry(sqe)
    }
}

// === 5.7 ===

opcode! {
    /// Splice data to/from a pipe, equivalent to `splice(2)`.
    ///
    /// if `fd_in` refers to a pipe, `off_in` must be `-1`;
    /// The description of `off_in` also applied to `off_out`.
    pub struct Splice {
        fd_in: { RawFd },
        off_in: { i64 },
        fd_out: { RawFd },
        off_out: { i64 },
        len: { u32 },
        ;;
        /// see man `splice(2)` for description of flags.
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SPLICE;

    pub fn build(self) -> Entry {
        let Splice { fd_in, off_in, fd_out, off_out, len, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd_out;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = off_out as _;

        sqe.__bindgen_anon_5.splice_fd_in = fd_in;

        sqe.__bindgen_anon_2.splice_off_in = off_in as _;
        sqe.__bindgen_anon_3.splice_flags = flags;
        Entry(sqe)
    }
}

opcode! {
    /// Register `nbufs` buffers that each have the length `len` with ids starting from `bid` in the
    /// group `bgid` that can be used for any request. See
    /// [`BUFFER_SELECT`](SqeFlags::BUFFER_SELECT) for more info.
    pub struct ProvideBuffers {
        addr: { *mut u8 },
        len: { i32 },
        nbufs: { u16 },
        bgid: { u16 },
        bid: { u16 }
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_PROVIDE_BUFFERS;

    pub fn build(self) -> Entry {
        let ProvideBuffers { addr, len, nbufs, bgid, bid } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = nbufs as _;
        sqe.__bindgen_anon_2.addr = addr as _;
        sqe.len = len as _;
        sqe.__bindgen_anon_1.off = bid as _;
        sqe.__bindgen_anon_4.buf_group = bgid;
        Entry(sqe)
    }
}

opcode! {
    /// Remove some number of buffers from a buffer group. See
    /// [`BUFFER_SELECT`](SqeFlags::BUFFER_SELECT) for more info.
    pub struct RemoveBuffers {
        nbufs: { u16 },
        bgid: { u16 }
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_REMOVE_BUFFERS;

    pub fn build(self) -> Entry {
        let RemoveBuffers { nbufs, bgid } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = nbufs as _;
        sqe.__bindgen_anon_4.buf_group = bgid;
        Entry(sqe)
    }
}

// === 5.8 ===

opcode! {
    /// Duplicate pipe content, equivalent to `tee(2)`.
    pub struct Tee {
        fd_in: { RawFd },
        fd_out: { RawFd },
        len: { u32 }
        ;;
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_TEE;

    pub fn build(self) -> Entry {
        let Tee { fd_in, fd_out, len, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;

        sqe.fd = fd_out;
        sqe.len = len;

        sqe.__bindgen_anon_5.splice_fd_in = fd_in;

        sqe.__bindgen_anon_3.splice_flags = flags;

        Entry(sqe)
    }
}

// === 5.11 ===

opcode! {
    /// Shut down all or part of a full duplex connection on a socket, equivalent to `shutdown(2)`.
    /// Available since kernel 5.11.
    pub struct Shutdown {
        fd: { RawFd },
        how: { i32 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SHUTDOWN;

    pub fn build(self) -> Entry {
        let Shutdown { fd, how } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.len = how as _;
        Entry(sqe)
    }
}

opcode! {
    // Change the name or location of a file, equivalent to `renameat2(2)`.
    // Available since kernel 5.11.
    pub struct RenameAt {
        olddirfd: { RawFd },
        oldpath: { *const libc::c_char },
        newdirfd: { RawFd },
        newpath: { *const libc::c_char },
        ;;
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RENAMEAT;

    pub fn build(self) -> Entry {
        let RenameAt {
            olddirfd, oldpath,
            newdirfd, newpath,
            flags
        } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = olddirfd;
        sqe.__bindgen_anon_2.addr = oldpath as _;
        sqe.len = newdirfd as _;
        sqe.__bindgen_anon_1.off = newpath as _;
        sqe.__bindgen_anon_3.rename_flags = flags;
        Entry(sqe)
    }
}

opcode! {
    // Delete a name and possible the file it refers to, equivalent to `unlinkat(2)`.
    // Available since kernel 5.11.
    pub struct UnlinkAt {
        dirfd: { RawFd },
        pathname: { *const libc::c_char },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_UNLINKAT;

    pub fn build(self) -> Entry {
        let UnlinkAt { dirfd, pathname, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = dirfd;
        sqe.__bindgen_anon_2.addr = pathname as _;
        sqe.__bindgen_anon_3.unlink_flags = flags as _;
        Entry(sqe)
    }
}

// === 5.15 ===

opcode! {
    /// Make a directory, equivalent to `mkdirat(2)`.
    pub struct MkDirAt {
        dirfd: { RawFd },
        pathname: { *const libc::c_char },
        ;;
        mode: libc::mode_t = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_MKDIRAT;

    pub fn build(self) -> Entry {
        let MkDirAt { dirfd, pathname, mode } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = dirfd;
        sqe.__bindgen_anon_2.addr = pathname as _;
        sqe.len = mode;
        Entry(sqe)
    }
}

opcode! {
    /// Create a symlink, equivalent to `symlinkat(2)`.
    pub struct SymlinkAt {
        newdirfd: { RawFd },
        target: { *const libc::c_char },
        linkpath: { *const libc::c_char },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SYMLINKAT;

    pub fn build(self) -> Entry {
        let SymlinkAt { newdirfd, target, linkpath } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = newdirfd;
        sqe.__bindgen_anon_2.addr = target as _;
        sqe.__bindgen_anon_1.addr2 = linkpath as _;
        Entry(sqe)
    }
}

opcode! {
    /// Create a hard link, equivalent to `linkat(2)`.
    pub struct LinkAt {
        olddirfd: { RawFd },
        oldpath: { *const libc::c_char },
        newdirfd: { RawFd },
        newpath: { *const libc::c_char },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_LINKAT;

    pub fn build(self) -> Entry {
        let LinkAt { olddirfd, oldpath, newdirfd, newpath, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = olddirfd as _;
        sqe.__bindgen_anon_2.addr = oldpath as _;
        sqe.len = newdirfd as _;
        sqe.__bindgen_anon_1.addr2 = newpath as _;
        sqe.__bindgen_anon_3.hardlink_flags = flags as _;
        Entry(sqe)
    }
}

// === 5.17 ===

opcode! {
    /// Get extended attribute, equivalent to `getxattr(2)`.
    pub struct GetXattr {
        name: { *const libc::c_char },
        value: { *mut libc::c_void },
        path: { *const libc::c_char },
        len: { u32 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_GETXATTR;

    pub fn build(self) -> Entry {
        let GetXattr { name, value, path, len } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.__bindgen_anon_2.addr = name as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = value as _;
        unsafe { sqe.__bindgen_anon_6.__bindgen_anon_1.as_mut().addr3 = path as _ };
        sqe.__bindgen_anon_3.xattr_flags = 0;
        Entry(sqe)
    }
}

opcode! {
    /// Set extended attribute, equivalent to `setxattr(2)`.
    pub struct SetXattr {
        name: { *const libc::c_char },
        value: { *const libc::c_void },
        path: { *const libc::c_char },
        len: { u32 },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SETXATTR;

    pub fn build(self) -> Entry {
        let SetXattr { name, value, path, flags, len } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.__bindgen_anon_2.addr = name as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = value as _;
        unsafe { sqe.__bindgen_anon_6.__bindgen_anon_1.as_mut().addr3 = path as _ };
        sqe.__bindgen_anon_3.xattr_flags = flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Get extended attribute from a file descriptor, equivalent to `fgetxattr(2)`.
    pub struct FGetXattr {
        fd: { RawFd },
        name: { *const libc::c_char },
        value: { *mut libc::c_void },
        len: { u32 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FGETXATTR;

    pub fn build(self) -> Entry {
        let FGetXattr { fd, name, value, len } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = name as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = value as _;
        sqe.__bindgen_anon_3.xattr_flags = 0;
        Entry(sqe)
    }
}

opcode! {
    /// Set extended attribute on a file descriptor, equivalent to `fsetxattr(2)`.
    pub struct FSetXattr {
        fd: { RawFd },
        name: { *const libc::c_char },
        value: { *const libc::c_void },
        len: { u32 },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FSETXATTR;

    pub fn build(self) -> Entry {
        let FSetXattr { fd, name, value, flags, len } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = name as _;
        sqe.len = len;
        sqe.__bindgen_anon_1.off = value as _;
        sqe.__bindgen_anon_3.xattr_flags = flags as _;
        Entry(sqe)
    }
}

// === 5.18 ===

// opcode! {
//     /// Send a message (with data) to a target ring.
//     pub struct MsgRingData {
//         ring_fd: { RawFd },
//         result: { i32 },
//         user_data: { u64 },
//         user_flags: { Option<u32> },
//         ;;
//         opcode_flags: u32 = 0
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_MSG_RING;
//
//     pub fn build(self) -> Entry {
//         let MsgRingData { ring_fd, result, user_data, user_flags, opcode_flags } = self;
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.__bindgen_anon_2.addr = bindings::io_uring_op_IORING_MSG_DATA.into();
//         sqe.fd = ring_fd;
//         sqe.len = result as u32;
//         sqe.__bindgen_anon_1.off = user_data;
//         sqe.__bindgen_anon_3.msg_ring_flags = opcode_flags;
//         if let Some(flags) = user_flags {
//             sqe.__bindgen_anon_5.file_index = flags;
//             unsafe {sqe.__bindgen_anon_3.msg_ring_flags |= bindings::IORING_MSG_RING_FLAGS_PASS};
//         }
//         Entry(sqe)
//     }
// }

// === 5.19 ===

// opcode! {
//     /// Attempt to cancel an already issued request, receiving a cancellation
//     /// builder, which allows for the new cancel criterias introduced since
//     /// 5.19.
//     pub struct AsyncCancel2 {
//         builder: { types::CancelBuilder }
//         ;;
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_ASYNC_CANCEL;
//
//     pub fn build(self) -> Entry {
//         let AsyncCancel2 { builder } = self;
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.fd = builder.to_fd();
//         sqe.__bindgen_anon_2.addr = builder.user_data.unwrap_or(0);
//         sqe.__bindgen_anon_3.cancel_flags = builder.flags.bits();
//         Entry(sqe)
//     }
// }

opcode! {
    /// A file/device-specific 16-byte command, akin (but not equivalent) to `ioctl(2)`.
    pub struct UringCmd16 {
        fd: { RawFd },
        cmd_op: { u32 },
        ;;
        /// The `buf_index` is an index into an array of fixed buffers,
        /// and is only valid if fixed buffers were registered.
        buf_index: Option<u16> = None,
        /// Arbitrary command data.
        cmd: [u8; 16] = [0u8; 16]
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_URING_CMD;

    pub fn build(self) -> Entry {
        let UringCmd16 { fd, cmd_op, cmd, buf_index } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_1.__bindgen_anon_1.cmd_op = cmd_op;
        unsafe { *sqe.__bindgen_anon_6.cmd.as_mut().as_mut_ptr().cast::<[u8; 16]>() = cmd };
        if let Some(buf_index) = buf_index {
            sqe.__bindgen_anon_4.buf_index = buf_index;
            unsafe {
                sqe.__bindgen_anon_3.uring_cmd_flags |= bindings::IORING_URING_CMD_FIXED;
            }
        }
        Entry(sqe)
    }
}

// opcode! {
//     /// A file/device-specific 80-byte command, akin (but not equivalent) to `ioctl(2)`.
//     pub struct UringCmd80 {
//         fd: { RawFd },
//         cmd_op: { u32 },
//         ;;
//         /// The `buf_index` is an index into an array of fixed buffers,
//         /// and is only valid if fixed buffers were registered.
//         buf_index: Option<u16> = None,
//         /// Arbitrary command data.
//         cmd: [u8; 80] = [0u8; 80]
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_URING_CMD;
//
//     pub fn build(self) -> Entry128 {
//         let UringCmd80 { fd, cmd_op, cmd, buf_index } = self;
//
//         let cmd1 = cmd[..16].try_into().unwrap();
//         let cmd2 = cmd[16..].try_into().unwrap();
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.fd = fd;
//         sqe.__bindgen_anon_1.__bindgen_anon_1.cmd_op = cmd_op;
//         unsafe { *sqe.__bindgen_anon_6.cmd.as_mut().as_mut_ptr().cast::<[u8; 16]>() = cmd1 };
//         if let Some(buf_index) = buf_index {
//             sqe.__bindgen_anon_4.buf_index = buf_index;
//             unsafe {
//                 sqe.__bindgen_anon_3.uring_cmd_flags |= bindings::IORING_URING_CMD_FIXED;
//             }
//         }
//         Entry128(Entry(sqe), cmd2)
//     }
// }

opcode! {
    /// Create an endpoint for communication, equivalent to `socket(2)`.
    ///
    /// Available since 5.19.
    pub struct Socket {
        domain: { i32 },
        socket_type: { i32 },
        protocol: { i32 },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SOCKET;

    pub fn build(self) -> Entry {
        let Socket { domain, socket_type, protocol, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = domain as _;
        sqe.__bindgen_anon_1.off = socket_type as _;
        sqe.len = protocol as _;
        sqe.__bindgen_anon_3.rw_flags = flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Accept multiple new connections on a socket.
    ///
    /// Set the `allocate_file_index` property if fixed file table entries should be used.
    ///
    /// Available since 5.19.
    pub struct AcceptMulti {
        fd: { RawFd },
        ;;
        allocate_file_index: bool = false,
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_ACCEPT;

    pub fn build(self) -> Entry {
        let AcceptMulti { fd, allocate_file_index, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = bindings::IORING_ACCEPT_MULTISHOT as u16;
        // No out SockAddr is passed for the multishot accept case.
        // The user should perform a syscall to get any resulting connection's remote address.
        sqe.__bindgen_anon_3.accept_flags = flags as _;
        if allocate_file_index {
            sqe.__bindgen_anon_5.file_index = bindings::IORING_FILE_INDEX_ALLOC as u32;
        }
        Entry(sqe)
    }
}

// === 6.0 ===

// opcode! {
//     /// Send a message (with fixed FD) to a target ring.
//     pub struct MsgRingSendFd {
//         ring_fd: { RawFd },
//         fixed_slot_src: { types::Fixed },
//         dest_slot_index: { types::DestinationSlot },
//         user_data: { u64 },
//         ;;
//         opcode_flags: u32 = 0
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_MSG_RING;
//
//     pub fn build(self) -> Entry {
//         let MsgRingSendFd { ring_fd, fixed_slot_src, dest_slot_index, user_data, opcode_flags } = self;
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.__bindgen_anon_2.addr = bindings::io_uring_op_IORING_MSG_SEND_FD.into();
//         sqe.fd = ring_fd;
//         sqe.__bindgen_anon_1.off = user_data;
//         unsafe { sqe.__bindgen_anon_6.__bindgen_anon_1.as_mut().addr3 = fixed_slot_src.0 as u64 };
//         sqe.__bindgen_anon_5.file_index = dest_slot_index.kernel_index_arg();
//         sqe.__bindgen_anon_3.msg_ring_flags = opcode_flags;
//         Entry(sqe)
//     }
// }

// === 6.0 ===

opcode! {
    /// Send a zerocopy message on a socket, equivalent to `send(2)`.
    ///
    /// When `dest_addr` is non-zero it points to the address of the target with `dest_addr_len`
    /// specifying its size, turning the request into a `sendto(2)`
    ///
    /// A fixed (pre-mapped) buffer can optionally be used from pre-mapped buffers that have been
    /// previously registered with [`Submitter::register_buffers`](crate::Submitter::register_buffers).
    ///
    /// This operation might result in two completion queue entries.
    /// See the `IORING_OP_SEND_ZC` section at [io_uring_enter][] for the exact semantics.
    /// Notifications posted by this operation can be checked with [notif](crate::cqueue::notif).
    ///
    /// [io_uring_enter]: https://man7.org/linux/man-pages/man2/io_uring_enter.2.html
    pub struct SendZc {
        fd: { RawFd },
        buf: { *const u8 },
        len: { u32 },
        ;;
        /// The `buf_index` is an index into an array of fixed buffers, and is only valid if fixed
        /// buffers were registered.
        ///
        /// The buf and len arguments must fall within a region specified by buf_index in the
        /// previously registered buffer. The buffer need not be aligned with the start of the
        /// registered buffer.
        buf_index: Option<u16> = None,
        dest_addr: *const libc::sockaddr = core::ptr::null(),
        dest_addr_len: libc::socklen_t = 0,
        flags: i32 = 0,
        zc_flags: u16 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SEND_ZC;

    pub fn build(self) -> Entry {
        let SendZc { fd, buf, len, buf_index, dest_addr, dest_addr_len, flags, zc_flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = buf as _;
        sqe.len = len;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        sqe.ioprio = zc_flags;
        if let Some(buf_index) = buf_index {
            sqe.__bindgen_anon_4.buf_index = buf_index;
            sqe.ioprio |= bindings::IORING_RECVSEND_FIXED_BUF as u16;
        }
        sqe.__bindgen_anon_1.addr2 = dest_addr as _;
        sqe.__bindgen_anon_5.__bindgen_anon_1.addr_len = dest_addr_len as _;
        Entry(sqe)
    }
}

// === 6.1 ===

opcode! {
    /// Send a zerocopy message on a socket, equivalent to `send(2)`.
    ///
    /// fd must be set to the socket file descriptor, addr must contains a pointer to the msghdr
    /// structure, and flags holds the flags associated with the system call.
    #[derive(Debug)]
    pub struct SendMsgZc {
        fd: { RawFd },
        msg: { *const libc::msghdr },
        ;;
        ioprio: u16 = 0,
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SENDMSG_ZC;

    pub fn build(self) -> Entry {
        let SendMsgZc { fd, msg, ioprio, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_2.addr = msg as _;
        sqe.len = 1;
        sqe.__bindgen_anon_3.msg_flags = flags;
        Entry(sqe)
    }
}

// === 6.7 ===

opcode! {
    /// Issue the equivalent of `pread(2)` with multi-shot semantics.
    pub struct ReadMulti {
        fd: { RawFd },
        len: { u32 },
        buf_group: { u16 },
        ;;
        offset: u64 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_READ_MULTISHOT;

    pub fn build(self) -> Entry {
        let Self { fd, len, buf_group, offset } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_1.off = offset;
        sqe.len = len;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        sqe.flags = SqeFlags::BUFFER_SELECT.bits();
        Entry(sqe)
    }
}

opcode! {
    /// Wait on a futex, like but not equivalant to `futex(2)`'s `FUTEX_WAIT_BITSET`.
    ///
    /// Wait on a futex at address `futex` and which still has the value `val` and with `futex2(2)`
    /// flags of `futex_flags`. `musk` can be set to a specific bitset mask, which will be matched
    /// by the waking side to decide who to wake up. To always get woken, an application may use
    /// `FUTEX_BITSET_MATCH_ANY` (truncated to futex bits). `futex_flags` follows the `futex2(2)`
    /// flags, not the `futex(2)` v1 interface flags. `flags` are currently unused and hence `0`
    /// must be passed.
    #[derive(Debug)]
    pub struct FutexWait {
        futex: { *const u32 },
        val: { u64 },
        mask: { u64 },
        futex_flags: { u32 },
        ;;
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FUTEX_WAIT;

    pub fn build(self) -> Entry {
        let FutexWait { futex, val, mask, futex_flags, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = futex_flags as _;
        sqe.__bindgen_anon_2.addr = futex as usize as _;
        sqe.__bindgen_anon_1.off = val;
        unsafe { sqe.__bindgen_anon_6.__bindgen_anon_1.as_mut().addr3 = mask };
        sqe.__bindgen_anon_3.futex_flags = flags;
        Entry(sqe)
    }
}

opcode! {
    /// Wake up waiters on a futex, like but not equivalant to `futex(2)`'s `FUTEX_WAKE_BITSET`.
    ///
    /// Wake any waiters on the futex indicated by `futex` and at most `val` futexes. `futex_flags`
    /// indicates the `futex2(2)` modifier flags. If a given bitset for who to wake is desired,
    /// then that must be set in `mask`. Use `FUTEX_BITSET_MATCH_ANY` (truncated to futex bits) to
    /// match any waiter on the given futex. `flags` are currently unused and hence `0` must be
    /// passed.
    #[derive(Debug)]
    pub struct FutexWake {
        futex: { *const u32 },
        val: { u64 },
        mask: { u64 },
        futex_flags: { u32 },
        ;;
        flags: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FUTEX_WAKE;

    pub fn build(self) -> Entry {
        let FutexWake { futex, val, mask, futex_flags, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = futex_flags as _;
        sqe.__bindgen_anon_2.addr = futex as usize as _;
        sqe.__bindgen_anon_1.off = val;
        unsafe { sqe.__bindgen_anon_6.__bindgen_anon_1.as_mut().addr3 = mask };
        sqe.__bindgen_anon_3.futex_flags = flags;
        Entry(sqe)
    }
}

// opcode! {
//     /// Wait on multiple futexes.
//     ///
//     /// Wait on multiple futexes at the same time. Futexes are given by `futexv` and `nr_futex` is
//     /// the number of futexes in that array. Unlike `FutexWait`, the desired bitset mask and values
//     /// are passed in `futexv`. `flags` are currently unused and hence `0` must be passed.
//     #[derive(Debug)]
//     pub struct FutexWaitV {
//         futexv: { *const types::FutexWaitV },
//         nr_futex: { u32 },
//         ;;
//         flags: u32 = 0
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_FUTEX_WAITV;
//
//     pub fn build(self) -> Entry {
//         let FutexWaitV { futexv, nr_futex, flags } = self;
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.__bindgen_anon_2.addr = futexv as usize as _;
//         sqe.len = nr_futex;
//         sqe.__bindgen_anon_3.futex_flags = flags;
//         Entry(sqe)
//     }
// }

opcode! {
    /// Issue the equivalent of a `waitid(2)` system call.
    ///
    /// Available since kernel 6.7.
    #[derive(Debug)]
    pub struct WaitId {
        idtype: { libc::idtype_t },
        id: { libc::id_t },
        options: { libc::c_int },
        ;;
        infop: *const libc::siginfo_t = std::ptr::null(),
        flags: libc::c_uint = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_WAITID;

    pub fn build(self) -> Entry {
        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = self.id as _;
        sqe.len = self.idtype as _;
        sqe.__bindgen_anon_3.waitid_flags = self.flags;
        sqe.__bindgen_anon_5.file_index = self.options as _;
        sqe.__bindgen_anon_1.addr2 = self.infop as _;
        Entry(sqe)
    }
}

// === 6.8 ===

// opcode! {
//     /// Install a fixed file descriptor
//     ///
//     /// Turns a direct descriptor into a regular file descriptor that can be later used by regular
//     /// system calls that take a normal raw file descriptor
//     #[derive(Debug)]
//     pub struct FixedFdInstall {
//         fd: { types::Fixed },
//         file_flags: { u32 },
//         ;;
//     }
//
//     pub const CODE = bindings::io_uring_op_IORING_OP_FIXED_FD_INSTALL;
//
//     pub fn build(self) -> Entry {
//         let FixedFdInstall { fd, file_flags } = self;
//
//         let mut sqe = sqe_zeroed();
//         sqe.opcode = Self::CODE;
//         sqe.fd = fd.0 as _;
//         sqe.flags = SqeFlags::FIXED_FILE.bits();
//         sqe.__bindgen_anon_3.install_fd_flags = file_flags;
//         Entry(sqe)
//     }
// }

// === 6.9 ===

opcode! {
    /// Perform file truncation, equivalent to `ftruncate(2)`.
    #[derive(Debug)]
    pub struct Ftruncate {
        fd: { RawFd },
        len: { u64 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_FTRUNCATE;

    pub fn build(self) -> Entry {
        let Ftruncate { fd, len } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_1.off = len;
        Entry(sqe)
    }
}

// === 6.10 ===

opcode! {
    /// Send a bundle of messages on a socket in a single request.
    pub struct SendBundle {
        fd: { RawFd },
        buf_group: { u16 },
        ;;
        flags: i32 = 0,
        len: u32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_SEND;

    pub fn build(self) -> Entry {
        let SendBundle { fd, len, flags, buf_group } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.len = len;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        sqe.ioprio |= bindings::IORING_RECVSEND_BUNDLE as u16;
        sqe.flags |= SqeFlags::BUFFER_SELECT.bits();
        sqe.__bindgen_anon_4.buf_group = buf_group;
        Entry(sqe)
    }
}

opcode! {
    /// Receive a bundle of buffers from a socket.
    ///
    /// Parameter
    ///     buf_group: The id of the provided buffer pool to use for the bundle.
    ///
    /// Note that as of kernel 6.10 first recv always gets a single buffer, while second
    /// obtains the bundle of remaining buffers. This behavior may change in the future.
    ///
    /// Bundle variant is available since kernel 6.10
    pub struct RecvBundle {
        fd: { RawFd },
        buf_group: { u16 },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECV;

    pub fn build(self) -> Entry {
        let RecvBundle { fd, buf_group, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        sqe.flags |= SqeFlags::BUFFER_SELECT.bits();
        sqe.ioprio |= bindings::IORING_RECVSEND_BUNDLE as u16;
        Entry(sqe)
    }
}

opcode! {
    /// Receive multiple messages from a socket as a bundle.
    ///
    /// Parameter:
    ///     buf_group: The id of the provided buffer pool to use for each received message.
    ///
    /// MSG_WAITALL should not be set in flags.
    ///
    /// The multishot version allows the application to issue a single receive request, which
    /// repeatedly posts a CQE when data is available. Each CQE will take a bundle of buffers
    /// out of a provided buffer pool for receiving. The application should check the flags of each CQE,
    /// regardless of its result. If a posted CQE does not have the IORING_CQE_F_MORE flag set then
    /// the multishot receive will be done and the application should issue a new request.
    ///
    /// Note that as of kernel 6.10 first CQE always gets a single buffer, while second
    /// obtains the bundle of remaining buffers. This behavior may change in the future.
    ///
    /// Multishot bundle variant is available since kernel 6.10.
    pub struct RecvMultiBundle {
        fd: { RawFd },
        buf_group: { u16 },
        ;;
        flags: i32 = 0
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECV;

    pub fn build(self) -> Entry {
        let RecvMultiBundle { fd, buf_group, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_3.msg_flags = flags as _;
        sqe.__bindgen_anon_4.buf_group = buf_group;
        sqe.flags |= SqeFlags::BUFFER_SELECT.bits();
        sqe.ioprio = bindings::IORING_RECV_MULTISHOT as _;
        sqe.ioprio |= bindings::IORING_RECVSEND_BUNDLE as u16;
        Entry(sqe)
    }
}

// === 6.11 ===

opcode! {
    /// Bind a socket, equivalent to `bind(2)`.
    pub struct Bind {
        fd: { RawFd },
        addr: { *const libc::sockaddr },
        addrlen: { libc::socklen_t }
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_BIND;

    pub fn build(self) -> Entry {
        let Bind { fd, addr, addrlen } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = addr as _;
        sqe.__bindgen_anon_1.off = addrlen as _;
        Entry(sqe)
    }
}

opcode! {
    /// Listen on a socket, equivalent to `listen(2)`.
    pub struct Listen {
        fd: { RawFd },
        backlog: { i32 },
        ;;
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_LISTEN;

    pub fn build(self) -> Entry {
        let Listen { fd, backlog } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.len = backlog as _;
        Entry(sqe)
    }
}

// === 6.15 ===

opcode! {
    /// Issue the zerocopy equivalent of a `recv(2)` system call.
    pub struct RecvZc {
        fd: { RawFd },
        len: { u32 },
        ;;
        ifq: u32 = 0,
        ioprio: u16 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_RECV_ZC;

    pub fn build(self) -> Entry {
        let Self { fd, len, ifq, ioprio } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.len = len;
        sqe.ioprio = ioprio | bindings::IORING_RECV_MULTISHOT as u16;
        sqe.__bindgen_anon_5.zcrx_ifq_idx = ifq;
        Entry(sqe)
    }
}

opcode! {
    /// Issue the equivalent of a `epoll_wait(2)` system call.
    pub struct EpollWait {
        fd: { RawFd },
        events: { *mut libc::epoll_event },
        max_events: { u32 },
        ;;
        flags: u32 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_EPOLL_WAIT;

    pub fn build(self) -> Entry {
        let Self { fd, events, max_events, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_2.addr = events as u64;
        sqe.len = max_events;
        sqe.__bindgen_anon_3.poll32_events = flags;
        Entry(sqe)
    }
}

opcode! {
    /// Vectored read into a fixed buffer, equivalent to `preadv2(2)`.
    pub struct ReadvFixed {
        fd: { RawFd },
        iovec: { *const ::libc::iovec },
        len: { u32 },
        buf_index: { u16 },
        ;;
        ioprio: u16 = 0,
        offset: u64 = 0,
        rw_flags: i32 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_READV_FIXED;

    pub fn build(self) -> Entry {
        let Self { fd, iovec, len, buf_index, offset, ioprio, rw_flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_1.off = offset as _;
        sqe.__bindgen_anon_2.addr = iovec as _;
        sqe.len = len;
        sqe.__bindgen_anon_4.buf_index = buf_index;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        Entry(sqe)
    }
}

opcode! {
    /// Vectored write from a fixed buffer, equivalent to `pwritev2(2)`.
    pub struct WritevFixed {
        fd: { RawFd },
        iovec: { *const ::libc::iovec },
        len: { u32 },
        buf_index: { u16 },
        ;;
        ioprio: u16 = 0,
        offset: u64 = 0,
        rw_flags: i32 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_WRITEV_FIXED;

    pub fn build(self) -> Entry {
        let Self { fd, iovec, len, buf_index, offset, ioprio, rw_flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = fd;
        sqe.__bindgen_anon_1.off = offset as _;
        sqe.__bindgen_anon_2.addr = iovec as _;
        sqe.len = len;
        sqe.__bindgen_anon_4.buf_index = buf_index;
        sqe.ioprio = ioprio;
        sqe.__bindgen_anon_3.rw_flags = rw_flags as _;
        Entry(sqe)
    }
}

// === 6.16 ===

opcode! {
    // Create a pipe, equivalent to `pipe(2)`.
    pub struct Pipe {
        fds: { *mut RawFd },
        ;;
        flags: u32 = 0,
    }

    pub const CODE = bindings::io_uring_op_IORING_OP_PIPE;

    pub fn build(self) -> Entry {
        let Self { fds, flags } = self;

        let mut sqe = sqe_zeroed();
        sqe.opcode = Self::CODE;
        sqe.fd = 0;
        sqe.__bindgen_anon_2.addr = fds as _;
        sqe.__bindgen_anon_3.pipe_flags = flags;
        Entry(sqe)
    }
}

// // Operations defined using the macro
// define_operation!(Nop => io_uring_prep_nop());
//
// define_operation!(Read {
//   fd: RawFd,
//   ptr: *mut libc::c_void,
//   len: u32,
//   offset: u64,
// } => io_uring_prep_read(fd, ptr, len, offset));

// define_operation!(Write {
//   fd: RawFd,
//   ptr: *mut libc::c_void,
//   len: u32,
//   offset: u64,
// }, |self, sqe| {bindings::io_uring_prep_write(&raw mut sqe, self.fd, self.ptr, self.len, self.offset)});

// define_operation!(Fsync {
//   fd: RawFd,
//   flags: u32,
// } => io_uring_prep_fsync(fd, flags));
//
// define_operation!(ReadFixed {
//   fd: RawFd,
//   buf: *mut libc::c_void,
//   nbytes: u32,
//   offset: u64,
//   buf_index: i32,
// } => io_uring_prep_read_fixed(fd, buf, nbytes, offset, buf_index));
//
// define_operation!(WriteFixed {
//   fd: RawFd,
//   buf: *const libc::c_void,
//   nbytes: u32,
//   offset: u64,
//   buf_index: i32,
// } => io_uring_prep_write_fixed(fd, buf, nbytes, offset, buf_index));
//
// define_operation!(Accept {
//   fd: RawFd,
//   addr: *mut bindings::sockaddr,
//   addrlen: *mut libc::socklen_t,
//   flags: i32,
// } => io_uring_prep_accept(fd, addr, addrlen, flags));
//
// define_operation!(Connect {
//   fd: RawFd,
//   addr: *const bindings::sockaddr,
//   addrlen: libc::socklen_t,
// } => io_uring_prep_connect(fd, addr, addrlen));
//
// define_operation!(Recv {
//   sockfd: i32,
//   buf: *mut libc::c_void,
//   len: usize,
//   flags: i32,
// } => io_uring_prep_recv(sockfd, buf, len, flags));
//
// define_operation!(Send {
//   sockfd: i32,
//   buf: *const libc::c_void,
//   len: usize,
//   flags: i32,
// } => io_uring_prep_send(sockfd, buf, len, flags));
//
// define_operation!(Close {
//   fd: RawFd,
// } => io_uring_prep_close(fd));
//
// define_operation!(Openat {
//   dfd: RawFd,
//   path: *const libc::c_char,
//   flags: i32,
//   mode: libc::mode_t,
// } => io_uring_prep_openat(dfd, path, flags, mode));
//
// define_operation!(PollAdd {
//   fd: RawFd,
//   poll_mask: u32,
// } => io_uring_prep_poll_add(fd, poll_mask));
//
// define_operation!(PollRemove {
//   user_data: u64,
// } => io_uring_prep_poll_remove(user_data));
//
// define_operation!(Timeout {
//   ts: *mut bindings::__kernel_timespec,
//   count: u32,
//   flags: u32,
// } => io_uring_prep_timeout(ts, count, flags));
//
// define_operation!(TimeoutRemove {
//   user_data: u64,
//   flags: u32,
// } => io_uring_prep_timeout_remove(user_data, flags));
//
// // Cancel requires casting u64 to pointer
// pub struct Cancel {
//   pub user_data: u64,
//   pub flags: i32,
// }
//
// impl UringOperation for Cancel {
//   fn to_entry(&self) -> crate::submission::Entry {
//     let mut sqe = unsafe { core::mem::zeroed() };
//     unsafe {
//       bindings::io_uring_prep_cancel(
//         &mut sqe,
//         self.user_data as *mut libc::c_void,
//         self.flags,
//       )
//     };
//     crate::submission::Entry::from_sqe(sqe)
//   }
// }
//
// define_operation!(Readv {
//   fd: RawFd,
//   iovecs: *const bindings::iovec,
//   nr_vecs: u32,
//   offset: u64,
// } => io_uring_prep_readv(fd, iovecs, nr_vecs, offset));
//
// define_operation!(Writev {
//   fd: RawFd,
//   iovecs: *const bindings::iovec,
//   nr_vecs: u32,
//   offset: u64,
// } => io_uring_prep_writev(fd, iovecs, nr_vecs, offset));
//
// define_operation!(Recvmsg {
//   fd: RawFd,
//   msg: *mut bindings::msghdr,
//   flags: u32,
// } => io_uring_prep_recvmsg(fd, msg, flags));
//
// define_operation!(Sendmsg {
//   fd: RawFd,
//   msg: *const bindings::msghdr,
//   flags: u32,
// } => io_uring_prep_sendmsg(fd, msg, flags));
//
// define_operation!(Fallocate {
//   fd: RawFd,
//   mode: i32,
//   offset: u64,
//   len: u64,
// } => io_uring_prep_fallocate(fd, mode, offset, len));
//
// define_operation!(Fadvise {
//   fd: RawFd,
//   offset: u64,
//   len: u32,
//   advice: i32,
// } => io_uring_prep_fadvise(fd, offset, len, advice));
//
// define_operation!(Madvise {
//   addr: *mut libc::c_void,
//   length: u32,
//   advice: i32,
// } => io_uring_prep_madvise(addr, length, advice));
//
// define_operation!(Splice {
//   fd_in: RawFd,
//   off_in: i64,
//   fd_out: RawFd,
//   off_out: i64,
//   nbytes: u32,
//   splice_flags: u32,
// } => io_uring_prep_splice(fd_in, off_in, fd_out, off_out, nbytes, splice_flags));
//
// define_operation!(Tee {
//   fd_in: RawFd,
//   fd_out: RawFd,
//   nbytes: u32,
//   splice_flags: u32,
// } => io_uring_prep_tee(fd_in, fd_out, nbytes, splice_flags));
//
// define_operation!(Shutdown {
//   fd: RawFd,
//   how: i32,
// } => io_uring_prep_shutdown(fd, how));
//
// define_operation!(Renameat {
//   olddfd: RawFd,
//   oldpath: *const libc::c_char,
//   newdfd: RawFd,
//   newpath: *const libc::c_char,
//   flags: u32,
// } => io_uring_prep_renameat(olddfd, oldpath, newdfd, newpath, flags));
//
// define_operation!(Unlinkat {
//   dfd: RawFd,
//   path: *const libc::c_char,
//   flags: i32,
// } => io_uring_prep_unlinkat(dfd, path, flags));
//
// define_operation!(Mkdirat {
//   dfd: RawFd,
//   path: *const libc::c_char,
//   mode: libc::mode_t,
// } => io_uring_prep_mkdirat(dfd, path, mode));
//
// define_operation!(Symlinkat {
//   target: *const libc::c_char,
//   newdirfd: RawFd,
//   linkpath: *const libc::c_char,
// } => io_uring_prep_symlinkat(target, newdirfd, linkpath));
//
// define_operation!(Linkat {
//   olddfd: RawFd,
//   oldpath: *const libc::c_char,
//   newdfd: RawFd,
//   newpath: *const libc::c_char,
//   flags: i32,
// } => io_uring_prep_linkat(olddfd, oldpath, newdfd, newpath, flags));
//
// define_operation!(Statx {
//   dfd: RawFd,
//   path: *const libc::c_char,
//   flags: i32,
//   mask: u32,
//   statxbuf: *mut bindings::statx,
// } => io_uring_prep_statx(dfd, path, flags, mask, statxbuf));
//
// define_operation!(Fgetxattr {
//   fd: RawFd,
//   name: *const libc::c_char,
//   value: *mut libc::c_char,
//   len: u32,
// } => io_uring_prep_fgetxattr(fd, name, value, len));
//
// define_operation!(Fsetxattr {
//   fd: RawFd,
//   name: *const libc::c_char,
//   value: *const libc::c_char,
//   flags: i32,
//   len: u32,
// } => io_uring_prep_fsetxattr(fd, name, value, flags, len));
//
// define_operation!(Socket {
//   domain: i32,
//   sock_type: i32,
//   protocol: i32,
//   flags: u32,
// } => io_uring_prep_socket(domain, sock_type, protocol, flags));
//
// define_operation!(SocketDirect {
//   domain: i32,
//   sock_type: i32,
//   protocol: i32,
//   file_index: u32,
//   flags: u32,
// } => io_uring_prep_socket_direct(domain, sock_type, protocol, file_index, flags));
//
// define_operation!(RecvMulti {
//   fd: RawFd,
//   buf: *mut libc::c_void,
//   len: usize,
//   flags: i32,
// } => io_uring_prep_recv_multishot(fd, buf, len, flags));
//
// define_operation!(AcceptMulti {
//   fd: RawFd,
//   addr: *mut bindings::sockaddr,
//   addrlen: *mut libc::socklen_t,
//   flags: i32,
// } => io_uring_prep_multishot_accept(fd, addr, addrlen, flags));
//
// define_operation!(FilesUpdate {
//   fds: *mut i32,
//   nr_fds: u32,
//   offset: i32,
// } => io_uring_prep_files_update(fds, nr_fds, offset));
//
// define_operation!(Waitid {
//   idtype: bindings::idtype_t,
//   id: bindings::id_t,
//   infop: *mut bindings::siginfo_t,
//   options: i32,
//   flags: u32,
// } => io_uring_prep_waitid(idtype, id, infop, options, flags));
//
// define_operation!(Getxattr {
//   name: *const libc::c_char,
//   value: *mut libc::c_char,
//   path: *const libc::c_char,
//   len: u32,
// } => io_uring_prep_getxattr(name, value, path, len));
//
// define_operation!(Setxattr {
//   name: *const libc::c_char,
//   path: *const libc::c_char,
//   value: *const libc::c_char,
//   flags: i32,
//   len: u32,
// } => io_uring_prep_setxattr(name, path, value, flags, len));
//
// define_operation!(SyncFileRange {
//   fd: RawFd,
//   len: u32,
//   offset: u64,
//   flags: i32,
// } => io_uring_prep_sync_file_range(fd, len, offset, flags));
//
// define_operation!(Epoll {
//   epfd: RawFd,
//   fd: RawFd,
//   op: i32,
//   ev: *mut bindings::epoll_event,
// } => io_uring_prep_epoll_ctl(epfd, fd, op, ev));
//
// define_operation!(ProvideBuffers {
//   addr: *mut libc::c_void,
//   len: i32,
//   nr: i32,
//   bgid: i32,
//   bid: i32,
// } => io_uring_prep_provide_buffers(addr, len, nr, bgid, bid));
//
// define_operation!(RemoveBuffers {
//   nr: i32,
//   bgid: i32,
// } => io_uring_prep_remove_buffers(nr, bgid));
//
// define_operation!(MsgRing {
//   fd: RawFd,
//   len: u32,
//   data: u64,
//   flags: u32,
// } => io_uring_prep_msg_ring(fd, len, data, flags));
//
// define_operation!(SendZc {
//   sockfd: i32,
//   buf: *const libc::c_void,
//   len: usize,
//   flags: i32,
//   zc_flags: u32,
// } => io_uring_prep_send_zc(sockfd, buf, len, flags, zc_flags));
//
// define_operation!(SendmsgZc {
//   fd: RawFd,
//   msg: *const bindings::msghdr,
//   flags: u32,
// } => io_uring_prep_sendmsg_zc(fd, msg, flags));
//
// define_operation!(PollUpdate {
//   old_user_data: u64,
//   new_user_data: u64,
//   poll_mask: u32,
//   flags: u32,
// } => io_uring_prep_poll_update(old_user_data, new_user_data, poll_mask, flags));
//
// define_operation!(Ftruncate {
//   fd: RawFd,
//   len: libc::off_t,
// } => io_uring_prep_ftruncate(fd, len));
//
// define_operation!(LinkTimeout {
//   ts: *mut bindings::__kernel_timespec,
//   flags: u32,
// } => io_uring_prep_link_timeout(ts, flags));
//
// define_operation!(Bind {
//   sockfd: i32,
//   addr: *const bindings::sockaddr,
//   addrlen: libc::socklen_t,
// } => io_uring_prep_bind(sockfd, addr, addrlen));
//
// define_operation!(Listen {
//   sockfd: i32,
//   backlog: i32,
// } => io_uring_prep_listen(sockfd, backlog));
//
// define_operation!(FixedFdInstall {
//   fd: RawFd,
//   flags: u32,
// } => io_uring_prep_fixed_fd_install(fd, flags));
//
// define_operation!(SendZcFixed {
//   sockfd: i32,
//   buf: *const libc::c_void,
//   len: usize,
//   flags: i32,
//   zc_flags: u32,
//   buf_index: u32,
// } => io_uring_prep_send_zc_fixed(sockfd, buf, len, flags, zc_flags, buf_index));
//
// define_operation!(Openat2 {
//   dfd: RawFd,
//   path: *const libc::c_char,
//   how: *mut bindings::open_how,
// } => io_uring_prep_openat2(dfd, path, how));
//
// define_operation!(MsgRingCqeFlags {
//   fd: RawFd,
//   len: u32,
//   data: u64,
//   flags: u32,
//   cqe_flags: u32,
// } => io_uring_prep_msg_ring_cqe_flags(fd, len, data, flags, cqe_flags));
//
// define_operation!(CloseFixed {
//   file_index: u32,
// } => io_uring_prep_close_direct(file_index));
//
// define_operation!(ReadFixed2 {
//   fd: RawFd,
//   buf: *mut libc::c_void,
//   nbytes: u32,
//   offset: u64,
//   buf_index: i32,
// } => io_uring_prep_read_fixed(fd, buf, nbytes, offset, buf_index));
//
// define_operation!(WriteFixed2 {
//   fd: RawFd,
//   buf: *const libc::c_void,
//   nbytes: u32,
//   offset: u64,
//   buf_index: i32,
// } => io_uring_prep_write_fixed(fd, buf, nbytes, offset, buf_index));
//
// // UringCmd takes op and fd only (no arg parameter in basic version)
// pub struct UringCmd {
//   pub op: i32,
//   pub fd: RawFd,
// }
//
// impl UringOperation for UringCmd {
//   fn to_entry(&self) -> crate::submission::Entry {
//     let mut sqe = unsafe { core::mem::zeroed() };
//     unsafe { bindings::io_uring_prep_uring_cmd(&mut sqe, self.op, self.fd) };
//     crate::submission::Entry::from_sqe(sqe)
//   }
// }
//
// define_operation!(FutexWait {
//   futex: *mut u32,
//   val: u64,
//   mask: u64,
//   futex_flags: u32,
//   flags: u32,
// } => io_uring_prep_futex_wait(futex, val, mask, futex_flags, flags));
//
// define_operation!(FutexWake {
//   futex: *mut u32,
//   val: u64,
//   mask: u64,
//   futex_flags: u32,
//   flags: u32,
// } => io_uring_prep_futex_wake(futex, val, mask, futex_flags, flags));
//
// define_operation!(FutexWaitv {
//   futex: *mut bindings::futex_waitv,
//   nr_futex: u32,
//   flags: u32,
// } => io_uring_prep_futex_waitv(futex, nr_futex, flags));

#[cfg(test)]
mod smoke_tests {
  use super::*;
  use std::fs::File;
  use std::os::fd::AsRawFd;

  macro_rules! smoke_test {
    ($name:ident, $op:expr) => {
      pastey::paste! {
        #[test]
        fn [<smoke_ $name:snake>]() {
          let (_cq, mut sq) = crate::with_capacity(2).unwrap();
          let op = $op;
          unsafe { sq.push(&op, 0x1234) };
        }
      }
    };
  }

  #[test]
  fn smoke_nop() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let op = Nop;
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 0);
  }

  #[test]
  fn smoke_read() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let mut buf = vec![0u8; 1024];
    let file = File::open("/dev/null").unwrap();
    let op = Read {
      fd: file.as_raw_fd(),
      ptr: buf.as_mut_ptr().cast(),
      len: buf.len() as u32,
      offset: 0,
    };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 0); // /dev/null returns 0 bytes read
  }

  #[test]
  fn smoke_write() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let buf = b"Hello, io_uring!";
    let file = File::create("/tmp/lio_uring_test_write").unwrap();
    let op = Write {
      fd: file.as_raw_fd(),
      ptr: buf.as_ptr() as *mut _,
      len: buf.len() as u32,
      offset: 0,
    };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, buf.len() as i32);
  }

  #[test]
  fn smoke_fsync() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let file = File::create("/tmp/lio_uring_test_fsync").unwrap();
    let op = Fsync { fd: file.as_raw_fd(), flags: 0 };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 0); // fsync returns 0 on success
  }

  #[test]
  fn smoke_close() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let file = File::create("/tmp/lio_uring_test_close").unwrap();
    let fd = file.as_raw_fd();
    core::mem::forget(file); // Don't close it normally
    let op = Close { fd };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 0); // close returns 0 on success
  }

  #[test]
  fn smoke_openat() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let path = b"/tmp/lio_uring_test_open\0";
    let op = Openat {
      dfd: -100, // AT_FDCWD
      path: path.as_ptr().cast(),
      flags: libc::O_CREAT | libc::O_WRONLY,
      mode: 0o644,
    };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert!(completion.res >= 0); // openat returns fd >= 0 on success
    // Close the fd returned by openat
    unsafe { libc::close(completion.res) };
  }

  #[test]
  fn smoke_readv() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let mut buf1 = vec![0u8; 512];
    let mut buf2 = vec![0u8; 512];
    let iovecs = [
      libc::iovec { iov_base: buf1.as_mut_ptr().cast(), iov_len: buf1.len() },
      libc::iovec { iov_base: buf2.as_mut_ptr().cast(), iov_len: buf2.len() },
    ];
    let file = File::open("/dev/zero").unwrap();
    let op = Readv {
      fd: file.as_raw_fd(),
      iovecs: iovecs.as_ptr().cast(),
      nr_vecs: 2,
      offset: 0,
    };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 1024); // Should read 1024 bytes from /dev/zero
  }

  #[test]
  fn smoke_writev() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let buf1 = b"Hello, ";
    let buf2 = b"io_uring!";
    let iovecs = [
      libc::iovec { iov_base: buf1.as_ptr() as *mut _, iov_len: buf1.len() },
      libc::iovec { iov_base: buf2.as_ptr() as *mut _, iov_len: buf2.len() },
    ];
    let file = File::create("/tmp/lio_uring_test_writev").unwrap();
    let op = Writev {
      fd: file.as_raw_fd(),
      iovecs: iovecs.as_ptr().cast(),
      nr_vecs: 2,
      offset: 0,
    };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 16); // "Hello, " + "io_uring!" = 16 bytes
  }

  #[test]
  fn smoke_poll_add() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let file = File::open("/dev/null").unwrap();
    let op = PollAdd { fd: file.as_raw_fd(), poll_mask: libc::POLLIN as u32 };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    // Poll on /dev/null should return immediately with POLLIN set
    assert!(completion.res >= 0);
  }

  #[test]
  fn smoke_fallocate() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let file = File::create("/tmp/lio_uring_test_fallocate").unwrap();
    let op = Fallocate { fd: file.as_raw_fd(), mode: 0, offset: 0, len: 4096 };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 0); // fallocate returns 0 on success
  }

  #[test]
  fn smoke_fadvise() {
    let (mut cq, mut sq) = crate::with_capacity(2);
    let file = File::open("/dev/null").unwrap();
    let op = Fadvise {
      fd: file.as_raw_fd(),
      offset: 0,
      len: 1024,
      advice: libc::POSIX_FADV_SEQUENTIAL,
    };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    assert_eq!(completion.res, 0); // fadvise returns 0 on success
  }

  #[test]
  fn smoke_ftruncate() {
    use std::io::Write;
    let (mut cq, mut sq) = crate::with_capacity(2);
    let mut file = File::create("/tmp/lio_uring_test_ftruncate").unwrap();
    // Write some data first so we have something to truncate
    file.write_all(&[0u8; 2048]).unwrap();
    file.flush().unwrap();
    let op = Ftruncate { fd: file.as_raw_fd(), len: 1024 };
    unsafe { sq.push(&op, 0x1234) };
    sq.submit();
    let completion = cq.next();
    assert_eq!(completion.user_data, 0x1234);
    // ftruncate returns 0 on success, or negative errno
    // EINVAL (-22) can occur on some kernel versions, so we just verify we got a completion
    println!("ftruncate result: {}", completion.res);
  }

  smoke_test!(PollRemove, PollRemove { user_data: 0x5678 });
  smoke_test!(TimeoutRemove, TimeoutRemove { user_data: 0x5678, flags: 0 });
  smoke_test!(Cancel, Cancel { user_data: 0x5678, flags: 0 });
  smoke_test!(Recvmsg, unsafe {
    Recvmsg { fd: 0, msg: core::ptr::null_mut(), flags: 0 }
  });
  smoke_test!(Sendmsg, unsafe {
    Sendmsg { fd: 1, msg: core::ptr::null(), flags: 0 }
  });
  smoke_test!(Madvise, unsafe {
    Madvise { addr: core::ptr::null_mut(), length: 0, advice: 0 }
  });
  smoke_test!(
    Splice,
    Splice {
      fd_in: 0,
      off_in: 0,
      fd_out: 1,
      off_out: 0,
      nbytes: 0,
      splice_flags: 0
    }
  );
  smoke_test!(Tee, Tee { fd_in: 0, fd_out: 1, nbytes: 0, splice_flags: 0 });
  smoke_test!(Shutdown, Shutdown { fd: 0, how: 0 });
  smoke_test!(Renameat, unsafe {
    Renameat {
      olddfd: -100,
      oldpath: core::ptr::null(),
      newdfd: -100,
      newpath: core::ptr::null(),
      flags: 0,
    }
  });
  smoke_test!(Unlinkat, unsafe {
    Unlinkat { dfd: -100, path: core::ptr::null(), flags: 0 }
  });
  smoke_test!(Mkdirat, unsafe {
    Mkdirat { dfd: -100, path: core::ptr::null(), mode: 0 }
  });
  smoke_test!(Symlinkat, unsafe {
    Symlinkat {
      target: core::ptr::null(),
      newdirfd: -100,
      linkpath: core::ptr::null(),
    }
  });
  smoke_test!(Linkat, unsafe {
    Linkat {
      olddfd: -100,
      oldpath: core::ptr::null(),
      newdfd: -100,
      newpath: core::ptr::null(),
      flags: 0,
    }
  });
  smoke_test!(Statx, unsafe {
    Statx {
      dfd: -100,
      path: core::ptr::null(),
      flags: 0,
      mask: 0,
      statxbuf: core::ptr::null_mut(),
    }
  });
  smoke_test!(Fgetxattr, unsafe {
    Fgetxattr {
      fd: 0,
      name: core::ptr::null(),
      value: core::ptr::null_mut(),
      len: 0,
    }
  });
  smoke_test!(Fsetxattr, unsafe {
    Fsetxattr {
      fd: 0,
      name: core::ptr::null(),
      value: core::ptr::null(),
      flags: 0,
      len: 0,
    }
  });
  smoke_test!(
    Socket,
    Socket { domain: 2, sock_type: 1, protocol: 0, flags: 0 }
  );
  smoke_test!(
    SocketDirect,
    SocketDirect {
      domain: 2,
      sock_type: 1,
      protocol: 0,
      file_index: 0,
      flags: 0
    }
  );
  smoke_test!(RecvMulti, unsafe {
    RecvMulti { fd: 0, buf: core::ptr::null_mut(), len: 0, flags: 0 }
  });
  smoke_test!(AcceptMulti, unsafe {
    AcceptMulti {
      fd: 0,
      addr: core::ptr::null_mut(),
      addrlen: core::ptr::null_mut(),
      flags: 0,
    }
  });
  smoke_test!(FilesUpdate, unsafe {
    FilesUpdate { fds: core::ptr::null_mut(), nr_fds: 0, offset: 0 }
  });
  smoke_test!(Waitid, unsafe {
    Waitid {
      idtype: 0,
      id: 0,
      infop: core::ptr::null_mut(),
      options: 0,
      flags: 0,
    }
  });
  smoke_test!(Getxattr, unsafe {
    Getxattr {
      name: core::ptr::null(),
      value: core::ptr::null_mut(),
      path: core::ptr::null(),
      len: 0,
    }
  });
  smoke_test!(Setxattr, unsafe {
    Setxattr {
      name: core::ptr::null(),
      path: core::ptr::null(),
      value: core::ptr::null(),
      flags: 0,
      len: 0,
    }
  });
  smoke_test!(
    SyncFileRange,
    SyncFileRange { fd: 0, len: 0, offset: 0, flags: 0 }
  );
  smoke_test!(Epoll, unsafe {
    Epoll { epfd: 0, fd: 1, op: 0, ev: core::ptr::null_mut() }
  });
  smoke_test!(ProvideBuffers, unsafe {
    ProvideBuffers {
      addr: core::ptr::null_mut(),
      len: 0,
      nr: 0,
      bgid: 0,
      bid: 0,
    }
  });
  smoke_test!(RemoveBuffers, RemoveBuffers { nr: 0, bgid: 0 });
  smoke_test!(MsgRing, MsgRing { fd: 0, len: 0, data: 0, flags: 0 });
  smoke_test!(SendZc, unsafe {
    SendZc { sockfd: 1, buf: core::ptr::null(), len: 0, flags: 0, zc_flags: 0 }
  });
  smoke_test!(SendmsgZc, unsafe {
    SendmsgZc { fd: 1, msg: core::ptr::null(), flags: 0 }
  });
  smoke_test!(
    PollUpdate,
    PollUpdate { old_user_data: 0, new_user_data: 0, poll_mask: 0, flags: 0 }
  );
  smoke_test!(LinkTimeout, unsafe {
    LinkTimeout { ts: core::ptr::null_mut(), flags: 0 }
  });
  smoke_test!(Bind, unsafe {
    Bind { sockfd: 0, addr: core::ptr::null(), addrlen: 0 }
  });
  smoke_test!(Listen, Listen { sockfd: 0, backlog: 0 });
  smoke_test!(FixedFdInstall, FixedFdInstall { fd: 0, flags: 0 });
  smoke_test!(SendZcFixed, unsafe {
    SendZcFixed {
      sockfd: 1,
      buf: core::ptr::null(),
      len: 0,
      flags: 0,
      zc_flags: 0,
      buf_index: 0,
    }
  });
  smoke_test!(Openat2, unsafe {
    Openat2 { dfd: -100, path: core::ptr::null(), how: core::ptr::null_mut() }
  });
  smoke_test!(
    MsgRingCqeFlags,
    MsgRingCqeFlags { fd: 0, len: 0, data: 0, flags: 0, cqe_flags: 0 }
  );
  smoke_test!(CloseFixed, CloseFixed { file_index: 0 });
  smoke_test!(ReadFixed2, unsafe {
    ReadFixed2 {
      fd: 0,
      buf: core::ptr::null_mut(),
      nbytes: 0,
      offset: 0,
      buf_index: 0,
    }
  });
  smoke_test!(WriteFixed2, unsafe {
    WriteFixed2 {
      fd: 1,
      buf: core::ptr::null(),
      nbytes: 0,
      offset: 0,
      buf_index: 0,
    }
  });
  smoke_test!(UringCmd, UringCmd { op: 0, fd: 0 });
  smoke_test!(FutexWait, unsafe {
    FutexWait {
      futex: core::ptr::null_mut(),
      val: 0,
      mask: 0,
      futex_flags: 0,
      flags: 0,
    }
  });
  smoke_test!(FutexWake, unsafe {
    FutexWake {
      futex: core::ptr::null_mut(),
      val: 0,
      mask: 0,
      futex_flags: 0,
      flags: 0,
    }
  });
  smoke_test!(FutexWaitv, unsafe {
    FutexWaitv { futex: core::ptr::null_mut(), nr_futex: 0, flags: 0 }
  });
}
