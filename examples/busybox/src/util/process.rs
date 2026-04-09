use std::{
  env,
  ffi::CString,
  io,
  os::unix::ffi::OsStrExt,
  path::{Path, PathBuf},
  time::Duration,
};

use lio::api::{self, Pid};

use crate::{app::AppContext, util::io as io_util};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildStatus {
  Exited(i32),
  Signaled(i32),
  Other(i32),
}

impl ChildStatus {
  pub fn success(self) -> bool {
    matches!(self, Self::Exited(0))
  }
}

pub fn spawn_command(
  ctx: &AppContext,
  command: &str,
  args: &[String],
) -> io::Result<Pid> {
  let resolved = resolve_command_path(command)?;
  let path = CString::new(resolved.as_os_str().as_bytes()).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid command path")
  })?;
  let argv = build_argv(command, args)?;

  let rx = api::spawn(path, argv, None).with_lio(ctx.lio()).send();
  io_util::run(ctx.lio(), rx)
}

pub fn run_command_capture(
  ctx: &AppContext,
  command: &str,
  args: &[String],
) -> io::Result<(Vec<u8>, ChildStatus)> {
  let resolved = resolve_command_path(command)?;
  let path = CString::new(resolved.as_os_str().as_bytes()).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid command path")
  })?;
  let argv = build_argv(command, args)?;

  let mut pipe_fds = [0; 2];
  if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
    return Err(io::Error::last_os_error());
  }
  let read_fd = pipe_fds[0];
  let write_fd = pipe_fds[1];

  let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
  if flags < 0 {
    unsafe {
      libc::close(read_fd);
      libc::close(write_fd);
    }
    return Err(io::Error::last_os_error());
  }
  if unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }
    < 0
  {
    unsafe {
      libc::close(read_fd);
      libc::close(write_fd);
    }
    return Err(io::Error::last_os_error());
  }

  let mut file_actions: libc::posix_spawn_file_actions_t =
    unsafe { std::mem::zeroed() };
  let init_result =
    unsafe { libc::posix_spawn_file_actions_init(&mut file_actions) };
  if init_result != 0 {
    unsafe {
      libc::close(read_fd);
      libc::close(write_fd);
    }
    return Err(io::Error::from_raw_os_error(init_result));
  }

  let spawn_result = (|| -> io::Result<libc::pid_t> {
    add_dup2(&mut file_actions, write_fd, libc::STDOUT_FILENO)?;
    add_dup2(&mut file_actions, write_fd, libc::STDERR_FILENO)?;
    add_close(&mut file_actions, read_fd)?;
    add_close(&mut file_actions, write_fd)?;

    unsafe extern "C" {
      static mut environ: *mut *mut libc::c_char;
    }
    let mut pid = 0;
    let result = unsafe {
      libc::posix_spawn(
        &mut pid,
        path.as_ptr(),
        &file_actions,
        std::ptr::null(),
        argv
          .iter()
          .map(|arg| arg.as_ptr().cast_mut())
          .chain(std::iter::once(std::ptr::null_mut()))
          .collect::<Vec<_>>()
          .as_ptr(),
        environ,
      )
    };
    if result != 0 {
      return Err(io::Error::from_raw_os_error(result));
    }
    Ok(pid)
  })();

  unsafe {
    libc::posix_spawn_file_actions_destroy(&mut file_actions);
    libc::close(write_fd);
  }

  let pid = match spawn_result {
    Ok(pid) => pid,
    Err(err) => {
      unsafe {
        libc::close(read_fd);
      }
      return Err(err);
    }
  };

  let mut output = Vec::new();
  let mut child_status = None;
  let mut buf = [0u8; 8192];
  let mut saw_eof = false;

  loop {
    loop {
      let n =
        unsafe { libc::read(read_fd, buf.as_mut_ptr().cast(), buf.len()) };
      if n > 0 {
        output.extend_from_slice(&buf[..n as usize]);
        continue;
      }
      if n == 0 {
        saw_eof = true;
        break;
      }

      let err = io::Error::last_os_error();
      let raw = err.raw_os_error().unwrap_or(0);
      if raw == libc::EAGAIN || raw == libc::EWOULDBLOCK {
        break;
      }
      unsafe {
        libc::close(read_fd);
      }
      return Err(err);
    }

    if child_status.is_none() {
      child_status = waitpid_raw(pid, libc::WNOHANG)?;
    }
    if child_status.is_some() && saw_eof {
      break;
    }

    let rx = api::sleep(Duration::from_millis(10)).with_lio(ctx.lio()).send();
    io_util::run(ctx.lio(), rx)?;
  }

  unsafe {
    libc::close(read_fd);
  }

  Ok((output, child_status.expect("child status available")))
}

fn resolve_command_path(command: &str) -> io::Result<PathBuf> {
  let path = Path::new(command);
  if path.components().count() > 1 || command.contains('/') {
    return Ok(path.to_path_buf());
  }

  let Some(paths) = env::var_os("PATH") else {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      format!("{command}: command not found"),
    ));
  };

  for dir in env::split_paths(&paths) {
    let candidate = dir.join(command);
    if is_executable_file(&candidate) {
      return Ok(candidate);
    }
  }

  Err(io::Error::new(
    io::ErrorKind::NotFound,
    format!("{command}: command not found"),
  ))
}

fn is_executable_file(path: &Path) -> bool {
  let cpath = match CString::new(path.as_os_str().as_bytes()) {
    Ok(path) => path,
    Err(_) => return false,
  };

  unsafe { libc::access(cpath.as_ptr(), libc::X_OK) == 0 }
}

fn build_argv(command: &str, args: &[String]) -> io::Result<Vec<CString>> {
  let mut argv = Vec::with_capacity(args.len() + 1);
  argv.push(CString::new(command).map_err(|_| {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid argv[0]")
  })?);
  for arg in args {
    argv.push(CString::new(arg.as_str()).map_err(|_| {
      io::Error::new(io::ErrorKind::InvalidInput, "invalid argument")
    })?);
  }
  Ok(argv)
}

fn add_dup2(
  file_actions: &mut libc::posix_spawn_file_actions_t,
  from: libc::c_int,
  to: libc::c_int,
) -> io::Result<()> {
  let result =
    unsafe { libc::posix_spawn_file_actions_adddup2(file_actions, from, to) };
  if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
}

fn add_close(
  file_actions: &mut libc::posix_spawn_file_actions_t,
  fd: libc::c_int,
) -> io::Result<()> {
  let result =
    unsafe { libc::posix_spawn_file_actions_addclose(file_actions, fd) };
  if result == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(result)) }
}

pub fn wait_for_child(pid: Pid) -> io::Result<ChildStatus> {
  waitpid(pid, 0)
    .map(|status| status.expect("blocking wait must return status"))
}

pub fn try_wait_for_child(pid: Pid) -> io::Result<Option<ChildStatus>> {
  waitpid(pid, libc::WNOHANG)
}

pub fn signal_child(pid: Pid, signal: i32) -> io::Result<()> {
  let result = unsafe { libc::kill(pid.as_raw() as libc::pid_t, signal) };
  if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn waitpid(pid: Pid, options: i32) -> io::Result<Option<ChildStatus>> {
  waitpid_raw(pid.as_raw() as libc::pid_t, options)
}

fn waitpid_raw(
  pid: libc::pid_t,
  options: i32,
) -> io::Result<Option<ChildStatus>> {
  let mut status = 0;
  let waited = unsafe { libc::waitpid(pid, &mut status, options) };
  if waited < 0 {
    return Err(io::Error::last_os_error());
  }
  if waited == 0 {
    return Ok(None);
  }

  let status = if libc::WIFEXITED(status) {
    ChildStatus::Exited(libc::WEXITSTATUS(status))
  } else if libc::WIFSIGNALED(status) {
    ChildStatus::Signaled(libc::WTERMSIG(status))
  } else {
    ChildStatus::Other(status)
  };
  Ok(Some(status))
}
