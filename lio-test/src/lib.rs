//! Test support crate for `lio` and crates that implement compatible
//! low-level I/O contracts.
//!
//! `lio-test` packages the reusable contract-test macros and fixture traits
//! used across the workspace:
//!
//! - [`test_io_backend!`] validates a backend against the deterministic
//!   `IoBackend` contract.
//! - [`test_op_model_contract!`] validates generic `OpModel`-style state
//!   machines that implement [`OpModelContract`].
//!
//! Typical usage from a consumer crate:
//!
//! ```ignore
//! #[cfg(test)]
//! mod tests {
//!   use super::*;
//!
//!   mod my_model_contract {
//!     use super::*;
//!
//!     lio_test::test_op_model_contract!(MyModel);
//!   }
//! }
//! ```

/// Shared contract tests for [`IoBackend`](crate::backend::IoBackend)
/// implementations.
///
/// Backends that pass `test_io_backend!` conform to the standard observable
/// contract for the deterministic scenarios covered here.
///
/// Runtime / queue semantics:
/// - empty `wait()` after initialization yields no completions
/// - pushing more than the initialized backend capacity panics
/// - `wait(Duration::ZERO)` never blocks
/// - `wait(None)` blocks until at least one completion is available
/// - `flush()` with an empty backlog is harmless
/// - `flush()` is allowed to produce completions without creating pending work
/// - immediate completions queued during `flush()` are surfaced on the next `wait()`
/// - batched immediate completions are surfaced together with exact results
/// - a completed `wait()` buffer is cleared on the next `wait()`
/// - completions are not duplicated across `wait()` calls
/// - deferred readiness completions are delivered exactly once
/// - mixed pending batches complete each registration exactly once
/// - completions are routed to the correct registration id
///
/// `Op::Nop`:
/// - produces one immediate success completion with exact result `0`
/// - completion is surfaced on the next `wait()` and never replayed
///
/// `Op::Read`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - pending reads remain pending until data arrives
/// - EOF returns exact `0`
/// - an offset past `MAX_FILE_OFFSET` (`i64::MAX`) returns exact raw
///   `-EINVAL`; it is never truncated, wrapped or taken as the current
///   position
/// - unsupported flags return exact raw `-ENOTSUP`
/// - null iovec pointer returns exact raw `-EINVAL`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Write`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - an offset past `MAX_FILE_OFFSET` (`i64::MAX`) returns exact raw
///   `-EINVAL`; it is never truncated, wrapped or taken as the current
///   position
/// - unsupported flags return exact raw `-ENOTSUP`
/// - null iovec pointer returns exact raw `-EINVAL`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Recv`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - pending receives remain pending until data arrives
/// - EOF returns exact `0`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Send`:
/// - success may return short positive byte counts, but repeated successful
///   submissions must eventually transfer the exact total byte count
/// - vectored success may return short positive byte counts, but repeated
///   successful submissions must eventually transfer the exact total byte count
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Accept`:
/// - success produces exactly one non-error completion on Unix platforms where
///   local listener registration is supported
/// - pending accepts remain pending until a client arrives
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Connect`:
/// - success returns exact `0` on Unix platforms where local listener
///   registration is supported
/// - missing Unix socket path returns exact raw `-ENOENT`
/// - invalid fd returns exact raw `-EBADF`
///
/// `Op::Socket`:
/// - success produces a non-error file descriptor / handle result
/// - invalid domain/type/protocol combinations return exact raw errno
///
/// `Op::OpenAt`:
/// - success produces a non-error file descriptor result
/// - missing path returns exact raw `-ENOENT`
///
/// `Op::Stat`:
/// - success returns exact `0` and fills the metadata output
/// - missing path returns exact raw `-ENOENT`
/// - nofollow mode reports symlink metadata instead of target metadata
///
/// `Op::ReadDir`:
/// - success returns exact `0` and fills parsed directory entries
/// - `"."` and `".."` are omitted from results
///
/// `Op::UnlinkAt`:
/// - success returns exact `0`
/// - missing path returns exact raw `-ENOENT`
///
/// `Op::RenameAt`:
/// - success returns exact `0`
/// - missing source returns exact raw `-ENOENT`
///
/// `Op::MkdirAt`:
/// - success returns exact `0`
/// - existing path returns exact raw `-EEXIST`
///
/// `Op::LinkAt`:
/// - hard-link success returns exact `0`
/// - symbolic-link success returns exact `0`
/// - hard-link with missing source returns exact raw `-ENOENT`
///
/// `Op::ReadlinkAt`:
/// - success returns the exact symlink-target byte count
/// - missing path returns exact raw `-ENOENT`
///
/// `Op::GetCwd`:
/// - success returns exact `0` and fills the platform-native cwd output
///
/// `Op::Spawn`:
/// - success returns a positive child pid
/// - missing executable returns exact raw `-ENOENT`
///
/// The suite intentionally avoids nondeterministic network cases. It only tests
/// scenarios where the expected raw `isize` output is stable for a conforming
/// backend.
#[macro_export]
macro_rules! test_io_backend {
  ($backend_ctor:expr) => {
    lio_test::test_io_backend!(lio, $backend_ctor);
  };
  ($lio:ident, $backend_ctor:expr) => {
    use $lio as __lio_test_lio;
    use bumpalo::Bump;
    use std::{
      cell::RefCell,
      collections::HashMap,
      time::{Duration, SystemTime, Instant},
      env, ffi::OsString, fs, mem, path::PathBuf, thread,
    };
    #[cfg(unix)]
    use std::os::{
      unix::net::{UnixListener, UnixStream},
      fd::{FromRawFd, IntoRawFd, RawFd},
    };
    #[cfg(windows)]
    use std::os::windows::io::{
      FromRawHandle, FromRawSocket, IntoRawHandle, IntoRawSocket, RawHandle,
    };

    use __lio_test_lio::api::resource::Resource;
    use __lio_test_lio::backend::{
      IoBackend,
      op::{
        FileMode, FileStat, LinkKind, MsgBuf, MsgBufMut, MsgRecv, MsgSend,
        Op, OpenFlags, RawBuf, ReadFlags, RecvFlags, SendFlags, SpawnSpec,
        UnlinkKind, WriteFlags,
      },
    };
    use std::ptr::NonNull;

    thread_local! {
      static STEP_BUMPS: RefCell<HashMap<(usize, u64), Bump>> =
        RefCell::new(HashMap::new());
    }

    fn push_op(backend: &mut impl IoBackend, id: u64, op: Op) {
      let backend_key = (backend as *mut _ as *mut ()) as usize;
      STEP_BUMPS.with(|step_bumps| {
        let mut step_bumps = step_bumps.borrow_mut();
        let step_bump =
          step_bumps.entry((backend_key, id)).or_insert_with(Bump::new);
        step_bump.reset();
        IoBackend::push(backend, id, op, step_bump);
      });
    }

    fn nonnull<T>(ptr: *mut T) -> NonNull<T> {
      NonNull::new(ptr).expect("test pointer must be non-null")
    }

    fn nonnull_const<T>(ptr: *const T) -> NonNull<T> {
      NonNull::new(ptr.cast_mut()).expect("test pointer must be non-null")
    }

    fn raw_buf_from_mut_slice(buf: &mut [u8]) -> RawBuf {
      // SAFETY: the returned `RawBuf` borrows the caller-provided slice, and
      // every test only submits it while that slice remains alive.
      unsafe { RawBuf::from_raw_parts(buf.as_mut_ptr(), buf.len()) }
    }

    fn new_backend() -> impl IoBackend {
      $backend_ctor
    }

    fn assert_exact_result(
      completed: &__lio_test_lio::backend::OpCompleted,
      id: u64,
      expected: isize,
    ) {
      assert_eq!(completed.registration_id(), id);
      assert_eq!(
        completed.result(),
        expected,
        "unexpected raw result for registration {}",
        id
      );
    }

    fn wait_completions(
      backend: &mut impl IoBackend,
      timeout: Option<Duration>,
    ) -> Vec<__lio_test_lio::backend::OpCompleted> {
      let mut completed = Vec::new();
      backend.wait(timeout, &mut completed).unwrap();
      for done in &completed {
        // No operation in the contract is multishot: each completes once.
        assert!(
          !done.has_more(),
          "registration {} completed with MORE, but its op produces one result",
          done.registration_id()
        );
      }
      completed
    }

    #[test]
    fn init_and_empty_wait() {
      let mut backend = new_backend();
      backend.init(64).unwrap();
      backend.flush().unwrap();

      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      assert!(
        completed.is_empty(),
        "backend returned completions without submitted operations"
      );
    }

    #[test]
    fn zero_timeout_wait_never_blocks() {
      let mut backend = new_backend();
      backend.init(64).unwrap();

      let start = Instant::now();
      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      let elapsed = start.elapsed();

      assert!(completed.is_empty(), "zero-timeout wait without work must be empty");
      assert!(
        elapsed < Duration::from_millis(200),
        "wait(Duration::ZERO) blocked for {:?}",
        elapsed
      );
    }

    #[test]
    fn wait_none_blocks_until_completion() {

      let mut backend = new_backend();
      backend.init(64).unwrap();

      let (read_res, write_res) = socket_pair();
      let payload = *b"wake";
      let mut buf = [0_u8; 4];
      let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

      push_op(&mut backend,
        3,
        Op::Read {
          fd: read_res.clone(),
          iovecs: nonnull(&mut raw_buf),
          iov_count: 1,
          offset: None,
          flags: ReadFlags::empty(),
        },
      );
      backend.flush().unwrap();

      let writer = thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);
      });

      let start = Instant::now();
      let completed = wait_completions(&mut backend, None);
      let elapsed = start.elapsed();

      assert_eq!(completed.len(), 1);
      assert_exact_result(&completed[0], 3, payload.len() as isize);
      assert!(
        elapsed >= Duration::from_millis(5),
        "wait(None) returned too early to have actually blocked: {:?}",
        elapsed
      );

      writer.join().unwrap();
    }

    #[test]
    fn empty_flush_is_harmless() {
      let mut backend = new_backend();
      backend.init(64).unwrap();

      backend.flush().unwrap();
      backend.flush().unwrap();

      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      assert!(
        completed.is_empty(),
        "flush() with an empty backlog must not create completions"
      );
    }

    #[test]
    fn pushed_work_is_not_observable_until_flush() {
      let mut backend = new_backend();
      backend.init(64).unwrap();

      let path = unix_socket_path("not-flushed");
      let storage = unix_sockaddr_un(&path);

      push_op(&mut backend,
        90,
        Op::Connect {
          fd: invalid_fd_resource(),
          addr: storage,
        },
      );

      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      assert!(
        completed.is_empty(),
        "queued work must not become observable before flush()"
      );

      backend.flush().unwrap();

      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      assert_eq!(completed.len(), 1);
      assert_exact_result(&completed[0], 90, expect::BADF_SOCKET);
    }

    #[test]
    fn flush_can_produce_immediate_completions_without_pending_work() {
      let mut backend = new_backend();
      backend.init(64).unwrap();

      let invalid_fd = invalid_fd_resource();
      // SAFETY: a null pointer with zero length is a valid empty raw buffer.
      let mut raw_buf = unsafe { RawBuf::from_raw_parts(std::ptr::null_mut(), 0) };

      push_op(&mut backend,
        91,
        Op::Read {
          fd: invalid_fd,
          iovecs: nonnull(&mut raw_buf),
          iov_count: 1,
          offset: Some(u64::MAX),
          flags: ReadFlags::empty(),
        },
      );

      backend.flush().unwrap();

      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      assert_eq!(
        completed.len(),
        1,
        "flush-produced completion must be surfaced on the next wait()"
      );
      assert_exact_result(&completed[0], 91, expect::INVAL);

      let completed = wait_completions(&mut backend, Some(Duration::ZERO));
      assert!(
        completed.is_empty(),
        "flush-produced completion must not be replayed on later wait() calls"
      );
    }

    #[test]
    #[should_panic(expected = "IoBackend capacity exceeded")]
    fn pushing_more_than_capacity_panics() {
      let mut backend = new_backend();
      backend.init(1).unwrap();

      push_op(&mut backend, 1, Op::Nop);
      push_op(&mut backend, 2, Op::Nop);
    }

    /// Raw error codes a conforming backend reports. Windows backends report
    /// Win32/Winsock codes (what `io::Error::from_raw_os_error` expects there).
    #[allow(dead_code)]
    mod expect {
      #[cfg(unix)]
      mod codes {
        pub const BADF: i32 = libc::EBADF;
        pub const BADF_SOCKET: i32 = libc::EBADF;
        pub const INVAL: i32 = libc::EINVAL;
        pub const INVAL_SOCKET: i32 = libc::EINVAL;
        pub const NOTSUP: i32 = libc::ENOTSUP;
        pub const NOENT: i32 = libc::ENOENT;
        pub const EXIST: i32 = libc::EEXIST;
        pub const CONNECT_MISSING: i32 = libc::ENOENT;
        pub const SPAWN_MISSING: i32 = libc::ENOENT;
      }

      #[cfg(windows)]
      mod codes {
        /// `ERROR_INVALID_HANDLE`
        pub const BADF: i32 = 6;
        /// `WSAENOTSOCK`
        pub const BADF_SOCKET: i32 = 10038;
        /// `ERROR_INVALID_PARAMETER`
        pub const INVAL: i32 = 87;
        /// `WSAEINVAL`
        pub const INVAL_SOCKET: i32 = 10022;
        /// `ERROR_NOT_SUPPORTED`
        pub const NOTSUP: i32 = 50;
        /// `ERROR_FILE_NOT_FOUND`
        pub const NOENT: i32 = 2;
        /// `ERROR_ALREADY_EXISTS`
        pub const EXIST: i32 = 183;
        /// `WSAECONNREFUSED`
        pub const CONNECT_MISSING: i32 = 10061;
        /// `ERROR_PATH_NOT_FOUND`
        pub const SPAWN_MISSING: i32 = 3;
      }

      pub const fn neg(code: i32) -> isize {
        -(code as isize)
      }

      pub const BADF: isize = neg(codes::BADF);
      pub const BADF_SOCKET: isize = neg(codes::BADF_SOCKET);
      pub const INVAL: isize = neg(codes::INVAL);
      pub const INVAL_SOCKET: isize = neg(codes::INVAL_SOCKET);
      pub const NOTSUP: isize = neg(codes::NOTSUP);
      pub const NOENT: isize = neg(codes::NOENT);
      pub const EXIST: isize = neg(codes::EXIST);
      pub const CONNECT_MISSING: isize = neg(codes::CONNECT_MISSING);
      pub const SPAWN_MISSING: isize = neg(codes::SPAWN_MISSING);
    }

    /// Takes ownership of a raw descriptor/handle returned by the backend.
    fn own_raw(raw: isize) -> Resource {
      #[cfg(unix)]
      // SAFETY: the backend hands out a fresh descriptor the test now owns.
      return unsafe { Resource::from_raw_fd(raw as RawFd) };
      #[cfg(windows)]
      // SAFETY: the backend hands out a fresh handle the test now owns.
      return unsafe { Resource::from_raw_handle(raw as RawHandle) };
    }

    /// Closes a raw descriptor/handle returned by the backend.
    fn close_raw(raw: isize) {
      drop(own_raw(raw));
    }

    #[cfg(unix)]
    fn send_payload(res: &Resource, payload: &[u8]) -> isize {
      use std::os::fd::AsRawFd;
      // SAFETY: `res` is a live socket and `payload` is readable for this
      // synchronous send.
      unsafe { libc::send(res.as_raw_fd(), payload.as_ptr().cast(), payload.len(), 0) }
    }

    #[cfg(windows)]
    fn send_payload(res: &Resource, payload: &[u8]) -> isize {
      use std::io::Write;
      use std::os::windows::io::AsRawHandle;
      // SAFETY: `res` owns the socket; `ManuallyDrop` keeps it open.
      let mut stream = mem::ManuallyDrop::new(unsafe {
        std::net::TcpStream::from_raw_socket(res.as_raw_handle() as u64)
      });
      match stream.write(payload) {
        Ok(n) => n as isize,
        Err(err) => -(err.raw_os_error().unwrap_or(0) as isize),
      }
    }

    #[cfg(unix)]
    fn recv_payload(res: &Resource, buf: &mut [u8]) -> isize {
      use std::os::fd::AsRawFd;
      // SAFETY: `res` is a live socket and `buf` is writable for this
      // synchronous recv.
      unsafe { libc::recv(res.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) }
    }

    #[cfg(windows)]
    fn recv_payload(res: &Resource, buf: &mut [u8]) -> isize {
      use std::io::Read;
      use std::os::windows::io::AsRawHandle;
      // SAFETY: `res` owns the socket; `ManuallyDrop` keeps it open.
      let mut stream = mem::ManuallyDrop::new(unsafe {
        std::net::TcpStream::from_raw_socket(res.as_raw_handle() as u64)
      });
      match stream.read(buf) {
        Ok(n) => n as isize,
        Err(err) => -(err.raw_os_error().unwrap_or(0) as isize),
      }
    }

    #[cfg(unix)]
    fn socket_pair() -> (Resource, Resource) {
      let mut fds = [0; 2];
      // SAFETY: `fds` points to two writable integers for `socketpair` to fill.
      let rc = unsafe {
        libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
      };
      assert_eq!(rc, 0, "socketpair() failed: {}", std::io::Error::last_os_error());
      for &fd in &fds {
        set_nonblocking(fd);
      }
      (own_raw(fds[0] as isize), own_raw(fds[1] as isize))
    }

    /// A connected loopback TCP pair; Windows has no `socketpair`.
    #[cfg(windows)]
    fn socket_pair() -> (Resource, Resource) {
      let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
      let client =
        std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
      let (server, _) = listener.accept().unwrap();
      client.set_nodelay(true).unwrap();
      server.set_nodelay(true).unwrap();
      (
        own_raw(server.into_raw_socket() as isize),
        own_raw(client.into_raw_socket() as isize),
      )
    }

    #[cfg(unix)]
    fn set_nonblocking(fd: RawFd) {
      // SAFETY: `fd` is expected to be a live descriptor in these tests.
      let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
      assert!(flags >= 0, "fcntl(F_GETFL) failed: {}", std::io::Error::last_os_error());
      // SAFETY: `fd` is expected to be a live descriptor in these tests.
      let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
      assert_eq!(rc, 0, "fcntl(F_SETFL, O_NONBLOCK) failed: {}", std::io::Error::last_os_error());
    }

    fn unique_name(prefix: &str) -> String {
      let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before UNIX epoch")
        .as_nanos();
      let mut name = format!("lio-{}-{:x}-{:x}", prefix, std::process::id(), now);
      name.truncate(40);
      name
    }

    fn unix_socket_path(prefix: &str) -> PathBuf {
      let mut path = env::temp_dir();
      path.push(unique_name(prefix));
      let mut idx = 0u32;
      while path.exists() {
        path.set_file_name(format!("{}-{:x}", unique_name(prefix), idx));
        idx += 1;
      }
      assert!(
        path.as_os_str().len() < 104,
        "unix socket test path too long: {}",
        path.display()
      );
      path
    }

    fn temp_path(prefix: &str) -> PathBuf {
      let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before UNIX epoch")
        .as_nanos();
      let mut path = env::temp_dir();
      path.push(format!("lio-{}-{:x}-{:x}", prefix, std::process::id(), now));
      path
    }

    fn unix_sockaddr_un(
      path: &std::path::Path,
    ) -> __lio_test_lio::backend::op::SocketAddrBuf {
      __lio_test_lio::backend::op::unix_socket_addr_buf(path.as_os_str().as_encoded_bytes())
        .expect("path too long for unix socket test")
    }

    /// An unconnected Unix-domain stream socket.
    #[cfg(unix)]
    fn unix_stream_socket() -> Resource {
      // SAFETY: plain socket creation.
      let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
      assert!(fd >= 0, "socket() failed: {}", std::io::Error::last_os_error());
      set_nonblocking(fd);
      own_raw(fd as isize)
    }

    /// An unconnected overlapped stream socket of the given address family.
    #[cfg(windows)]
    fn windows_stream_socket(family: u16) -> Resource {
      use windows_sys::Win32::Networking::WinSock as ws;
      // Ensure Winsock is initialized before raw socket calls.
      drop(std::net::UdpSocket::bind("127.0.0.1:0"));
      // SAFETY: plain socket creation.
      let socket = unsafe {
        ws::WSASocketW(
          family as i32,
          ws::SOCK_STREAM,
          0,
          std::ptr::null(),
          0,
          ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT,
        )
      };
      assert_ne!(socket, ws::INVALID_SOCKET, "WSASocketW failed: {}", std::io::Error::last_os_error());
      own_raw(socket as isize)
    }

    #[cfg(windows)]
    fn unix_stream_socket() -> Resource {
      windows_stream_socket(windows_sys::Win32::Networking::WinSock::AF_UNIX)
    }

    /// A listening stream socket plus helpers to connect to it: a Unix
    /// socket where std supports one, loopback TCP on Windows.
    struct LocalListener {
      #[cfg(unix)]
      path: PathBuf,
      #[cfg(unix)]
      listener: Option<UnixListener>,
      #[cfg(windows)]
      listener: Option<std::net::TcpListener>,
      #[cfg(windows)]
      addr: std::net::SocketAddr,
    }

    impl LocalListener {
      #[cfg(unix)]
      fn new(prefix: &str) -> Self {
        let path = unix_socket_path(prefix);
        fs::remove_file(&path).ok();
        let listener = UnixListener::bind(&path).unwrap();
        Self { path, listener: Some(listener) }
      }

      #[cfg(windows)]
      fn new(_prefix: &str) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        Self { listener: Some(listener), addr }
      }

      /// Hands the listening socket to the backend under test.
      fn resource(&mut self) -> Resource {
        let listener = self.listener.take().expect("listener already taken");
        listener.set_nonblocking(true).unwrap();
        #[cfg(unix)]
        return own_raw(listener.into_raw_fd() as isize);
        #[cfg(windows)]
        return own_raw(listener.into_raw_socket() as isize);
      }

      fn addr(&self) -> __lio_test_lio::backend::op::SocketAddrBuf {
        #[cfg(unix)]
        return unix_sockaddr_un(&self.path);
        #[cfg(windows)]
        return __lio_test_lio::backend::op::socket_addr_into_buf(self.addr);
      }

      /// An unconnected socket that can connect to this listener.
      fn client_socket(&self) -> Resource {
        #[cfg(unix)]
        return unix_stream_socket();
        #[cfg(windows)]
        return windows_stream_socket(windows_sys::Win32::Networking::WinSock::AF_INET);
      }

      /// Connects a std client from another thread, then hangs up.
      fn spawn_client(&self) -> thread::JoinHandle<()> {
        #[cfg(unix)]
        let target = self.path.clone();
        #[cfg(windows)]
        let target = self.addr;
        thread::spawn(move || {
          #[cfg(unix)]
          drop(UnixStream::connect(&target).unwrap());
          #[cfg(windows)]
          drop(std::net::TcpStream::connect(target).unwrap());
        })
      }

      /// Accepts one connection from another thread, then hangs up.
      fn spawn_acceptor(&mut self) -> thread::JoinHandle<()> {
        let listener = self.listener.take().expect("listener already taken");
        thread::spawn(move || {
          let (stream, _) = listener.accept().unwrap();
          drop(stream);
        })
      }
    }

    #[cfg(unix)]
    impl Drop for LocalListener {
      fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
      }
    }

    /// Opens a directory for `Op::ReadDir`.
    fn open_dir(path: &std::path::Path) -> Resource {
      #[cfg(unix)]
      let dir = fs::File::open(path).unwrap();
      #[cfg(windows)]
      let dir = {
        use std::os::windows::fs::OpenOptionsExt;
        // Directories can only be opened with FILE_FLAG_BACKUP_SEMANTICS.
        fs::OpenOptions::new()
          .read(true)
          .custom_flags(0x0200_0000)
          .open(path)
          .unwrap()
      };
      #[cfg(unix)]
      return own_raw(dir.into_raw_fd() as isize);
      #[cfg(windows)]
      return own_raw(dir.into_raw_handle() as isize);
    }

    fn symlink_file(target: &std::path::Path, link: &std::path::Path) {
      #[cfg(unix)]
      std::os::unix::fs::symlink(target, link).unwrap();
      #[cfg(windows)]
      std::os::windows::fs::symlink_file(target, link).unwrap();
    }

    /// A spawn spec for a short-lived child that exits successfully.
    fn success_spawn_spec() -> SpawnSpec {
      #[cfg(unix)]
      return SpawnSpec {
        program: OsString::from("/bin/sh"),
        args: vec![OsString::from("sh"), OsString::from("-c"), OsString::from("exit 0")],
        env: None,
      };
      #[cfg(windows)]
      return SpawnSpec {
        program: OsString::from(r"C:\Windows\System32\cmd.exe"),
        args: vec![OsString::from("cmd"), OsString::from("/C"), OsString::from("exit 0")],
        env: None,
      };
    }

    /// Waits for a spawned child so it does not linger.
    fn reap_child(pid: isize) {
      #[cfg(unix)]
      // SAFETY: the pid comes from the backend and is waited on exactly once.
      unsafe {
        libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), 0);
      }
      #[cfg(windows)]
      let _ = pid;
    }

    fn wait_for_single_completion(
      backend: &mut impl IoBackend,
    ) -> __lio_test_lio::backend::OpCompleted {
      // Generous deadline: returns as soon as a completion arrives.
      for _ in 0..500 {
        let completed =
          wait_completions(backend, Some(Duration::from_millis(10)));
        if let Some(first) = completed.first() {
          return __lio_test_lio::backend::OpCompleted::new(
            first.registration_id(),
            first.result(),
          );
        }
      }

      panic!("backend did not produce a completion within the expected timeout");
    }

    fn invalid_fd_resource() -> Resource {
      #[cfg(unix)]
      // SAFETY: this intentionally uses an invalid fd to verify raw errors.
      return unsafe { Resource::borrow(-1) };
      #[cfg(windows)]
      // SAFETY: the null handle is never valid; it is borrowed, never closed.
      return unsafe { Resource::borrow(std::ptr::null_mut()) };
    }

    fn wait_for_positive_completion(
      backend: &mut impl IoBackend,
    ) -> __lio_test_lio::backend::OpCompleted {
      let completed = wait_for_single_completion(backend);
      assert!(
        completed.result() > 0,
        "expected a positive completion result, got {}",
        completed.result()
      );
      completed
    }

    mod pipe {
      use super::*;
      use std::io::Write;

      /// An anonymous pipe as (`Resource` read end, std write end).
      fn pipe() -> (Resource, std::io::PipeWriter) {
        let (reader, writer) = std::io::pipe().expect("pipe() failed");
        #[cfg(unix)]
        let reader = own_raw(reader.into_raw_fd() as isize);
        #[cfg(windows)]
        let reader = own_raw(reader.into_raw_handle() as isize);
        (reader, writer)
      }

      #[test]
      fn read_without_data_does_not_block_the_loop() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (reader, mut writer) = pipe();
        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        push_op(&mut backend,
          95,
          Op::Read {
            fd: reader.clone(),
            iovecs: nonnull(&mut raw_buf),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );

        // Neither submitting nor polling may wait for the pipe.
        let start = Instant::now();
        backend.flush().unwrap();
        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(completed.is_empty(), "read on an empty pipe must stay pending");
        assert!(
          start.elapsed() < Duration::from_millis(500),
          "flush/wait blocked on an empty pipe for {:?}",
          start.elapsed()
        );

        writer.write_all(b"pipe").unwrap();
        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 95, 4);
        assert_eq!(&buf, b"pipe");
      }
    }

    mod cancel {
      use super::*;

      /// Collects completions for a while, to check none arrive.
      fn drain_for(
        backend: &mut impl IoBackend,
        period: Duration,
      ) -> Vec<__lio_test_lio::backend::OpCompleted> {
        let deadline = Instant::now() + period;
        let mut all = Vec::new();
        while Instant::now() < deadline {
          all.extend(wait_completions(backend, Some(Duration::from_millis(10))));
        }
        all
      }

      #[test]
      fn cancel_finishes_a_pending_recv_exactly_once() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let mut buf = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut buf)];
        push_op(&mut backend,
          97,
          Op::Recv { fd: read_res.clone(), msg: MsgRecv::new(&bufs), flags: RecvFlags::empty() },
        );
        backend.flush().unwrap();
        assert!(
          wait_completions(&mut backend, Some(Duration::ZERO)).is_empty(),
          "recv without data must stay pending"
        );

        backend.cancel(97);
        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 97);
        assert!(
          completed.result() < 0,
          "a cancelled recv reports an error, got {}",
          completed.result()
        );

        // Data arriving afterwards belongs to no operation.
        assert_eq!(send_payload(&write_res, b"late"), 4);
        let extra = drain_for(&mut backend, Duration::from_millis(100));
        assert!(extra.is_empty(), "cancelled op completed again: {extra:?}");
        drop(bufs);
        assert_eq!(buf, [0; 4], "a cancelled recv must not fill its buffer");
      }

      #[test]
      fn cancel_before_flush_completes_the_op() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, _write_res) = socket_pair();
        let mut buf = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut buf)];
        push_op(&mut backend,
          98,
          Op::Recv { fd: read_res.clone(), msg: MsgRecv::new(&bufs), flags: RecvFlags::empty() },
        );
        backend.cancel(98);
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 98);
        assert!(completed.result() < 0, "got {}", completed.result());
        let extra = drain_for(&mut backend, Duration::from_millis(50));
        assert!(extra.is_empty(), "cancelled op completed again: {extra:?}");
      }

      #[test]
      fn cancel_of_an_unknown_id_is_ignored() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        backend.cancel(12_345);
        push_op(&mut backend, 99, Op::Nop);
        backend.flush().unwrap();
        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 99, 0);
        let extra = drain_for(&mut backend, Duration::from_millis(50));
        assert!(extra.is_empty(), "unexpected completions: {extra:?}");
      }
    }

    mod nop {
      use super::*;

      #[test]
      fn nop_produces_immediate_success_completion() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        push_op(&mut backend, 40, Op::Nop);
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert_eq!(completed.len(), 1, "Op::Nop must complete immediately");
        assert_exact_result(&completed[0], 40, 0);

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "Op::Nop completion must not be replayed on later wait() calls"
        );
      }
    }

    mod read {
      use super::*;

      #[test]
      fn success_reports_raw_result_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"read";
        let mut buf = [0_u8; 4];
        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let mut total = 0usize;
        let mut id = 43_u64;
        while total < payload.len() {
          // SAFETY: this slice points into `buf`, which stays alive until the
          // backend completes the submitted read operation.
          let mut raw_buf = unsafe {
            RawBuf::from_raw_parts(
              buf[total..].as_mut_ptr(),
              buf.len() - total,
            )
          };

          push_op(&mut backend,
            id,
            Op::Read {
              fd: read_res.clone(),
              iovecs: nonnull(&mut raw_buf),
              iov_count: 1,
              offset: None,
              flags: ReadFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(
            n <= payload.len() - total,
            "read completed more bytes than remaining: {} > {}",
            n,
            payload.len() - total
          );
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn pending_then_success() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"late";
        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        push_op(&mut backend,
          54,
          Op::Read {
            fd: read_res.clone(),
            iovecs: nonnull(&mut raw_buf),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "read without available data must remain pending"
        );

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "read must remain pending across repeated zero-timeout waits"
        );

        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let completed = wait_for_positive_completion(&mut backend);
        assert_eq!(completed.registration_id(), 54);
        let mut total = completed.result() as usize;

        let mut id = 55_u64;
        while total < payload.len() {
          // SAFETY: this slice points into `buf`, which stays alive until the
          // backend completes the submitted read operation.
          let mut raw_buf = unsafe {
            RawBuf::from_raw_parts(
              buf[total..].as_mut_ptr(),
              buf.len() - total,
            )
          };
          push_op(&mut backend,
            id,
            Op::Read {
              fd: read_res.clone(),
              iovecs: nonnull(&mut raw_buf),
              iov_count: 1,
              offset: None,
              flags: ReadFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(
            n <= payload.len() - total,
            "read completed more bytes than remaining: {} > {}",
            n,
            payload.len() - total
          );
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn deferred_completion_is_delivered_exactly_once() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"once";
        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        push_op(&mut backend,
          541,
          Op::Read {
            fd: read_res.clone(),
            iovecs: nonnull(&mut raw_buf),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        backend.flush().unwrap();

        assert!(
          wait_completions(&mut backend, Some(Duration::ZERO)).is_empty(),
          "read without available data must remain pending"
        );

        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 541, payload.len() as isize);
        assert_eq!(&buf, &payload);

        for _ in 0..3 {
          let completed = wait_completions(&mut backend, Some(Duration::ZERO));
          assert!(
            completed.is_empty(),
            "a deferred read completion already returned once must not be returned again"
          );
        }
      }

      #[test]
      fn vectored_success_reports_total_byte_count() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"vector";
        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);
        let mut left = [0_u8; 3];
        let mut right = [0_u8; 3];
        let mut total = 0usize;
        let mut id = 65_u64;
        while total < payload.len() {
          let left_written = total.min(left.len());
          let right_written = total.saturating_sub(left.len()).min(right.len());
          let mut bufs = [
            // SAFETY: each slice points into `left`, which remains valid until
            // the submitted vectored read completes.
            unsafe {
              RawBuf::from_raw_parts(
                left[left_written..].as_mut_ptr(),
                left.len() - left_written,
              )
            },
            // SAFETY: each slice points into `right`, which remains valid until
            // the submitted vectored read completes.
            unsafe {
              RawBuf::from_raw_parts(
                right[right_written..].as_mut_ptr(),
                right.len() - right_written,
              )
            },
          ];

          push_op(&mut backend,
            id,
            Op::Read {
              fd: read_res.clone(),
              iovecs: nonnull(bufs.as_mut_ptr()),
              iov_count: bufs.len(),
              offset: None,
              flags: ReadFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(
            n <= payload.len() - total,
            "read completed more bytes than remaining: {} > {}",
            n,
            payload.len() - total
          );
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());
        assert_eq!(&left, b"vec");
        assert_eq!(&right, b"tor");
      }

      #[test]
      fn eof_reports_zero() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        push_op(&mut backend,
          55,
          Op::Read {
            fd: read_res.clone(),
            iovecs: nonnull(&mut raw_buf),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        backend.flush().unwrap();
        drop(write_res);

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 55, 0);
      }

      #[test]
      fn invalid_fd_reports_ebadf() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        push_op(&mut backend,
          47,
          Op::Read {
            fd: invalid_fd_resource(),
            iovecs: nonnull(&mut raw_buf),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 47, expect::BADF);
      }

      #[test]
      fn invalid_offset_reports_einval() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, _write_res) = socket_pair();
        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        // The first offset past `i64::MAX`, and the one whose bits read as
        // `-1` (the kernel's "current position").
        for (id, offset) in [(69, i64::MAX as u64 + 1), (70, u64::MAX)] {
          push_op(&mut backend,
            id,
            Op::Read {
              fd: read_res.clone(),
              iovecs: nonnull(&mut raw_buf),
              iov_count: 1,
              offset: Some(offset),
              flags: ReadFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_completions(&mut backend, Some(Duration::ZERO));
          assert_eq!(completed.len(), 1);
          assert_exact_result(&completed[0], id, expect::INVAL);
        }
      }

    }

    mod write {
      use super::*;

      #[test]
      fn success_reports_raw_result_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let mut payload = *b"writ";
        let mut total = 0usize;
        let mut id = 44_u64;
        while total < payload.len() {
          // SAFETY: this slice points into `payload`, which remains valid until
          // the backend completes the submitted write operation.
          let raw_buf = unsafe {
            RawBuf::from_raw_parts(
              payload[total..].as_mut_ptr(),
              payload.len() - total,
            )
          };

          push_op(&mut backend,
            id,
          Op::Write {
            fd: write_res.clone(),
            iovecs: nonnull_const(&raw_buf),
              iov_count: 1,
              offset: None,
              flags: WriteFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(
            n <= payload.len() - total,
            "write completed more bytes than remaining: {} > {}",
            n,
            payload.len() - total
          );
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());

        let mut buf = [0_u8; 4];
        let n = recv_payload(&read_res, &mut buf);
        assert_eq!(n, payload.len() as isize);
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn vectored_success_reports_total_byte_count() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let left = *b"vec";
        let right = *b"tor";
        let mut total = 0usize;
        let mut id = 66_u64;
        while total < 6 {
          let left_done = total.min(left.len());
          let right_done = total.saturating_sub(left.len()).min(right.len());
          let bufs = [
            RawBuf::new(&left[left_done..]),
            RawBuf::new(&right[right_done..]),
          ];

          push_op(&mut backend,
            id,
          Op::Write {
            fd: write_res.clone(),
            iovecs: nonnull_const(bufs.as_ptr()),
              iov_count: bufs.len(),
              offset: None,
              flags: WriteFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(n <= 6 - total, "write completed more bytes than remaining");
          total += n;
          id += 1;
        }

        assert_eq!(total, 6);

        let mut buf = [0_u8; 6];
        let n = recv_payload(&read_res, &mut buf);
        assert_eq!(n, 6);
        assert_eq!(&buf, b"vector");
      }

      #[test]
      fn invalid_fd_reports_ebadf() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let payload = *b"err!";
        // SAFETY: `payload` remains alive for the duration of this submitted
        // write operation and is only read from.
        let raw_buf = unsafe { RawBuf::from_raw_parts(payload.as_ptr().cast_mut(), payload.len()) };

        push_op(&mut backend,
          48,
          Op::Write {
            fd: invalid_fd_resource(),
            iovecs: nonnull_const(&raw_buf),
            iov_count: 1,
            offset: None,
            flags: WriteFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 48, expect::BADF);
      }

      #[test]
      fn invalid_offset_reports_einval() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (_read_res, write_res) = socket_pair();
        let payload = *b"arg!";
        // SAFETY: `payload` remains alive for the duration of this submitted
        // write operation and is only read from.
        let raw_buf = unsafe { RawBuf::from_raw_parts(payload.as_ptr().cast_mut(), payload.len()) };

        // The first offset past `i64::MAX`, and the one whose bits read as
        // `-1` (the kernel's "current position").
        for (id, offset) in [(72, i64::MAX as u64 + 1), (73, u64::MAX)] {
          push_op(&mut backend,
            id,
            Op::Write {
              fd: write_res.clone(),
              iovecs: nonnull_const(&raw_buf),
              iov_count: 1,
              offset: Some(offset),
              flags: WriteFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_single_completion(&mut backend);
          assert_exact_result(&completed, id, expect::INVAL);
        }
      }

      #[test]
      fn append_flag_writes_at_end_or_reports_enotsup() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("write-append");
        fs::write(&path, b"head").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        #[cfg(unix)]
        let file = own_raw(file.into_raw_fd() as isize);
        #[cfg(windows)]
        let file = own_raw(file.into_raw_handle() as isize);
        let payload = *b"tail";
        let raw_buf = RawBuf::new(&payload);

        // APPEND overrides the offset.
        push_op(&mut backend,
          73,
          Op::Write {
            fd: file,
            iovecs: nonnull_const(&raw_buf),
            iov_count: 1,
            offset: Some(0),
            flags: WriteFlags::APPEND,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        // A backend that cannot honour a flag reports `ENOTSUP` instead.
        if completed.result() != expect::NOTSUP {
          assert_exact_result(&completed, 73, payload.len() as isize);
          assert_eq!(fs::read(&path).unwrap(), b"headtail");
        }
        fs::remove_file(path).ok();
      }

    }

    mod recv {
      use super::*;

      #[test]
      fn success_reports_raw_result_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"pong";
        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let mut buf = [0_u8; 4];
        let mut total = 0usize;
        let mut id = 42_u64;
        while total < payload.len() {
          let bufs = [MsgBufMut::from_slice(&mut buf[total..])];

          push_op(&mut backend,
            id,
            Op::Recv {
              fd: read_res.clone(),
              msg: MsgRecv::new(&bufs),
              flags: RecvFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(n <= payload.len() - total, "recv completed more bytes than remaining");
          total += n;
          id += 1;
        }
        assert_eq!(total, payload.len());
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn peek_leaves_data_queued() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"peek";
        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let mut peeked = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut peeked)];

        push_op(&mut backend,
          47,
          Op::Recv {
            fd: read_res.clone(),
            msg: MsgRecv::new(&bufs),
            flags: RecvFlags::PEEK,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_positive_completion(&mut backend);
        assert_eq!(completed.registration_id(), 47);
        let n = completed.result() as usize;
        assert!(n <= payload.len(), "peek completed more bytes than sent");
        assert_eq!(&peeked[..n], &payload[..n]);

        let mut buf = [0_u8; 4];
        let n = recv_payload(&read_res, &mut buf);
        assert_eq!(n, payload.len() as isize, "peek must not consume data");
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn pending_then_success() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"wait";
        let mut buf = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut buf)];

        push_op(&mut backend,
          56,
          Op::Recv {
            fd: read_res.clone(),
            msg: MsgRecv::new(&bufs),
            flags: RecvFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "recv without available data must remain pending"
        );

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "recv must remain pending across repeated zero-timeout waits"
        );

        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let completed = wait_for_positive_completion(&mut backend);
        assert_eq!(completed.registration_id(), 56);
        let mut total = completed.result() as usize;

        let mut id = 57_u64;
        while total < payload.len() {
          let bufs = [MsgBufMut::from_slice(&mut buf[total..])];

          push_op(&mut backend,
            id,
            Op::Recv {
              fd: read_res.clone(),
              msg: MsgRecv::new(&bufs),
              flags: RecvFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(n <= payload.len() - total, "recv completed more bytes than remaining");
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn deferred_completion_is_delivered_exactly_once() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"once";
        let mut buf = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut buf)];

        push_op(&mut backend,
          561,
          Op::Recv {
            fd: read_res.clone(),
            msg: MsgRecv::new(&bufs),
            flags: RecvFlags::empty(),
          },
        );
        backend.flush().unwrap();

        assert!(
          wait_completions(&mut backend, Some(Duration::ZERO)).is_empty(),
          "recv without available data must remain pending"
        );

        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 561, payload.len() as isize);
        assert_eq!(&buf, &payload);

        for _ in 0..3 {
          let completed = wait_completions(&mut backend, Some(Duration::ZERO));
          assert!(
            completed.is_empty(),
            "a deferred recv completion already returned once must not be returned again"
          );
        }
      }

      #[test]
      fn vectored_success_reports_total_byte_count() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"vector";
        let wrote = send_payload(&write_res, &payload);
        assert_eq!(wrote, payload.len() as isize);

        let mut left = [0_u8; 3];
        let mut right = [0_u8; 3];
        let mut total = 0usize;
        let mut id = 67_u64;
        while total < payload.len() {
          let left_done = total.min(left.len());
          let right_done = total.saturating_sub(left.len()).min(right.len());
          let bufs = [
            MsgBufMut::from_slice(&mut left[left_done..]),
            MsgBufMut::from_slice(&mut right[right_done..]),
          ];

          push_op(&mut backend,
            id,
            Op::Recv {
              fd: read_res.clone(),
              msg: MsgRecv::new(&bufs),
              flags: RecvFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(n <= payload.len() - total, "recv completed more bytes than remaining");
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());
        assert_eq!(&left, b"vec");
        assert_eq!(&right, b"tor");
      }

      #[test]
      fn eof_reports_zero() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let mut buf = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut buf)];

        push_op(&mut backend,
          57,
          Op::Recv {
            fd: read_res.clone(),
            msg: MsgRecv::new(&bufs),
            flags: RecvFlags::empty(),
          },
        );
        backend.flush().unwrap();
        drop(write_res);

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 57, 0);
      }

      #[test]
      fn invalid_fd_reports_ebadf() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let mut buf = [0_u8; 4];
        let bufs = [MsgBufMut::from_slice(&mut buf)];

        push_op(&mut backend,
          49,
          Op::Recv {
            fd: invalid_fd_resource(),
            msg: MsgRecv::new(&bufs),
            flags: RecvFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 49, expect::BADF_SOCKET);
      }
    }

    mod send {
      use super::*;

      #[test]
      fn success_reports_raw_result_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let payload = *b"ping";
        let mut total = 0usize;
        let mut id = 41_u64;
        while total < payload.len() {
          let bufs = [MsgBuf::from_slice(&payload[total..])];

          push_op(&mut backend,
            id,
            Op::Send {
              fd: write_res.clone(),
              msg: MsgSend::new(&bufs, None),
              flags: SendFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(n <= payload.len() - total, "send completed more bytes than remaining");
          total += n;
          id += 1;
        }

        assert_eq!(total, payload.len());

        let mut buf = [0_u8; 4];
        let n = recv_payload(&read_res, &mut buf);
        assert_eq!(n, payload.len() as isize);
        assert_eq!(&buf, &payload);
      }

      #[test]
      fn vectored_success_reports_total_byte_count() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_res, write_res) = socket_pair();
        let left = *b"vec";
        let right = *b"tor";
        let mut total = 0usize;
        let mut id = 68_u64;
        while total < 6 {
          let left_done = total.min(left.len());
          let right_done = total.saturating_sub(left.len()).min(right.len());
          let bufs = [
            MsgBuf::from_slice(&left[left_done..]),
            MsgBuf::from_slice(&right[right_done..]),
          ];

          push_op(&mut backend,
            id,
            Op::Send {
              fd: write_res.clone(),
              msg: MsgSend::new(&bufs, None),
              flags: SendFlags::empty(),
            },
          );
          backend.flush().unwrap();

          let completed = wait_for_positive_completion(&mut backend);
          assert_eq!(completed.registration_id(), id);
          let n = completed.result() as usize;
          assert!(n <= 6 - total, "send completed more bytes than remaining");
          total += n;
          id += 1;
        }

        assert_eq!(total, 6);

        let mut buf = [0_u8; 6];
        let n = recv_payload(&read_res, &mut buf);
        assert_eq!(n, 6);
        assert_eq!(&buf, b"vector");
      }

      #[test]
      fn invalid_fd_reports_ebadf() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let payload = *b"oops";
        let bufs = [MsgBuf::from_slice(&payload)];

        push_op(&mut backend,
          50,
          Op::Send {
            fd: invalid_fd_resource(),
            msg: MsgSend::new(&bufs, None),
            flags: SendFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 50, expect::BADF_SOCKET);
      }
    }

    mod accept {
      use super::*;

      #[test]
      fn success_reports_success_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let mut listener = LocalListener::new("accept-ok");
        let listener_res = listener.resource();

        let mut storage = __lio_test_lio::backend::op::SocketAddrBuf::unspecified();

        push_op(&mut backend,
          52,
          Op::Accept {
            fd: listener_res,
            addr: NonNull::from(&mut storage),
          },
        );
        backend.flush().unwrap();

        let client = listener.spawn_client();

        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 52);
        assert!(
          completed.result() >= 0,
          "accept must succeed with a nonnegative fd, got {}",
          completed.result()
        );

        client.join().unwrap();
        close_raw(completed.result());
      }

      #[test]
      fn pending_then_success() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let mut listener = LocalListener::new("accept-pending");
        let listener_res = listener.resource();

        let mut storage = __lio_test_lio::backend::op::SocketAddrBuf::unspecified();

        push_op(&mut backend,
          58,
          Op::Accept {
            fd: listener_res,
            addr: NonNull::from(&mut storage),
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "accept without a pending client must remain pending"
        );

        let client = listener.spawn_client();

        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 58);
        assert!(
          completed.result() >= 0,
          "accept must succeed with a nonnegative fd, got {}",
          completed.result()
        );

        client.join().unwrap();
        close_raw(completed.result());
      }

      #[test]
      fn invalid_fd_reports_ebadf() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let mut storage = __lio_test_lio::backend::op::SocketAddrBuf::unspecified();

        push_op(&mut backend,
          51,
          Op::Accept {
            fd: invalid_fd_resource(),
            addr: NonNull::from(&mut storage),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 51, expect::BADF_SOCKET);
      }
    }

    mod connect {
      use super::*;

      #[test]
      fn missing_path_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("connect");
        fs::remove_file(&path).ok();

        let client_res = unix_stream_socket();
        let storage = unix_sockaddr_un(&path);

        push_op(&mut backend,
          46,
          Op::Connect {
            fd: client_res.clone(),
            addr: storage,
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert_eq!(completed.len(), 1);
        assert_exact_result(&completed[0], 46, expect::CONNECT_MISSING);
      }

      #[test]
      fn success_reports_zero_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let mut listener = LocalListener::new("connect-ok");
        let acceptor = listener.spawn_acceptor();
        let client_res = listener.client_socket();
        let storage = listener.addr();

        push_op(&mut backend,
          53,
          Op::Connect {
            fd: client_res,
            addr: storage,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 53, 0);

        acceptor.join().unwrap();
      }

      #[test]
      fn invalid_fd_reports_ebadf() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("connect-ebadf");
        let storage = unix_sockaddr_un(&path);

        push_op(&mut backend,
          59,
          Op::Connect {
            fd: invalid_fd_resource(),
            addr: storage,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 59, expect::BADF_SOCKET);
      }

      #[test]
      fn immediate_completion_is_surfaced_on_next_wait() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("connect-immediate");
        fs::remove_file(&path).ok();

        let client_res = unix_stream_socket();
        let storage = unix_sockaddr_un(&path);

        push_op(&mut backend,
          63,
          Op::Connect {
            fd: client_res,
            addr: storage,
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert_eq!(completed.len(), 1);
        assert_exact_result(&completed[0], 63, expect::CONNECT_MISSING);
      }
    }

    mod socket {
      use super::*;

      #[test]
      fn success_reports_nonnegative_fd_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        push_op(&mut backend,
          69,
          Op::Socket {
            domain: __lio_test_lio::backend::op::SockDomain::IPV4,
            ty: __lio_test_lio::backend::op::SockType::STREAM,
            proto: __lio_test_lio::backend::op::SockProto::DEFAULT,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 69);
        assert!(
          completed.result() >= 0,
          "socket must succeed with a nonnegative fd, got {}",
          completed.result()
        );
        close_raw(completed.result());
      }

      #[test]
      fn invalid_combo_reports_einval() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        push_op(&mut backend,
          79,
          Op::Socket {
            domain: __lio_test_lio::backend::op::SockDomain::UNIX,
            ty: __lio_test_lio::backend::op::SockType::STREAM,
            proto: __lio_test_lio::backend::op::SockProto::TCP,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 79, expect::INVAL_SOCKET);
      }
    }

    mod openat {
      use super::*;

      #[test]
      fn success_reports_nonnegative_fd_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("openat-ok");
        fs::write(&path, b"hello").unwrap();

        push_op(&mut backend,
          80,
          Op::OpenAt {
            dir_fd: Resource::cwd(),
            path: path.as_os_str().to_os_string(),
            flags: OpenFlags::READ,
            mode: FileMode::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 80);
        assert!(
          completed.result() >= 0,
          "openat must succeed with a nonnegative fd, got {}",
          completed.result()
        );
        close_raw(completed.result());
        fs::remove_file(path).ok();
      }

      #[test]
      fn missing_path_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("openat-missing");

        push_op(&mut backend,
          81,
          Op::OpenAt {
            dir_fd: Resource::cwd(),
            path: path.as_os_str().to_os_string(),
            flags: OpenFlags::READ,
            mode: FileMode::empty(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 81, expect::NOENT);
      }
    }

    mod stat {
      use super::*;

      #[test]
      fn success_reports_zero_and_fills_metadata() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("stat-ok");
        fs::write(&path, b"hello").unwrap();
        let mut out = FileStat::zeroed();

        push_op(&mut backend,
          82,
          Op::Stat {
            target: __lio_test_lio::backend::op::StatTarget::Path {
              dir_fd: Resource::cwd(),
              path: path.as_os_str().to_os_string(),
              follow_symlinks: true,
            },
            out: nonnull(&mut out),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 82, 0);
        assert!(out.is_file(), "stat should report regular-file metadata");
        assert_eq!(out.len(), 5);
        fs::remove_file(path).ok();
      }

      #[test]
      fn missing_path_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("stat-missing");
        let mut out = FileStat::zeroed();

        push_op(&mut backend,
          83,
          Op::Stat {
            target: __lio_test_lio::backend::op::StatTarget::Path {
              dir_fd: Resource::cwd(),
              path: path.as_os_str().to_os_string(),
              follow_symlinks: true,
            },
            out: nonnull(&mut out),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 83, expect::NOENT);
      }

      #[test]
      fn nofollow_reports_symlink_metadata() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let target = temp_path("stat-target");
        let link = temp_path("stat-link");
        fs::write(&target, b"hello").unwrap();
        symlink_file(&target, &link);
        let mut out = FileStat::zeroed();

        push_op(&mut backend,
          84,
          Op::Stat {
            target: __lio_test_lio::backend::op::StatTarget::Path {
              dir_fd: Resource::cwd(),
              path: link.as_os_str().to_os_string(),
              follow_symlinks: false,
            },
            out: nonnull(&mut out),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 84, 0);
        assert!(out.is_symlink(), "nofollow stat should report the symlink");
        fs::remove_file(link).ok();
        fs::remove_file(target).ok();
      }
    }

    mod readdir {
      use super::*;

      #[test]
      fn success_reports_zero_and_fills_entries() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let dir = temp_path("readdir-ok");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("file.txt"), b"hello").unwrap();
        fs::create_dir(dir.join("nested")).unwrap();

        let dir_fd = open_dir(&dir);
        let mut raw = vec![0u8; 4096];
        let mut out =
          vec![__lio_test_lio::backend::op::DirEntryRef::default(); 32];
        let mut result = __lio_test_lio::backend::op::ReadDirResult::default();
        let mut opaque: *mut () = std::ptr::null_mut();
        let mut opaque_drop: Option<__lio_test_lio::backend::op::OpaqueDropFn> =
          None;

        push_op(&mut backend,
          85,
          Op::ReadDir {
            fd: dir_fd,
            raw_buf: nonnull(raw.as_mut_ptr()),
            raw_cap: raw.len(),
            entries: nonnull(out.as_mut_ptr()),
            entries_cap: out.len(),
            opaque: nonnull(&mut opaque),
            opaque_drop: nonnull(&mut opaque_drop),
            out: nonnull(&mut result),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 85, 0);
        assert!(
          out[..result.entries].iter().any(|entry| {
            &raw[entry.name_offset as usize
              ..entry.name_offset as usize + entry.name_len as usize]
              == b"file.txt"
          }),
          "readdir should return regular files"
        );
        assert!(
          out[..result.entries].iter().any(|entry| {
            &raw[entry.name_offset as usize
              ..entry.name_offset as usize + entry.name_len as usize]
              == b"nested"
          }),
          "readdir should return subdirectories"
        );
        assert!(
          out[..result.entries].iter().all(|entry| {
            let name = &raw[entry.name_offset as usize
              ..entry.name_offset as usize + entry.name_len as usize];
            name != b"." && name != b".."
          }),
          "readdir should omit dot entries"
        );

        fs::remove_file(dir.join("file.txt")).ok();
        fs::remove_dir(dir.join("nested")).ok();
        fs::remove_dir(dir).ok();
      }
    }

    mod unlinkat {
      use super::*;

      #[test]
      fn success_reports_zero_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("unlinkat-ok");
        fs::write(&path, b"hello").unwrap();

        push_op(&mut backend,
          70,
          Op::UnlinkAt {
            dir_fd: Resource::cwd(),
            path: path.as_os_str().to_os_string(),
            kind: UnlinkKind::File,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 70, 0);
        assert!(!path.exists(), "unlinkat should remove the file");
      }

      #[test]
      fn missing_path_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("unlinkat-missing");

        push_op(&mut backend,
          71,
          Op::UnlinkAt {
            dir_fd: Resource::cwd(),
            path: path.as_os_str().to_os_string(),
            kind: UnlinkKind::File,
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 71, expect::NOENT);
      }
    }

    mod renameat {
      use super::*;

      #[test]
      fn success_reports_zero_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let source = temp_path("renameat-source");
        let dest = temp_path("renameat-dest");
        fs::write(&source, b"hello").unwrap();
        fs::remove_file(&dest).ok();

        push_op(&mut backend,
          72,
          Op::RenameAt {
            old_dir_fd: Resource::cwd(),
            old_path: source.as_os_str().to_os_string(),
            new_dir_fd: Resource::cwd(),
            new_path: dest.as_os_str().to_os_string(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 72, 0);
        assert!(!source.exists(), "renameat should remove the old path");
        assert!(dest.exists(), "renameat should create the new path");
        fs::remove_file(dest).ok();
      }

      #[test]
      fn missing_source_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let source = temp_path("renameat-missing");
        let dest = temp_path("renameat-target");
        fs::remove_file(&dest).ok();

        push_op(&mut backend,
          73,
          Op::RenameAt {
            old_dir_fd: Resource::cwd(),
            old_path: source.as_os_str().to_os_string(),
            new_dir_fd: Resource::cwd(),
            new_path: dest.as_os_str().to_os_string(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 73, expect::NOENT);
      }
    }

    mod mkdirat {
      use super::*;

      #[test]
      fn success_reports_zero_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("mkdirat-ok");
        fs::remove_dir(&path).ok();

        push_op(&mut backend,
          74,
          Op::MkdirAt {
            dir_fd: Resource::cwd(),
            path: path.as_os_str().to_os_string(),
            mode: FileMode::from_octal(0o755).unwrap(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 74, 0);
        assert!(path.is_dir(), "mkdirat should create the directory");
        fs::remove_dir(path).ok();
      }

      #[test]
      fn existing_path_reports_eexist() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = temp_path("mkdirat-exists");
        fs::create_dir(&path).unwrap();

        push_op(&mut backend,
          75,
          Op::MkdirAt {
            dir_fd: Resource::cwd(),
            path: path.as_os_str().to_os_string(),
            mode: FileMode::from_octal(0o755).unwrap(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 75, expect::EXIST);
        fs::remove_dir(path).ok();
      }
    }

    mod linkat {
      use super::*;

      #[test]
      fn hard_link_success_reports_zero_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let source = temp_path("linkat-hard-source");
        let dest = temp_path("linkat-hard-dest");
        fs::write(&source, b"hello").unwrap();
        fs::remove_file(&dest).ok();

        push_op(&mut backend,
          76,
          Op::LinkAt {
            kind: LinkKind::Hard,
            source_dir_fd: Resource::cwd(),
            source_path: source.as_os_str().to_os_string(),
            new_dir_fd: Resource::cwd(),
            new_path: dest.as_os_str().to_os_string(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 76, 0);
        assert!(dest.exists(), "hard link should create the destination");
        fs::remove_file(dest).ok();
        fs::remove_file(source).ok();
      }

      #[test]
      fn soft_link_success_reports_zero_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let source = temp_path("linkat-soft-source");
        let dest = temp_path("linkat-soft-dest");
        fs::write(&source, b"hello").unwrap();
        fs::remove_file(&dest).ok();

        push_op(&mut backend,
          77,
          Op::LinkAt {
            kind: LinkKind::Soft,
            source_dir_fd: Resource::cwd(),
            source_path: source.as_os_str().to_os_string(),
            new_dir_fd: Resource::cwd(),
            new_path: dest.as_os_str().to_os_string(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 77, 0);
        assert!(
          fs::symlink_metadata(&dest).unwrap().file_type().is_symlink(),
          "soft link should create a symlink destination"
        );
        fs::remove_file(dest).ok();
        fs::remove_file(source).ok();
      }

      #[test]
      fn hard_link_missing_source_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let source = temp_path("linkat-missing-source");
        let dest = temp_path("linkat-missing-dest");
        fs::remove_file(&dest).ok();

        push_op(&mut backend,
          78,
          Op::LinkAt {
            kind: LinkKind::Hard,
            source_dir_fd: Resource::cwd(),
            source_path: source.as_os_str().to_os_string(),
            new_dir_fd: Resource::cwd(),
            new_path: dest.as_os_str().to_os_string(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 78, expect::NOENT);
      }
    }

    mod readlinkat {
      use super::*;

      #[test]
      fn success_reports_target_length_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let target = temp_path("readlinkat-target");
        let link = temp_path("readlinkat-link");
        fs::write(&target, b"hello").unwrap();
        symlink_file(&target, &link);
        let mut buf = [0_u8; 512];

        push_op(&mut backend,
          79,
          Op::ReadlinkAt {
            dir_fd: Resource::cwd(),
            path: link.as_os_str().to_os_string(),
            buf: nonnull(buf.as_mut_ptr()),
            buf_len: buf.len(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 79, target.as_os_str().as_encoded_bytes().len() as isize);
        assert_eq!(
          &buf[..target.as_os_str().as_encoded_bytes().len()],
          target.as_os_str().as_encoded_bytes()
        );

        fs::remove_file(link).ok();
        fs::remove_file(target).ok();
      }

      #[test]
      fn missing_path_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let link = temp_path("readlinkat-missing");
        let mut buf = [0_u8; 64];

        push_op(&mut backend,
          80,
          Op::ReadlinkAt {
            dir_fd: Resource::cwd(),
            path: link.as_os_str().to_os_string(),
            buf: nonnull(buf.as_mut_ptr()),
            buf_len: buf.len(),
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 80, expect::NOENT);
      }
    }

    mod getcwd {
      use super::*;

      #[test]
      fn success_reports_cwd_length_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let expected = std::env::current_dir().unwrap().into_os_string();
        let mut out = OsString::new();

        push_op(&mut backend, 81, Op::GetCwd { out: nonnull(&mut out) });
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 81, 0);
        assert_eq!(out, expected);
      }

      #[test]
      fn replaces_existing_output() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let expected = std::env::current_dir().unwrap().into_os_string();
        let mut out = OsString::from("stale");

        push_op(&mut backend, 82, Op::GetCwd { out: nonnull(&mut out) });
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 82, 0);
        assert_eq!(out, expected);
      }
    }

    mod spawn {
      use super::*;

      #[test]
      fn success_reports_positive_pid_and_id() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        push_op(&mut backend,
          83,
          Op::Spawn { spec: success_spawn_spec() },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_eq!(completed.registration_id(), 83);
        assert!(completed.result() > 0, "spawn should return a positive pid");
        reap_child(completed.result());
      }

      #[test]
      fn missing_executable_reports_enoent() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        push_op(&mut backend,
          84,
          Op::Spawn {
            spec: SpawnSpec {
              program: OsString::from("/definitely/missing/spawn-binary"),
              args: vec![OsString::from("spawn-binary")],
              env: None,
            },
          },
        );
        backend.flush().unwrap();

        let completed = wait_for_single_completion(&mut backend);
        assert_exact_result(&completed, 84, expect::SPAWN_MISSING);
      }
    }

    mod batching {
      use super::*;

      #[test]
      fn multiple_immediate_completions_are_returned_together() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("batch-connect");
        let storage = unix_sockaddr_un(&path);

        let mut buf = [0_u8; 4];
        let mut raw_buf = raw_buf_from_mut_slice(&mut buf);

        push_op(&mut backend,
          60,
          Op::Connect {
            fd: invalid_fd_resource(),
            addr: storage,
          },
        );
        push_op(&mut backend,
          61,
          Op::Read {
            fd: invalid_fd_resource(),
            iovecs: nonnull(&mut raw_buf),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        backend.flush().unwrap();

        let mut completed =
          wait_completions(&mut backend, Some(Duration::ZERO));
        completed.sort_by_key(|item| item.registration_id());

        assert_eq!(completed.len(), 2);
        assert_exact_result(&completed[0], 60, expect::BADF_SOCKET);
        assert_exact_result(&completed[1], 61, expect::BADF);
      }

      #[test]
      fn wait_buffer_is_cleared_after_consumption() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("buffer-reuse");
        let storage = unix_sockaddr_un(&path);

        push_op(&mut backend,
          62,
          Op::Connect {
            fd: invalid_fd_resource(),
            addr: storage,
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert_eq!(completed.len(), 1);
        assert_exact_result(&completed[0], 62, expect::BADF_SOCKET);

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "completed wait buffer must not retain stale completions"
        );
      }

      #[test]
      fn second_flush_does_not_replay_already_submitted_work() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("flush-idempotent");
        let storage = unix_sockaddr_un(&path);

        push_op(&mut backend,
          63,
          Op::Connect {
            fd: invalid_fd_resource(),
            addr: storage,
          },
        );
        backend.flush().unwrap();
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert_eq!(completed.len(), 1);
        assert_exact_result(&completed[0], 63, expect::BADF_SOCKET);

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "a second flush() must not replay already-submitted work"
        );
      }

      #[test]
      fn completions_are_not_duplicated_across_waits() {
        let mut backend = new_backend();
        backend.init(64).unwrap();

        let path = unix_socket_path("no-dup");
        let storage = unix_sockaddr_un(&path);

        push_op(&mut backend,
          64,
          Op::Connect {
            fd: invalid_fd_resource(),
            addr: storage,
          },
        );
        backend.flush().unwrap();

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert_eq!(completed.len(), 1);
        assert_exact_result(&completed[0], 64, expect::BADF_SOCKET);

        let completed = wait_completions(&mut backend, Some(Duration::ZERO));
        assert!(
          completed.is_empty(),
          "a completion already returned once must not be returned again"
        );
      }

      #[test]
      fn mixed_pending_batch_completes_each_registration_exactly_once() {

        let mut backend = new_backend();
        backend.init(64).unwrap();

        let (read_a, write_a) = socket_pair();
        let (read_b, write_b) = socket_pair();
        let payload_a = *b"ab";
        let payload_b = *b"cd";
        let mut buf_a = [0_u8; 2];
        let mut buf_b = [0_u8; 2];
        // SAFETY: `buf_a` is a live stack buffer that remains valid until the
        // read completion is collected in this test.
        let mut raw_a = unsafe { RawBuf::from_raw_parts(buf_a.as_mut_ptr(), buf_a.len()) };
        // SAFETY: `buf_b` is a live stack buffer that remains valid until the
        // read completion is collected in this test.
        let mut raw_b = unsafe { RawBuf::from_raw_parts(buf_b.as_mut_ptr(), buf_b.len()) };

        push_op(&mut backend,
          701,
          Op::Read {
            fd: read_a.clone(),
            iovecs: nonnull(&mut raw_a),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        push_op(&mut backend,
          702,
          Op::Read {
            fd: read_b.clone(),
            iovecs: nonnull(&mut raw_b),
            iov_count: 1,
            offset: None,
            flags: ReadFlags::empty(),
          },
        );
        backend.flush().unwrap();

        assert!(
          wait_completions(&mut backend, Some(Duration::ZERO)).is_empty(),
          "pending reads must not complete before data arrives"
        );

        let wrote_a = send_payload(&write_a, &payload_a);
        let wrote_b = send_payload(&write_b, &payload_b);
        assert_eq!(wrote_a, payload_a.len() as isize);
        assert_eq!(wrote_b, payload_b.len() as isize);

        let mut seen = std::collections::BTreeMap::new();
        for _ in 0..10 {
          let completed =
            wait_completions(&mut backend, Some(Duration::from_millis(10)));
          for item in completed {
            let previous = seen.insert(item.registration_id(), item.result());
            assert!(
              previous.is_none(),
              "registration {} completed more than once",
              item.registration_id()
            );
          }
          if seen.len() == 2 {
            break;
          }
        }

        assert_eq!(seen.len(), 2, "expected exactly two completions");
        assert_eq!(seen.get(&701), Some(&(payload_a.len() as isize)));
        assert_eq!(seen.get(&702), Some(&(payload_b.len() as isize)));
        assert_eq!(&buf_a, &payload_a);
        assert_eq!(&buf_b, &payload_b);

        for _ in 0..3 {
          let completed = wait_completions(&mut backend, Some(Duration::ZERO));
          assert!(
            completed.is_empty(),
            "completed pending batch registrations must not be returned again"
          );
        }
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
  Oneshot,
  Stream,
}

/// One scripted contract step for an `OpModel` contract test.
pub struct ContractStep<M: OpModelContract> {
  pub assert_action: fn(&M::Action) -> bool,
  pub before_complete: fn(&mut M),
  pub completion: M::Completion,
  pub assert_result: fn(&M::Result) -> bool,
}

impl<M: OpModelContract> ContractStep<M> {
  pub fn new(
    assert_action: fn(&M::Action) -> bool,
    completion: M::Completion,
    assert_result: fn(&M::Result) -> bool,
  ) -> Self {
    Self { assert_action, before_complete: |_| {}, completion, assert_result }
  }

  pub fn with_setup(
    assert_action: fn(&M::Action) -> bool,
    before_complete: fn(&mut M),
    completion: M::Completion,
    assert_result: fn(&M::Result) -> bool,
  ) -> Self {
    Self { assert_action, before_complete, completion, assert_result }
  }
}

/// Test fixture for generating generic `OpModel` contract tests per type.
pub trait OpModelContract: Sized {
  type Action;
  type Completion;
  type Result;

  fn contract_kind() -> ContractKind;
  fn contract_model() -> Self;
  fn contract_steps() -> Vec<ContractStep<Self>>;

  fn action(&mut self) -> Self::Action;
  fn complete(&mut self, completion: Self::Completion) -> Self::Result;

  fn is_again(result: &Self::Result) -> bool;
  fn is_yield(result: &Self::Result) -> bool;
  fn is_done(result: &Self::Result) -> bool;
}

/// Shared contract tests for `OpModel`-like implementations described through
/// [`lio_test::OpModelContract`].
#[macro_export]
macro_rules! test_op_model_contract {
  ($model_ty:ty) => {
    mod op_model_contract {
      use super::*;

      #[test]
      fn scripted_contract() {
        let mut model =
          <$model_ty as ::lio_test::OpModelContract>::contract_model();
        let kind = <$model_ty as ::lio_test::OpModelContract>::contract_kind();
        let steps =
          <$model_ty as ::lio_test::OpModelContract>::contract_steps();
        assert!(!steps.is_empty(), "contract script must not be empty");

        let mut saw_done = false;
        let mut saw_yield = false;
        let mut saw_terminal = false;
        let mut must_remain_live_after_script = false;
        let last_index = steps.len() - 1;

        for (index, step) in steps.into_iter().enumerate() {
          assert!(
            !saw_terminal,
            "contract script continued after terminal completion"
          );
          let action =
            <$model_ty as ::lio_test::OpModelContract>::action(&mut model);
          assert!(
            (step.assert_action)(&action),
            "action() did not satisfy the model contract"
          );
          (step.before_complete)(&mut model);
          let result = <$model_ty as ::lio_test::OpModelContract>::complete(
            &mut model,
            step.completion,
          );
          assert!(
            (step.assert_result)(&result),
            "complete() did not satisfy the model contract"
          );

          if <$model_ty as ::lio_test::OpModelContract>::is_again(&result) {
            must_remain_live_after_script = index == last_index;
          } else if <$model_ty as ::lio_test::OpModelContract>::is_yield(
            &result,
          ) {
            saw_yield = true;
            must_remain_live_after_script = index == last_index;
            assert!(
              kind != ::lio_test::ContractKind::Oneshot,
              "oneshot models must not yield"
            );
          } else if <$model_ty as ::lio_test::OpModelContract>::is_done(&result)
          {
            saw_done = true;
            saw_terminal = true;
            assert_eq!(
              index, last_index,
              "Done must be the final scripted step"
            );
          } else {
            panic!("contract result was neither Again, Yield, nor Done");
          }
        }

        if must_remain_live_after_script {
          let _ =
            <$model_ty as ::lio_test::OpModelContract>::action(&mut model);
        }

        match kind {
          ::lio_test::ContractKind::Oneshot => {
            assert!(
              saw_done,
              "oneshot model contract must terminate with Done"
            );
            assert!(
              !saw_yield,
              "oneshot model contract must not contain Yield"
            );
          }
          ::lio_test::ContractKind::Stream => {
            assert!(
              saw_yield || saw_done,
              "stream model contract must produce at least one Yield or Done"
            );
          }
        }
      }
    }
  };
}
