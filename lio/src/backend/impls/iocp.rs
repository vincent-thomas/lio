//! Windows I/O completion port (IOCP) backend.
//!
//! # Execution model
//!
//! - Socket I/O (`Read`/`Write` on sockets, `Recv`, `Send`, `Accept`, and
//!   `Connect` to IP addresses) is submitted as overlapped I/O during
//!   `flush()` and completes through the port. Each in-flight operation owns
//!   a boxed [`Overlapped`] whose first field is the `OVERLAPPED` handed to
//!   the kernel, so the completion entry leads straight back to its state.
//! - Everything else (files, path operations, socket setup) runs
//!   synchronously during `flush()`; its completion is surfaced on the next
//!   `wait()`, matching the readiness backends.
//!
//! # Errors
//!
//! Failures complete with the negated Win32/Winsock error code, which is what
//! `io::Error::from_raw_os_error` expects on Windows.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::{self, ManuallyDrop};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use bumpalo::Bump;
use windows_sys::Win32::Foundation::{
  CloseHandle, ERROR_BROKEN_PIPE, ERROR_DIRECTORY, ERROR_HANDLE_EOF,
  ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, ERROR_NOT_SUPPORTED,
  ERROR_OPERATION_ABORTED, FALSE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
  NO_ERROR, TRUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock as ws;
use windows_sys::Win32::Storage::FileSystem::{
  BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
  FILE_FLAG_OPEN_REPARSE_POINT, FILE_TYPE_UNKNOWN, FlushFileBuffers,
  GetFileInformationByHandle, GetFileType, GetFinalPathNameByHandleW, ReadFile,
  WriteFile,
};
use windows_sys::Win32::System::IO::{
  CancelIoEx, CancelSynchronousIo, CreateIoCompletionPort, GetOverlappedResult,
  GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY,
  PostQueuedCompletionStatus,
};

use crate::api::resource::Resource;
use crate::backend::impls::sockaddr::{
  SockLen, Storage, socket_addr_buf_from_storage, socket_addr_buf_to_storage,
  socket_addr_to_storage, socket_to_raw,
};
use crate::backend::native::CWD as CWD_HANDLE;
use crate::backend::op::{
  DirEntryRef, FileStat, FileType, LinkKind, MsgRecv, MsgSend, Op,
  OpaqueDropFn, OpenFlags, RawBuf, ReadDirResult, ShutdownHow, SocketAddrBuf,
  SocketAddrFamily, SpawnSpec, StatTarget, UnlinkKind,
};
use crate::backend::{IoBackend, OpCompleted};

/// Completion key for wake-up packets posted by [`Iocp::notify`].
const NOTIFY_KEY: usize = usize::MAX;

/// Completion key for results posted by blocking-pool workers; the packet's
/// "overlapped" pointer is a `Box<BlockingDone>`.
const BLOCKING_KEY: usize = usize::MAX - 1;

/// Bytes `AcceptEx` needs for each of the local and remote addresses.
const ACCEPT_ADDR_LEN: usize = mem::size_of::<Storage>() + 16;

/// How many completion entries one `GetQueuedCompletionStatusEx` call reaps.
const MAX_ENTRIES: usize = 256;

type ConnectExFn = unsafe extern "system" fn(
  ws::SOCKET,
  *const ws::SOCKADDR,
  i32,
  *const std::ffi::c_void,
  u32,
  *mut u32,
  *mut OVERLAPPED,
) -> i32;

/// How long dropping the backend waits for cancelled operations to finish.
/// Cancelled socket operations report back at once; this only bounds I/O
/// the kernel is still busy with.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// State for one in-flight overlapped operation.
///
/// `ov` must stay the first field: completion entries hand back a pointer to
/// it, which is cast back to the whole `Overlapped`.
#[repr(C)]
struct Overlapped {
  ov: OVERLAPPED,
  id: u64,
  socket: ws::SOCKET,
  /// Keeps the socket open until the kernel is done with this operation,
  /// even if the caller drops its last handle to it meanwhile.
  _resource: Resource,
  kind: AsyncKind,
  bufs: Vec<ws::WSABUF>,
  flags: u32,
  addr: Storage,
  addr_len: i32,
}

// Always stored inside a boxed `Overlapped`, so the variant size is moot.
#[allow(clippy::large_enum_variant)]
enum AsyncKind {
  /// Completes with the number of bytes transferred.
  Transfer,
  /// Like `Transfer`, and writes the sender address to `out`.
  RecvFrom {
    out: NonNull<SocketAddrBuf>,
  },
  Accept {
    accepted: ws::SOCKET,
    out: NonNull<SocketAddrBuf>,
    buf: [u8; 2 * ACCEPT_ADDR_LEN],
  },
  Connect,
}

impl Overlapped {
  fn new(id: u64, resource: &Resource, kind: AsyncKind) -> Box<Self> {
    Box::new(Self {
      // SAFETY: `OVERLAPPED` is plain old data; all-zero is its initial state.
      ov: unsafe { mem::zeroed() },
      id,
      socket: socket_of(resource),
      _resource: resource.clone(),
      kind,
      bufs: Vec::new(),
      flags: 0,
      // SAFETY: plain old data.
      addr: unsafe { mem::zeroed() },
      addr_len: mem::size_of::<Storage>() as i32,
    })
  }
}

impl Drop for Overlapped {
  fn drop(&mut self) {
    if let AsyncKind::Accept { accepted, .. } = self.kind
      && accepted != ws::INVALID_SOCKET
    {
      // SAFETY: the accept socket is owned by this state until handed out.
      unsafe { ws::closesocket(accepted) };
    }
  }
}

/// An operation with no overlapped form, run on a pool worker.
struct BlockingJob {
  id: u64,
  op: Op,
}

// SAFETY: the op's raw pointers refer to op-model storage that stays alive
// until its completion is reaped, and `Iocp::drop` waits for every worker
// before the op models can be freed (`Lio` drops its backend first).
unsafe impl Send for BlockingJob {}

/// A worker's result, posted back through the completion port.
struct BlockingDone {
  id: u64,
  result: isize,
}

/// Threads that run blocking operations (files, pipes, path operations,
/// process spawning) so they never stall the event loop. Results come back
/// through the completion port like any other completion.
struct BlockingPool {
  jobs: Option<mpsc::Sender<BlockingJob>>,
  workers: Vec<thread::JoinHandle<()>>,
  shutting_down: Arc<AtomicBool>,
}

impl BlockingPool {
  fn new(port: HANDLE) -> io::Result<Self> {
    let (tx, rx) = mpsc::channel::<BlockingJob>();
    let rx = Arc::new(Mutex::new(rx));
    let shutting_down = Arc::new(AtomicBool::new(false));
    let threads =
      thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
    let port = port as usize;
    let workers = (0..threads)
      .map(|i| {
        let rx = Arc::clone(&rx);
        let shutting_down = Arc::clone(&shutting_down);
        thread::Builder::new()
          .name(format!("lio-iocp-blocking-{i}"))
          .spawn(move || worker_loop(port as HANDLE, &rx, &shutting_down))
      })
      .collect::<io::Result<Vec<_>>>()?;
    Ok(Self { jobs: Some(tx), workers, shutting_down })
  }

  /// Stops the workers, cancelling any blocked synchronous I/O, and waits
  /// for them so no worker can touch op memory afterwards.
  fn shutdown(&mut self) {
    self.shutting_down.store(true, Ordering::SeqCst);
    self.jobs = None;
    while self.workers.iter().any(|w| !w.is_finished()) {
      for worker in &self.workers {
        if !worker.is_finished() {
          // SAFETY: the thread handle is alive while we hold its JoinHandle;
          // cancelling with no I/O pending is harmless.
          unsafe { CancelSynchronousIo(worker.as_raw_handle()) };
        }
      }
      thread::sleep(Duration::from_millis(1));
    }
    for worker in self.workers.drain(..) {
      let _ = worker.join();
    }
  }
}

fn worker_loop(
  port: HANDLE,
  rx: &Mutex<mpsc::Receiver<BlockingJob>>,
  shutting_down: &AtomicBool,
) {
  loop {
    let job = match rx.lock() {
      Ok(rx) => rx.recv(),
      Err(_) => return,
    };
    let Ok(BlockingJob { id, op }) = job else { return };
    if shutting_down.load(Ordering::SeqCst) {
      // The backend is going away; nobody will reap this.
      return;
    }
    let result = run_blocking(op);
    let done = Box::into_raw(Box::new(BlockingDone { id, result }));
    // SAFETY: the pointer is only passed through the port and turned back
    // into a `Box` by `reap`.
    let posted =
      unsafe { PostQueuedCompletionStatus(port, 0, BLOCKING_KEY, done.cast()) };
    if posted == FALSE {
      // SAFETY: the packet was not queued, so we still own `done`.
      drop(unsafe { Box::from_raw(done) });
    }
  }
}

struct Queued {
  id: u64,
  op: Op,
}

/// Windows I/O completion port backend.
pub struct Iocp {
  port: HANDLE,
  capacity: usize,
  backlog: Vec<Queued>,
  in_flight: HashSet<usize>,
  ready: Vec<OpCompleted>,
  entries: Vec<OVERLAPPED_ENTRY>,
  connect_ex: Option<ConnectExFn>,
  pool: Option<BlockingPool>,
  /// Jobs handed to the pool whose results have not been reaped yet.
  blocking_in_flight: usize,
}

impl Default for Iocp {
  fn default() -> Self {
    Self {
      port: ptr::null_mut(),
      capacity: 0,
      backlog: Vec::new(),
      in_flight: HashSet::new(),
      ready: Vec::new(),
      entries: Vec::new(),
      connect_ex: None,
      pool: None,
      blocking_in_flight: 0,
    }
  }
}

fn neg(code: u32) -> isize {
  -(code as isize)
}

fn last_error() -> isize {
  // SAFETY: reads the calling thread's last-error value.
  neg(unsafe { GetLastError() })
}

fn last_wsa_error() -> isize {
  // SAFETY: reads the calling thread's last Winsock error.
  -(unsafe { ws::WSAGetLastError() } as isize)
}

fn io_error(err: io::Error) -> isize {
  -(err.raw_os_error().unwrap_or(ERROR_INVALID_PARAMETER as i32) as isize)
}

fn handle_of(res: &Resource) -> HANDLE {
  res.as_raw_handle()
}

fn socket_of(res: &Resource) -> ws::SOCKET {
  res.as_raw_handle() as ws::SOCKET
}

/// Returns `Ok` when `socket` is a socket, or the Winsock error otherwise.
fn probe_socket(socket: ws::SOCKET) -> Result<(), isize> {
  let mut ty = 0i32;
  let mut len = mem::size_of::<i32>() as i32;
  // SAFETY: `ty`/`len` are valid output storage for `SO_TYPE`.
  let rc = unsafe {
    ws::getsockopt(
      socket,
      ws::SOL_SOCKET,
      ws::SO_TYPE,
      (&mut ty as *mut i32).cast(),
      &mut len,
    )
  };
  if rc == 0 { Ok(()) } else { Err(last_wsa_error()) }
}

fn ensure_wsa() -> io::Result<()> {
  static INIT: Once = Once::new();
  let mut result = 0;
  INIT.call_once(|| {
    // SAFETY: `WSADATA` is plain old data.
    let mut data: ws::WSADATA = unsafe { mem::zeroed() };
    // SAFETY: `data` is valid output storage for `WSAStartup`.
    result = unsafe { ws::WSAStartup(0x0202, &mut data) };
  });
  if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
}

impl Iocp {
  /// Creates a new uninitialized IOCP backend.
  ///
  /// Call [`init`](IoBackend::init) before using it.
  pub fn new() -> Self {
    Self::default()
  }

  /// Wakes up a thread blocked in [`wait`](IoBackend::wait).
  pub fn notify(&self) -> io::Result<()> {
    // SAFETY: posting a packet without an `OVERLAPPED` is always valid.
    let ok = unsafe {
      PostQueuedCompletionStatus(self.port, 0, NOTIFY_KEY, ptr::null())
    };
    if ok == FALSE { Err(io::Error::last_os_error()) } else { Ok(()) }
  }

  fn associate(&self, handle: HANDLE) -> Result<(), isize> {
    // SAFETY: associating any handle with our port is sound; invalid handles
    // are rejected by the kernel.
    let port = unsafe { CreateIoCompletionPort(handle, self.port, 0, 0) };
    if !port.is_null() {
      return Ok(());
    }
    // SAFETY: reads the calling thread's last-error value.
    match unsafe { GetLastError() } {
      // Already associated (handle values are reused, so this cannot be
      // tracked on our side).
      ERROR_INVALID_PARAMETER => Ok(()),
      code => Err(neg(code)),
    }
  }

  /// Hands `state` to the kernel bookkeeping; `rc` is the result of the call
  /// that started the overlapped operation.
  fn track(&mut self, state: Box<Overlapped>, started: bool) -> Option<isize> {
    if !started {
      // SAFETY: reads the calling thread's last Winsock error.
      let code = unsafe { ws::WSAGetLastError() };
      if code != ws::WSA_IO_PENDING {
        return Some(-(code as isize));
      }
    }
    self.in_flight.insert(Box::into_raw(state) as usize);
    None
  }

  fn submit(&mut self, id: u64, op: Op) -> Option<isize> {
    match op {
      Op::Nop => Some(0),
      Op::Read { fd, iovecs, iov_count, offset, flags } => {
        // SAFETY: the op model keeps `iov_count` buffers alive until done.
        let bufs =
          unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
        if offset < -1 {
          return Some(neg(ERROR_INVALID_PARAMETER));
        }
        if flags.bits() != 0 {
          return Some(neg(ERROR_NOT_SUPPORTED));
        }
        if probe_socket(socket_of(&fd)).is_ok() {
          let bufs = bufs.iter().map(|b| (b.ptr, b.len));
          return self.start_recv(id, &fd, bufs, 0, None);
        }
        if let Err(code) = check_handle(handle_of(&fd)) {
          return Some(code);
        }
        self.offload(id, Op::Read { fd, iovecs, iov_count, offset, flags })
      }
      Op::Write { fd, iovecs, iov_count, offset, flags } => {
        // SAFETY: the op model keeps `iov_count` buffers alive until done.
        let bufs =
          unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
        if offset < -1 {
          return Some(neg(ERROR_INVALID_PARAMETER));
        }
        if flags.bits() != 0 {
          return Some(neg(ERROR_NOT_SUPPORTED));
        }
        if probe_socket(socket_of(&fd)).is_ok() {
          let bufs = bufs.iter().map(|b| (b.ptr, b.len));
          return self.start_send(id, &fd, bufs, 0, None);
        }
        if let Err(code) = check_handle(handle_of(&fd)) {
          return Some(code);
        }
        self.offload(id, Op::Write { fd, iovecs, iov_count, offset, flags })
      }
      Op::Recv { fd, msg, flags } => {
        let socket = socket_of(&fd);
        if let Err(code) = probe_socket(socket) {
          return Some(code);
        }
        let bufs = recv_bufs(&msg);
        self.start_recv(id, &fd, bufs, flags.bits() as u32, msg.from)
      }
      Op::Send { fd, msg, flags } => {
        let socket = socket_of(&fd);
        if let Err(code) = probe_socket(socket) {
          return Some(code);
        }
        let bufs = send_bufs(&msg);
        self.start_send(id, &fd, bufs, flags.bits() as u32, msg.to)
      }
      Op::Accept { fd, addr } => self.start_accept(id, &fd, addr),
      Op::Connect { fd, addr } => self.start_connect(id, &fd, &addr),
      Op::Socket { domain, ty, proto } => {
        Some(match socket_to_raw(domain, ty, proto) {
          Ok((domain, ty, proto)) => {
            // SAFETY: plain socket creation; no pointers are passed.
            let socket = unsafe {
              ws::WSASocketW(
                domain,
                ty,
                proto,
                ptr::null(),
                0,
                ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT,
              )
            };
            if socket == ws::INVALID_SOCKET {
              last_wsa_error()
            } else {
              socket as isize
            }
          }
          Err(code) => -(code as isize),
        })
      }
      Op::Bind { fd, addr } => Some(match socket_addr_buf_to_storage(&addr) {
        Ok((storage, len)) => {
          // SAFETY: `storage` holds a valid address of `len` bytes.
          let rc = unsafe {
            ws::bind(socket_of(&fd), (&storage as *const Storage).cast(), len)
          };
          if rc == 0 { 0 } else { last_wsa_error() }
        }
        Err(err) => io_error(err),
      }),
      Op::Listen { fd, backlog } => {
        // SAFETY: no pointers are passed.
        let rc = unsafe { ws::listen(socket_of(&fd), backlog) };
        Some(if rc == 0 { 0 } else { last_wsa_error() })
      }
      Op::Shutdown { fd, how } => {
        let how = match how {
          ShutdownHow::Read => ws::SD_RECEIVE,
          ShutdownHow::Write => ws::SD_SEND,
          ShutdownHow::Both => ws::SD_BOTH,
        };
        // SAFETY: no pointers are passed.
        let rc = unsafe { ws::shutdown(socket_of(&fd), how) };
        Some(if rc == 0 { 0 } else { last_wsa_error() })
      }
      Op::Fsync { .. }
      | Op::OpenAt { .. }
      | Op::Stat { .. }
      | Op::ReadDir { .. }
      | Op::UnlinkAt { .. }
      | Op::RenameAt { .. }
      | Op::MkdirAt { .. }
      | Op::LinkAt { .. }
      | Op::ReadlinkAt { .. }
      | Op::GetCwd { .. }
      | Op::Spawn { .. } => self.offload(id, op),
    }
  }

  /// Hands an operation to the blocking pool; its result arrives through the
  /// port.
  fn offload(&mut self, id: u64, op: Op) -> Option<isize> {
    if self.pool.is_none() {
      match BlockingPool::new(self.port) {
        Ok(pool) => self.pool = Some(pool),
        Err(err) => return Some(io_error(err)),
      }
    }
    let sent = self
      .pool
      .as_ref()
      .and_then(|pool| pool.jobs.as_ref())
      .is_some_and(|jobs| jobs.send(BlockingJob { id, op }).is_ok());
    if !sent {
      return Some(neg(ERROR_NOT_SUPPORTED));
    }
    self.blocking_in_flight += 1;
    None
  }

  fn start_recv(
    &mut self,
    id: u64,
    fd: &Resource,
    bufs: impl Iterator<Item = (*mut u8, usize)>,
    flags: u32,
    from: Option<NonNull<SocketAddrBuf>>,
  ) -> Option<isize> {
    let socket = socket_of(fd);
    if let Err(code) = self.associate(socket as HANDLE) {
      return Some(code);
    }
    let kind = match from {
      Some(out) => AsyncKind::RecvFrom { out },
      None => AsyncKind::Transfer,
    };
    let mut state = Overlapped::new(id, fd, kind);
    state.bufs = wsa_bufs(bufs);
    state.flags = flags;
    let state_ptr: *mut Overlapped = &mut *state;
    // SAFETY: every pointer refers to `state`, which stays alive (boxed and
    // tracked) until the kernel reports completion.
    let rc = unsafe {
      let state = &mut *state_ptr;
      if from.is_some() {
        ws::WSARecvFrom(
          socket,
          state.bufs.as_ptr(),
          state.bufs.len() as u32,
          ptr::null_mut(),
          &mut state.flags,
          (&mut state.addr as *mut Storage).cast(),
          &mut state.addr_len,
          &mut state.ov,
          None,
        )
      } else {
        ws::WSARecv(
          socket,
          state.bufs.as_ptr(),
          state.bufs.len() as u32,
          ptr::null_mut(),
          &mut state.flags,
          &mut state.ov,
          None,
        )
      }
    };
    self.track(state, rc == 0)
  }

  fn start_send(
    &mut self,
    id: u64,
    fd: &Resource,
    bufs: impl Iterator<Item = (*mut u8, usize)>,
    flags: u32,
    to: Option<std::net::SocketAddr>,
  ) -> Option<isize> {
    let socket = socket_of(fd);
    if let Err(code) = self.associate(socket as HANDLE) {
      return Some(code);
    }
    let mut state = Overlapped::new(id, fd, AsyncKind::Transfer);
    state.bufs = wsa_bufs(bufs);
    if let Some(to) = to {
      let (addr, len) = socket_addr_to_storage(to);
      state.addr = addr;
      state.addr_len = len;
    }
    let state_ptr: *mut Overlapped = &mut *state;
    // SAFETY: as in `start_recv`.
    let rc = unsafe {
      let state = &mut *state_ptr;
      if to.is_some() {
        ws::WSASendTo(
          socket,
          state.bufs.as_ptr(),
          state.bufs.len() as u32,
          ptr::null_mut(),
          flags,
          (&state.addr as *const Storage).cast(),
          state.addr_len,
          &mut state.ov,
          None,
        )
      } else {
        ws::WSASend(
          socket,
          state.bufs.as_ptr(),
          state.bufs.len() as u32,
          ptr::null_mut(),
          flags,
          &mut state.ov,
          None,
        )
      }
    };
    self.track(state, rc == 0)
  }

  fn start_accept(
    &mut self,
    id: u64,
    fd: &Resource,
    out: NonNull<SocketAddrBuf>,
  ) -> Option<isize> {
    let listener = socket_of(fd);
    if let Err(code) = probe_socket(listener) {
      return Some(code);
    }
    let family = match local_addr(listener) {
      Ok((storage, _)) => storage.ss_family as i32,
      Err(code) => return Some(code),
    };
    // SAFETY: plain socket creation; no pointers are passed.
    let accepted = unsafe {
      ws::WSASocketW(
        family,
        ws::SOCK_STREAM,
        0,
        ptr::null(),
        0,
        ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT,
      )
    };
    if accepted == ws::INVALID_SOCKET {
      return Some(last_wsa_error());
    }
    let mut state = Overlapped::new(
      id,
      fd,
      AsyncKind::Accept { accepted, out, buf: [0; 2 * ACCEPT_ADDR_LEN] },
    );
    if let Err(code) = self.associate(listener as HANDLE) {
      return Some(code);
    }
    let state_ptr: *mut Overlapped = &mut *state;
    let mut received = 0u32;
    // SAFETY: the output buffer and `OVERLAPPED` live in `state`, which stays
    // alive until completion.
    let ok = unsafe {
      let state = &mut *state_ptr;
      let AsyncKind::Accept { buf, .. } = &mut state.kind else {
        unreachable!()
      };
      ws::AcceptEx(
        listener,
        accepted,
        buf.as_mut_ptr().cast(),
        0,
        ACCEPT_ADDR_LEN as u32,
        ACCEPT_ADDR_LEN as u32,
        &mut received,
        &mut state.ov,
      )
    };
    self.track(state, ok != FALSE)
  }

  fn start_connect(
    &mut self,
    id: u64,
    fd: &Resource,
    addr: &SocketAddrBuf,
  ) -> Option<isize> {
    let socket = socket_of(fd);
    if let Err(code) = probe_socket(socket) {
      return Some(code);
    }
    let (storage, len) = match socket_addr_buf_to_storage(addr) {
      Ok(lowered) => lowered,
      Err(err) => return Some(io_error(err)),
    };

    // ConnectEx only supports connection-oriented IP sockets; Unix-domain
    // connects are local and complete synchronously.
    if addr.family == SocketAddrFamily::Unix {
      // SAFETY: `storage` holds a valid address of `len` bytes.
      let rc = unsafe {
        ws::connect(socket, (&storage as *const Storage).cast(), len)
      };
      return Some(if rc == 0 { 0 } else { last_wsa_error() });
    }

    if let Err(code) = ensure_bound(socket, storage.ss_family) {
      return Some(code);
    }
    let connect_ex = match self.connect_ex(socket) {
      Ok(connect_ex) => connect_ex,
      Err(code) => return Some(code),
    };
    if let Err(code) = self.associate(socket as HANDLE) {
      return Some(code);
    }
    let mut state = Overlapped::new(id, fd, AsyncKind::Connect);
    state.addr = storage;
    state.addr_len = len;
    let state_ptr: *mut Overlapped = &mut *state;
    // SAFETY: the address and `OVERLAPPED` live in `state` until completion.
    let ok = unsafe {
      let state = &mut *state_ptr;
      connect_ex(
        socket,
        (&state.addr as *const Storage).cast(),
        state.addr_len,
        ptr::null(),
        0,
        ptr::null_mut(),
        &mut state.ov,
      )
    };
    self.track(state, ok != FALSE)
  }

  fn connect_ex(&mut self, socket: ws::SOCKET) -> Result<ConnectExFn, isize> {
    if let Some(connect_ex) = self.connect_ex {
      return Ok(connect_ex);
    }
    let guid = ws::WSAID_CONNECTEX;
    let mut func: ws::LPFN_CONNECTEX = None;
    let mut bytes = 0u32;
    // SAFETY: in/out buffers are valid for the sizes passed.
    let rc = unsafe {
      ws::WSAIoctl(
        socket,
        ws::SIO_GET_EXTENSION_FUNCTION_POINTER,
        (&guid as *const windows_sys::core::GUID).cast(),
        mem::size_of_val(&guid) as u32,
        (&mut func as *mut ws::LPFN_CONNECTEX).cast(),
        mem::size_of::<ws::LPFN_CONNECTEX>() as u32,
        &mut bytes,
        ptr::null_mut(),
        None,
      )
    };
    if rc != 0 {
      return Err(last_wsa_error());
    }
    let func = func.ok_or(neg(ERROR_NOT_SUPPORTED))?;
    self.connect_ex = Some(func);
    Ok(func)
  }

  /// Turns a completed overlapped operation into its raw result.
  fn finish(&mut self, mut state: Box<Overlapped>) -> isize {
    let mut bytes = 0u32;
    let mut flags = 0u32;
    // SAFETY: the operation has completed, so querying its result does not
    // block and `state.ov` is no longer written by the kernel.
    let ok = unsafe {
      ws::WSAGetOverlappedResult(
        state.socket,
        &state.ov,
        &mut bytes,
        FALSE,
        &mut flags,
      )
    };
    if ok == FALSE {
      // SAFETY: reads the calling thread's last Winsock error.
      let code = unsafe { ws::WSAGetLastError() };
      // A datagram larger than the buffer is truncated, as on Unix, rather
      // than failing the receive.
      let truncated = code == ws::WSAEMSGSIZE
        && matches!(
          state.kind,
          AsyncKind::Transfer | AsyncKind::RecvFrom { .. }
        );
      if !truncated {
        return -(code as isize);
      }
    }

    match &mut state.kind {
      AsyncKind::Transfer => bytes as isize,
      AsyncKind::RecvFrom { out } => {
        if let Ok(addr) =
          socket_addr_buf_from_storage(&state.addr, state.addr_len)
        {
          // SAFETY: `out` is op-owned output storage.
          unsafe { out.as_ptr().write(addr) };
        }
        bytes as isize
      }
      AsyncKind::Accept { accepted, out, .. } => {
        let listener = state.socket;
        // SAFETY: `listener` is a valid `SOCKET` value to copy from.
        let rc = unsafe {
          ws::setsockopt(
            *accepted,
            ws::SOL_SOCKET,
            ws::SO_UPDATE_ACCEPT_CONTEXT,
            (&listener as *const ws::SOCKET).cast(),
            mem::size_of::<ws::SOCKET>() as i32,
          )
        };
        if rc != 0 {
          return last_wsa_error();
        }
        if let Ok((storage, len)) = peer_addr(*accepted)
          && let Ok(addr) = socket_addr_buf_from_storage(&storage, len)
        {
          // SAFETY: `out` is op-owned output storage.
          unsafe { out.as_ptr().write(addr) };
        }
        mem::replace(accepted, ws::INVALID_SOCKET) as isize
      }
      AsyncKind::Connect => {
        // SAFETY: no option value is passed.
        let rc = unsafe {
          ws::setsockopt(
            state.socket,
            ws::SOL_SOCKET,
            ws::SO_UPDATE_CONNECT_CONTEXT,
            ptr::null(),
            0,
          )
        };
        if rc != 0 { last_wsa_error() } else { 0 }
      }
    }
  }

  /// Dequeues completion packets, waiting up to `timeout` ms for the first.
  ///
  /// Returns `false` when the wait timed out without any packet (including
  /// wake-ups from [`Iocp::notify`]).
  fn reap(
    &mut self,
    timeout: u32,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<bool> {
    let mut removed = 0u32;
    // SAFETY: `entries` has room for `MAX_ENTRIES` entries.
    let ok = unsafe {
      GetQueuedCompletionStatusEx(
        self.port,
        self.entries.as_mut_ptr(),
        MAX_ENTRIES as u32,
        &mut removed,
        timeout,
        FALSE,
      )
    };
    if ok == FALSE {
      // SAFETY: reads the calling thread's last-error value.
      return match unsafe { GetLastError() } {
        WAIT_TIMEOUT => Ok(false),
        code => Err(io::Error::from_raw_os_error(code as i32)),
      };
    }
    // SAFETY: the kernel initialized the first `removed` entries.
    unsafe { self.entries.set_len(removed as usize) };
    for entry in mem::take(&mut self.entries) {
      if entry.lpCompletionKey == BLOCKING_KEY {
        // SAFETY: workers post `Box<BlockingDone>` pointers under this key.
        let done =
          unsafe { Box::from_raw(entry.lpOverlapped.cast::<BlockingDone>()) };
        self.blocking_in_flight -= 1;
        completed.push(OpCompleted::new(done.id, done.result));
        continue;
      }
      if entry.lpCompletionKey == NOTIFY_KEY || entry.lpOverlapped.is_null() {
        continue;
      }
      let ptr = entry.lpOverlapped as *mut Overlapped;
      if !self.in_flight.remove(&(ptr as usize)) {
        continue;
      }
      // SAFETY: `ptr` came from `Box::into_raw` in `track` and the kernel is
      // done with it.
      let state = unsafe { Box::from_raw(ptr) };
      let id = state.id;
      completed.push(OpCompleted::new(id, self.finish(state)));
    }
    self.entries = Vec::with_capacity(MAX_ENTRIES);
    Ok(true)
  }
}

impl Drop for Iocp {
  fn drop(&mut self) {
    if self.port.is_null() {
      return;
    }
    self.drain_in_flight();
    // SAFETY: the port is owned by this backend.
    unsafe { CloseHandle(self.port) };
  }
}

impl Iocp {
  /// Stops the worker pool, cancels every overlapped operation and waits,
  /// for at most `DRAIN_GRACE`, for their packets. Anything left after that
  /// is leaked rather than freed while possibly in use.
  ///
  /// Returns whether nothing is left in the kernel or on a worker.
  fn drain_in_flight(&mut self) -> bool {
    if self.port.is_null() {
      return true;
    }
    // Workers write into op memory; stop them first.
    if let Some(mut pool) = self.pool.take() {
      pool.shutdown();
    }
    for &ptr in &self.in_flight {
      let state = ptr as *const Overlapped;
      // SAFETY: `state` is a live in-flight operation.
      unsafe { CancelIoEx((*state).socket as HANDLE, &(*state).ov) };
    }
    // Cancelled operations still post a packet; drain them so the kernel is
    // done with their memory before it is freed.
    let mut completed = Vec::new();
    let deadline = Instant::now() + DRAIN_GRACE;
    while !self.in_flight.is_empty() {
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        break;
      }
      let ms = remaining.as_millis().clamp(1, 100) as u32;
      if self.reap(ms, &mut completed).is_err() {
        break;
      }
      completed.clear();
    }
    // Free results the workers posted that were never reaped.
    while self.reap(0, &mut completed).unwrap_or(false) {}
    self.in_flight.is_empty()
  }
}

// SAFETY: each pushed operation is completed once: overlapped operations from
// their completion packet, which the kernel posts only when it is done with the
// operation's memory, and blocking ones once their worker finished. `cancel`
// waits for that packet as well, and `drain` stops the workers and waits for
// every packet, returning `false` if any remains. Results are the OS's own:
// resource-creating operations yield fresh handles.
unsafe impl IoBackend for Iocp {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    ensure_wsa()?;
    // SAFETY: creating a fresh completion port.
    let port = unsafe {
      CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1)
    };
    if port.is_null() {
      return Err(io::Error::last_os_error());
    }
    if let Some(mut pool) = self.pool.take() {
      pool.shutdown();
    }
    self.blocking_in_flight = 0;
    if !self.port.is_null() {
      // SAFETY: the previous port is owned by this backend.
      unsafe { CloseHandle(self.port) };
    }
    self.port = port;
    self.capacity = cap;
    self.backlog = Vec::with_capacity(cap);
    self.in_flight = HashSet::with_capacity(cap);
    self.ready = Vec::with_capacity(cap.min(256));
    self.entries = Vec::with_capacity(MAX_ENTRIES);
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, _step_bump: &mut Bump) {
    assert!(
      self.backlog.len() + self.in_flight.len() + self.blocking_in_flight
        < self.capacity,
      "IoBackend capacity exceeded: attempted to queue more than {} operations",
      self.capacity
    );
    self.backlog.push(Queued { id, op });
  }

  fn flush(&mut self) -> io::Result<()> {
    for Queued { id, op } in mem::take(&mut self.backlog) {
      if let Some(result) = self.submit(id, op) {
        self.ready.push(OpCompleted::new(id, result));
      }
    }
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    if let Some(index) = self.backlog.iter().position(|queued| queued.id == id)
    {
      self.backlog.swap_remove(index);
      self.ready.push(OpCompleted::new(id, neg(ERROR_OPERATION_ABORTED)));
      return;
    }
    // SAFETY: every pointer in `in_flight` is a live boxed `Overlapped`.
    let found = self
      .in_flight
      .iter()
      .map(|&ptr| ptr as *const Overlapped)
      .find(|&state| unsafe { (*state).id } == id);
    if let Some(state) = found {
      // The operation stays in flight: the kernel reports it, aborted or
      // finished, through the port like any other completion. A job on the
      // blocking pool is not found here; it runs to completion.
      // SAFETY: `state` is a live in-flight operation.
      unsafe { CancelIoEx((*state).socket as HANDLE, &(*state).ov) };
    }
  }

  fn drain(&mut self) -> bool {
    self.drain_in_flight()
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    completed.append(&mut self.ready);

    if self.in_flight.is_empty()
      && self.blocking_in_flight == 0
      && !completed.is_empty()
    {
      return Ok(());
    }
    if !completed.is_empty() {
      self.reap(0, completed)?;
      return Ok(());
    }
    let Some(timeout) = timeout else {
      self.reap(u32::MAX, completed)?;
      return Ok(());
    };

    // Windows waits are tick-based and may return before the timeout. Keep
    // waiting for the remainder so callers never wake before their deadline,
    // as with epoll/kqueue.
    let deadline = Instant::now() + timeout;
    loop {
      let remaining = deadline.saturating_duration_since(Instant::now());
      let ms =
        remaining.as_nanos().div_ceil(1_000_000).min(u32::MAX as u128 - 1);
      if self.reap(ms as u32, completed)? || remaining.is_zero() {
        return Ok(());
      }
    }
  }
}

/// Runs an operation that has no overlapped form on a pool worker.
fn run_blocking(op: Op) -> isize {
  blocking_result(op).unwrap_or(neg(ERROR_NOT_SUPPORTED))
}

fn blocking_result(op: Op) -> Option<isize> {
  match op {
    Op::Read { fd, iovecs, iov_count, offset, .. } => {
      // SAFETY: the op model keeps `iov_count` buffers alive until done.
      let bufs =
        unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
      Some(file_transfer(handle_of(&fd), bufs, offset, false))
    }
    Op::Write { fd, iovecs, iov_count, offset, .. } => {
      // SAFETY: the op model keeps `iov_count` buffers alive until done.
      let bufs =
        unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
      Some(file_transfer(handle_of(&fd), bufs, offset, true))
    }
    Op::Fsync { fd } => {
      // SAFETY: no pointers are passed.
      let ok = unsafe { FlushFileBuffers(handle_of(&fd)) };
      Some(if ok == FALSE { last_error() } else { 0 })
    }
    Op::OpenAt { dir_fd, path, flags, mode: _ } => {
      Some(open_at(&dir_fd, &path, flags).unwrap_or_else(io_error))
    }
    Op::Stat { target, out } => Some(match stat(&target) {
      Ok(stat) => {
        // SAFETY: `out` is op-owned output storage valid until completion.
        unsafe { out.as_ptr().write(stat) };
        0
      }
      Err(err) => io_error(err),
    }),
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
      // SAFETY: all pointers are op-owned storage valid until completion.
      let result = unsafe {
        read_dir(
          &fd,
          std::slice::from_raw_parts_mut(raw_buf.as_ptr(), raw_cap),
          std::slice::from_raw_parts_mut(entries.as_ptr(), entries_cap),
          &mut *opaque.as_ptr(),
          &mut *opaque_drop.as_ptr(),
        )
      };
      Some(match result {
        Ok(result) => {
          // SAFETY: `out` is op-owned output storage.
          unsafe { out.as_ptr().write(result) };
          0
        }
        Err(err) => io_error(err),
      })
    }
    Op::UnlinkAt { dir_fd, path, kind } => Some(
      resolve(&dir_fd, &path)
        .and_then(|path| match kind {
          UnlinkKind::File => fs::remove_file(path),
          UnlinkKind::Directory => fs::remove_dir(path),
        })
        .map_or_else(io_error, |()| 0),
    ),
    Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => Some(
      resolve(&old_dir_fd, &old_path)
        .and_then(|old| fs::rename(old, resolve(&new_dir_fd, &new_path)?))
        .map_or_else(io_error, |()| 0),
    ),
    Op::MkdirAt { dir_fd, path, mode: _ } => Some(
      resolve(&dir_fd, &path)
        .and_then(fs::create_dir)
        .map_or_else(io_error, |()| 0),
    ),
    Op::LinkAt { kind, source_dir_fd, source_path, new_dir_fd, new_path } => {
      Some(
        link_at(kind, &source_dir_fd, &source_path, &new_dir_fd, &new_path)
          .map_or_else(io_error, |()| 0),
      )
    }
    Op::ReadlinkAt { dir_fd, path, buf, buf_len } => {
      Some(resolve(&dir_fd, &path).and_then(fs::read_link).map_or_else(
        io_error,
        |target| {
          let bytes = target.as_os_str().as_encoded_bytes();
          let len = bytes.len().min(buf_len);
          // SAFETY: `buf` is op-owned output storage of `buf_len` bytes.
          unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf.as_ptr(), len)
          };
          len as isize
        },
      ))
    }
    Op::GetCwd { out } => Some(match std::env::current_dir() {
      Ok(cwd) => {
        // SAFETY: `out` is op-owned output storage.
        unsafe { out.as_ptr().write(cwd.into_os_string()) };
        0
      }
      Err(err) => io_error(err),
    }),
    Op::Spawn { spec } => Some(spawn(&spec).unwrap_or_else(io_error)),
    _ => None,
  }
}

/// Fails fast on handles that are not open, as the readiness backends do,
/// instead of handing them to a worker.
fn check_handle(handle: HANDLE) -> Result<(), isize> {
  // SAFETY: `GetFileType` accepts any handle value.
  if unsafe { GetFileType(handle) } == FILE_TYPE_UNKNOWN {
    // SAFETY: reads the calling thread's last-error value.
    let code = unsafe { GetLastError() };
    if code != NO_ERROR {
      return Err(neg(code));
    }
  }
  Ok(())
}

fn wsa_bufs(bufs: impl Iterator<Item = (*mut u8, usize)>) -> Vec<ws::WSABUF> {
  bufs
    .map(|(ptr, len)| ws::WSABUF {
      len: len.min(u32::MAX as usize) as u32,
      buf: ptr,
    })
    .collect()
}

fn recv_bufs(msg: &MsgRecv) -> impl Iterator<Item = (*mut u8, usize)> + '_ {
  // SAFETY: the op model keeps `buf_count` descriptors alive until done.
  let bufs = unsafe {
    std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
  };
  bufs.iter().map(|b| (b.ptr.as_ptr(), b.len))
}

fn send_bufs(msg: &MsgSend) -> impl Iterator<Item = (*mut u8, usize)> + '_ {
  // SAFETY: the op model keeps `buf_count` descriptors alive until done.
  let bufs = unsafe {
    std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
  };
  bufs.iter().map(|b| (b.ptr.as_ptr(), b.len))
}

fn local_addr(socket: ws::SOCKET) -> Result<(Storage, SockLen), isize> {
  // SAFETY: plain old data.
  let mut storage: Storage = unsafe { mem::zeroed() };
  let mut len = mem::size_of::<Storage>() as i32;
  // SAFETY: `storage`/`len` are valid output storage.
  let rc = unsafe {
    ws::getsockname(socket, (&mut storage as *mut Storage).cast(), &mut len)
  };
  if rc == 0 { Ok((storage, len)) } else { Err(last_wsa_error()) }
}

fn peer_addr(socket: ws::SOCKET) -> Result<(Storage, SockLen), isize> {
  // SAFETY: plain old data.
  let mut storage: Storage = unsafe { mem::zeroed() };
  let mut len = mem::size_of::<Storage>() as i32;
  // SAFETY: `storage`/`len` are valid output storage.
  let rc = unsafe {
    ws::getpeername(socket, (&mut storage as *mut Storage).cast(), &mut len)
  };
  if rc == 0 { Ok((storage, len)) } else { Err(last_wsa_error()) }
}

/// `ConnectEx` requires a bound socket; bind to the wildcard address if the
/// caller has not bound it yet.
fn ensure_bound(socket: ws::SOCKET, family: u16) -> Result<(), isize> {
  match local_addr(socket) {
    Ok(_) => return Ok(()),
    Err(code) if code == -(ws::WSAEINVAL as isize) => {}
    Err(code) => return Err(code),
  }
  let wildcard: std::net::SocketAddr = if family == ws::AF_INET6 {
    (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
  } else {
    (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
  };
  let (storage, len) = socket_addr_to_storage(wildcard);
  // SAFETY: `storage` holds a valid address of `len` bytes.
  let rc =
    unsafe { ws::bind(socket, (&storage as *const Storage).cast(), len) };
  if rc == 0 { Ok(()) } else { Err(last_wsa_error()) }
}

/// Synchronous read/write on a non-socket handle.
fn file_transfer(
  handle: HANDLE,
  bufs: &[RawBuf],
  offset: i64,
  write: bool,
) -> isize {
  let mut total = 0usize;
  for buf in bufs {
    let at = if offset < 0 { None } else { Some(offset as u64 + total as u64) };
    let len = buf.len.min(u32::MAX as usize) as u32;
    match transfer_once(handle, buf.ptr, len, at, write) {
      Ok(n) => {
        total += n as usize;
        if n < len {
          break;
        }
      }
      Err(code) if total == 0 => return code,
      Err(_) => break,
    }
  }
  total as isize
}

fn transfer_once(
  handle: HANDLE,
  ptr: *mut u8,
  len: u32,
  at: Option<u64>,
  write: bool,
) -> Result<u32, isize> {
  // SAFETY: plain old data.
  let mut ov: OVERLAPPED = unsafe { mem::zeroed() };
  let ov_ptr = match at {
    Some(at) => {
      ov.Anonymous.Anonymous.Offset = at as u32;
      ov.Anonymous.Anonymous.OffsetHigh = (at >> 32) as u32;
      &mut ov as *mut OVERLAPPED
    }
    None => ptr::null_mut(),
  };
  let mut n = 0u32;
  // SAFETY: `ptr` is valid for `len` bytes for the op lifetime; `ov` outlives
  // the call (and the wait below for handles opened for overlapped I/O).
  let ok = unsafe {
    if write {
      WriteFile(handle, ptr, len, &mut n, ov_ptr)
    } else {
      ReadFile(handle, ptr, len, &mut n, ov_ptr)
    }
  };
  if ok != FALSE {
    return Ok(n);
  }
  // SAFETY: reads the calling thread's last-error value.
  match unsafe { GetLastError() } {
    ERROR_IO_PENDING if !ov_ptr.is_null() => {
      // SAFETY: waits for the operation started above on `ov`.
      let ok = unsafe { GetOverlappedResult(handle, ov_ptr, &mut n, TRUE) };
      if ok != FALSE {
        Ok(n)
      } else {
        // SAFETY: reads the calling thread's last-error value.
        match unsafe { GetLastError() } {
          ERROR_HANDLE_EOF | ERROR_BROKEN_PIPE if !write => Ok(0),
          code => Err(neg(code)),
        }
      }
    }
    ERROR_HANDLE_EOF | ERROR_BROKEN_PIPE if !write => Ok(0),
    code => Err(neg(code)),
  }
}

fn final_path(handle: HANDLE) -> io::Result<PathBuf> {
  let mut buf = vec![0u16; 512];
  loop {
    // SAFETY: `buf` is valid for `buf.len()` UTF-16 units.
    let len = unsafe {
      GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, 0)
    } as usize;
    if len == 0 {
      return Err(io::Error::last_os_error());
    }
    if len < buf.len() {
      return Ok(PathBuf::from(OsString::from_wide(&buf[..len])));
    }
    buf.resize(len + 1, 0);
  }
}

/// Resolves `path` relative to the directory `dir`, like the `*at` syscalls.
fn resolve(dir: &Resource, path: &OsStr) -> io::Result<PathBuf> {
  let path = Path::new(path);
  if path.has_root() || handle_of(dir) == CWD_HANDLE {
    return Ok(path.to_path_buf());
  }
  Ok(final_path(handle_of(dir))?.join(path))
}

fn open_at(
  dir: &Resource,
  path: &OsStr,
  flags: OpenFlags,
) -> io::Result<isize> {
  if !flags.reads() && !flags.writes() {
    return Err(io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as i32));
  }
  let path = resolve(dir, path)?;
  let mut options = OpenOptions::new();
  // Directories can only be opened with backup semantics. std opens handles
  // non-inheritable, so they never leak into child processes.
  options
    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
    .read(flags.reads())
    .write(flags.contains(OpenFlags::WRITE))
    .append(flags.contains(OpenFlags::APPEND))
    .truncate(flags.contains(OpenFlags::TRUNCATE));
  if flags.contains(OpenFlags::CREATE) {
    if flags.contains(OpenFlags::EXCLUSIVE) {
      options.create_new(true);
    } else {
      options.create(true);
    }
  }
  let file = options.open(path)?;
  if flags.contains(OpenFlags::DIRECTORY) && !file.metadata()?.is_dir() {
    return Err(io::Error::from_raw_os_error(ERROR_DIRECTORY as i32));
  }
  Ok(file.into_raw_handle() as isize)
}

fn stat(target: &StatTarget) -> io::Result<FileStat> {
  match target {
    StatTarget::Fd { fd } => stat_handle(handle_of(fd)),
    StatTarget::Path { dir_fd, path, follow_symlinks } => {
      let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
      if !follow_symlinks {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
      }
      let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(flags)
        .open(resolve(dir_fd, path)?)?;
      stat_handle(file.as_raw_handle())
    }
  }
}

fn stat_handle(handle: HANDLE) -> io::Result<FileStat> {
  // SAFETY: the `File` only borrows `handle`; `ManuallyDrop` keeps it open.
  let file = ManuallyDrop::new(unsafe { File::from_raw_handle(handle) });
  let metadata = file.metadata()?;
  // SAFETY: plain old data.
  let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { mem::zeroed() };
  // SAFETY: `info` is valid output storage.
  let nlink =
    if unsafe { GetFileInformationByHandle(handle, &mut info) } != FALSE {
      info.nNumberOfLinks as u64
    } else {
      1
    };

  let file_type = metadata.file_type();
  let (file_type, type_bits) = if file_type.is_symlink() {
    (FileType::Symlink, 0o120000)
  } else if file_type.is_dir() {
    (FileType::Directory, 0o040000)
  } else {
    (FileType::File, 0o100000)
  };
  let mut permissions =
    if metadata.permissions().readonly() { 0o444 } else { 0o644 };
  if file_type == FileType::Directory {
    permissions |= 0o111;
  }
  Ok(FileStat {
    file_type,
    size: metadata.len(),
    permissions,
    mode: type_bits | permissions,
    nlink,
    uid: 0,
    gid: 0,
  })
}

/// Continuation state for `ReadDir`, kept in the op's opaque slot.
struct DirState {
  iter: fs::ReadDir,
  pending: Option<(OsString, Option<FileType>)>,
}

/// # Safety
///
/// `state` must come from `Box::into_raw(Box<DirState>)` in `read_dir`.
unsafe fn drop_dir_state(state: *mut ()) {
  // SAFETY: guaranteed by the caller.
  drop(unsafe { Box::from_raw(state.cast::<DirState>()) });
}

fn read_dir(
  dir: &Resource,
  raw: &mut [u8],
  out: &mut [DirEntryRef],
  opaque: &mut *mut (),
  opaque_drop: &mut Option<OpaqueDropFn>,
) -> io::Result<ReadDirResult> {
  if opaque.is_null() {
    let iter = fs::read_dir(final_path(handle_of(dir))?)?;
    *opaque = Box::into_raw(Box::new(DirState { iter, pending: None })).cast();
    *opaque_drop = Some(drop_dir_state);
  }
  // SAFETY: `opaque` was set from a `Box<DirState>` above or on a prior call.
  let state = unsafe { &mut *opaque.cast::<DirState>() };

  let mut written = 0;
  let mut raw_written = 0;
  let mut eof = false;
  loop {
    let (name, file_type) = match state.pending.take() {
      Some(entry) => entry,
      None => match state.iter.next() {
        None => {
          eof = true;
          break;
        }
        Some(entry) => {
          let entry = entry?;
          let file_type = entry.file_type().ok().map(|ty| {
            if ty.is_symlink() {
              FileType::Symlink
            } else if ty.is_dir() {
              FileType::Directory
            } else {
              FileType::File
            }
          });
          (entry.file_name(), file_type)
        }
      },
    };
    let bytes = name.as_encoded_bytes();
    if written == out.len() || raw_written + bytes.len() > raw.len() {
      state.pending = Some((name, file_type));
      break;
    }
    raw[raw_written..raw_written + bytes.len()].copy_from_slice(bytes);
    out[written] = DirEntryRef {
      name_offset: raw_written as u32,
      name_len: bytes.len() as u16,
      file_type,
      ino: None,
    };
    raw_written += bytes.len();
    written += 1;
  }

  if eof {
    // SAFETY: `opaque` holds the `DirState` created above.
    unsafe { drop_dir_state(*opaque) };
    *opaque = ptr::null_mut();
    *opaque_drop = None;
  }
  Ok(ReadDirResult { entries: written, raw_written, eof })
}

fn link_at(
  kind: LinkKind,
  source_dir: &Resource,
  source: &OsStr,
  new_dir: &Resource,
  new: &OsStr,
) -> io::Result<()> {
  let new = resolve(new_dir, new)?;
  match kind {
    LinkKind::Hard => fs::hard_link(resolve(source_dir, source)?, new),
    LinkKind::Soft => {
      // Windows distinguishes file and directory symlinks; pick by what the
      // target currently is, relative to the link's own directory.
      let target = new.parent().unwrap_or(Path::new("")).join(source);
      if target.is_dir() {
        std::os::windows::fs::symlink_dir(source, new)
      } else {
        std::os::windows::fs::symlink_file(source, new)
      }
    }
  }
}

fn spawn(spec: &SpawnSpec) -> io::Result<isize> {
  let mut command = std::process::Command::new(&spec.program);
  // `args[0]` is the conventional program name (as for `posix_spawn`);
  // `Command` supplies its own.
  command.args(spec.args.iter().skip(1));
  if let Some(env) = &spec.env {
    command.env_clear();
    for var in env {
      let bytes = var.as_encoded_bytes();
      let split = bytes.iter().position(|&b| b == b'=').unwrap_or(bytes.len());
      // SAFETY: splitting at an ASCII byte keeps both halves valid encoded
      // `OsStr` data, as documented for `as_encoded_bytes`.
      let (key, value) = unsafe {
        (
          OsStr::from_encoded_bytes_unchecked(&bytes[..split]),
          OsStr::from_encoded_bytes_unchecked(
            bytes.get(split + 1..).unwrap_or_default(),
          ),
        )
      };
      command.env(key, value);
    }
  }
  Ok(command.spawn()?.id() as isize)
}
