//! Windows I/O Completion Ports (IOCP) backend for lio.
//!
//! This module provides the IOCP backend implementation, which is the native
//! high-performance async I/O mechanism on Windows.
//!
//! # Architecture
//!
//! IOCP is a completion-based model (like io_uring), not readiness-based (like epoll).
//! Operations are submitted with an OVERLAPPED structure and complete asynchronously.
//!
//! # Operation Categories
//!
//! - **Native IOCP**: Read, Write, ReadAt, WriteAt, Send, Recv, Accept, Connect
//!   - Use OVERLAPPED for async completion
//! - **Blocking**: Socket, Bind, Listen, Close, Fsync, Truncate, Shutdown, OpenAt, LinkAt, SymlinkAt, Nop
//!   - Execute synchronously in push(), complete immediately
//! - **Timer**: Timeout
//!   - Use CreateTimerQueueTimer, post to IOCP on expiry

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
  CloseHandle, ERROR_IO_PENDING, FALSE, GetLastError, HANDLE,
  INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
  AF_INET, AF_INET6, INVALID_SOCKET, SD_BOTH, SD_RECEIVE, SD_SEND,
  SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, SOCK_DGRAM, SOCK_STREAM,
  SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR,
  SOL_SOCKET, WSA_IO_PENDING, WSABUF, WSADATA, WSAGetLastError, WSARecv,
  WSASend, WSASocketW, WSAStartup, bind, closesocket, connect, getaddrinfo,
  listen, setsockopt, shutdown, socket,
};
use windows_sys::Win32::Storage::FileSystem::{
  CreateFileW, FILE_BEGIN, FlushFileBuffers, OPEN_EXISTING, ReadFile,
  SetEndOfFile, SetFilePointerEx, WriteFile,
};
use windows_sys::Win32::System::IO::{
  CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
  PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::{
  CreateTimerQueueTimer, DeleteTimerQueueTimer, WT_EXECUTEONLYONCE,
};

use crate::backends::{IoBackend, OpCompleted};
use crate::op::{Op, OpBuf, RawBuf};

/// Marker value for wake-up notifications (not a real operation).
const NOTIFY_KEY: usize = usize::MAX;

/// Marker value for timer completions.
const TIMER_KEY: usize = usize::MAX - 1;

/// State for an in-flight IOCP operation.
///
/// This is heap-allocated to ensure the OVERLAPPED pointer remains stable
/// for the duration of the async operation.
#[repr(C)]
struct IocpOpState {
  /// Must be first field for pointer casting to work
  overlapped: OVERLAPPED,
  /// The operation ID for completion mapping
  op_id: u64,
}

impl IocpOpState {
  fn new(op_id: u64) -> Box<Self> {
    Box::new(Self { overlapped: unsafe { std::mem::zeroed() }, op_id })
  }

  /// Get the OVERLAPPED pointer for this state.
  fn overlapped_ptr(&mut self) -> *mut OVERLAPPED {
    &mut self.overlapped as *mut _
  }
}

/// Immediate completion for operations that don't use IOCP.
struct ImmediateCompletion {
  op_id: u64,
  result: isize,
}

/// Timer state for Timeout operations.
struct TimerState {
  timer_handle: HANDLE,
  op_id: u64,
}

/// Windows I/O Completion Ports backend.
///
/// IOCP is the native asynchronous I/O mechanism on Windows, providing
/// high-performance I/O multiplexing through kernel-managed completion ports.
///
/// # Example
///
/// ```rust,ignore
/// let mut backend = Iocp::default();
/// backend.init(1024)?;
///
/// backend.push(id, Op::Nop)?;
/// backend.flush()?;
///
/// let completions = backend.wait_timeout(Some(Duration::ZERO))?;
/// ```
#[derive(Default)]
pub struct Iocp {
  /// Handle to the I/O completion port
  port: Option<HANDLE>,

  /// Handles that have been associated with the completion port.
  /// We track these to avoid double-association.
  associated_handles: HashSet<isize>,

  /// Mapping from OVERLAPPED pointer address to operation state.
  /// Used to identify which operation completed and clean up.
  active_ops: HashMap<usize, Box<IocpOpState>>,

  /// Stored operations for buffer ownership (op_id -> Op).
  /// We keep the Op alive until completion to maintain buffer validity.
  stored_ops: HashMap<u64, Op>,

  /// Active timers (op_id -> timer state).
  active_timers: HashMap<u64, TimerState>,

  /// Results from immediate/blocking operations.
  /// These are drained during poll/wait.
  immediate: Vec<ImmediateCompletion>,

  /// Reusable buffer for completed operations.
  completed: Vec<OpCompleted>,

  /// WSA initialization flag
  wsa_initialized: bool,
}

impl Iocp {
  /// Creates a new uninitialized IOCP backend.
  ///
  /// Call [`init`](Self::init) before using.
  pub fn new() -> Self {
    Self::default()
  }

  /// Returns the completion port handle, panicking if not initialized.
  #[inline]
  fn port(&self) -> HANDLE {
    self.port.expect("Iocp not initialized - call init() first")
  }

  /// Ensure WSA is initialized (for socket operations).
  fn ensure_wsa(&mut self) -> io::Result<()> {
    if !self.wsa_initialized {
      let mut wsa_data: WSADATA = unsafe { std::mem::zeroed() };
      let result = unsafe { WSAStartup(0x0202, &mut wsa_data) };
      if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
      }
      self.wsa_initialized = true;
    }
    Ok(())
  }

  /// Associates a handle with the completion port if not already associated.
  fn ensure_associated(&mut self, handle: HANDLE) -> io::Result<()> {
    let handle_val = handle as isize;

    if self.associated_handles.contains(&handle_val) {
      return Ok(());
    }

    // Associate the handle with the completion port.
    // We use 0 as the completion key since we identify operations via OVERLAPPED pointer.
    let result = unsafe {
      CreateIoCompletionPort(
        handle,
        self.port(),
        0, // Completion key (we use OVERLAPPED pointer instead)
        0, // Number of concurrent threads (0 = system default)
      )
    };

    if result == 0 {
      return Err(io::Error::last_os_error());
    }

    self.associated_handles.insert(handle_val);
    Ok(())
  }

  /// Convert a Windows error code to lio convention (negative errno).
  #[inline]
  fn error_result(error: u32) -> isize {
    -(error as isize)
  }

  /// Run a blocking operation and return the result.
  fn run_blocking(op: &Op) -> isize {
    match op {
      Op::Socket { domain, ty, proto } => {
        let sock = unsafe { socket(*domain, *ty, *proto) };
        if sock == INVALID_SOCKET {
          Self::error_result(unsafe { WSAGetLastError() } as u32)
        } else {
          sock as isize
        }
      }

      Op::Bind { fd, addr } => {
        let handle = fd.as_raw_handle() as SOCKET;
        let (addr_ptr, addr_len) = sockaddr_storage_to_ptr(addr);
        let result = unsafe { bind(handle, addr_ptr, addr_len) };
        if result == SOCKET_ERROR {
          Self::error_result(unsafe { WSAGetLastError() } as u32)
        } else {
          0
        }
      }

      Op::Listen { fd, backlog } => {
        let handle = fd.as_raw_handle() as SOCKET;
        let result = unsafe { listen(handle, *backlog) };
        if result == SOCKET_ERROR {
          Self::error_result(unsafe { WSAGetLastError() } as u32)
        } else {
          0
        }
      }

      Op::Shutdown { fd, how } => {
        let handle = fd.as_raw_handle() as SOCKET;
        // Map libc shutdown constants to Windows
        let win_how = match *how {
          0 => SD_RECEIVE, // SHUT_RD
          1 => SD_SEND,    // SHUT_WR
          _ => SD_BOTH,    // SHUT_RDWR
        };
        let result = unsafe { shutdown(handle, win_how) };
        if result == SOCKET_ERROR {
          Self::error_result(unsafe { WSAGetLastError() } as u32)
        } else {
          0
        }
      }

      Op::Close { handle, is_socket } => {
        let result = if *is_socket {
          unsafe { closesocket(*handle as SOCKET) }
        } else {
          let r = unsafe { CloseHandle(*handle as HANDLE) };
          if r == 0 { -1 } else { 0 }
        };
        if result != 0 {
          if *is_socket {
            Self::error_result(unsafe { WSAGetLastError() } as u32)
          } else {
            Self::error_result(unsafe { GetLastError() })
          }
        } else {
          0
        }
      }

      Op::Fsync { fd } => {
        let handle = fd.as_raw_handle() as HANDLE;
        let result = unsafe { FlushFileBuffers(handle) };
        if result == 0 {
          Self::error_result(unsafe { GetLastError() })
        } else {
          0
        }
      }

      Op::Truncate { fd, size } => {
        let handle = fd.as_raw_handle() as HANDLE;
        // Move file pointer to the desired position
        let mut new_pos: i64 = 0;
        let result = unsafe {
          SetFilePointerEx(handle, *size as i64, &mut new_pos, FILE_BEGIN)
        };
        if result == 0 {
          return Self::error_result(unsafe { GetLastError() });
        }
        // Set end of file at current position
        let result = unsafe { SetEndOfFile(handle) };
        if result == 0 {
          Self::error_result(unsafe { GetLastError() })
        } else {
          0
        }
      }

      Op::OpenAt { dir_fd: _, path, flags } => {
        // Windows doesn't have openat() - we require absolute paths
        // For now, just use CreateFileW with the path
        // Note: This is a simplified implementation
        let _ = flags; // TODO: Map flags properly
        let handle = unsafe {
          CreateFileW(
            *path as *const u16,
            0x80000000 | 0x40000000, // GENERIC_READ | GENERIC_WRITE
            0x00000001 | 0x00000002, // FILE_SHARE_READ | FILE_SHARE_WRITE
            ptr::null(),
            OPEN_EXISTING,
            0x80, // FILE_ATTRIBUTE_NORMAL
            0,
          )
        };
        if handle == INVALID_HANDLE_VALUE {
          Self::error_result(unsafe { GetLastError() })
        } else {
          handle as isize
        }
      }

      Op::LinkAt { .. } => {
        // Windows doesn't support linkat directly
        // Would need CreateHardLinkW but lacks dir_fd semantics
        Self::error_result(windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED)
      }

      Op::SymlinkAt { .. } => {
        // Windows symlinks require elevated privileges
        // Would need CreateSymbolicLinkW
        Self::error_result(windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED)
      }

      Op::Nop => 0,

      // These should not be called via run_blocking
      _ => {
        Self::error_result(windows_sys::Win32::Foundation::ERROR_NOT_SUPPORTED)
      }
    }
  }

  /// Start a read operation using ReadFile with OVERLAPPED.
  fn start_read(&mut self, id: u64, op: Op) -> io::Result<()> {
    let (fd, buffer) = match &op {
      Op::Read { fd, buffer } => (fd.as_raw_handle(), buffer),
      Op::ReadAt { fd, offset, buffer } => {
        // For ReadAt, we need to set the offset in OVERLAPPED
        let handle = fd.as_raw_handle() as HANDLE;
        self.ensure_associated(handle)?;

        let mut state = IocpOpState::new(id);
        state.overlapped.Anonymous.Anonymous.Offset =
          (*offset as u64 & 0xFFFFFFFF) as u32;
        state.overlapped.Anonymous.Anonymous.OffsetHigh =
          ((*offset as u64) >> 32) as u32;

        let RawBuf { ptr, len } = unsafe { buffer.peek::<RawBuf>() };
        let overlapped_ptr = state.overlapped_ptr();
        let mut bytes_read: u32 = 0;

        let result = unsafe {
          ReadFile(
            handle,
            ptr as *mut _,
            len as u32,
            &mut bytes_read,
            overlapped_ptr,
          )
        };

        return self.handle_async_result(result, id, state, op);
      }
      _ => unreachable!(),
    };

    let handle = fd as HANDLE;
    self.ensure_associated(handle)?;

    let mut state = IocpOpState::new(id);
    let RawBuf { ptr, len } = unsafe { buffer.peek::<RawBuf>() };
    let overlapped_ptr = state.overlapped_ptr();
    let mut bytes_read: u32 = 0;

    let result = unsafe {
      ReadFile(
        handle,
        ptr as *mut _,
        len as u32,
        &mut bytes_read,
        overlapped_ptr,
      )
    };

    self.handle_async_result(result, id, state, op)
  }

  /// Start a write operation using WriteFile with OVERLAPPED.
  fn start_write(&mut self, id: u64, op: Op) -> io::Result<()> {
    let (fd, buffer, offset) = match &op {
      Op::Write { fd, buffer } => (fd.as_raw_handle(), buffer, None),
      Op::WriteAt { fd, offset, buffer } => {
        (fd.as_raw_handle(), buffer, Some(*offset))
      }
      _ => unreachable!(),
    };

    let handle = fd as HANDLE;
    self.ensure_associated(handle)?;

    let mut state = IocpOpState::new(id);

    // Set offset if this is WriteAt
    if let Some(off) = offset {
      state.overlapped.Anonymous.Anonymous.Offset =
        (off as u64 & 0xFFFFFFFF) as u32;
      state.overlapped.Anonymous.Anonymous.OffsetHigh =
        ((off as u64) >> 32) as u32;
    }

    let (ptr, len) = unsafe { buffer.peek::<(*mut u8, usize)>() };
    let overlapped_ptr = state.overlapped_ptr();
    let mut bytes_written: u32 = 0;

    let result = unsafe {
      WriteFile(
        handle,
        ptr as *const _,
        len as u32,
        &mut bytes_written,
        overlapped_ptr,
      )
    };

    self.handle_async_result(result, id, state, op)
  }

  /// Start a WSASend operation.
  fn start_wsa_send(&mut self, id: u64, op: Op) -> io::Result<()> {
    let (fd, buffer, flags) = match &op {
      Op::Send { fd, buffer, flags } => (fd.as_raw_handle(), buffer, *flags),
      _ => unreachable!(),
    };

    let socket = fd as SOCKET;
    // Note: Sockets should already be associated when created
    // but ensure association anyway
    self.ensure_associated(fd as HANDLE)?;

    let mut state = IocpOpState::new(id);
    let (ptr, len) = unsafe { buffer.peek::<(*mut u8, usize)>() };

    let mut wsa_buf = WSABUF { len: len as u32, buf: ptr as *mut _ };
    let mut bytes_sent: u32 = 0;
    let overlapped_ptr = state.overlapped_ptr();

    let result = unsafe {
      WSASend(
        socket,
        &mut wsa_buf,
        1,
        &mut bytes_sent,
        flags as u32,
        overlapped_ptr,
        None,
      )
    };

    self.handle_wsa_result(result, id, state, op)
  }

  /// Start a WSARecv operation.
  fn start_wsa_recv(&mut self, id: u64, op: Op) -> io::Result<()> {
    let (fd, buffer, flags) = match &op {
      Op::Recv { fd, buffer, flags } => (fd.as_raw_handle(), buffer, *flags),
      _ => unreachable!(),
    };

    let socket = fd as SOCKET;
    self.ensure_associated(fd as HANDLE)?;

    let mut state = IocpOpState::new(id);
    let (ptr, len) = unsafe { buffer.peek::<(*mut u8, usize)>() };

    let mut wsa_buf = WSABUF { len: len as u32, buf: ptr as *mut _ };
    let mut bytes_received: u32 = 0;
    let mut recv_flags: u32 = flags as u32;
    let overlapped_ptr = state.overlapped_ptr();

    let result = unsafe {
      WSARecv(
        socket,
        &mut wsa_buf,
        1,
        &mut bytes_received,
        &mut recv_flags,
        overlapped_ptr,
        None,
      )
    };

    self.handle_wsa_result(result, id, state, op)
  }

  /// Start an accept operation.
  ///
  /// Note: AcceptEx requires loading the function pointer via WSAIoctl.
  /// For simplicity, this implementation uses a blocking accept for now.
  fn start_accept(&mut self, id: u64, op: Op) -> io::Result<()> {
    let (fd, addr, len) = match &op {
      Op::Accept { fd, addr, len } => (fd.as_raw_handle(), *addr, *len),
      _ => unreachable!(),
    };

    let socket = fd as SOCKET;

    // Use blocking accept for now (AcceptEx requires more setup)
    let result = unsafe {
      windows_sys::Win32::Networking::WinSock::accept(
        socket,
        addr as *mut SOCKADDR,
        len as *mut i32,
      )
    };

    let completion_result = if result == INVALID_SOCKET {
      Self::error_result(unsafe { WSAGetLastError() } as u32)
    } else {
      result as isize
    };

    self
      .immediate
      .push(ImmediateCompletion { op_id: id, result: completion_result });

    Ok(())
  }

  /// Start a connect operation.
  ///
  /// Note: ConnectEx requires loading the function pointer and pre-binding.
  /// For simplicity, this implementation uses blocking connect for now.
  fn start_connect(&mut self, id: u64, op: Op) -> io::Result<()> {
    let (fd, addr, len, _connect_called) = match &op {
      Op::Connect { fd, addr, len, connect_called } => {
        (fd.as_raw_handle(), addr, *len, *connect_called)
      }
      _ => unreachable!(),
    };

    let socket = fd as SOCKET;
    let (addr_ptr, _) = sockaddr_storage_to_ptr(addr);

    let result = unsafe { connect(socket, addr_ptr, len as i32) };

    let completion_result = if result == SOCKET_ERROR {
      let error = unsafe { WSAGetLastError() };
      // WSAEWOULDBLOCK means connect is in progress for non-blocking sockets
      if error == windows_sys::Win32::Networking::WinSock::WSAEWOULDBLOCK as i32
      {
        // For true async, we'd need to poll for completion
        // For now, return in-progress as error
        Self::error_result(error as u32)
      } else {
        Self::error_result(error as u32)
      }
    } else {
      0
    };

    self
      .immediate
      .push(ImmediateCompletion { op_id: id, result: completion_result });

    Ok(())
  }

  /// Start a timer operation using the timer queue.
  fn start_timer(&mut self, id: u64, op: Op) -> io::Result<()> {
    let duration = match &op {
      Op::Timeout { duration, .. } => *duration,
      _ => unreachable!(),
    };

    let due_time = duration.as_millis() as u32;
    let mut timer_handle: HANDLE = 0;

    // Create a timer that posts to our IOCP on expiry
    let port = self.port();

    // We need to pass the op_id to the callback
    // Store the id in a box that will be passed as context
    let context =
      Box::into_raw(Box::new(TimerCallbackContext { port, op_id: id }));

    let result = unsafe {
      CreateTimerQueueTimer(
        &mut timer_handle,
        0, // Use default timer queue
        Some(timer_callback),
        context as *const _,
        due_time,
        0, // No repeat
        WT_EXECUTEONLYONCE,
      )
    };

    if result == 0 {
      // Clean up context on failure
      let _ = unsafe { Box::from_raw(context) };
      return Err(io::Error::last_os_error());
    }

    self.active_timers.insert(id, TimerState { timer_handle, op_id: id });
    self.stored_ops.insert(id, op);

    Ok(())
  }

  /// Handle the result of an async Windows file I/O operation.
  fn handle_async_result(
    &mut self,
    result: i32,
    id: u64,
    state: Box<IocpOpState>,
    op: Op,
  ) -> io::Result<()> {
    let error = unsafe { GetLastError() };

    if result != 0 {
      // Operation completed synchronously
      // The completion will still be posted to the IOCP
      let addr = &state.overlapped as *const _ as usize;
      self.active_ops.insert(addr, state);
      self.stored_ops.insert(id, op);
      Ok(())
    } else if error == ERROR_IO_PENDING {
      // Operation is pending asynchronously
      let addr = &state.overlapped as *const _ as usize;
      self.active_ops.insert(addr, state);
      self.stored_ops.insert(id, op);
      Ok(())
    } else {
      // Error - complete immediately
      self.immediate.push(ImmediateCompletion {
        op_id: id,
        result: Self::error_result(error),
      });
      Ok(())
    }
  }

  /// Handle the result of an async WSA operation.
  fn handle_wsa_result(
    &mut self,
    result: i32,
    id: u64,
    state: Box<IocpOpState>,
    op: Op,
  ) -> io::Result<()> {
    if result == 0 {
      // Operation completed synchronously or is pending
      let addr = &state.overlapped as *const _ as usize;
      self.active_ops.insert(addr, state);
      self.stored_ops.insert(id, op);
      Ok(())
    } else {
      let error = unsafe { WSAGetLastError() };
      if error == WSA_IO_PENDING as i32 {
        // Operation is pending
        let addr = &state.overlapped as *const _ as usize;
        self.active_ops.insert(addr, state);
        self.stored_ops.insert(id, op);
        Ok(())
      } else {
        // Error - complete immediately
        self.immediate.push(ImmediateCompletion {
          op_id: id,
          result: Self::error_result(error as u32),
        });
        Ok(())
      }
    }
  }

  /// Poll for completions with optional timeout.
  fn poll_inner(
    &mut self,
    timeout_duration: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.completed.clear();

    // First, drain any immediate completions
    for result in self.immediate.drain(..) {
      self.completed.push(OpCompleted::new(result.op_id, result.result));
    }

    // Calculate timeout for GetQueuedCompletionStatus
    let timeout = match timeout_duration {
      None => u32::MAX, // INFINITE
      Some(d) => d.as_millis().min(u32::MAX as u128 - 1) as u32,
    };

    let is_blocking = timeout_duration != Some(Duration::ZERO);

    loop {
      let mut bytes_transferred: u32 = 0;
      let mut completion_key: usize = 0;
      let mut overlapped_ptr: *mut OVERLAPPED = ptr::null_mut();

      let success = unsafe {
        GetQueuedCompletionStatus(
          self.port(),
          &mut bytes_transferred,
          &mut completion_key,
          &mut overlapped_ptr,
          if self.completed.is_empty() { timeout } else { 0 },
        )
      };

      // Check for timeout or error with no completion
      if success == FALSE && overlapped_ptr.is_null() {
        let error = unsafe { GetLastError() };
        if error == WAIT_TIMEOUT {
          break;
        }
        return Err(io::Error::from_raw_os_error(error as i32));
      }

      // Handle wake-up notifications
      if completion_key == NOTIFY_KEY {
        if is_blocking && !self.completed.is_empty() {
          break;
        }
        continue;
      }

      // Handle timer completions
      if completion_key == TIMER_KEY {
        let op_id = bytes_transferred as u64;
        if let Some(timer_state) = self.active_timers.remove(&op_id) {
          // Delete the timer
          unsafe {
            DeleteTimerQueueTimer(0, timer_state.timer_handle, 0);
          }
          // Remove the stored op
          self.stored_ops.remove(&op_id);
          self.completed.push(OpCompleted::new(op_id, 0));
        }
        continue;
      }

      // Look up the operation from the OVERLAPPED pointer
      let overlapped_addr = overlapped_ptr as usize;
      if let Some(state) = self.active_ops.remove(&overlapped_addr) {
        let op_id = state.op_id;

        // Remove stored op (releases buffer ownership)
        self.stored_ops.remove(&op_id);

        let result = if success != FALSE {
          bytes_transferred as isize
        } else {
          let error = unsafe { GetLastError() };
          Self::error_result(error)
        };

        self.completed.push(OpCompleted::new(op_id, result));
      }

      // For blocking wait, try to get more completions without blocking
      if is_blocking && !self.completed.is_empty() {
        break;
      }

      // For non-blocking poll, only process one then check for more
      if !is_blocking {
        break;
      }
    }

    // Drain any additional ready completions
    if !self.completed.is_empty() {
      loop {
        let mut bytes_transferred: u32 = 0;
        let mut completion_key: usize = 0;
        let mut overlapped_ptr: *mut OVERLAPPED = ptr::null_mut();

        let success = unsafe {
          GetQueuedCompletionStatus(
            self.port(),
            &mut bytes_transferred,
            &mut completion_key,
            &mut overlapped_ptr,
            0, // Non-blocking
          )
        };

        if success == FALSE && overlapped_ptr.is_null() {
          break;
        }

        if completion_key == NOTIFY_KEY {
          continue;
        }

        if completion_key == TIMER_KEY {
          let op_id = bytes_transferred as u64;
          if let Some(timer_state) = self.active_timers.remove(&op_id) {
            unsafe {
              DeleteTimerQueueTimer(0, timer_state.timer_handle, 0);
            }
            self.stored_ops.remove(&op_id);
            self.completed.push(OpCompleted::new(op_id, 0));
          }
          continue;
        }

        let overlapped_addr = overlapped_ptr as usize;
        if let Some(state) = self.active_ops.remove(&overlapped_addr) {
          let op_id = state.op_id;
          self.stored_ops.remove(&op_id);

          let result = if success != FALSE {
            bytes_transferred as isize
          } else {
            let error = unsafe { GetLastError() };
            Self::error_result(error)
          };

          self.completed.push(OpCompleted::new(op_id, result));
        }
      }
    }

    Ok(&self.completed)
  }
}

impl Drop for Iocp {
  fn drop(&mut self) {
    // Clean up any active timers
    for (_, timer_state) in self.active_timers.drain() {
      unsafe {
        DeleteTimerQueueTimer(
          0,
          timer_state.timer_handle,
          INVALID_HANDLE_VALUE,
        );
      }
    }

    // Close the completion port
    if let Some(port) = self.port {
      unsafe {
        CloseHandle(port);
      }
    }
  }
}

impl IoBackend for Iocp {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    // Initialize WSA
    self.ensure_wsa()?;

    // Create the I/O completion port
    let port = unsafe {
      CreateIoCompletionPort(
        INVALID_HANDLE_VALUE,
        0, // No existing port
        0, // Completion key (unused for creation)
        0, // Concurrent threads (0 = system decides)
      )
    };

    if port == 0 {
      return Err(io::Error::last_os_error());
    }

    self.port = Some(port);
    self.associated_handles = HashSet::with_capacity(cap);
    self.active_ops = HashMap::with_capacity(cap);
    self.stored_ops = HashMap::with_capacity(cap);
    self.active_timers = HashMap::with_capacity(16);
    self.immediate = Vec::with_capacity(64);
    self.completed = Vec::with_capacity(cap.min(256));

    Ok(())
  }

  fn push(&mut self, id: u64, op: Op) -> io::Result<()> {
    match &op {
      // Native IOCP operations
      Op::Read { .. } | Op::ReadAt { .. } => self.start_read(id, op),
      Op::Write { .. } | Op::WriteAt { .. } => self.start_write(id, op),
      Op::Send { .. } => self.start_wsa_send(id, op),
      Op::Recv { .. } => self.start_wsa_recv(id, op),
      Op::Accept { .. } => self.start_accept(id, op),
      Op::Connect { .. } => self.start_connect(id, op),
      Op::Timeout { .. } => self.start_timer(id, op),

      // Blocking operations
      Op::Socket { .. }
      | Op::Bind { .. }
      | Op::Listen { .. }
      | Op::Close { .. }
      | Op::Fsync { .. }
      | Op::Truncate { .. }
      | Op::Shutdown { .. }
      | Op::OpenAt { .. }
      | Op::LinkAt { .. }
      | Op::SymlinkAt { .. }
      | Op::Nop => {
        let result = Self::run_blocking(&op);
        self.immediate.push(ImmediateCompletion { op_id: id, result });
        Ok(())
      }
    }
  }

  fn flush(&mut self) -> io::Result<usize> {
    // IOCP operations are submitted immediately in push()
    Ok(0)
  }

  fn wait_timeout(
    &mut self,
    timeout: Option<Duration>,
  ) -> io::Result<&[OpCompleted]> {
    self.poll_inner(timeout)
  }
}

/// Wake up a blocked wait() call.
impl Iocp {
  pub fn notify(&self) -> io::Result<()> {
    let success = unsafe {
      PostQueuedCompletionStatus(self.port(), 0, NOTIFY_KEY, ptr::null_mut())
    };

    if success == FALSE { Err(io::Error::last_os_error()) } else { Ok(()) }
  }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helper types and functions
// ═══════════════════════════════════════════════════════════════════════════════

/// Context passed to timer callback.
#[repr(C)]
struct TimerCallbackContext {
  port: HANDLE,
  op_id: u64,
}

/// Timer callback function - posts completion to IOCP.
unsafe extern "system" fn timer_callback(
  context: *mut std::ffi::c_void,
  _timer_or_wait_fired: u8,
) {
  let ctx = &*(context as *const TimerCallbackContext);

  // Post completion to the IOCP
  // Use TIMER_KEY as completion key and op_id as bytes_transferred
  PostQueuedCompletionStatus(
    ctx.port,
    ctx.op_id as u32,
    TIMER_KEY,
    ptr::null_mut(),
  );

  // Clean up the context
  let _ = Box::from_raw(context as *mut TimerCallbackContext);
}

/// Convert sockaddr_storage to pointer and length for Windows APIs.
fn sockaddr_storage_to_ptr(
  addr: &libc::sockaddr_storage,
) -> (*const SOCKADDR, i32) {
  // Determine the actual address length based on family
  let len = if addr.ss_family == AF_INET as u16 {
    std::mem::size_of::<SOCKADDR_IN>() as i32
  } else if addr.ss_family == AF_INET6 as u16 {
    std::mem::size_of::<SOCKADDR_IN6>() as i32
  } else {
    std::mem::size_of::<SOCKADDR_STORAGE>() as i32
  };

  (addr as *const _ as *const SOCKADDR, len)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_init() {
    let mut backend = Iocp::new();
    backend.init(64).unwrap();
  }

  #[test]
  fn test_notify() {
    let mut backend = Iocp::new();
    backend.init(64).unwrap();
    backend.notify().unwrap();
  }

  #[test]
  fn test_nop() {
    let mut backend = Iocp::new();
    backend.init(64).unwrap();

    backend.push(1, Op::Nop).unwrap();
    backend.flush().unwrap();

    let completions =
      backend.wait_timeout(Some(Duration::from_millis(100))).unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].op_id, 1);
    assert_eq!(completions[0].result, 0);
  }
}
