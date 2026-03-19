//! Tests for process operations (spawn and waitid).

use lio::api::io::Receiver;
use lio::api::ops::{WaitOptions, WaitTarget};
use lio::{Lio, api};
use std::ffi::CString;
use std::process::Command;
use std::time::{Duration, Instant};

/// Poll the lio event loop until the receiver has a result.
fn poll_recv<T>(lio: &Lio, recv: &mut Receiver<T>) -> T {
  let start = Instant::now();
  let timeout = Duration::from_secs(5);

  loop {
    if let Some(result) = recv.try_recv() {
      return result;
    }
    if start.elapsed() > timeout {
      panic!("poll_recv timed out waiting for operation to complete");
    }
    lio.run_timeout(Duration::from_millis(10)).unwrap();
  }
}

#[test]
fn test_waitid_child_exit() {
  let lio = Lio::new(64).unwrap();

  // Spawn a child process that exits immediately with code 42
  let child = Command::new("sh")
    .args(["-c", "exit 42"])
    .spawn()
    .expect("failed to spawn child");

  let pid = child.id() as i32;

  // Wait for the child to exit
  let mut result = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED)
    .with_lio(&lio)
    .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  let status = status.expect("expected child status");
  assert_eq!(status.pid, pid);
  assert!(status.exited());
  assert_eq!(status.exit_code(), Some(42));
}

#[test]
fn test_waitid_nohang_no_child() {
  let lio = Lio::new(64).unwrap();

  // Spawn a long-running child
  let mut child =
    Command::new("sleep").arg("10").spawn().expect("failed to spawn child");

  let pid = child.id() as i32;

  // Try to wait with NOHANG - should return None since child is still running
  let mut result = api::waitid(
    WaitTarget::Pid(pid),
    WaitOptions::EXITED | WaitOptions::NOHANG,
  )
  .with_lio(&lio)
  .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  // Should be None because child hasn't exited yet
  assert!(status.is_none());

  // Clean up - kill the child and wait via std to reap it
  child.kill().ok();
  child.wait().ok();
}

#[test]
fn test_waitid_exit_zero() {
  let lio = Lio::new(64).unwrap();

  // Spawn a child that exits with code 0
  let child = Command::new("true").spawn().expect("failed to spawn child");
  let pid = child.id() as i32;

  // Wait for specific child
  let mut result = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED)
    .with_lio(&lio)
    .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  let status = status.expect("expected child status");
  assert_eq!(status.pid, pid);
  assert!(status.exited());
  assert_eq!(status.exit_code(), Some(0));
}

#[test]
fn test_waitid_signal_death() {
  let lio = Lio::new(64).unwrap();

  // Use a raw fork to have full control over the child process.
  // This avoids Rust's Child type which may reap the process.
  let pid = unsafe { libc::fork() };

  match pid {
    -1 => panic!("fork failed"),
    0 => {
      // Child: sleep forever
      unsafe { libc::pause() };
      std::process::exit(0);
    }
    child_pid => {
      // Parent: kill the child with SIGKILL
      unsafe { libc::kill(child_pid, libc::SIGKILL) };

      // Wait for the child using our API
      let mut result =
        api::waitid(WaitTarget::Pid(child_pid), WaitOptions::EXITED)
          .with_lio(&lio)
          .send();
      let status = poll_recv(&lio, &mut result).expect("waitid failed");

      let status = status.expect("expected child status");
      assert_eq!(status.pid, child_pid);
      assert!(status.signaled());
      assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
  }
}

// ============================================================================
// Spawn tests
// ============================================================================

#[test]
fn test_spawn_true() {
  let lio = Lio::new(64).unwrap();

  let path = CString::new("/usr/bin/true").unwrap();
  let argv = vec![CString::new("true").unwrap()];

  // Spawn the process
  let mut result = api::spawn(path, argv, None).with_lio(&lio).send();
  let pid = poll_recv(&lio, &mut result).expect("spawn failed");

  assert!(pid > 0);

  // Wait for it
  let mut result = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED)
    .with_lio(&lio)
    .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  let status = status.expect("expected child status");
  assert_eq!(status.pid, pid);
  assert!(status.exited());
  assert_eq!(status.exit_code(), Some(0));
}

#[test]
fn test_spawn_false() {
  let lio = Lio::new(64).unwrap();

  let path = CString::new("/usr/bin/false").unwrap();
  let argv = vec![CString::new("false").unwrap()];

  // Spawn the process
  let mut result = api::spawn(path, argv, None).with_lio(&lio).send();
  let pid = poll_recv(&lio, &mut result).expect("spawn failed");

  // Wait for it
  let mut result = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED)
    .with_lio(&lio)
    .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  let status = status.expect("expected child status");
  assert!(status.exited());
  assert_eq!(status.exit_code(), Some(1));
}

#[test]
fn test_spawn_with_args() {
  let lio = Lio::new(64).unwrap();

  // Use sh -c "exit 42" to test argument passing
  let path = CString::new("/bin/sh").unwrap();
  let argv = vec![
    CString::new("sh").unwrap(),
    CString::new("-c").unwrap(),
    CString::new("exit 42").unwrap(),
  ];

  let mut result = api::spawn(path, argv, None).with_lio(&lio).send();
  let pid = poll_recv(&lio, &mut result).expect("spawn failed");

  let mut result = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED)
    .with_lio(&lio)
    .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  let status = status.expect("expected child status");
  assert!(status.exited());
  assert_eq!(status.exit_code(), Some(42));
}

#[test]
fn test_spawn_with_env() {
  let lio = Lio::new(64).unwrap();

  // sh -c 'exit $MY_EXIT_CODE' with custom environment
  let path = CString::new("/bin/sh").unwrap();
  let argv = vec![
    CString::new("sh").unwrap(),
    CString::new("-c").unwrap(),
    CString::new("exit $MY_EXIT_CODE").unwrap(),
  ];
  let envp = vec![CString::new("MY_EXIT_CODE=77").unwrap()];

  let mut result = api::spawn(path, argv, Some(envp)).with_lio(&lio).send();
  let pid = poll_recv(&lio, &mut result).expect("spawn failed");

  let mut result = api::waitid(WaitTarget::Pid(pid), WaitOptions::EXITED)
    .with_lio(&lio)
    .send();
  let status = poll_recv(&lio, &mut result).expect("waitid failed");

  let status = status.expect("expected child status");
  assert!(status.exited());
  assert_eq!(status.exit_code(), Some(77));
}

#[test]
fn test_spawn_nonexistent() {
  let lio = Lio::new(64).unwrap();

  let path = CString::new("/nonexistent/path/to/binary").unwrap();
  let argv = vec![CString::new("nonexistent").unwrap()];

  let mut result = api::spawn(path, argv, None).with_lio(&lio).send();
  let err = poll_recv(&lio, &mut result)
    .expect_err("spawn should fail for nonexistent path");

  // Should fail with ENOENT (No such file or directory)
  assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
}
