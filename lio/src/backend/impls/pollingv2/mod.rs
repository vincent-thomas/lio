#![allow(clippy::undocumented_unsafe_blocks)]

//! `lio`-provided [`IoBackend`] impl for `epoll`/`kqueue` (platform-specific).

mod os;

#[cfg(target_os = "linux")]
use os::epoll as sys;

#[cfg(any(
  target_os = "macos",
  target_os = "ios",
  target_os = "tvos",
  target_os = "watchos",
  target_os = "freebsd",
  target_os = "dragonfly",
  target_os = "openbsd",
  target_os = "netbsd"
))]
use os::kqueue as sys;

#[cfg(test)]
pub(crate) mod tests;

use std::{
  collections::{HashMap, VecDeque},
  io,
  os::fd::{AsRawFd, RawFd},
  time::Duration,
};

use bumpalo::Bump;

use crate::backend::{
  IoBackend, OpCompleted,
  impls::unix_stat::{file_stat_from_raw, file_type_from_dirent_dtype},
  op::{
    DirEntryRef, MsgRecv, MsgSend, Op, ReadDirResult, WriteFlags,
    is_valid_file_offset,
  },
};
use crate::slab::{Slab, SlabKey};
mod interest;
pub use interest::Interest;

/// What a [`Poller`] needs from the operating system: watch descriptors,
/// and say which became ready.
///
/// lio implements it for `epoll` and `kqueue`, and for `poll(2)` in its
/// tests. A [`Poller`] over an implementation is an [`IoBackend`]: the
/// poller supplies the rest, performing the operations, queueing several on
/// one descriptor, cancelling, and keeping to its capacity.
///
/// The trait cannot be named outside this crate. A backend from elsewhere
/// implements [`IoBackend`].
///
/// # What the poller relies on
///
/// - A descriptor has at most one registration. [`add`](Self::add) for one
///   that is registered fails, with `EEXIST`, and leaves the registration
///   as it was.
/// - A registration is one-shot: once [`wait`](Self::wait) has reported an
///   event for it, it reports nothing more until it is deleted and added
///   again.
/// - An event carries the key its registration was added with.
///
/// ## Thread Safety
///
/// Implementations are used from a single thread, and need not be `Send`.
pub trait ReadinessPoll {
  /// The event record [`wait`](Self::wait) fills in.
  type NativeEvent: Copy;

  /// Opens a poller. A [`Poller`] made with [`Poller::new`] calls this when
  /// the driver starts it.
  fn open() -> io::Result<Self>
  where
    Self: Sized;

  /// An event record that stands for nothing, to fill the buffer
  /// [`wait`](Self::wait) writes into.
  fn blank_event() -> Self::NativeEvent;

  /// Add interest for a file descriptor (one-shot mode).
  /// This is not idempotent.
  fn add(&self, fd: RawFd, key: u64, interest: Interest) -> io::Result<()>;

  /// Remove all interest for a file descriptor
  /// This fails if 'fd' hasn't previously been added.
  fn delete(&self, fd: RawFd) -> io::Result<()>;

  /// Waits for events and writes them to the front of `events`.
  ///
  /// Returns how many were written. A count past `events.len()` is read as
  /// `events.len()`.
  fn wait(
    &self,
    events: &mut [Self::NativeEvent],
    timeout: Option<Duration>,
  ) -> io::Result<usize>;

  /// Extract the key from a native event
  fn event_key(event: &Self::NativeEvent) -> u64;

  /// Interest the kernel reported for this event: which of a descriptor's
  /// waiting operations to try.
  fn event_interest(event: &Self::NativeEvent) -> Interest;
}

type LoweredMsg = (
  [libc::iovec; crate::buf::MAX_IOV_COUNT],
  libc::msghdr,
  Option<(libc::sockaddr_storage, libc::socklen_t)>,
);

/// The buffer a poller's `wait` writes events into, and the events of the
/// last wait that are still to be handled.
struct Events<P: ReadinessPoll> {
  /// Every entry holds an event record: blank, or one a wait wrote.
  events: Vec<P::NativeEvent>,
  /// How many at the front are still to be handled.
  ready: usize,
}

impl<P: ReadinessPoll> Events<P> {
  /// Room for `capacity` events per wait.
  fn with_capacity(capacity: usize) -> Self {
    Self { events: vec![P::blank_event(); capacity], ready: 0 }
  }

  /// The whole buffer, for a wait to write into.
  fn as_buf(&mut self) -> &mut [P::NativeEvent] {
    assert!(
      self.ready == 0,
      "lio logic error: Left over items during Events::as_buf call."
    );
    &mut self.events
  }

  /// A wait wrote `count` events to the front of the buffer.
  fn set_ready(&mut self, count: usize) {
    self.ready = count.min(self.events.len());
  }

  /// The next event: its key, and what the kernel says is ready.
  fn pop_event(&mut self) -> Option<(u64, Interest)> {
    self.ready = self.ready.checked_sub(1)?;
    let native_event = self.events.get(self.ready)?;
    Some((P::event_key(native_event), P::event_interest(native_event)))
  }
}

/// The operations waiting for one descriptor to become ready, oldest first.
///
/// The OS poller holds one registration per descriptor, however many
/// operations wait on it. Its key is the descriptor itself.
#[derive(Default)]
struct Waiters {
  readers: VecDeque<SlabKey>,
  writers: VecDeque<SlabKey>,
  /// Whether the OS poller holds a registration for the descriptor.
  registered: bool,
}

impl Waiters {
  /// What somebody is waiting for.
  fn interest(&self) -> Interest {
    let mut interest = Interest::NONE;
    if !self.readers.is_empty() {
      interest |= Interest::READ;
    }
    if !self.writers.is_empty() {
      interest |= Interest::WRITE;
    }
    interest
  }

  /// The operations waiting for what `interest` names.
  fn queue(&mut self, interest: Interest) -> &mut VecDeque<SlabKey> {
    if interest.is_writable() { &mut self.writers } else { &mut self.readers }
  }
}

/// Polling-based I/O backend: performs each operation itself once the
/// operating system says its descriptor is ready.
///
/// [`Poller::new`] uses epoll (Linux) or kqueue (BSD/macOS).
pub struct Poller<P: ReadinessPoll = sys::OsPoller> {
  sys: Option<P>,
  events: Events<P>,
  capacity: usize,
  backlog: Vec<PendingOp>,
  /// Operations that are waiting for their descriptor to become ready.
  pending: Slab<PendingOp>,
  /// Which of `pending` wait on which descriptor.
  waiters: HashMap<RawFd, Waiters>,
  /// Immediate completions produced during `flush()` and surfaced on the next `wait()`.
  queued_completed: Vec<OpCompleted>,
  /// Reusable buffer for completed operations
  completed: Vec<OpCompleted>,
}

impl Default for Poller {
  fn default() -> Self {
    Self::new()
  }
}

impl Poller {
  /// A poller on epoll (Linux) or kqueue (BSD/macOS), opened when the
  /// driver starts it.
  pub fn new() -> Self {
    Self::unstarted(None)
  }
}

impl<P: ReadinessPoll> Poller<P> {
  /// A poller over `poller`.
  #[cfg(test)]
  pub(crate) fn with_poller(poller: P) -> Self {
    Self::unstarted(Some(poller))
  }

  fn unstarted(sys: Option<P>) -> Self {
    Self {
      sys,
      events: Events::with_capacity(0),
      capacity: 0,
      backlog: Vec::new(),
      pending: Slab::new(0),
      waiters: HashMap::new(),
      queued_completed: Vec::new(),
      completed: Vec::new(),
    }
  }
}

/// One operation the poller holds, queued or waiting for its descriptor.
///
/// The poller performs the I/O itself, in [`Poller::exec_op`]: no address
/// from `op` is ever left with the kernel or another thread between calls.
/// That is what lets every method here give the ticket up.
pub struct PendingOp {
  op: crate::backend::op::Op,
  ticket: crate::backend::Ticket,
}

impl PendingOp {
  fn id(&self) -> u64 {
    self.ticket.id()
  }

  /// Reports the operation. `result` is what [`Poller::exec_op`] returned
  /// for it, or a negative code for an operation that was not performed.
  fn report(self, result: isize) -> OpCompleted {
    // SAFETY: nothing but this thread ever uses the operation's addresses,
    // and it is done with them. The result is the syscall's own, or an
    // error, which claims nothing about the operation's memory.
    unsafe { self.ticket.complete(result) }
  }

  /// Reports the operation as stopped before it was performed.
  fn stopped(self) -> OpCompleted {
    // SAFETY: as in `report`.
    unsafe { self.ticket.cancelled(-(libc::ECANCELED as isize)) }
  }

  /// Gives the operation up unreported.
  fn release(self) {
    // SAFETY: as in `report`.
    unsafe { self.ticket.release() }
  }
}

// EAGAIN and EWOULDBLOCK are the same on Linux, but may differ on other systems
const EAGAIN_NEG: isize = -(libc::EAGAIN as isize);
const EWOULDBLOCK_NEG: isize = -(libc::EWOULDBLOCK as isize);
/// An error from the OS poller, as an operation's raw result.
fn raw_error(err: &io::Error) -> isize {
  err.raw_os_error().map_or(-(libc::EIO as isize), |code| -(code as isize))
}

impl<P: ReadinessPoll> Poller<P> {
  /// Registers `fd` for what its waiters wait for, in place of what was
  /// registered. With no waiter left, the descriptor is forgotten.
  ///
  /// A registration is used up by the event it reports, and names every
  /// direction at once, so this runs after an event and whenever a
  /// direction gains its first waiter or loses its last.
  fn rearm(
    sys: &P,
    waiters: &mut HashMap<RawFd, Waiters>,
    fd: RawFd,
  ) -> io::Result<()> {
    let Some(waiting) = waiters.get_mut(&fd) else {
      return Ok(());
    };
    if std::mem::take(&mut waiting.registered) {
      let _ = sys.delete(fd);
    }
    let interest = waiting.interest();
    if interest.is_none() {
      waiters.remove(&fd);
      return Ok(());
    }
    sys.add(fd, fd as u64, interest)?;
    waiting.registered = true;
    Ok(())
  }

  /// Completes every operation waiting on `fd` with `result`: the
  /// descriptor can no longer be watched.
  fn fail_waiters(
    pending: &mut Slab<PendingOp>,
    waiters: &mut HashMap<RawFd, Waiters>,
    fd: RawFd,
    result: isize,
    completed: &mut Vec<OpCompleted>,
  ) {
    let Some(waiting) = waiters.remove(&fd) else {
      return;
    };
    for key in waiting.readers.into_iter().chain(waiting.writers) {
      if let Some(entry) = pending.remove_value(key) {
        completed.push(entry.report(result));
      }
    }
  }

  /// Runs the operations at the front of `queue` until one would have to
  /// wait. The rest stay queued behind it, in order.
  fn serve(
    queue: &mut VecDeque<SlabKey>,
    pending: &mut Slab<PendingOp>,
    completed: &mut Vec<OpCompleted>,
  ) {
    while let Some(&key) = queue.front() {
      let Some(entry) = pending.get_mut(key) else {
        queue.pop_front();
        continue;
      };
      let result = Self::exec_op(&entry.op);
      if result == EAGAIN_NEG || result == EWOULDBLOCK_NEG {
        return;
      }
      queue.pop_front();
      if let Some(entry) = pending.remove_value(key) {
        completed.push(entry.report(result));
      }
    }
  }

  /// Queues `entry` behind the operations already waiting on its
  /// descriptor, and has the OS poller watch for it.
  fn park(&mut self, entry: PendingOp) {
    let (fd, interest) = self.extract_fd_interest(&entry.op);
    let Some((key, _)) = self.pending.insert_get_mut(entry) else {
      unreachable!("poller: no room to park an operation `push` accepted");
    };
    let queue = self.waiters.entry(fd).or_default().queue(interest);
    let already_watched = !queue.is_empty();
    queue.push_back(key);
    if already_watched {
      return;
    }

    let Some(sys) = self.sys.as_ref() else {
      panic!("Poller not initialized: the driver calls init first");
    };
    let Err(err) = Self::rearm(sys, &mut self.waiters, fd) else {
      return;
    };
    // The OS will not watch this descriptor for that. Such a descriptor
    // never makes an operation wait, so the operation runs now. Registering
    // is the operation's own business: its failure is the operation's
    // result, not the backend's.
    if let Some(waiting) = self.waiters.get_mut(&fd) {
      waiting.queue(interest).pop_back();
    }
    if let Some(entry) = self.pending.remove_value(key) {
      let mut result = Self::exec_op(&entry.op);
      if result == EAGAIN_NEG || result == EWOULDBLOCK_NEG {
        result = raw_error(&err);
      }
      self.queued_completed.push(entry.report(result));
    }
    // Whoever waits for the other direction goes on waiting.
    if let Err(err) = Self::rearm(sys, &mut self.waiters, fd) {
      Self::fail_waiters(
        &mut self.pending,
        &mut self.waiters,
        fd,
        raw_error(&err),
        &mut self.queued_completed,
      );
    }
  }

  fn set_fd_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = syscall!(fcntl(fd, libc::F_GETFD))?;
    syscall!(fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    Ok(())
  }

  fn set_fd_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
    syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
    Ok(())
  }

  fn configure_socket_fd(fd: RawFd) -> io::Result<()> {
    Self::set_fd_cloexec(fd)?;
    Self::set_fd_nonblocking(fd)?;
    Ok(())
  }

  #[inline]
  fn dirent_ino(entry: *const libc::dirent) -> u64 {
    #[cfg(any(
      target_os = "linux",
      target_os = "macos",
      target_os = "ios",
      target_os = "tvos",
      target_os = "watchos"
    ))]
    // SAFETY: `entry` is a live `libc::dirent` pointer returned by `readdir`
    // for the duration of the current iteration.
    unsafe {
      (*entry).d_ino
    }

    #[cfg(any(
      target_os = "freebsd",
      target_os = "dragonfly",
      target_os = "openbsd",
      target_os = "netbsd"
    ))]
    // SAFETY: `entry` is a live `libc::dirent` pointer returned by `readdir`
    // for the duration of the current iteration.
    unsafe {
      (*entry).d_fileno
    }
  }

  unsafe fn drop_readdir_state(state: *mut ()) {
    if !state.is_null() {
      // SAFETY: `state` was created by `fdopendir` and is owned by the
      // backend continuation state for this directory stream.
      unsafe {
        libc::closedir(state.cast());
      }
    }
  }

  fn read_dir_entries_readdir(
    fd: RawFd,
    opaque: &mut *mut (),
    opaque_drop: &mut Option<crate::backend::op::OpaqueDropFn>,
    raw: &mut [u8],
    out: &mut [DirEntryRef],
  ) -> io::Result<ReadDirResult> {
    let mut created_here = false;
    let dir = if opaque.is_null() {
      let dup_fd = syscall!(dup(fd))?;

      // SAFETY: `dup_fd` is a valid duplicated directory descriptor.
      let dir = unsafe { libc::fdopendir(dup_fd) };
      if dir.is_null() {
        let err = io::Error::last_os_error();
        let _ = syscall!(close(dup_fd));
        return Err(err);
      }
      *opaque = dir.cast();
      // SAFETY: `drop_readdir_state` frees the state this backend allocated.
      *opaque_drop = Some(unsafe {
        crate::backend::op::OpaqueDrop::new(Self::drop_readdir_state)
      });
      created_here = true;
      dir
    } else {
      opaque.cast()
    };

    let result = (|| {
      let mut written = 0usize;
      let mut raw_written = 0usize;
      let mut eof = false;
      loop {
        // SAFETY: `dir` is a valid `DIR*` stream owned by this function.
        let pos = unsafe { libc::telldir(dir) };
        if pos < 0 {
          return Err(io::Error::last_os_error());
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: resetting thread-local errno before `readdir`.
        unsafe {
          *libc::__errno_location() = 0;
        }
        #[cfg(any(
          target_os = "macos",
          target_os = "ios",
          target_os = "freebsd",
          target_os = "dragonfly",
          target_os = "openbsd",
          target_os = "netbsd"
        ))]
        // SAFETY: resetting thread-local errno before `readdir`.
        unsafe {
          *libc::__error() = 0;
        }

        // SAFETY: `dir` is a valid `DIR*` stream owned by this function.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
          let err = io::Error::last_os_error();
          if err.raw_os_error().unwrap_or(0) == 0 {
            eof = true;
            break;
          }
          return Err(err);
        }

        // SAFETY: `readdir` returned a valid directory entry whose `d_name`
        // is NUL-terminated for the lifetime of this iteration.
        let name =
          unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_bytes();
        if name == b"." || name == b".." {
          continue;
        }

        if written == out.len() || raw_written + name.len() > raw.len() {
          // SAFETY: `pos` came from `telldir(dir)` on the same directory stream.
          unsafe {
            libc::seekdir(dir, pos);
          }
          break;
        }
        raw[raw_written..raw_written + name.len()].copy_from_slice(name);
        out[written] = DirEntryRef {
          name_offset: raw_written as u32,
          name_len: name.len() as u16,
          // SAFETY: `entry` is valid for this iteration.
          file_type: file_type_from_dirent_dtype(unsafe { (*entry).d_type }),
          ino: Some(Self::dirent_ino(entry)),
        };
        raw_written += name.len();
        written += 1;
      }
      Ok(ReadDirResult { entries: written, raw_written, eof })
    })();

    match result {
      Ok(entries) => {
        if entries.eof {
          let close_result = syscall!(closedir(dir));
          *opaque = std::ptr::null_mut();
          *opaque_drop = None;

          let _ = close_result?;
          Ok(entries)
        } else {
          Ok(entries)
        }
      }
      Err(err) => {
        if !created_here || !dir.is_null() {
          let _ = syscall!(raw closedir(dir));
          *opaque = std::ptr::null_mut();
          *opaque_drop = None;
        }
        Err(err)
      }
    }
  }

  fn read_dir_entries(
    fd: RawFd,
    opaque: &mut *mut (),
    opaque_drop: &mut Option<crate::backend::op::OpaqueDropFn>,
    raw: &mut [u8],
    out: &mut [DirEntryRef],
  ) -> io::Result<ReadDirResult> {
    Self::read_dir_entries_readdir(fd, opaque, opaque_drop, raw, out)
  }

  fn lower_socket_addr(
    addr: &crate::backend::op::SocketAddrBuf,
  ) -> io::Result<(libc::sockaddr_storage, libc::socklen_t)> {
    crate::backend::impls::sockaddr::socket_addr_buf_to_storage(addr)
  }

  fn raise_socket_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
  ) -> io::Result<crate::backend::op::SocketAddrBuf> {
    crate::backend::impls::sockaddr::socket_addr_buf_from_storage(storage, len)
  }

  fn lower_recv_msg(msg: &MsgRecv) -> Option<LoweredMsg> {
    // SAFETY: `msg.bufs` points at `buf_count` live `MsgBufMut` entries.
    let iovecs = unsafe {
      crate::backend::impls::iovec::fill_iovecs(msg.bufs, msg.buf_count.get())
    }?;

    let addr = if msg.from.is_some() {
      Some((
        // SAFETY: `sockaddr_storage` is POD and may be zero-initialized.
        unsafe { std::mem::zeroed::<libc::sockaddr_storage>() },
        std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
      ))
    } else {
      None
    };

    let hdr = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: std::ptr::null_mut(),
      msg_iovlen: 0,
      msg_control: std::ptr::null_mut(),
      msg_controllen: 0,
      msg_flags: 0,
    };
    Some((iovecs, hdr, addr))
  }

  fn lower_send_msg(msg: &MsgSend) -> Option<LoweredMsg> {
    // SAFETY: `msg.bufs` points at `buf_count` live `MsgBuf` entries.
    let iovecs = unsafe {
      crate::backend::impls::iovec::fill_iovecs(msg.bufs, msg.buf_count.get())
    }?;

    let addr =
      msg.to.map(crate::backend::impls::sockaddr::socket_addr_to_storage);
    let hdr = libc::msghdr {
      msg_name: std::ptr::null_mut(),
      msg_namelen: 0,
      msg_iov: std::ptr::null_mut(),
      msg_iovlen: 0,
      msg_control: std::ptr::null_mut(),
      msg_controllen: 0,
      msg_flags: 0,
    };
    Some((iovecs, hdr, addr))
  }

  #[cfg(unix)]
  fn os_path_to_cstring(
    path: &std::ffi::OsString,
  ) -> Result<std::ffi::CString, isize> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
      .map_err(|_| -(libc::EINVAL as isize))
  }

  /// Perform vectored read, choosing the appropriate syscall based on offset
  ///
  /// [`ReadFlags`](crate::backend::op::ReadFlags) has no flags yet, so there
  /// are none to apply.
  #[inline]
  fn do_readv(
    fd: RawFd,
    iovecs: *const libc::iovec,
    iov_count: usize,
    offset: Option<u64>,
  ) -> isize {
    let Ok(offset) = offset.map(libc::off_t::try_from).transpose() else {
      return -(libc::EINVAL as isize);
    };
    if iovecs.is_null() {
      return -(libc::EINVAL as isize);
    }

    let result = match offset {
      None => syscall!(raw readv(fd, iovecs, iov_count as i32)?),
      Some(offset) => {
        syscall!(raw preadv(fd, iovecs, iov_count as i32, offset)?)
      }
    };

    assert!(result >= 0, "error did not return");
    result
  }

  /// Perform vectored write, choosing the appropriate syscall based on offset and flags
  #[inline]
  fn do_writev(
    fd: RawFd,
    iovecs: *const libc::iovec,
    iov_count: usize,
    offset: Option<u64>,
    flags: WriteFlags,
  ) -> isize {
    let Ok(offset) = offset.map(libc::off_t::try_from).transpose() else {
      return -(libc::EINVAL as isize);
    };
    if iovecs.is_null() {
      return -(libc::EINVAL as isize);
    }

    // Linux: native pwritev2 support
    #[cfg(target_os = "linux")]
    return syscall!(raw pwritev2(
      fd,
      iovecs,
      iov_count as i32,
      offset.unwrap_or(-1),
      crate::backend::native::write_flags(flags)
    ));

    #[cfg(not(target_os = "linux"))]
    {
      // Emulate APPEND: write at end of file
      let write_offset = if flags.contains(WriteFlags::APPEND) {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = syscall!(raw fstat(fd, stat.as_mut_ptr())?);
        assert_eq!(result, 0);
        // SAFETY: successful `fstat` initialized `stat`.
        Some(unsafe { stat.assume_init() }.st_size)
      } else {
        offset
      };

      let result = match write_offset {
        None => syscall!(raw writev(fd, iovecs, iov_count as i32)?),
        Some(offset) => {
          syscall!(raw pwritev(fd, iovecs, iov_count as i32, offset)?)
        }
      };

      assert!(result >= 0, "error did not return");

      // SYNC takes precedence over DSYNC
      if flags.contains(WriteFlags::SYNC) {
        let _ = syscall!(raw fsync(fd)?);
      } else if flags.contains(WriteFlags::DSYNC) {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let _ = syscall!(raw fcntl(fd, libc::F_FULLFSYNC)?);
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let _ = syscall!(raw fdatasync(fd)?);
      }
      result
    }
  }

  /// Extract fd and interest from an operation
  fn extract_fd_interest(
    &self,
    op: &crate::backend::op::Op,
  ) -> (RawFd, Interest) {
    match op {
      crate::backend::op::Op::Read { fd, .. } => {
        (fd.as_raw_fd(), Interest::READ)
      }
      crate::backend::op::Op::Write { fd, .. } => {
        (fd.as_raw_fd(), Interest::WRITE)
      }
      crate::backend::op::Op::Recv { fd, .. } => {
        (fd.as_raw_fd(), Interest::READ)
      }
      crate::backend::op::Op::Send { fd, .. } => {
        (fd.as_raw_fd(), Interest::WRITE)
      }
      crate::backend::op::Op::Accept { fd, .. } => {
        (fd.as_raw_fd(), Interest::READ)
      }
      crate::backend::op::Op::Connect { fd, .. } => {
        (fd.as_raw_fd(), Interest::WRITE)
      }
      crate::backend::op::Op::Socket { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Bind { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Listen { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Shutdown { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Fsync { .. } => (0, Interest::NONE),
      crate::backend::op::Op::OpenAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Stat { .. } => (0, Interest::NONE),
      crate::backend::op::Op::ReadDir { .. } => (0, Interest::NONE),
      crate::backend::op::Op::UnlinkAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::RenameAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::MkdirAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::LinkAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::ReadlinkAt { .. } => (0, Interest::NONE),
      crate::backend::op::Op::GetCwd { .. } => (0, Interest::NONE),
      #[cfg(unix)]
      crate::backend::op::Op::Spawn { .. } => (0, Interest::NONE),
      crate::backend::op::Op::Nop => (0, Interest::NONE),
    }
  }

  fn should_complete_immediately(op: &crate::backend::op::Op) -> bool {
    let fd = match op {
      Op::Read { fd, .. } | Op::Write { fd, .. } => fd.as_raw_fd(),
      Op::Socket { .. } => return true,
      Op::Bind { .. } => return true,
      Op::Listen { .. } => return true,
      Op::Shutdown { .. } => return true,
      Op::Fsync { .. } => return true,
      Op::OpenAt { .. } => return true,
      Op::Stat { .. } => return true,
      Op::ReadDir { .. } => return true,
      Op::UnlinkAt { .. } => return true,
      Op::RenameAt { .. } => return true,
      Op::MkdirAt { .. } => return true,
      Op::LinkAt { .. } => return true,
      Op::ReadlinkAt { .. } => return true,
      Op::GetCwd { .. } => return true,
      #[cfg(unix)]
      Op::Spawn { .. } => return true,
      _ => return false,
    };

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if syscall!(raw fstat(fd, stat.as_mut_ptr())) < 0 {
      return false;
    }
    // SAFETY: successful `fstat` initialized `stat`.
    let stat = unsafe { stat.assume_init() };

    (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
  }

  /// Validate malformed ops that should fail immediately without readiness registration.
  fn validate_op(op: &crate::backend::op::Op) -> Option<isize> {
    match op {
      Op::Read { offset, .. } | Op::Write { offset, .. }
        if !is_valid_file_offset(*offset) =>
      {
        Some(-(libc::EINVAL as isize))
      }
      _ => None,
    }
  }

  /// Try to execute an operation (non-blocking syscall)
  /// Returns positive on success (bytes transferred), negative on error (-errno)
  fn exec_op(op: &crate::backend::op::Op) -> isize {
    Self::exec_op_with_socket_flags(op, 0)
  }

  /// Try nonblocking sockets before paying for readiness registration. Preserve
  /// the existing behavior for externally supplied blocking descriptors.
  fn try_socket_immediately(op: &Op) -> Option<isize> {
    let fd = match op {
      Op::Recv { fd, .. } | Op::Send { fd, .. } => fd.as_raw_fd(),
      _ => return None,
    };
    let flags = syscall!(raw fcntl(fd, libc::F_GETFL));
    if flags < 0 || flags & libc::O_NONBLOCK as isize == 0 {
      return None;
    }
    // Also set the per-call flag so a concurrent change to O_NONBLOCK cannot
    // turn this speculative attempt into a blocking call.
    let result = Self::exec_op_with_socket_flags(op, libc::MSG_DONTWAIT);
    if result == EAGAIN_NEG || result == EWOULDBLOCK_NEG {
      None
    } else {
      Some(result)
    }
  }

  fn exec_op_with_socket_flags(op: &Op, socket_flags: i32) -> isize {
    use crate::backend::op::Op;

    match op {
      Op::Read { fd, iovecs, iov_count, offset, flags: _ } => {
        // SAFETY: the op's descriptor array stays live until this syscall returns.
        let Some(native_iovecs) = (unsafe {
          crate::backend::impls::iovec::fill_iovecs(*iovecs, *iov_count)
        }) else {
          return -(libc::EINVAL as isize);
        };
        Self::do_readv(
          fd.as_raw_fd(),
          native_iovecs.as_ptr(),
          *iov_count,
          *offset,
        )
      }

      Op::Write { fd, iovecs, iov_count, offset, flags } => {
        // SAFETY: the op's descriptor array stays live until this syscall returns.
        let Some(native_iovecs) = (unsafe {
          crate::backend::impls::iovec::fill_iovecs(*iovecs, *iov_count)
        }) else {
          return -(libc::EINVAL as isize);
        };
        Self::do_writev(
          fd.as_raw_fd(),
          native_iovecs.as_ptr(),
          *iov_count,
          *offset,
          *flags,
        )
      }

      Op::Recv { fd, msg, flags } => {
        let fd = fd.as_raw_fd();
        let Some((mut iovecs, mut hdr, mut addr)) = Self::lower_recv_msg(msg)
        else {
          return -(libc::EINVAL as isize);
        };
        hdr.msg_iov = iovecs.as_mut_ptr();
        hdr.msg_iovlen = msg.buf_count.get() as _;
        if let Some((storage, len)) = addr.as_mut() {
          hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
          hdr.msg_namelen = *len;
        }
        let result = syscall!(raw recvmsg(fd, &mut hdr, crate::backend::native::recv_flags(*flags) | socket_flags));
        if result >= 0
          && let (Some(out), Some((storage, len))) = (msg.from, addr.as_ref())
          && let Ok(addr) = Self::raise_socket_addr(storage, *len)
        {
          unsafe {
            *out.as_ptr() = addr;
          }
        }
        result
      }

      Op::Send { fd, msg, flags } => {
        let fd = fd.as_raw_fd();
        let Some((mut iovecs, mut hdr, mut addr)) = Self::lower_send_msg(msg)
        else {
          return -(libc::EINVAL as isize);
        };
        hdr.msg_iov = iovecs.as_mut_ptr();
        hdr.msg_iovlen = msg.buf_count.get() as _;
        if let Some((storage, len)) = addr.as_mut() {
          hdr.msg_name = (storage as *mut libc::sockaddr_storage).cast();
          hdr.msg_namelen = *len;
        }
        syscall!(raw sendmsg(fd, &mut hdr, crate::backend::native::send_flags(*flags) | socket_flags))
      }

      Op::Accept { fd, addr } => {
        let fd = fd.as_raw_fd();
        // SAFETY: `sockaddr_storage` is POD and may be zero-initialized.
        let mut storage =
          unsafe { std::mem::zeroed::<libc::sockaddr_storage>() };
        let mut len =
          std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let result = syscall!(raw accept(fd, (&mut storage as *mut libc::sockaddr_storage).cast(), &mut len));
        if result >= 0 {
          let accepted_fd = result as RawFd;
          if let Err(err) = Self::configure_socket_fd(accepted_fd) {
            let _ = syscall!(raw close(accepted_fd));
            return -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize);
          }
          match Self::raise_socket_addr(&storage, len) {
            Ok(peer) => {
              // SAFETY: `addr` points to caller-provided writable peer storage.
              unsafe { *addr.as_ptr() = peer }
            }
            Err(err) => {
              let _ = syscall!(raw close(accepted_fd));
              return -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize);
            }
          }
        }
        result
      }

      Op::Connect { fd, .. } => {
        let fd = fd.as_raw_fd();
        let mut err: i32 = 0;
        let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
        let ret = syscall!(raw getsockopt(
          fd,
          libc::SOL_SOCKET,
          libc::SO_ERROR,
          (&mut err as *mut i32).cast(),
          &mut len,
        ));

        if ret < 0 {
          ret
        } else if err == 0 {
          0
        } else {
          -(err as isize)
        }
      }

      Op::OpenAt { dir_fd, path, flags, mode } => {
        let Ok(path) = Self::os_path_to_cstring(path) else {
          return -(libc::EINVAL as isize);
        };
        match crate::backend::native::open_flags(*flags) {
          Ok(flags) => syscall!(raw openat(
            dir_fd.as_raw_fd(),
            path.as_ptr(),
            flags,
            libc::c_uint::from(crate::backend::native::file_mode(*mode)),
          )),
          Err(errno) => -(errno as isize),
        }
      }
      Op::Stat { target, out } => {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = match target {
          crate::backend::op::StatTarget::Path {
            dir_fd,
            path,
            follow_symlinks,
          } => {
            let flags =
              if *follow_symlinks { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
            let Ok(path) = Self::os_path_to_cstring(path) else {
              return -(libc::EINVAL as isize);
            };
            syscall!(raw fstatat(
              dir_fd.as_raw_fd(),
              path.as_ptr(),
              stat.as_mut_ptr(),
              flags,
            ))
          }
          crate::backend::op::StatTarget::Fd { fd } => {
            syscall!(raw fstat(fd.as_raw_fd(), stat.as_mut_ptr()))
          }
        };
        if result >= 0 {
          // SAFETY: successful `fstatat`/`fstat` initialized `stat`.
          let stat = unsafe { stat.assume_init() };
          // SAFETY: `out` points to caller-provided writable result storage.
          unsafe {
            *out.as_ptr() = file_stat_from_raw(&stat);
          }
        }
        result
      }
      Op::ReadDir {
        fd,
        raw_buf,
        raw_cap,
        entries,
        entries_cap,
        opaque,
        opaque_drop,
        out,
      } => {
        // SAFETY: `raw_buf` points to `raw_cap` writable bytes.
        let raw =
          unsafe { std::slice::from_raw_parts_mut(raw_buf.as_ptr(), *raw_cap) };
        // SAFETY: `entries` points to `entries_cap` writable `DirEntryRef`s.
        let entries = unsafe {
          std::slice::from_raw_parts_mut(entries.as_ptr(), *entries_cap)
        };
        // SAFETY: `opaque` references backend-owned continuation state.
        let opaque = unsafe { &mut *opaque.as_ptr() };
        // SAFETY: `opaque_drop` references backend-owned continuation state.
        let opaque_drop = unsafe { &mut *opaque_drop.as_ptr() };
        match Self::read_dir_entries(
          fd.as_raw_fd(),
          opaque,
          opaque_drop,
          raw,
          entries,
        ) {
          Ok(result) => {
            // SAFETY: `out` points to caller-provided writable result storage.
            unsafe {
              *out.as_ptr() = result;
            }
            0
          }
          Err(err) => -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize),
        }
      }
      Op::UnlinkAt { dir_fd, path, kind } => {
        let Ok(path) = Self::os_path_to_cstring(path) else {
          return -(libc::EINVAL as isize);
        };
        let flags = match kind {
          crate::backend::op::UnlinkKind::File => 0,
          crate::backend::op::UnlinkKind::Directory => libc::AT_REMOVEDIR,
        };
        syscall!(raw unlinkat(dir_fd.as_raw_fd(), path.as_ptr(), flags))
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        let Ok(old_path) = Self::os_path_to_cstring(old_path) else {
          return -(libc::EINVAL as isize);
        };
        let Ok(new_path) = Self::os_path_to_cstring(new_path) else {
          return -(libc::EINVAL as isize);
        };
        syscall!(raw renameat(
          old_dir_fd.as_raw_fd(),
          old_path.as_ptr(),
          new_dir_fd.as_raw_fd(),
          new_path.as_ptr(),
        ))
      }
      Op::MkdirAt { dir_fd, path, mode } => {
        let Ok(path) = Self::os_path_to_cstring(path) else {
          return -(libc::EINVAL as isize);
        };
        syscall!(raw mkdirat(
          dir_fd.as_raw_fd(),
          path.as_ptr(),
          crate::backend::native::file_mode(*mode),
        ))
      }
      Op::LinkAt { kind, source_dir_fd, source_path, new_dir_fd, new_path } => {
        match kind {
          crate::backend::op::LinkKind::Hard => {
            let Ok(source_path) = Self::os_path_to_cstring(source_path) else {
              return -(libc::EINVAL as isize);
            };
            let Ok(new_path) = Self::os_path_to_cstring(new_path) else {
              return -(libc::EINVAL as isize);
            };
            syscall!(raw linkat(
              source_dir_fd.as_raw_fd(),
              source_path.as_ptr(),
              new_dir_fd.as_raw_fd(),
              new_path.as_ptr(),
              0,
            ))
          }
          crate::backend::op::LinkKind::Soft => {
            let Ok(source_path) = Self::os_path_to_cstring(source_path) else {
              return -(libc::EINVAL as isize);
            };
            let Ok(new_path) = Self::os_path_to_cstring(new_path) else {
              return -(libc::EINVAL as isize);
            };
            syscall!(raw symlinkat(
              source_path.as_ptr(),
              new_dir_fd.as_raw_fd(),
              new_path.as_ptr(),
            ))
          }
        }
      }
      Op::ReadlinkAt { dir_fd, path, buf, buf_len } => {
        let Ok(path) = Self::os_path_to_cstring(path) else {
          return -(libc::EINVAL as isize);
        };
        syscall!(raw readlinkat(
          dir_fd.as_raw_fd(),
          path.as_ptr(),
          buf.as_ptr().cast::<libc::c_char>(),
          *buf_len,
        ))
      }
      Op::GetCwd { out } => match std::env::current_dir() {
        Ok(cwd) => {
          // SAFETY: `out` points to operation-owned output storage.
          unsafe { out.as_ptr().write(cwd.into_os_string()) };
          0
        }
        Err(err) => -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize),
      },
      #[cfg(unix)]
      Op::Spawn { spec } => {
        use std::os::unix::ffi::OsStrExt;

        unsafe extern "C" {
          static mut environ: *mut *mut libc::c_char;
        }

        let path = match std::ffi::CString::new(spec.program.as_bytes()) {
          Ok(path) => path,
          Err(_) => return -(libc::EINVAL as isize),
        };
        let argv = match spec
          .args
          .iter()
          .map(|arg| std::ffi::CString::new(arg.as_bytes()))
          .collect::<Result<Vec<_>, _>>()
        {
          Ok(argv) => argv,
          Err(_) => return -(libc::EINVAL as isize),
        };
        let mut argv_ptrs: Vec<*mut libc::c_char> = argv
          .iter()
          .map(|arg| arg.as_ptr().cast_mut())
          .chain(std::iter::once(std::ptr::null_mut()))
          .collect();
        let env = match spec.env.as_ref().map(|env| {
          env
            .iter()
            .map(|var| std::ffi::CString::new(var.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
        }) {
          Some(Ok(env)) => Some(env),
          Some(Err(_)) => return -(libc::EINVAL as isize),
          None => None,
        };
        let mut env_ptrs = env.as_ref().map(|env| {
          env
            .iter()
            .map(|var| var.as_ptr().cast_mut())
            .chain(std::iter::once(std::ptr::null_mut()))
            .collect::<Vec<_>>()
        });

        let mut pid: libc::pid_t = 0;
        let envp = env_ptrs
          .as_mut()
          .map(|vars| vars.as_mut_ptr().cast_const())
          .unwrap_or_else(|| {
            // SAFETY: `environ` is libc's process-global environment vector.
            // Mutating it while another thread reads it is ruled out by the
            // contract of `std::env::set_var`/`remove_var`, which are
            // `unsafe` in Rust 2024 for exactly this reason.
            unsafe { environ as *const *mut libc::c_char }
          });
        // SAFETY: native C strings and pointer arrays remain alive for this call.
        let result = unsafe {
          libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            argv_ptrs.as_mut_ptr().cast_const(),
            envp,
          )
        };
        if result != 0 { -(result as isize) } else { pid as isize }
      }

      Op::Socket { domain, ty, proto } => {
        match crate::backend::impls::sockaddr::socket_to_raw(
          *domain, *ty, *proto,
        ) {
          Ok((domain, ty, proto)) => {
            let result = syscall!(raw socket(domain, ty, proto));
            if result < 0 {
              result
            } else {
              let fd = result as RawFd;
              if let Err(err) = Self::configure_socket_fd(fd) {
                let _ = syscall!(raw close(fd));
                -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize)
              } else {
                result
              }
            }
          }
          Err(errno) => -(errno as isize),
        }
      }
      Op::Bind { fd, addr } => match Self::lower_socket_addr(addr) {
        Ok((storage, len)) => syscall!(raw bind(
          fd.as_raw_fd(),
          (&storage as *const libc::sockaddr_storage).cast(),
          len
        )),
        Err(err) => -(err.raw_os_error().unwrap_or(libc::EINVAL) as isize),
      },
      Op::Listen { fd, backlog } => {
        syscall!(raw listen(fd.as_raw_fd(), *backlog))
      }
      Op::Shutdown { fd, how } => {
        let how = match how {
          crate::backend::op::ShutdownHow::Read => libc::SHUT_RD,
          crate::backend::op::ShutdownHow::Write => libc::SHUT_WR,
          crate::backend::op::ShutdownHow::Both => libc::SHUT_RDWR,
        };
        syscall!(raw shutdown(fd.as_raw_fd(), how))
      }
      Op::Fsync { fd } => {
        syscall!(raw fsync(fd.as_raw_fd()))
      }

      Op::Nop => 0,
    }
  }
}

impl<P: ReadinessPoll> IoBackend for Poller<P> {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    if self.sys.is_none() {
      self.sys = Some(P::open()?);
    }
    self.events = Events::with_capacity(cap.min(4096));
    self.capacity = cap;
    self.backlog.clear();
    self.backlog.reserve_exact(cap);
    self.pending = Slab::new(cap);
    self.waiters.clear();
    self.queued_completed = Vec::with_capacity(cap.min(256));
    self.completed = Vec::with_capacity(cap.min(256));
    Ok(())
  }

  fn push(&mut self, op: crate::backend::InFlight<'_>, _step_bump: &mut Bump) {
    let (op, ticket) = op.into_parts();
    assert!(
      self.backlog.len() + self.pending.len() < self.capacity,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.capacity
    );
    self.backlog.push(PendingOp { op, ticket });
  }

  fn flush(&mut self) -> io::Result<()> {
    // In the order they were pushed: operations on one descriptor wait in
    // that order.
    let mut backlog = std::mem::take(&mut self.backlog);
    for entry in backlog.drain(..) {
      if matches!(entry.op, Op::Nop) {
        self.queued_completed.push(entry.report(0));
        continue;
      };

      if let Some(result) = Self::validate_op(&entry.op) {
        self.queued_completed.push(entry.report(result));
        continue;
      }

      if Self::should_complete_immediately(&entry.op) {
        let result = Self::exec_op(&entry.op);
        self.queued_completed.push(entry.report(result));
        continue;
      }

      if let Some(result) = Self::try_socket_immediately(&entry.op) {
        self.queued_completed.push(entry.report(result));
        continue;
      }

      if let Op::Connect { fd, addr } = &entry.op {
        let Ok((storage, len)) = Self::lower_socket_addr(addr) else {
          self.queued_completed.push(entry.report(-(libc::EINVAL as isize)));
          continue;
        };
        let result = syscall!(raw connect(
          fd.as_raw_fd(),
          (&storage as *const libc::sockaddr_storage).cast(),
          len,
        ));

        match result {
          0 => {
            self.queued_completed.push(entry.report(0));
            continue;
          }
          #[allow(unreachable_patterns)]
          EAGAIN_NEG | EWOULDBLOCK_NEG => {}
          #[cfg(unix)]
          x if x == -(libc::EINPROGRESS as isize) => {}
          _ => {
            self.queued_completed.push(entry.report(result));
            continue;
          }
        }
      }

      self.park(entry);
    }
    // Keep the allocation for the next batch.
    self.backlog = backlog;

    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    if let Some(index) = self.backlog.iter().position(|entry| entry.id() == id)
    {
      let entry = self.backlog.swap_remove(index);
      self.queued_completed.push(entry.stopped());
      return;
    }
    // This backend performs the I/O itself once the descriptor is ready, so
    // nothing outside it uses the op's memory: it can be dropped right away.
    let Some(key) = self.pending.keys_where(|entry| entry.id() == id).next()
    else {
      return;
    };
    let Some(entry) = self.pending.remove_value(key) else {
      return;
    };
    // The others waiting on its descriptor go on waiting.
    let (fd, interest) = self.extract_fd_interest(&entry.op);
    self.queued_completed.push(entry.stopped());
    let Some(waiting) = self.waiters.get_mut(&fd) else {
      return;
    };
    let queue = waiting.queue(interest);
    queue.retain(|waiting| *waiting != key);
    if !queue.is_empty() {
      return;
    }
    let Some(sys) = self.sys.as_ref() else {
      return;
    };
    if let Err(err) = Self::rearm(sys, &mut self.waiters, fd) {
      Self::fail_waiters(
        &mut self.pending,
        &mut self.waiters,
        fd,
        raw_error(&err),
        &mut self.queued_completed,
      );
    }
  }

  fn drain(&mut self) -> bool {
    // Readiness interest is the only thing the kernel holds. The bytes live
    // in `backlog` and `pending`, and this backend only dereferences them
    // from `flush` and `wait`, so every operation can be given up here.
    for entry in self.backlog.drain(..) {
      entry.release();
    }
    for (fd, waiting) in self.waiters.drain() {
      if waiting.registered
        && let Some(sys) = self.sys.as_ref()
      {
        let _ = sys.delete(fd);
      }
    }
    let keys: Vec<_> = self.pending.keys_where(|_| true).collect();
    for key in keys {
      if let Some(entry) = self.pending.remove_value(key) {
        entry.release();
      }
    }
    true
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    if !self.queued_completed.is_empty() {
      completed.append(&mut self.queued_completed);
      if self.pending.len() == 0 {
        return Ok(());
      }
    }

    // Immediate socket completions must not starve previously registered I/O.
    // Poll pending readiness too, without blocking delivery of ready results.
    let timeout =
      if completed.is_empty() { timeout } else { Some(Duration::ZERO) };

    // Borrows only the field, unlike `sys()`: the loop below updates
    // `pending` while `sys` is live.
    let Some(sys) = self.sys.as_ref() else {
      panic!("Poller not initialized: the driver calls init first");
    };

    {
      let event_count = match sys.wait(self.events.as_buf(), timeout) {
        Ok(count) => count,
        Err(err) => {
          // The driver cannot consume `completed` on an error. Preserve any
          // immediate results so its next call can still deliver them.
          self.queued_completed.append(completed);
          return Err(err);
        }
      };
      self.events.set_ready(event_count);
    }

    while let Some((key, ready)) = self.events.pop_event() {
      let Ok(fd) = RawFd::try_from(key) else {
        continue; // Not a descriptor's key.
      };
      let Some(waiting) = self.waiters.get_mut(&fd) else {
        continue; // Everything that waited on it was cancelled.
      };

      // An event that names a direction nobody waits for (a hang-up, say)
      // still has to reach whoever is waiting.
      let readers = ready.is_readable() || waiting.writers.is_empty();
      let writers = ready.is_writable() || waiting.readers.is_empty();
      if readers {
        Self::serve(
          &mut waiting.readers,
          &mut self.pending,
          &mut self.completed,
        );
      }
      if writers {
        Self::serve(
          &mut waiting.writers,
          &mut self.pending,
          &mut self.completed,
        );
      }

      // The event used the registration up.
      if let Err(err) = Self::rearm(sys, &mut self.waiters, fd) {
        Self::fail_waiters(
          &mut self.pending,
          &mut self.waiters,
          fd,
          raw_error(&err),
          &mut self.completed,
        );
      }
    }

    completed.append(&mut self.completed);

    Ok(())
  }

  fn resource_close(&self) -> crate::api::resource::ResourceClose {
    crate::api::resource::ResourceClose::native()
  }
}
