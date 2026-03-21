//! `lio`-provided [`IoBackend`] impl for `io_uring`.

use lio_uring::{
  Entry, LioUring,
  operation::{
    self, Accept, AcceptMulti, AsyncCancel, Bind, Close, Connect, Fsync,
    Ftruncate, LinkAt, Listen, MkDirAt, OpenAt, PollAdd, Readv, Recv, RecvMsg,
    RenameAt, Send, SendMsg, Shutdown, Socket, Splice, SymlinkAt, Tee, Timeout,
    TimeoutRemove, UnlinkAt, WaitId, Writev,
  },
};

/// Reserved user_data for the timing wheel's kernel timer.
const WHEEL_TIMER_KEY: u64 = u64::MAX - 1;

/// Flag bit used to mark linked timeout completions.
/// When set, the completion is for the LINK_TIMEOUT part of a timeout wrapper
/// and should be filtered out (we only care about the inner op's completion).
const TIMEOUT_LINK_FLAG: u64 = 1 << 63;

// Note: All read/write operations use Readv/Writev with a single iovec for
// single-buffer ops, or multiple iovecs for vectored ops. The .offset() method
// is used for positional I/O (preadv/pwritev).

use crate::backend::{IoBackend, OpCompleted, op::Op};
use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

fn create_io_uring_entry(op: &Op) -> Entry {
  match op {
    Op::Nop => operation::Nop::new().build(),
    Op::Send { fd, flags, buf } => {
      Send::new(fd.as_raw_fd(), buf.ptr, buf.len as u32).flags(*flags).build()
    }
    Op::Recv { fd, flags, buf } => {
      Recv::new(fd.as_raw_fd(), buf.ptr, buf.len as u32).flags(*flags).build()
    }
    Op::SendTo { fd, flags, msghdr, .. } => {
      // msghdr is pre-constructed in TypedOp with iovec and address already set
      SendMsg::new(fd.as_raw_fd(), *msghdr).flags(*flags as u32).build()
    }
    Op::RecvFrom { fd, flags, msghdr, .. } => {
      // msghdr is pre-constructed in TypedOp with iovec and address already set
      RecvMsg::new(fd.as_raw_fd(), *msghdr).flags(*flags as u32).build()
    }
    Op::Accept { fd, addr, len } => {
      // Cast sockaddr_storage* to sockaddr*
      Accept::new(fd.as_raw_fd(), (*addr) as *mut libc::sockaddr, *len).build()
    }
    Op::AcceptStream { fd } => {
      // Use AcceptMulti for multishot accept (no address - caller uses getpeername)
      AcceptMulti::new(fd.as_raw_fd()).build()
    }
    Op::Connect { fd, addr, len, .. } => {
      Connect::new(fd.as_raw_fd(), (*addr) as *const libc::sockaddr, *len)
        .build()
    }
    Op::Bind { fd, addr, addrlen } => {
      Bind::new(fd.as_raw_fd(), (*addr) as *const libc::sockaddr, *addrlen)
        .build()
    }
    Op::Listen { fd, backlog } => Listen::new(fd.as_raw_fd(), *backlog).build(),
    Op::Shutdown { fd, how } => Shutdown::new(fd.as_raw_fd(), *how).build(),
    Op::Socket { domain, ty, proto } => {
      Socket::new(*domain, *ty, *proto).build()
    }
    Op::OpenAt { dir_fd, path, flags, mode } => {
      OpenAt::new(dir_fd.as_raw_fd(), *path).flags(*flags).mode(*mode).build()
    }
    Op::Close { fd } => Close::new(*fd).build(),
    Op::Fsync { fd } => Fsync::new(fd.as_raw_fd()).build(),
    // TODO: IORING_OP_FTRUNCATE (kernel 6.9+) returns success but doesn't
    // actually truncate in some tests. Needs investigation - might be SQE
    // setup issue or kernel-specific behavior.
    Op::Truncate { fd, size } => Ftruncate::new(fd.as_raw_fd(), *size).build(),
    Op::LinkAt { old_dir_fd, old_path, new_dir_fd, new_path } => LinkAt::new(
      old_dir_fd.as_raw_fd(),
      *old_path,
      new_dir_fd.as_raw_fd(),
      *new_path,
    )
    .build(),
    Op::SymlinkAt { target, linkpath, dir_fd } => {
      SymlinkAt::new(dir_fd.as_raw_fd(), *target, *linkpath).build()
    }
    Op::UnlinkAt { dir_fd, path, flags } => {
      UnlinkAt::new(dir_fd.as_raw_fd(), *path).flags(*flags).build()
    }
    Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
      RenameAt::new(
        old_dir_fd.as_raw_fd(),
        *old_path,
        new_dir_fd.as_raw_fd(),
        *new_path,
      )
      .build()
    }
    Op::MkdirAt { dir_fd, path, mode } => {
      MkDirAt::new(dir_fd.as_raw_fd(), *path)
        .mode(*mode as libc::mode_t)
        .build()
    }
    #[cfg(target_os = "linux")]
    Op::Tee { fd_in, fd_out, size } => {
      Tee::new(fd_in.as_raw_fd(), fd_out.as_raw_fd(), *size).build()
    }
    #[cfg(target_os = "linux")]
    Op::Splice { fd_in, off_in, fd_out, off_out, len, flags } => Splice::new(
      fd_in.as_raw_fd(),
      *off_in,
      fd_out.as_raw_fd(),
      *off_out,
      *len,
    )
    .flags(*flags)
    .build(),
    // These operations are handled in push() before reaching this match
    #[cfg(unix)]
    Op::SendFile { .. } => operation::Nop::new().build(),
    #[cfg(target_os = "linux")]
    Op::CopyFileRange { .. } => operation::Nop::new().build(),
    Op::Sleep { timespec, .. } => {
      // __kernel_timespec has same layout as libc::timespec
      // timespec is already a pointer to data in the boxed TypedOp
      Timeout::new(*timespec as *const _).build()
    }
    #[cfg(target_os = "linux")]
    Op::Timeout { .. } => operation::Nop::new().build(),
    Op::ReadV { fd, iovecs, iov_count, .. } => {
      Readv::new(fd.as_raw_fd(), *iovecs, *iov_count as u32).build()
    }
    Op::WriteV { fd, iovecs, iov_count, .. } => {
      Writev::new(fd.as_raw_fd(), *iovecs, *iov_count as u32).build()
    }
    Op::ReadVAt { fd, iovecs, iov_count, offset, .. } => {
      Readv::new(fd.as_raw_fd(), *iovecs, *iov_count as u32)
        .offset(*offset as u64)
        .build()
    }
    Op::WriteVAt { fd, iovecs, iov_count, offset, .. } => {
      Writev::new(fd.as_raw_fd(), *iovecs, *iov_count as u32)
        .offset(*offset as u64)
        .build()
    }
    #[cfg(unix)]
    Op::Watch { .. } => operation::Nop::new().build(),
    #[cfg(unix)]
    Op::Waitid { idtype, id, options, infop } => {
      WaitId::new(*idtype, *id, *options).infop(*infop).build()
    }
    #[cfg(unix)]
    Op::Spawn { .. } => operation::Nop::new().build(),
    // These operations are handled in push() before reaching this match
    Op::Flock { .. } => operation::Nop::new().build(),
    #[cfg(unix)]
    Op::GetDents { .. } => operation::Nop::new().build(),
    #[cfg(unix)]
    Op::Signal { .. } => operation::Nop::new().build(),
  }
}

/// io_uring backend for Linux.
///
/// This is the highest-performance backend, using Linux's io_uring interface
/// for truly asynchronous I/O with minimal syscall overhead.
///
/// # Example
///
/// ```rust,ignore
/// let mut backend = IoUring::default();
/// backend.init(1024)?;
///
/// backend.push(id, &op)?;
/// backend.flush()?;
///
/// let completions = backend.wait(&mut store)?;
/// ```
/// Immediate completion for operations that complete synchronously (e.g., SendFile, CopyFileRange).
struct ImmediateCompletion {
  id: u64,
  result: isize,
}

/// Flag bit used to mark watch poll completions.
/// When set, the completion is for a watch operation's poll and needs
/// post-processing to read from the inotify fd.
const WATCH_POLL_FLAG: u64 = 1 << 62;

/// Flag bit used to mark signal poll completions.
/// When set, the completion is for a signal operation's poll and needs
/// post-processing to read from the signalfd.
const SIGNAL_POLL_FLAG: u64 = 1 << 61;

#[derive(Default)]
pub struct IoUring {
  ring: Option<LioUring>,
  /// Reusable buffer for completed operations (avoids allocation per poll/wait).
  completed: Vec<OpCompleted>,
  /// Immediate completions (operations that completed without polling).
  immediate: Vec<ImmediateCompletion>,
  /// Storage for the wheel timer's timespec (must outlive the timeout operation).
  wheel_timer_ts: Option<Box<libc::timespec>>,
  /// Whether a wheel timer is currently armed.
  wheel_timer_armed: bool,
  /// Watch operation tracking: op_id -> inotify_fd
  /// Stores the inotify fd created for watch operations so we can read from it on completion.
  watch_fds: std::collections::HashMap<u64, i32>,
  /// Signal operation tracking: op_id -> signalfd
  /// Stores the signalfd created for signal operations so we can read from it on completion.
  signal_fds: std::collections::HashMap<u64, i32>,
}

impl IoUring {
  /// Create a new uninitialized io_uring backend.
  ///
  /// Call [`init`](Self::init) before using.
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  fn ring(&mut self) -> &mut LioUring {
    self.ring.as_mut().expect("IoUring not initialized - call init() first")
  }

  /// Reserved user_data for timeout remove operations.
  const TIMEOUT_REMOVE_KEY: u64 = u64::MAX - 2;

  /// Check if a completion is an internal event that should be filtered out.
  /// Returns true if the event should be skipped.
  fn is_internal_event(user_data: u64) -> bool {
    user_data == WHEEL_TIMER_KEY
      || user_data == Self::TIMEOUT_REMOVE_KEY
      || (user_data & TIMEOUT_LINK_FLAG) != 0
  }

  /// Poll for completions with optional timeout.
  ///
  /// - `timeout = None`: Block indefinitely
  /// - `timeout = Some(Duration::ZERO)`: Non-blocking poll
  /// - `timeout = Some(duration)`: Wait up to duration
  fn poll_inner(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();

    // First, drain any immediate completions (operations that completed in push())
    for imm in self.immediate.drain(..) {
      self.completed.push(OpCompleted::new(imm.id, imm.result));
    }

    let ring = self.ring.as_mut().expect("IoUring not initialized");

    // If we have immediate completions, don't block - just poll for more
    let effective_timeout =
      if !self.completed.is_empty() { Some(Duration::ZERO) } else { timeout };

    // Get first completion based on timeout mode
    let first = match effective_timeout {
      None => {
        // Block indefinitely for first completion
        Some(ring.wait()?)
      }
      Some(d) if d.is_zero() => {
        // Non-blocking: check if anything is ready
        ring.try_wait()?
      }
      Some(d) => {
        // Wait with timeout
        ring.wait_timeout(d)?
      }
    };

    // Collect all completions first to avoid borrow conflicts
    // Tuple: (user_data, result, has_more)
    let mut raw_completions: Vec<(u64, isize, bool)> = Vec::new();

    if let Some(first) = first {
      raw_completions.push((
        first.user_data(),
        first.result() as isize,
        first.has_more(),
      ));

      // Drain any additional completions (non-blocking)
      while let Ok(Some(op)) = ring.try_wait() {
        raw_completions.push((
          op.user_data(),
          op.result() as isize,
          op.has_more(),
        ));
      }
    }

    // Now process all completions
    for (user_data, result, has_more) in raw_completions {
      self.process_completion(user_data, result, has_more);
    }

    Ok(&self.completed)
  }

  /// Process a single completion, handling special cases like watch poll events.
  fn process_completion(
    &mut self,
    user_data: u64,
    result: isize,
    has_more: bool,
  ) {
    if user_data == WHEEL_TIMER_KEY {
      self.wheel_timer_armed = false;
      return;
    }

    if Self::is_internal_event(user_data) {
      return;
    }

    // Check if this is a signal poll completion
    if (user_data & SIGNAL_POLL_FLAG) != 0 {
      let real_id = user_data & !SIGNAL_POLL_FLAG;

      // Get the signalfd and read the signal info
      if let Some(signal_fd) = self.signal_fds.remove(&real_id) {
        let final_result = if result < 0 {
          // Poll failed
          result
        } else {
          // Poll succeeded, read from signalfd to get the signal number
          let mut info =
            std::mem::MaybeUninit::<libc::signalfd_siginfo>::uninit();
          // SAFETY: signal_fd is valid, info is a valid buffer of correct size
          let n = unsafe {
            libc::read(
              signal_fd,
              info.as_mut_ptr() as *mut _,
              std::mem::size_of::<libc::signalfd_siginfo>(),
            )
          };

          if n == std::mem::size_of::<libc::signalfd_siginfo>() as isize {
            // SAFETY: we read the full struct
            let info = unsafe { info.assume_init() };
            info.ssi_signo as isize
          } else if n < 0 {
            -(std::io::Error::last_os_error()
              .raw_os_error()
              .unwrap_or(libc::EIO) as isize)
          } else {
            -(libc::EIO as isize)
          }
        };

        // SAFETY: signal_fd is a valid fd that we own
        unsafe { libc::close(signal_fd) };

        self.completed.push(OpCompleted::new(real_id, final_result));
      }
      return;
    }

    // Check if this is a watch poll completion
    if (user_data & WATCH_POLL_FLAG) != 0 {
      let real_id = user_data & !WATCH_POLL_FLAG;

      // Get the inotify fd and read the event
      if let Some(inotify_fd) = self.watch_fds.remove(&real_id) {
        let final_result = if result < 0 {
          // Poll failed
          result
        } else {
          // Poll succeeded, read from inotify
          use crate::api::ops::WatchMask;

          let mut buf = [0u8; 256];
          // SAFETY: inotify_fd is valid, buf is a valid buffer
          let n = unsafe {
            libc::read(inotify_fd, buf.as_mut_ptr() as *mut _, buf.len())
          };

          if n < 0 {
            -(std::io::Error::last_os_error()
              .raw_os_error()
              .unwrap_or(libc::EIO) as isize)
          } else if n >= std::mem::size_of::<libc::inotify_event>() as isize {
            // SAFETY: we verified n >= sizeof(inotify_event), so buffer contains valid event
            let event =
              unsafe { &*(buf.as_ptr() as *const libc::inotify_event) };
            WatchMask::from_inotify_mask(event.mask).bits() as isize
          } else {
            0
          }
        };

        // SAFETY: inotify_fd is a valid fd that we own
        unsafe { libc::close(inotify_fd) };

        self.completed.push(OpCompleted::new(real_id, final_result));
      }
      return;
    }

    self
      .completed
      .push(OpCompleted::new(user_data, result).with_more(has_more));
  }
}

impl IoBackend for IoUring {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    let ring = LioUring::new(cap as u32)?;
    self.ring = Some(ring);
    // Pre-allocate completions buffer (reasonable batch size)
    self.completed = Vec::with_capacity(cap.min(256));
    self.immediate = Vec::with_capacity(64);
    self.watch_fds = std::collections::HashMap::with_capacity(16);
    self.signal_fds = std::collections::HashMap::with_capacity(16);
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op) -> io::Result<()> {
    // Handle operations that don't have io_uring support via blocking syscall
    #[cfg(unix)]
    if let Op::SendFile { out_fd, in_fd, offset, count } = &op {
      let result = {
        #[cfg(target_os = "linux")]
        {
          let mut off = *offset;
          syscall!(raw sendfile(out_fd.as_raw_fd(), in_fd.as_raw_fd(), &mut off, *count))
        }
        #[cfg(not(target_os = "linux"))]
        {
          let mut len: libc::off_t = *count as libc::off_t;
          let ret = syscall!(raw sendfile(in_fd.as_raw_fd(), out_fd.as_raw_fd(), *offset, &mut len, std::ptr::null_mut(), 0));
          if ret == 0 { len as isize } else { ret }
        }
      };
      self.immediate.push(ImmediateCompletion { id, result });
      return Ok(());
    }

    #[cfg(target_os = "linux")]
    if let Op::CopyFileRange { fd_in, off_in, fd_out, off_out, len, flags } =
      &op
    {
      let mut off_in_val = *off_in;
      let mut off_out_val = *off_out;
      let result = syscall!(raw copy_file_range(fd_in.as_raw_fd(), &mut off_in_val, fd_out.as_raw_fd(), &mut off_out_val, *len, *flags as libc::c_uint));
      self.immediate.push(ImmediateCompletion { id, result });
      return Ok(());
    }

    // Handle Spawn via posix_spawn (no io_uring support)
    #[cfg(unix)]
    if let Op::Spawn { path, argv, envp, pid, file_actions } = &op {
      // SAFETY: All pointers are valid and owned by the Op
      let ret = unsafe {
        libc::posix_spawn(
          *pid,
          *path,
          *file_actions,
          std::ptr::null(), // attrp
          *argv as *const *mut _,
          *envp as *const *mut _,
        )
      };
      let result = if ret == 0 { 0 } else { -(ret as isize) };
      self.immediate.push(ImmediateCompletion { id, result });
      return Ok(());
    }

    // Handle Flock via blocking syscall (no io_uring support)
    if let Op::Flock { fd, operation } = &op {
      let result = syscall!(raw flock(fd.as_raw_fd(), *operation));
      self.immediate.push(ImmediateCompletion { id, result });
      return Ok(());
    }

    // Handle GetDents via blocking syscall (no io_uring support)
    #[cfg(unix)]
    if let Op::GetDents { fd, buf } = &op {
      let result = syscall!(raw syscall(libc::SYS_getdents64, fd.as_raw_fd(), buf.ptr as *mut libc::c_void, buf.len));
      self.immediate.push(ImmediateCompletion { id, result });
      return Ok(());
    }

    // Handle Signal via signalfd polling (no direct io_uring support)
    #[cfg(unix)]
    if let Op::Signal { sigset } = &op {
      // Create signalfd for the signal set
      let sfd = syscall!(raw signalfd(-1, *sigset, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC));
      if sfd < 0 {
        self.immediate.push(ImmediateCompletion { id, result: sfd });
        return Ok(());
      }

      // Submit poll operation to wait for signalfd to become readable
      let poll_entry = PollAdd::new(sfd as i32, libc::POLLIN as u32).build();

      // Store signalfd for cleanup
      self.signal_fds.insert(id, sfd as i32);

      // SAFETY: pushing to io_uring submission queue
      unsafe { self.ring().push(poll_entry, id | SIGNAL_POLL_FLAG) }.map_err(
        |_| {
          self.signal_fds.remove(&id);
          // SAFETY: closing valid fd
          unsafe { libc::close(sfd as i32) };
          io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
        },
      )?;

      return Ok(());
    }

    // Handle Watch via io_uring poll on inotify fd
    #[cfg(unix)]
    if let Op::Watch { path, mask } = &op {
      use crate::api::ops::WatchMask;
      use std::ffi::CStr;

      // Create inotify fd (non-blocking for poll)
      let inotify_fd =
        match syscall!(inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC)) {
          Ok(fd) => fd,
          Err(e) => {
            let errno = e.raw_os_error().unwrap_or(libc::EIO);
            self
              .immediate
              .push(ImmediateCompletion { id, result: -(errno as isize) });
            return Ok(());
          }
        };

      // SAFETY: path is a valid null-terminated C string from the Op
      let path_cstr = unsafe { CStr::from_ptr(*path) };
      let inotify_mask = WatchMask::from_bits(*mask).to_inotify_mask();

      // Add watch
      if let Err(e) = syscall!(inotify_add_watch(
        inotify_fd,
        path_cstr.as_ptr(),
        inotify_mask
      )) {
        // SAFETY: inotify_fd is a valid fd we just created
        unsafe { libc::close(inotify_fd) };
        let errno = e.raw_os_error().unwrap_or(libc::EIO);
        self
          .immediate
          .push(ImmediateCompletion { id, result: -(errno as isize) });
        return Ok(());
      }

      // Track the inotify fd for later cleanup
      self.watch_fds.insert(id, inotify_fd);

      // Submit poll operation to wait for inotify fd to become readable
      let poll_entry = PollAdd::new(inotify_fd, libc::POLLIN as u32).build();

      // Use flagged user_data so we can identify this as a watch completion
      // SAFETY: poll_entry is a valid SQE, ring is initialized
      unsafe { self.ring().push(poll_entry, id | WATCH_POLL_FLAG) }.map_err(
        |_| {
          // Clean up on failure
          self.watch_fds.remove(&id);
          // SAFETY: inotify_fd is a valid fd we just created
          unsafe { libc::close(inotify_fd) };
          io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
        },
      )?;

      return Ok(());
    }

    // Handle timeout wrapper with linked operations
    #[cfg(target_os = "linux")]
    if let Op::Timeout { inner, timespec, .. } = &op {
      // Create the inner operation's entry with IO_LINK flag
      let inner_entry = create_io_uring_entry(inner);

      // Push inner op with IO_LINK flag
      // SAFETY: entry is valid, id is used as user_data
      unsafe {
        self.ring().push_with_flags(
          inner_entry,
          id,
          lio_uring::SqeFlags::IO_LINK,
        )
      }
      .map_err(|_| {
        io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
      })?;

      // Create and push the linked timeout
      // The timeout uses a flagged user_data so we can filter its completion
      let timeout_entry =
        operation::LinkTimeout::new(*timespec as *const _).build();

      // SAFETY: entry is valid, flagged id is used as user_data
      unsafe { self.ring().push(timeout_entry, id | TIMEOUT_LINK_FLAG) }
        .map_err(|_| {
          io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
        })?;

      return Ok(());
    }

    let entry = create_io_uring_entry(&op);

    // Push to submission queue without syscall
    // SAFETY: entry is a valid SQE created from op, id is used as user_data
    unsafe { self.ring().push(entry, id) }.map_err(|_| {
      io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
    })?;

    Ok(())
  }

  fn flush(&mut self) -> io::Result<usize> {
    // Submit all queued operations with a single syscall
    let submitted = self.ring().submit()?;
    Ok(submitted)
  }

  fn wait_timeout(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.poll_inner(timeout)
  }

  fn arm_timer(&mut self, duration: Duration) -> io::Result<()> {
    // If already armed, cancel first
    if self.wheel_timer_armed {
      self.disarm_timer()?;
    }

    // Allocate or reuse timespec storage
    let ts = self.wheel_timer_ts.get_or_insert_with(|| {
      Box::new(libc::timespec { tv_sec: 0, tv_nsec: 0 })
    });

    // Set the duration
    ts.tv_sec = duration.as_secs() as libc::time_t;
    ts.tv_nsec = duration.subsec_nanos() as libc::c_long;

    // Ensure at least 1ns
    if ts.tv_sec == 0 && ts.tv_nsec == 0 {
      ts.tv_nsec = 1;
    }

    let ts_ptr = ts.as_ref() as *const libc::timespec;

    // Create timeout entry
    let entry = Timeout::new(ts_ptr as *const _).build();

    // Push to ring
    // SAFETY: entry is valid, ts is kept alive in wheel_timer_ts
    unsafe { self.ring().push(entry, WHEEL_TIMER_KEY) }.map_err(|_| {
      io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
    })?;

    // Submit immediately so timer starts
    self.ring().submit()?;

    self.wheel_timer_armed = true;
    Ok(())
  }

  fn disarm_timer(&mut self) -> io::Result<()> {
    if !self.wheel_timer_armed {
      return Ok(());
    }

    // Use IORING_OP_TIMEOUT_REMOVE to cancel the timer
    let entry = TimeoutRemove::new(WHEEL_TIMER_KEY).build();

    // SAFETY: entry is valid
    unsafe { self.ring().push(entry, Self::TIMEOUT_REMOVE_KEY) }.map_err(
      |_| io::Error::new(io::ErrorKind::WouldBlock, "submission queue full"),
    )?;

    // Submit the cancellation
    self.ring().submit()?;

    self.wheel_timer_armed = false;
    Ok(())
  }

  fn cancel(&mut self, id: u64) -> io::Result<()> {
    // Clean up any signal fd associated with this operation
    if let Some(signal_fd) = self.signal_fds.remove(&id) {
      // SAFETY: signal_fd is a valid fd that we own
      unsafe { libc::close(signal_fd) };
    }

    // Clean up any watch fd associated with this operation
    if let Some(watch_fd) = self.watch_fds.remove(&id) {
      // SAFETY: watch_fd is a valid fd that we own
      unsafe { libc::close(watch_fd) };
    }

    // Use AsyncCancel to cancel the operation with the given user_data
    let entry = AsyncCancel::new(id).build();

    // Push cancellation request
    // Use a special internal key for the cancel completion
    const CANCEL_KEY: u64 = u64::MAX - 2;
    // SAFETY: entry is a valid SQE, ring is initialized
    unsafe { self.ring().push(entry, CANCEL_KEY) }.map_err(|_| {
      io::Error::new(io::ErrorKind::WouldBlock, "submission queue full")
    })?;

    // Submit immediately
    self.ring().submit()?;

    Ok(())
  }
}

#[cfg(test)]
crate::test_io_backend!(IoUring::new());
