//! Async process spawning and management for lio.
//!
//! This module provides high-level abstractions for spawning and managing child processes
//! using lio's async runtime. It offers a familiar API similar to `std::process` but with
//! async capabilities.
//!
//! # Main Types
//!
//! - [`Command`]: A builder for spawning processes with customizable arguments and environment
//! - [`Child`]: A handle to a running child process
//! - [`ExitStatus`]: The exit status of a completed process
//!
//! # Examples
//!
//! ## Spawning a simple command
//!
//! ```rust,no_run
//! use lio::process::Command;
//!
//! async fn example() -> std::io::Result<()> {
//!     let status = Command::new("/bin/echo")
//!         .arg("Hello, world!")
//!         .status()
//!         .await?;
//!
//!     println!("Process exited with: {:?}", status);
//!     Ok(())
//! }
//! ```
//!
//! ## Spawning and waiting separately
//!
//! ```rust,no_run
//! use lio::process::Command;
//!
//! async fn example() -> std::io::Result<()> {
//!     let mut child = Command::new("/bin/sleep")
//!         .arg("1")
//!         .spawn()
//!         .await?;
//!
//!     println!("Child PID: {}", child.id());
//!
//!     let status = child.wait().await?;
//!     println!("Child exited with code: {:?}", status.code());
//!     Ok(())
//! }
//! ```
//!
//! ## Using environment variables
//!
//! ```rust,no_run
//! use lio::process::Command;
//!
//! async fn example() -> std::io::Result<()> {
//!     let status = Command::new("/bin/sh")
//!         .arg("-c")
//!         .arg("echo $MY_VAR")
//!         .env("MY_VAR", "Hello from env!")
//!         .status()
//!         .await?;
//!
//!     Ok(())
//! }
//! ```

#![cfg(unix)]

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::api::resource::Resource;
use crate::api::{
  io::Io, op::TypedOp, ops::WaitOptions, ops::WaitTarget, ops::Waitid,
};
use crate::backend::op::Op;

use std::os::fd::{AsRawFd, FromRawFd, RawFd};

// Re-export WaitStatus for convenience
pub use crate::api::ops::WaitStatus;

/// The exit status of a finished process.
///
/// This type wraps the underlying `WaitStatus` to provide a more convenient API
/// similar to `std::process::ExitStatus`.
#[derive(Debug, Clone)]
pub struct ExitStatus {
  status: WaitStatus,
}

impl ExitStatus {
  /// Returns the exit code of the process, if it exited normally.
  ///
  /// Returns `None` if the process was terminated by a signal.
  pub fn code(&self) -> Option<i32> {
    self.status.exit_code()
  }

  /// Returns `true` if the process exited successfully (exit code 0).
  pub fn success(&self) -> bool {
    self.code() == Some(0)
  }

  /// Returns `true` if the process exited normally (not killed by a signal).
  pub fn exited(&self) -> bool {
    self.status.exited()
  }

  /// Returns `true` if the process was terminated by a signal.
  pub fn signaled(&self) -> bool {
    self.status.signaled()
  }

  /// Returns the signal that terminated the process, if any.
  pub fn signal(&self) -> Option<i32> {
    self.status.signal()
  }

  /// Returns the underlying `WaitStatus` for access to all fields.
  pub fn into_inner(self) -> WaitStatus {
    self.status
  }
}

impl From<WaitStatus> for ExitStatus {
  fn from(status: WaitStatus) -> Self {
    Self { status }
  }
}

// ============================================================================
// Stdio configuration
// ============================================================================

/// Describes what to do with a standard I/O stream for a child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stdio {
  /// Inherit the corresponding stdio from the parent process.
  #[default]
  Inherit,
  /// Create a pipe between the parent and child process.
  Piped,
  /// Redirect to /dev/null.
  Null,
}

// ============================================================================
// Child stdio handles
// ============================================================================

/// A handle to the stdin of a child process.
///
/// This handle can be used to write data to the child's stdin.
#[derive(Debug)]
pub struct ChildStdin {
  inner: Resource,
}

impl ChildStdin {
  /// Writes data to the child's stdin.
  pub fn write(&self, buf: Vec<u8>) -> Io<crate::api::ops::Send<Vec<u8>>> {
    crate::api::send(&self.inner, buf, None)
  }
}

impl AsRawFd for ChildStdin {
  fn as_raw_fd(&self) -> RawFd {
    self.inner.as_raw_fd()
  }
}

/// A handle to the stdout of a child process.
///
/// This handle can be used to read data from the child's stdout.
#[derive(Debug)]
pub struct ChildStdout {
  inner: Resource,
}

impl ChildStdout {
  /// Reads data from the child's stdout.
  pub fn read(&self, buf: Vec<u8>) -> Io<crate::api::ops::Recv<Vec<u8>>> {
    crate::api::recv(&self.inner, buf, None)
  }
}

impl AsRawFd for ChildStdout {
  fn as_raw_fd(&self) -> RawFd {
    self.inner.as_raw_fd()
  }
}

/// A handle to the stderr of a child process.
///
/// This handle can be used to read data from the child's stderr.
#[derive(Debug)]
pub struct ChildStderr {
  inner: Resource,
}

impl ChildStderr {
  /// Reads data from the child's stderr.
  pub fn read(&self, buf: Vec<u8>) -> Io<crate::api::ops::Recv<Vec<u8>>> {
    crate::api::recv(&self.inner, buf, None)
  }
}

impl AsRawFd for ChildStderr {
  fn as_raw_fd(&self) -> RawFd {
    self.inner.as_raw_fd()
  }
}

/// A handle to a running child process.
///
/// This struct represents a child process that has been spawned but may not have
/// exited yet. It provides methods to wait for the process to complete and retrieve
/// its exit status.
///
/// # Examples
///
/// ```rust,no_run
/// use lio::process::Command;
/// use lio::Lio;
///
/// fn example() -> std::io::Result<()> {
///     let lio = Lio::new(64)?;
///     let mut child = Command::new("/bin/sleep")
///         .arg("1")
///         .spawn()
///         .with_lio(&lio)
///         .send()
///         .recv()?;
///
///     // Do other work while child runs...
///
///     let status = child.wait().with_lio(&lio).send().recv()?;
///     println!("Child exited: {:?}", status.code());
///     Ok(())
/// }
/// ```
/// A running or finished child process.
///
/// The child process can have piped stdio if configured with [`Stdio::Piped`].
#[derive(Debug)]
pub struct Child {
  pid: libc::pid_t,
  /// Track whether we've already waited on this child.
  waited: bool,
  /// Handle to the child's stdin (if piped).
  pub stdin: Option<ChildStdin>,
  /// Handle to the child's stdout (if piped).
  pub stdout: Option<ChildStdout>,
  /// Handle to the child's stderr (if piped).
  pub stderr: Option<ChildStderr>,
}

impl Child {
  /// Returns the OS-assigned process ID of the child.
  pub fn id(&self) -> u32 {
    self.pid as u32
  }

  /// Returns the raw PID as used by system calls.
  pub fn pid(&self) -> libc::pid_t {
    self.pid
  }

  /// Waits for the child process to exit and returns its exit status.
  ///
  /// Returns an `Io` operation that can be awaited or used with `.with_lio()`.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::process::Command;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let mut child = Command::new("/bin/true").spawn().await?;
  ///     let status = child.wait().await?;
  ///     assert!(status.success());
  ///     Ok(())
  /// }
  /// ```
  pub fn wait(&mut self) -> Io<Wait> {
    self.waited = true;
    Io::from_op(Wait::new(self.pid))
  }

  /// Attempts to collect the exit status of the child if it has already exited.
  ///
  /// This method will not block. If the child has not yet exited, returns `Ok(None)`.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::process::Command;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let mut child = Command::new("/bin/sleep")
  ///         .arg("10")
  ///         .spawn()
  ///         .await?;
  ///
  ///     // Check if exited without blocking
  ///     if let Some(status) = child.try_wait().await? {
  ///         println!("Child exited with: {:?}", status.code());
  ///     } else {
  ///         println!("Child still running");
  ///     }
  ///     Ok(())
  /// }
  /// ```
  pub fn try_wait(&mut self) -> Io<TryWait> {
    Io::from_op(TryWait::new(self.pid))
  }

  /// Sends SIGKILL to the child process.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::process::Command;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let mut child = Command::new("/bin/sleep")
  ///         .arg("100")
  ///         .spawn()
  ///         .await?;
  ///
  ///     // Terminate the child
  ///     child.kill()?;
  ///     let status = child.wait().await?;
  ///     assert!(status.signaled());
  ///     Ok(())
  /// }
  /// ```
  pub fn kill(&self) -> io::Result<()> {
    self.signal(libc::SIGKILL)
  }

  /// Sends a specific signal to the child process.
  pub fn signal(&self, sig: libc::c_int) -> io::Result<()> {
    // SAFETY: self.pid is a valid process ID from posix_spawn
    let ret = unsafe { libc::kill(self.pid, sig) };
    if ret == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
  }
}

impl Drop for Child {
  fn drop(&mut self) {
    // If we haven't waited on the child yet, we should avoid leaving zombies.
    // However, we can't do async cleanup in drop, so we just send SIGKILL
    // and reap synchronously.
    if !self.waited {
      // Best effort: try to kill and reap synchronously
      let _ = self.signal(libc::SIGKILL);
      // SAFETY: self.pid is a valid process ID, we're passing valid pointers
      unsafe {
        let mut status: libc::c_int = 0;
        libc::waitpid(self.pid, &mut status, 0);
      }
    }
  }
}

// ============================================================================
// Wait TypedOp
// ============================================================================

/// TypedOp for waiting on a child process.
pub struct Wait {
  inner: Waitid,
}

impl Wait {
  fn new(pid: libc::pid_t) -> Self {
    Self {
      inner: crate::api::ops::Waitid::new(
        WaitTarget::Pid(pid),
        WaitOptions::EXITED,
      ),
    }
  }
}

impl TypedOp for Wait {
  type Result = io::Result<ExitStatus>;

  fn into_op(&mut self) -> Op {
    self.inner.into_op()
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let status = self.inner.extract_result(res)?;
    match status {
      Some(s) => Ok(ExitStatus::from(s)),
      None => Err(io::Error::other("waitid returned no status")),
    }
  }
}

// ============================================================================
// TryWait TypedOp
// ============================================================================

/// TypedOp for non-blocking wait on a child process.
pub struct TryWait {
  inner: Waitid,
}

impl TryWait {
  fn new(pid: libc::pid_t) -> Self {
    Self {
      inner: crate::api::ops::Waitid::new(
        WaitTarget::Pid(pid),
        WaitOptions::EXITED | WaitOptions::NOHANG,
      ),
    }
  }
}

impl TypedOp for TryWait {
  type Result = io::Result<Option<ExitStatus>>;

  fn into_op(&mut self) -> Op {
    self.inner.into_op()
  }

  fn extract_result(self, res: isize) -> Self::Result {
    let status = self.inner.extract_result(res)?;
    Ok(status.map(ExitStatus::from))
  }
}

// ============================================================================
// SpawnChild TypedOp
// ============================================================================

/// Pipe file descriptors (read_end, write_end).
struct Pipe {
  read: RawFd,
  write: RawFd,
}

impl Pipe {
  fn new() -> io::Result<Self> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid mutable array with 2 elements
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret < 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(Pipe { read: fds[0], write: fds[1] })
  }

  fn close_read(&self) {
    // SAFETY: self.read is a valid file descriptor from pipe()
    unsafe { libc::close(self.read) };
  }

  fn close_write(&self) {
    // SAFETY: self.write is a valid file descriptor from pipe()
    unsafe { libc::close(self.write) };
  }
}

/// TypedOp for spawning a child process.
pub struct SpawnChild {
  inner: Option<crate::api::ops::Spawn>,
  stdin_cfg: Stdio,
  stdout_cfg: Stdio,
  stderr_cfg: Stdio,
  /// Pipe for stdin (write end for parent, read end for child)
  stdin_pipe: Option<Pipe>,
  /// Pipe for stdout (read end for parent, write end for child)
  stdout_pipe: Option<Pipe>,
  /// Pipe for stderr (read end for parent, write end for child)
  stderr_pipe: Option<Pipe>,
  /// File actions for posix_spawn
  file_actions: Option<libc::posix_spawn_file_actions_t>,
  /// /dev/null fd for Null stdio
  dev_null: Option<RawFd>,
}

impl SpawnChild {
  fn new(
    path: CString,
    argv: Vec<CString>,
    envp: Option<Vec<CString>>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
  ) -> io::Result<Self> {
    let mut this = Self {
      inner: Some(crate::api::ops::Spawn::new(path, argv, envp)),
      stdin_cfg: stdin,
      stdout_cfg: stdout,
      stderr_cfg: stderr,
      stdin_pipe: None,
      stdout_pipe: None,
      stderr_pipe: None,
      file_actions: None,
      dev_null: None,
    };

    // Create pipes and file actions if needed
    this.setup_stdio()?;

    Ok(this)
  }

  fn setup_stdio(&mut self) -> io::Result<()> {
    // Check if we need file actions
    if self.stdin_cfg == Stdio::Inherit
      && self.stdout_cfg == Stdio::Inherit
      && self.stderr_cfg == Stdio::Inherit
    {
      return Ok(());
    }

    // Initialize file actions
    // SAFETY: posix_spawn_file_actions_t is safe to zero-initialize
    let mut file_actions: libc::posix_spawn_file_actions_t =
      unsafe { std::mem::zeroed() };
    // SAFETY: file_actions is a valid pointer to uninitialized posix_spawn_file_actions_t
    let ret = unsafe { libc::posix_spawn_file_actions_init(&mut file_actions) };
    if ret != 0 {
      return Err(io::Error::from_raw_os_error(ret));
    }

    // Open /dev/null if needed
    let needs_null = self.stdin_cfg == Stdio::Null
      || self.stdout_cfg == Stdio::Null
      || self.stderr_cfg == Stdio::Null;
    if needs_null {
      // SAFETY: c"/dev/null" is a valid path, O_RDWR is a valid flag
      let null_fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
      if null_fd < 0 {
        // SAFETY: file_actions was successfully initialized above
        unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
        return Err(io::Error::last_os_error());
      }
      self.dev_null = Some(null_fd);
    }

    // Setup stdin
    match self.stdin_cfg {
      Stdio::Inherit => {}
      Stdio::Piped => {
        let pipe = Pipe::new()?;
        // Child reads from pipe.read
        // SAFETY: file_actions is initialized, pipe.read is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            pipe.read,
            0,
          )
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
        // Close the unused ends in child
        // SAFETY: file_actions is initialized, pipe.write is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_addclose(&mut file_actions, pipe.write)
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
        self.stdin_pipe = Some(pipe);
      }
      Stdio::Null => {
        // SAFETY: file_actions is initialized, dev_null is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            self.dev_null.unwrap(),
            0,
          )
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
      }
    }

    // Setup stdout
    match self.stdout_cfg {
      Stdio::Inherit => {}
      Stdio::Piped => {
        let pipe = Pipe::new()?;
        // Child writes to pipe.write
        // SAFETY: file_actions is initialized, pipe.write is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            pipe.write,
            1,
          )
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
        // Close the unused ends in child
        // SAFETY: file_actions is initialized, pipe.read is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_addclose(&mut file_actions, pipe.read)
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
        self.stdout_pipe = Some(pipe);
      }
      Stdio::Null => {
        // SAFETY: file_actions is initialized, dev_null is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            self.dev_null.unwrap(),
            1,
          )
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
      }
    }

    // Setup stderr
    match self.stderr_cfg {
      Stdio::Inherit => {}
      Stdio::Piped => {
        let pipe = Pipe::new()?;
        // Child writes to pipe.write
        // SAFETY: file_actions is initialized, pipe.write is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            pipe.write,
            2,
          )
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
        // Close the unused ends in child
        // SAFETY: file_actions is initialized, pipe.read is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_addclose(&mut file_actions, pipe.read)
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
        self.stderr_pipe = Some(pipe);
      }
      Stdio::Null => {
        // SAFETY: file_actions is initialized, dev_null is a valid fd
        let ret = unsafe {
          libc::posix_spawn_file_actions_adddup2(
            &mut file_actions,
            self.dev_null.unwrap(),
            2,
          )
        };
        if ret != 0 {
          // SAFETY: file_actions was successfully initialized
          unsafe { libc::posix_spawn_file_actions_destroy(&mut file_actions) };
          return Err(io::Error::from_raw_os_error(ret));
        }
      }
    }

    self.file_actions = Some(file_actions);
    Ok(())
  }

  fn get_file_actions_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
    match &self.file_actions {
      Some(fa) => fa as *const _,
      None => std::ptr::null(),
    }
  }
}

impl Drop for SpawnChild {
  fn drop(&mut self) {
    // Clean up file_actions
    if let Some(mut fa) = self.file_actions.take() {
      // SAFETY: fa was successfully initialized by posix_spawn_file_actions_init
      unsafe { libc::posix_spawn_file_actions_destroy(&mut fa) };
    }
    // Close /dev/null if we opened it
    if let Some(fd) = self.dev_null.take() {
      // SAFETY: fd is a valid file descriptor from open()
      unsafe { libc::close(fd) };
    }
  }
}

// SAFETY: SpawnChild owns all the resources it references.
// The file_actions and dev_null are only used during the spawn syscall
// and are properly cleaned up in Drop.
unsafe impl Send for SpawnChild {}
// SAFETY: SpawnChild owns all the resources it references (same as Send).
unsafe impl Sync for SpawnChild {}

impl TypedOp for SpawnChild {
  type Result = io::Result<Child>;

  fn into_op(&mut self) -> Op {
    let file_actions_ptr = self.get_file_actions_ptr();
    self
      .inner
      .as_mut()
      .expect("inner already taken")
      .into_op_with_file_actions(file_actions_ptr)
  }

  fn extract_result(mut self, res: isize) -> Self::Result {
    let inner = self.inner.take().expect("inner already taken");
    let pid = inner.extract_result(res)?;

    // Close child ends of pipes in parent
    // For stdin: close read end, keep write end
    let stdin = if let Some(pipe) = self.stdin_pipe.take() {
      pipe.close_read();
      // Set non-blocking for async I/O
      let _ = set_nonblocking(pipe.write);
      // SAFETY: pipe.write is a valid fd that we now own (read end was closed)
      Some(ChildStdin { inner: unsafe { Resource::from_raw_fd(pipe.write) } })
    } else {
      None
    };

    // For stdout: close write end, keep read end
    let stdout = if let Some(pipe) = self.stdout_pipe.take() {
      pipe.close_write();
      // Set non-blocking for async I/O
      let _ = set_nonblocking(pipe.read);
      // SAFETY: pipe.read is a valid fd that we now own (write end was closed)
      Some(ChildStdout { inner: unsafe { Resource::from_raw_fd(pipe.read) } })
    } else {
      None
    };

    // For stderr: close write end, keep read end
    let stderr = if let Some(pipe) = self.stderr_pipe.take() {
      pipe.close_write();
      // Set non-blocking for async I/O
      let _ = set_nonblocking(pipe.read);
      // SAFETY: pipe.read is a valid fd that we now own (write end was closed)
      Some(ChildStderr { inner: unsafe { Resource::from_raw_fd(pipe.read) } })
    } else {
      None
    };

    Ok(Child { pid, waited: false, stdin, stdout, stderr })
  }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
  syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
  Ok(())
}

/// Builder TypedOp that handles the Result from SpawnChild::new.
pub struct SpawnChildBuilder {
  inner: Result<SpawnChild, io::Error>,
}

impl SpawnChildBuilder {
  fn new(
    path: CString,
    argv: Vec<CString>,
    envp: Option<Vec<CString>>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
  ) -> Self {
    Self { inner: SpawnChild::new(path, argv, envp, stdin, stdout, stderr) }
  }
}

impl TypedOp for SpawnChildBuilder {
  type Result = io::Result<Child>;

  fn into_op(&mut self) -> Op {
    match &mut self.inner {
      Ok(spawn_child) => spawn_child.into_op(),
      Err(_) => Op::Nop, // Will be handled in extract_result
    }
  }

  fn extract_result(self, res: isize) -> Self::Result {
    match self.inner {
      Ok(spawn_child) => spawn_child.extract_result(res),
      Err(e) => Err(e),
    }
  }
}

// ============================================================================
// Command builder
// ============================================================================

/// A builder for spawning processes.
///
/// `Command` provides a flexible way to configure and spawn child processes, similar to
/// `std::process::Command` but designed for async execution with lio.
///
/// # Examples
///
/// ## Basic usage
///
/// ```rust,no_run
/// use lio::process::Command;
///
/// async fn example() -> std::io::Result<()> {
///     let status = Command::new("/bin/echo")
///         .arg("Hello")
///         .status()
///         .await?;
///     Ok(())
/// }
/// ```
///
/// ## With environment variables
///
/// ```rust,no_run
/// use lio::process::Command;
///
/// async fn example() -> std::io::Result<()> {
///     let status = Command::new("/bin/sh")
///         .arg("-c")
///         .arg("echo $GREETING")
///         .env("GREETING", "Hello!")
///         .status()
///         .await?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Command {
  program: CString,
  args: Vec<CString>,
  env: Option<HashMap<CString, CString>>,
  stdin: Stdio,
  stdout: Stdio,
  stderr: Stdio,
}

impl Command {
  /// Creates a new `Command` for launching the program at path `program`.
  ///
  /// The program path should be an absolute path or a path that will be resolved
  /// by the operating system (though `posix_spawn` typically requires absolute paths
  /// or paths relative to the current directory).
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::Command;
  ///
  /// let cmd = Command::new("/bin/ls");
  /// ```
  pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
    let program = CString::new(program.as_ref().as_bytes())
      .expect("program path contains null byte");
    let arg0 = program.clone();
    Self {
      program,
      args: vec![arg0],
      env: None,
      stdin: Stdio::Inherit,
      stdout: Stdio::Inherit,
      stderr: Stdio::Inherit,
    }
  }

  /// Creates a new `Command` from a `Path`.
  pub fn from_path<P: AsRef<Path>>(program: P) -> Self {
    Self::new(program.as_ref().as_os_str())
  }

  /// Adds an argument to pass to the program.
  ///
  /// Arguments are passed in order to the program.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::Command;
  ///
  /// let cmd = Command::new("/bin/ls")
  ///     .arg("-l")
  ///     .arg("-a");
  /// ```
  pub fn arg<S: AsRef<OsStr>>(mut self, arg: S) -> Self {
    let arg = CString::new(arg.as_ref().as_bytes())
      .expect("argument contains null byte");
    self.args.push(arg);
    self
  }

  /// Adds multiple arguments to pass to the program.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::Command;
  ///
  /// let cmd = Command::new("/bin/ls")
  ///     .args(["-l", "-a", "/tmp"]);
  /// ```
  pub fn args<I, S>(mut self, args: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    for arg in args {
      let arg = CString::new(arg.as_ref().as_bytes())
        .expect("argument contains null byte");
      self.args.push(arg);
    }
    self
  }

  /// Sets an environment variable for the child process.
  ///
  /// Note: When any environment variable is set, the child process will only
  /// see the explicitly set variables (it won't inherit the parent's environment).
  /// Use [`env_clear`](Self::env_clear) and then set variables explicitly, or
  /// use [`envs`](Self::envs) to copy the current environment and modify it.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::Command;
  ///
  /// let cmd = Command::new("/bin/sh")
  ///     .arg("-c")
  ///     .arg("echo $MY_VAR")
  ///     .env("MY_VAR", "Hello");
  /// ```
  pub fn env<K, V>(mut self, key: K, val: V) -> Self
  where
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
  {
    let key = CString::new(key.as_ref().as_bytes())
      .expect("env key contains null byte");
    let val = CString::new(val.as_ref().as_bytes())
      .expect("env value contains null byte");
    self.env.get_or_insert_with(HashMap::new).insert(key, val);
    self
  }

  /// Sets multiple environment variables for the child process.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::Command;
  ///
  /// let cmd = Command::new("/bin/env")
  ///     .envs([("VAR1", "value1"), ("VAR2", "value2")]);
  /// ```
  pub fn envs<I, K, V>(mut self, vars: I) -> Self
  where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
  {
    for (key, val) in vars {
      let key = CString::new(key.as_ref().as_bytes())
        .expect("env key contains null byte");
      let val = CString::new(val.as_ref().as_bytes())
        .expect("env value contains null byte");
      self.env.get_or_insert_with(HashMap::new).insert(key, val);
    }
    self
  }

  /// Clears the environment for the child process.
  ///
  /// After calling this, only explicitly set environment variables will be
  /// visible to the child.
  pub fn env_clear(mut self) -> Self {
    self.env = Some(HashMap::new());
    self
  }

  /// Sets the stdin configuration for the child process.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::{Command, Stdio};
  ///
  /// let cmd = Command::new("/bin/cat")
  ///     .stdin(Stdio::Piped);
  /// ```
  pub fn stdin(mut self, cfg: Stdio) -> Self {
    self.stdin = cfg;
    self
  }

  /// Sets the stdout configuration for the child process.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::{Command, Stdio};
  ///
  /// let cmd = Command::new("/bin/echo")
  ///     .arg("hello")
  ///     .stdout(Stdio::Piped);
  /// ```
  pub fn stdout(mut self, cfg: Stdio) -> Self {
    self.stdout = cfg;
    self
  }

  /// Sets the stderr configuration for the child process.
  ///
  /// # Examples
  ///
  /// ```rust
  /// use lio::process::{Command, Stdio};
  ///
  /// let cmd = Command::new("/bin/sh")
  ///     .arg("-c")
  ///     .arg("echo error >&2")
  ///     .stderr(Stdio::Piped);
  /// ```
  pub fn stderr(mut self, cfg: Stdio) -> Self {
    self.stderr = cfg;
    self
  }

  fn build_envp(&self) -> Option<Vec<CString>> {
    self.env.as_ref().map(|env| {
      env
        .iter()
        .map(|(k, v)| {
          let mut entry = k.as_bytes().to_vec();
          entry.push(b'=');
          entry.extend_from_slice(v.as_bytes());
          CString::new(entry).expect("env entry contains null")
        })
        .collect()
    })
  }

  /// Spawns the process and returns an `Io` operation that yields a [`Child`] handle.
  ///
  /// The returned `Io` can be awaited directly or used with `.with_lio()` for explicit
  /// lio instance binding.
  ///
  /// # Examples
  ///
  /// ## Async usage
  ///
  /// ```rust,no_run
  /// use lio::process::Command;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let mut child = Command::new("/bin/sleep")
  ///         .arg("1")
  ///         .spawn()
  ///         .await?;
  ///
  ///     println!("Spawned child with PID: {}", child.id());
  ///     let status = child.wait().await?;
  ///     Ok(())
  /// }
  /// ```
  ///
  /// ## With explicit lio
  ///
  /// ```rust,no_run
  /// use lio::process::Command;
  /// use lio::Lio;
  ///
  /// fn example() -> std::io::Result<()> {
  ///     let lio = Lio::new(64)?;
  ///
  ///     let recv = Command::new("/bin/true")
  ///         .spawn()
  ///         .with_lio(&lio)
  ///         .send();
  ///     lio.run()?;
  ///     let mut child = recv.recv()?;
  ///
  ///     let recv = child.wait().with_lio(&lio).send();
  ///     lio.run()?;
  ///     let status = recv.recv()?;
  ///     Ok(())
  /// }
  /// ```
  /// Spawns the process and returns an `Io` operation that yields a [`Child`] handle.
  ///
  /// The returned `Io` can be awaited directly or used with `.with_lio()` for explicit
  /// lio instance binding.
  ///
  /// # Examples
  ///
  /// ## Async usage with piped stdio
  ///
  /// ```rust,no_run
  /// use lio::process::{Command, Stdio};
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let mut child = Command::new("/bin/echo")
  ///         .arg("Hello, world!")
  ///         .stdout(Stdio::Piped)
  ///         .spawn()
  ///         .await?;
  ///
  ///     if let Some(stdout) = child.stdout.take() {
  ///         let buffer = vec![0u8; 1024];
  ///         let (result, buffer) = stdout.read(buffer).await;
  ///         let bytes_read = result? as usize;
  ///         println!("Output: {}", String::from_utf8_lossy(&buffer[..bytes_read]));
  ///     }
  ///
  ///     let status = child.wait().await?;
  ///     Ok(())
  /// }
  /// ```
  pub fn spawn(self) -> Io<SpawnChildBuilder> {
    let envp = self.build_envp();
    Io::from_op(SpawnChildBuilder::new(
      self.program,
      self.args,
      envp,
      self.stdin,
      self.stdout,
      self.stderr,
    ))
  }

  /// Spawns the process, waits for it to complete, and returns its exit status.
  ///
  /// This is a convenience method equivalent to calling `spawn().await?.wait().await`.
  ///
  /// # Examples
  ///
  /// ```rust,no_run
  /// use lio::process::Command;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let status = Command::new("/bin/true").status().await?;
  ///     assert!(status.success());
  ///     Ok(())
  /// }
  /// ```
  pub async fn status(self) -> io::Result<ExitStatus> {
    let mut child = self.spawn().await?;
    child.wait().await
  }
}
