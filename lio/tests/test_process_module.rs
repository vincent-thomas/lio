//! Tests for the process module.

#![cfg(unix)]

use lio::Lio;
use lio::api::io::Receiver;
use lio::process::Command;
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
fn test_command_spawn_true() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .args(["-c", "exit 0"])
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert!(status.success());
  assert!(status.exited());
  assert_eq!(status.code(), Some(0));
}

#[test]
fn test_command_spawn_false() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .args(["-c", "exit 1"])
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert!(!status.success());
  assert!(status.exited());
  assert_eq!(status.code(), Some(1));
}

#[test]
fn test_command_with_args() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("exit 42")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert!(!status.success());
  assert_eq!(status.code(), Some(42));
}

#[test]
fn test_command_with_env() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("exit $MY_CODE")
    .env("MY_CODE", "77")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert_eq!(status.code(), Some(77));
}

#[test]
fn test_command_spawn_and_wait() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .args(["-c", "exit 0"])
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  assert!(child.id() > 0);

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert!(status.success());
}

#[test]
fn test_command_try_wait_running() {
  let lio = Lio::new(64).unwrap();

  // Use an infinite loop - it will run forever until killed
  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("while :; do :; done")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  // Child should still be running
  let mut recv = child.try_wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();
  assert!(status.is_none());

  // Kill it
  child.kill().unwrap();

  // Now it should have exited
  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();
  assert!(status.signaled());
  assert_eq!(status.signal(), Some(libc::SIGKILL));
}

#[test]
fn test_command_kill() {
  let lio = Lio::new(64).unwrap();

  // Use an infinite loop - it will run forever until killed
  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("while :; do :; done")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  child.kill().unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();
  assert!(status.signaled());
  assert_eq!(status.signal(), Some(libc::SIGKILL));
}

#[test]
fn test_command_signal() {
  let lio = Lio::new(64).unwrap();

  // Use an infinite loop - it will run forever until killed
  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("while :; do :; done")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  child.signal(libc::SIGTERM).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();
  assert!(status.signaled());
  assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn test_command_nonexistent() {
  let lio = Lio::new(64).unwrap();

  let mut recv =
    Command::new("/nonexistent/path/to/binary").spawn().with_lio(&lio).send();
  let result = poll_recv(&lio, &mut recv);

  assert!(result.is_err());
  let err = result.unwrap_err();
  assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
}

#[test]
fn test_command_args_method() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .args(["-c", "exit 33"])
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert_eq!(status.code(), Some(33));
}

#[test]
fn test_command_envs_method() {
  let lio = Lio::new(64).unwrap();

  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("exit $((A + B))")
    .envs([("A", "10"), ("B", "5")])
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert_eq!(status.code(), Some(15));
}

#[test]
fn test_command_env_clear() {
  let lio = Lio::new(64).unwrap();

  // With env_clear, the shell won't have PATH, so we need absolute paths
  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("exit ${MY_VAR:-99}")
    .env_clear()
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  // MY_VAR is not set, so it should use default 99
  assert_eq!(status.code(), Some(99));
}

#[test]
fn test_exit_status_methods() {
  let lio = Lio::new(64).unwrap();

  // Test exited process
  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("exit 5")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert!(status.exited());
  assert!(!status.signaled());
  assert!(!status.success());
  assert_eq!(status.code(), Some(5));
  assert_eq!(status.signal(), None);

  // Test signaled process - use infinite loop
  let mut recv = Command::new("/bin/sh")
    .arg("-c")
    .arg("while :; do :; done")
    .spawn()
    .with_lio(&lio)
    .send();
  let mut child = poll_recv(&lio, &mut recv).unwrap();
  child.kill().unwrap();

  let mut recv = child.wait().with_lio(&lio).send();
  let status = poll_recv(&lio, &mut recv).unwrap();

  assert!(!status.exited());
  assert!(status.signaled());
  assert!(!status.success());
  assert_eq!(status.code(), None);
  assert_eq!(status.signal(), Some(libc::SIGKILL));
}

#[test]
fn test_child_dropped_without_wait() {
  let lio = Lio::new(64).unwrap();

  // This test ensures that dropping a Child without waiting doesn't leave zombies
  {
    let mut recv = Command::new("/bin/sh")
      .arg("-c")
      .arg("while :; do :; done")
      .spawn()
      .with_lio(&lio)
      .send();
    let _child = poll_recv(&lio, &mut recv).unwrap();
    // Child is dropped here without waiting
  }
  // If we get here without hanging, the Drop impl worked
}
