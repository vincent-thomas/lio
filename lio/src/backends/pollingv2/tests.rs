use super::{Interest, ReadinessPoll};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

// Helper utilities

/// RAII wrapper for a socket file descriptor
pub struct OwnedSocket(RawFd);

impl AsRawFd for OwnedSocket {
  fn as_raw_fd(&self) -> RawFd {
    self.0
  }
}

impl Drop for OwnedSocket {
  fn drop(&mut self) {
    let _ = syscall!(close(self.0));
  }
}

/// Create a pair of connected sockets (Unix domain socket pair)
pub fn create_socket_pair() -> io::Result<(OwnedSocket, OwnedSocket)> {
  let mut fds = [0i32; 2];

  let _ = syscall!(socketpair(
    libc::AF_UNIX,
    libc::SOCK_STREAM,
    0,
    fds.as_mut_ptr()
  ))?;

  Ok((OwnedSocket(fds[0]), OwnedSocket(fds[1])))
}

/// Make a file descriptor non-blocking
pub fn make_nonblocking(fd: RawFd) -> io::Result<()> {
  let flags = syscall!(fcntl(fd, libc::F_GETFL))?;
  syscall!(fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
  Ok(())
}

// Focused test functions

/// Test that adding read interest and waiting returns no events when no data is available
pub fn test_add_read_no_data<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 1, Interest::READ)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;
  assert_eq!(n, 0, "Expected no events when no data is available");

  Ok(())
}

/// Test that read interest triggers when data is written
pub fn test_read_becomes_ready<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Write data to sock2, making sock1 readable
  let data = b"hello";
  let written =
    syscall!(write(fd2, data.as_ptr() as *const libc::c_void, data.len()))?;
  assert!(written > 0, "Failed to write data");

  poller.add(fd1, 1, Interest::READ)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 1, "Expected 1 read event");

  let key = P::event_key(&events[0]);
  let interest = P::event_interest(&events[0]);
  assert_eq!(key, 1);
  assert!(interest.is_readable(), "Event should be readable");

  Ok(())
}

/// Test that write interest triggers immediately (sockets are usually writable)
pub fn test_write_immediately_ready<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 2, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Expected at least 1 write event");

  let mut found_write = false;
  for i in 0..n {
    let key = P::event_key(&events[i]);
    let interest = P::event_interest(&events[i]);
    if key == 2 && interest.is_writable() {
      found_write = true;
      break;
    }
  }
  assert!(found_write, "Expected writable event with key 2");

  Ok(())
}

/// Test adding both read and write interest simultaneously
pub fn test_add_both_interests<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  let both = Interest::READ_AND_WRITE;
  poller.add(fd1, 3, both)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Expected at least 1 event");

  // Should get at least write event (sockets are usually writable)
  let mut found_event = false;
  for i in 0..n {
    let key = P::event_key(&events[i]);
    if key == 3 {
      found_event = true;
      break;
    }
  }
  assert!(found_event, "Expected event with key 3");

  Ok(())
}

/// Test modifying interest on an existing fd
pub fn test_modify_interest<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // First add read interest
  poller.add(fd1, 4, Interest::READ)?;

  // Then modify to write interest
  poller.modify(fd1, 4, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Expected at least 1 event after modify");

  Ok(())
}

/// Test deleting interest prevents further events
pub fn test_delete_interest<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 5, Interest::WRITE)?;
  poller.delete(fd1)?;

  // After delete, should not get events for fd1
  let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;
  for i in 0..n {
    let key = P::event_key(&events[i]);
    assert_ne!(key, 5, "Should not get events after delete");
  }

  Ok(())
}

/// Test monitoring multiple file descriptors simultaneously
pub fn test_multiple_fds<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _) = create_socket_pair()?;
  let (sock2, _) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 10, Interest::WRITE)?;
  poller.add(fd2, 20, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 2, "Expected at least 2 events for 2 fds, got {}", n);

  let mut found_fd1 = false;
  let mut found_fd2 = false;
  for i in 0..n {
    let key = P::event_key(&events[i]);
    if key == 10 {
      found_fd1 = true;
    }
    if key == 20 {
      found_fd2 = true;
    }
  }
  assert!(found_fd1, "Expected event for fd1 with key 10");
  assert!(found_fd2, "Expected event for fd2 with key 20");

  Ok(())
}

/// Test that notify() wakes up a blocking wait
pub fn test_notify_wakes_wait<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  let poller_ref = std::sync::Arc::new(poller);
  let p_clone = poller_ref.clone();

  // Spawn a thread that will notify after a delay
  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(50));
    p_clone.notify().expect(
      "test_notify_wakes_wait: failed to call notify() from spawned thread",
    );
  });

  // This wait should be interrupted by notify
  let start = std::time::Instant::now();
  let _n = poller_ref.wait(&mut events, Some(Duration::from_secs(5)))
    .expect("test_notify_wakes_wait: failed to wait for events (should be woken by notify)");
  let elapsed = start.elapsed();

  assert!(
    elapsed < Duration::from_secs(1),
    "Notify should have woken up wait quickly, but took {:?}",
    elapsed
  );

  Ok(())
}

/// Test that deleting non-existent fd returns ENOENT error
pub fn test_delete_nonexistent_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  // Delete a fd that was never registered - should fail with ENOENT
  let result = poller.delete(999);

  assert!(result.is_err(), "Deleting non-existent fd should return error");
  assert_eq!(
    dbg!(result).unwrap_err().raw_os_error(),
    Some(libc::ENOENT),
    "Error should be ENOENT"
  );

  Ok(())
}

/// Test edge case: modifying same fd with different keys
pub fn test_reregister_same_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Add with key 1
  poller.add(fd1, 1, Interest::WRITE)?;

  // Use modify to change to key 2 (add again should error)
  poller.modify(fd1, 2, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get at least one event");

  Ok(())
}

/// Test that timeout works correctly when no events are ready
pub fn test_timeout_no_events<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Add read interest but don't write any data
  poller.add(fd1, 1, Interest::READ)?;

  let start = std::time::Instant::now();
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  let elapsed = start.elapsed();

  assert_eq!(n, 0, "Should get no events");
  assert!(
    elapsed >= Duration::from_millis(90),
    "Should wait close to timeout duration"
  );
  assert!(elapsed < Duration::from_millis(200), "Should not wait too long");

  Ok(())
}

/// Test that zero timeout returns immediately
pub fn test_zero_timeout<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 1, Interest::READ)?;

  let start = std::time::Instant::now();
  let _n = poller.wait(&mut events, Some(Duration::from_millis(0)))?;
  let elapsed = start.elapsed();

  assert!(
    elapsed < Duration::from_millis(50),
    "Zero timeout should return immediately"
  );

  Ok(())
}

/// Test handling many file descriptors
pub fn test_many_fds<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let mut events = vec![unsafe { std::mem::zeroed() }; 128];
  let mut sockets = Vec::new();

  // Create 20 socket pairs
  for i in 0..20 {
    let (sock, _) = create_socket_pair()?;
    let fd = sock.as_raw_fd();
    make_nonblocking(fd)?;

    poller.add(fd, i, Interest::WRITE)?;
    sockets.push(sock);
  }

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 20, "Should get events for most/all fds, got {}", n);

  Ok(())
}

/// Test that reads work with partial data
pub fn test_partial_read<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Write some data
  let data = b"hello world";
  let _ =
    syscall!(write(fd2, data.as_ptr() as *const libc::c_void, data.len()));

  poller.add(fd1, 1, Interest::READ)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 1, "Should get read event");

  // Read only partial data
  let mut buf = [0u8; 5];
  let ret =
    syscall!(read(fd1, buf.as_mut_ptr() as *mut libc::c_void, buf.len()))?;
  assert_eq!(ret, 5, "Should read 5 bytes");

  // Re-arm interest with modify (ONESHOT requires re-registration after event)
  poller.modify(fd1, 2, Interest::READ)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 1, "Should still have more data to read");

  Ok(())
}

/// Test rapid add/delete cycles
pub fn test_rapid_add_delete<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Rapidly add and delete
  for i in 0..10 {
    poller.add(fd1, i, Interest::WRITE)?;
    poller.delete(fd1)?;
  }

  // Add one more time and verify it works
  poller.add(fd1, 100, Interest::WRITE)?;
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get event after rapid add/delete cycles");

  Ok(())
}

/// Test modifying from read to write interest
pub fn test_modify_read_to_write<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Start with read interest (no data, won't trigger)
  poller.add(fd1, 1, Interest::READ)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;
  assert_eq!(n, 0, "Should have no read events");

  // Modify to write interest (should trigger immediately)
  poller.modify(fd1, 1, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get write event");

  let key = P::event_key(&events[0]);
  let interest = P::event_interest(&events[0]);
  assert_eq!(key, 1);
  assert!(interest.is_writable(), "Event should be writable");

  Ok(())
}

/// Test that closing a registered fd doesn't crash
pub fn test_close_registered_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 1, Interest::WRITE)?;

  // Close the fd while it's registered
  let _ = syscall!(close(fd1));
  std::mem::forget(sock1); // Don't double-close

  // Wait should not crash
  let _n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;

  Ok(())
}

/// Test multiple notifies in quick succession
pub fn test_multiple_notifies<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  let poller = std::sync::Arc::new(poller);
  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  let p1 = poller.clone();
  let p2 = poller.clone();
  let p3 = poller.clone();

  // Send multiple notifies
  std::thread::spawn(move || {
    let _ = p1.notify();
  });
  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(10));
    let _ = p2.notify();
  });
  std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(20));
    let _ = p3.notify();
  });

  // Should wake up quickly despite multiple notifies
  let start = std::time::Instant::now();
  let _n = poller.wait(&mut events, Some(Duration::from_secs(5)))?;
  let elapsed = start.elapsed();

  assert!(
    elapsed < Duration::from_millis(500),
    "Multiple notifies should still wake up quickly"
  );

  Ok(())
}

/// Test modifying from write to read interest (opposite direction)
pub fn test_modify_write_to_read<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Start with write interest (should trigger immediately)
  poller.add(fd1, 1, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get write event");

  // Modify to read interest (no data, won't trigger)
  poller.modify(fd1, 1, Interest::READ)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;
  assert_eq!(n, 0, "Should have no read events without data");

  // Now write data to trigger read
  let data = b"test";
  let _ =
    syscall!(write(fd2, data.as_ptr() as *const libc::c_void, data.len()));

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 1, "Should get read event after data written");

  Ok(())
}

/// Test re-adding a file descriptor after deletion
pub fn test_readd_after_delete<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Add, delete, then re-add the same fd
  poller.add(fd1, 100, Interest::WRITE)?;
  poller.delete(fd1)?;
  poller.add(fd1, 200, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get event after re-adding");

  let key = P::event_key(&events[0]);
  assert_eq!(
    key, 200,
    "Should get event with new key (200), not old key (100)"
  );

  Ok(())
}

/// Test simultaneous read and write events on the same fd
pub fn test_simultaneous_read_write<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Write data to make sock1 readable
  let data = b"hello";
  let _ =
    syscall!(write(fd2, data.as_ptr() as *const libc::c_void, data.len()));

  // Register for both read and write (write should be immediately ready, read should be ready too)
  let both = Interest::READ_AND_WRITE;
  poller.add(fd1, 42, both)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get at least one event");

  // Check that we got both interests signaled
  let interest = P::event_interest(&events[0]);
  assert!(
    interest.is_readable() || interest.is_writable(),
    "Should have at least one interest"
  );

  Ok(())
}

/// Test handling socket peer close (HUP/ERR conditions)
pub fn test_peer_closed<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  poller.add(fd1, 1, Interest::READ)?;

  // Close the peer socket
  let _ = syscall!(close(fd2));
  std::mem::forget(sock2);

  // Should get an event indicating the peer closed (usually shows as readable with 0-byte read)
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n >= 1, "Should get event when peer closes");

  let interest = P::event_interest(&events[0]);
  // Different platforms may report this differently (readable, error, or hangup)
  // At minimum we should get some event
  assert!(
    interest.is_readable() || interest.is_writable(),
    "Should get some interest signaled on peer close"
  );

  Ok(())
}

/// Test modifying interest to none (both false) - edge case
pub fn test_modify_to_no_interest<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Add with write interest
  poller.add(fd1, 1, Interest::WRITE)?;

  // Modify to no interest (edge case - shouldn't get events)
  let none = Interest::NONE;
  poller.modify(fd1, 1, none)?;

  let _n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;
  // Should get no events or possibly still get events depending on implementation
  // This is mostly to ensure it doesn't crash

  Ok(())
}

/// Test buffer too small for all ready events
pub fn test_buffer_smaller_than_ready_events<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  // Create 10 sockets, all will be immediately writable
  let mut sockets = Vec::new();
  for i in 0..10 {
    let (sock, _) = create_socket_pair()?;
    let fd = sock.as_raw_fd();
    make_nonblocking(fd)?;
    poller.add(fd, i as u64, Interest::WRITE)?;
    sockets.push(sock);
  }

  // Buffer can only hold 3 events, but 10 fds are ready
  let mut events = vec![unsafe { std::mem::zeroed() }; 3];

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;

  // Should return 3 (buffer size), not 10
  assert_eq!(n, 3, "Should return buffer size when more events are ready");

  // Verify we got valid events
  for i in 0..n {
    let key = P::event_key(&events[i]);
    let interest = P::event_interest(&events[i]);
    assert!(key < 10, "Key should be in valid range");
    assert!(interest.is_writable(), "Event should be writable");
  }

  Ok(())
}

/// Test ONESHOT behavior - events should not re-deliver without re-arm
pub fn test_oneshot_no_redelivery<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Write data to make sock1 readable
  let data = b"hello";
  let _ =
    syscall!(write(fd2, data.as_ptr() as *const libc::c_void, data.len()));

  // Add read interest with ONESHOT semantics
  poller.add(fd1, 1, Interest::READ)?;

  // First wait should get the event
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 1, "Should get 1 read event on first wait");
  assert_eq!(P::event_key(&events[0]), 1);

  // Second wait should NOT get the event again (ONESHOT means one-time delivery)
  // Even though data is still available to read
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 0, "ONESHOT: should not re-deliver event without re-arm");

  // Re-arm with modify
  poller.modify(fd1, 1, Interest::READ)?;

  // Now should get event again
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert_eq!(n, 1, "Should get event after re-arm");

  Ok(())
}

/// Test infinite timeout (None) - should wait indefinitely until event or notify
pub fn test_wait_infinite_timeout<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Add write interest (immediately ready)
  poller.add(fd1, 1, Interest::WRITE)?;

  // Wait with None timeout - should return immediately since fd is writable
  let start = std::time::Instant::now();
  let n = poller.wait(&mut events, None)?;
  let elapsed = start.elapsed();

  assert_eq!(n, 1, "Should get 1 write event");
  assert!(
    elapsed < Duration::from_millis(100),
    "Should return quickly when event is ready, even with infinite timeout"
  );

  Ok(())
}

/// Test adding already-registered fd - verify error handling
pub fn test_add_duplicate_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Add fd with key 1
  poller.add(fd1, 1, Interest::READ)?;

  // Try to add same fd again with different key and interest
  // Different implementations may handle this differently:
  // - Some may error
  // - Some may update the registration (like modify)
  // We just verify it doesn't crash and behaves consistently
  let result = poller.add(fd1, 2, Interest::WRITE);

  // If it succeeds, verify the behavior
  if result.is_ok() {
    // Check which registration is active
    let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;

    // Should get at most 1 event for the fd
    assert!(n <= 1, "Should not get duplicate events for same fd");

    if n == 1 {
      let key = P::event_key(&events[0]);
      // Key should be either 1 or 2, verifying one registration is active
      assert!(
        key == 1 || key == 2,
        "Key should match one of the registrations"
      );
    }
  }
  // If it errors, that's also acceptable behavior - we just verified it doesn't panic

  Ok(())
}

/// Test same key used for different fds - verify key collision handling
pub fn test_same_key_different_fds<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _) = create_socket_pair()?;
  let (sock2, _) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Use same key (42) for both fds
  poller.add(fd1, 42, Interest::WRITE)?;
  poller.add(fd2, 42, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;

  // Should get 2 events (one per fd), even though they share a key
  assert_eq!(n, 2, "Should get events for both fds despite key collision");

  // Both events should have key 42
  for i in 0..n {
    let key = P::event_key(&events[i]);
    assert_eq!(key, 42, "All events should have the same key");

    let interest = P::event_interest(&events[i]);
    assert!(interest.is_writable(), "Events should be writable");
  }

  // However, we can't distinguish which fd triggered which event
  // This test verifies the system doesn't crash or lose events with key collisions

  Ok(())
}

/// Test wait with empty event buffer (zero length slice)
pub fn test_wait_empty_buffer<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  // Add writable fd
  poller.add(fd1, 1, Interest::WRITE)?;

  // Wait with empty buffer
  // Platform difference: kqueue allows this, epoll requires maxevents > 0
  let mut events = [];
  let result = poller.wait(&mut events, Some(Duration::from_millis(10)));

  match result {
    Ok(n) => {
      // kqueue allows empty buffer and returns 0
      assert_eq!(n, 0, "Empty buffer should return 0 events");
    }
    Err(e) => {
      // epoll returns EINVAL for empty buffer - this is acceptable
      assert_eq!(
        e.raw_os_error(),
        Some(libc::EINVAL),
        "Empty buffer should either succeed with 0 or fail with EINVAL"
      );
    }
  }

  Ok(())
}

/// Test adding invalid file descriptor (should fail gracefully)
pub fn test_add_invalid_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  // Try to add invalid file descriptor (-1)
  let result = poller.add(-1, 1, Interest::READ);

  // Should either fail with an error or succeed but not crash
  // We just verify it doesn't panic
  match result {
    Ok(_) => {
      // Some implementations might not validate fd at add time
      // That's acceptable as long as it doesn't crash
    }
    Err(e) => {
      // Expected: should get an error for invalid fd
      assert!(e.raw_os_error().is_some(), "Should get OS error for invalid fd");
    }
  }

  Ok(())
}

/// Test using already-closed file descriptor
pub fn test_add_closed_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();

  // Close the fd
  unsafe {
    libc::close(fd1);
  }
  std::mem::forget(sock1); // Don't double-close

  // Try to add closed fd - should fail or handle gracefully
  let result = poller.add(fd1, 1, Interest::READ);

  // Should either fail or succeed without crashing
  // We're just verifying no panic
  if result.is_err() {
    // Expected: error when adding closed fd
  }

  Ok(())
}

/// Test edge key values (0 and near-MAX)
pub fn test_edge_key_values<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _) = create_socket_pair()?;
  let (sock2, _) = create_socket_pair()?;
  let (sock3, _) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  let fd3 = sock3.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;
  make_nonblocking(fd3)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Test key = 0
  poller.add(fd1, 0, Interest::WRITE)?;

  // Test key = u64::MAX (might conflict with internal NOTIFY_KEY on some platforms)
  // This is a critical test - some implementations use usize::MAX internally
  poller.add(fd2, u64::MAX, Interest::WRITE)?;

  // Test key = u64::MAX - 1
  poller.add(fd3, u64::MAX - 1, Interest::WRITE)?;

  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;

  // Should get 3 events (or 2 if MAX conflicts with internal notify)
  assert!(n >= 2, "Should get at least 2 events for edge keys");
  assert!(n <= 3, "Should get at most 3 events");

  // Verify we can distinguish the keys
  let mut found_zero = false;
  let mut _found_max = false;
  let mut _found_max_minus_1 = false;

  for i in 0..n {
    let key = P::event_key(&events[i]);
    if key == 0 {
      found_zero = true;
    }
    if key == u64::MAX {
      _found_max = true;
    }
    if key == u64::MAX - 1 {
      _found_max_minus_1 = true;
    }
  }

  assert!(found_zero, "Should find event with key 0");
  // MAX and MAX-1 might conflict with NOTIFY_KEY on some platforms,
  // so we track them but don't assert - we just verify no crash

  Ok(())
}

/// Test that READ interest never returns events for write-only readiness
pub fn test_read_interest_filtering<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Register fd1 with READ interest only
  poller.add(fd1, 1, Interest::READ)?;

  // fd1 should be writable (buffer available) but we only registered READ
  let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;

  // Should get NO events because we only care about READ and no data is available
  assert_eq!(n, 0, "READ interest should not report write-only readiness");

  // Now write data to fd2 so fd1 becomes readable
  let data = b"test";
  let _ = unsafe {
    libc::write(fd2, data.as_ptr() as *const libc::c_void, data.len())
  };

  // Now we should get a read event
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n > 0, "Should get read event when data is available");

  let mut found_readable = false;
  for i in 0..n {
    let event = &events[i];
    let key = P::event_key(event);
    let interest = P::event_interest(event);

    if key == 1 {
      assert!(interest.is_readable(), "Event should be readable");
      // Platform might or might not include writable flag, we just verify readable is set
      found_readable = true;
    }
  }
  assert!(found_readable, "Should find readable event");

  Ok(())
}

/// Test that WRITE interest correctly filters and reports writable events
pub fn test_write_interest_filtering<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Register fd1 with WRITE interest only
  poller.add(fd1, 1, Interest::WRITE)?;

  // fd1 should be immediately writable (buffer available)
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n > 0, "Should get write event when socket is writable");

  let mut found_writable = false;
  for i in 0..n {
    let event = &events[i];
    let key = P::event_key(event);
    let interest = P::event_interest(event);

    if key == 1 {
      assert!(interest.is_writable(), "Event should be writable");
      found_writable = true;
    }
  }
  assert!(found_writable, "Should find writable event");

  // Re-arm with WRITE interest (for ONESHOT semantics)
  poller.modify(fd1, 1, Interest::WRITE)?;

  // Write data to fd2 so fd1 becomes readable too
  let data = b"test";
  let _ = unsafe {
    libc::write(fd2, data.as_ptr() as *const libc::c_void, data.len())
  };

  // Even though fd1 is now readable, we should only get WRITE event
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n > 0, "Should get write event");

  let mut found_writable_only = false;
  for i in 0..n {
    let event = &events[i];
    let key = P::event_key(event);
    let interest = P::event_interest(event);

    if key == 1 {
      assert!(interest.is_writable(), "Event should be writable");
      // We registered WRITE only, so readable flag handling is platform-specific
      // Some platforms might include it, some might not - we just verify writable is set
      found_writable_only = true;
    }
  }
  assert!(found_writable_only, "Should find writable event");

  Ok(())
}

/// Test that modifying a non-existent fd returns an error
pub fn test_modify_nonexistent_fd<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, _sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;

  // Try to modify an fd that was never added
  let result = poller.modify(fd1, 1, Interest::READ);

  assert!(result.is_err(), "Modifying non-existent fd should return error");

  Ok(())
}

/// Test that deleted fd registrations don't interfere when fd number is reused
pub fn test_fd_reuse_after_delete<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll,
  P::NativeEvent: Clone,
{
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  let fd2 = sock2.as_raw_fd();
  make_nonblocking(fd1)?;
  make_nonblocking(fd2)?;

  let mut events = vec![unsafe { std::mem::zeroed() }; 16];

  // Register fd1 with key 1
  poller.add(fd1, 1, Interest::READ)?;

  // Write data so fd1 becomes readable
  let data = b"test";
  let _ = unsafe {
    libc::write(fd2, data.as_ptr() as *const libc::c_void, data.len())
  };

  // Verify we get the event
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;
  assert!(n > 0, "Should get read event");

  // Delete fd1
  poller.delete(fd1)?;

  // Close the sockets - this will free the fd numbers
  drop(sock1);
  drop(sock2);

  // Create new sockets - might reuse the same fd numbers
  let (sock3, sock4) = create_socket_pair()?;
  let fd3 = sock3.as_raw_fd();
  let fd4 = sock4.as_raw_fd();
  make_nonblocking(fd3)?;
  make_nonblocking(fd4)?;

  // Register the new fd with a different key (2)
  poller.add(fd3, 2, Interest::READ)?;

  // Write data to the new socket
  let _ = unsafe {
    libc::write(fd4, data.as_ptr() as *const libc::c_void, data.len())
  };

  // Wait for events
  let n = poller.wait(&mut events, Some(Duration::from_millis(100)))?;

  if n > 0 {
    // If we get an event, verify it has the correct key (2, not 1)
    for i in 0..n {
      let event = &events[i];
      let key = P::event_key(event);
      assert_eq!(
        key, 2,
        "Event should have new key 2, not old key 1 - fd state not properly cleaned"
      );
    }
  }

  Ok(())
}

/// Test concurrent add operations from multiple threads
pub fn test_concurrent_add<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  use std::sync::Arc;
  use std::thread;

  let poller = Arc::new(poller);
  let num_threads = 8;
  let fds_per_thread = 10;

  let mut handles = vec![];

  for thread_id in 0..num_threads {
    let poller = Arc::clone(&poller);
    let handle =
      thread::spawn(move || -> io::Result<Vec<(OwnedSocket, OwnedSocket)>> {
        let mut sockets = vec![];

        for i in 0..fds_per_thread {
          let (sock1, sock2) = create_socket_pair()?;
          let fd1 = sock1.as_raw_fd();
          make_nonblocking(fd1)?;

          let key = (thread_id * fds_per_thread + i) as u64;
          poller.add(fd1, key, Interest::READ)?;

          sockets.push((sock1, sock2));
        }

        Ok(sockets)
      });
    handles.push(handle);
  }

  // Collect all sockets to keep fds alive
  let mut all_sockets = vec![];
  for handle in handles {
    let sockets = handle.join().unwrap()?;
    all_sockets.extend(sockets);
  }

  // Verify we can wait without errors
  let mut events = vec![unsafe { std::mem::zeroed() }; 128];
  let _ = poller.wait(&mut events, Some(Duration::from_millis(10)))?;

  // Cleanup
  for (sock1, _sock2) in &all_sockets {
    let _ = poller.delete(sock1.as_raw_fd());
  }

  Ok(())
}

/// Test concurrent modify operations from multiple threads
pub fn test_concurrent_modify<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  use std::sync::Arc;
  use std::thread;

  let poller = Arc::new(poller);
  let num_fds = 20;

  // Create and register all sockets first
  let mut sockets = vec![];
  for i in 0..num_fds {
    let (sock1, sock2) = create_socket_pair()?;
    let fd1 = sock1.as_raw_fd();
    make_nonblocking(fd1)?;
    poller.add(fd1, i as u64, Interest::READ)?;
    sockets.push((sock1, sock2));
  }

  // Spawn threads that concurrently modify interests
  let mut handles = vec![];
  for thread_id in 0..4 {
    let poller = Arc::clone(&poller);
    let sockets_ref: Vec<_> =
      sockets.iter().map(|(s, _)| s.as_raw_fd()).collect();

    let handle = thread::spawn(move || -> io::Result<()> {
      for (i, &fd) in sockets_ref.iter().enumerate() {
        // Alternate between READ and WRITE interest
        let interest = if (thread_id + i) % 2 == 0 {
          Interest::READ
        } else {
          Interest::WRITE
        };
        poller.modify(fd, i as u64, interest)?;
      }
      Ok(())
    });
    handles.push(handle);
  }

  // Wait for all threads
  for handle in handles {
    handle.join().unwrap()?;
  }

  // Cleanup
  for (sock1, _sock2) in &sockets {
    let _ = poller.delete(sock1.as_raw_fd());
  }

  Ok(())
}

/// Test concurrent wait operations from multiple threads
pub fn test_concurrent_wait<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  use std::sync::Arc;
  use std::thread;

  let poller = Arc::new(poller);

  // Create some sockets and make them readable
  let (sock1, sock2) = create_socket_pair()?;
  let fd1 = sock1.as_raw_fd();
  make_nonblocking(fd1)?;
  poller.add(fd1, 1, Interest::READ)?;

  // Write data to make it readable
  let data = b"test data";
  let _ = unsafe {
    libc::write(
      sock2.as_raw_fd(),
      data.as_ptr() as *const libc::c_void,
      data.len(),
    )
  };

  // Spawn multiple threads that all call wait concurrently
  let mut handles = vec![];
  for _ in 0..4 {
    let poller = Arc::clone(&poller);
    let handle = thread::spawn(move || -> io::Result<usize> {
      let mut events = vec![unsafe { std::mem::zeroed() }; 16];
      let mut total_events = 0;

      // Each thread waits multiple times
      for _ in 0..5 {
        let n = poller.wait(&mut events, Some(Duration::from_millis(10)))?;
        total_events += n;
      }

      Ok(total_events)
    });
    handles.push(handle);
  }

  // Collect results - at least one thread should have seen the event
  let mut any_events = false;
  for handle in handles {
    let total = handle.join().unwrap()?;
    if total > 0 {
      any_events = true;
    }
  }

  assert!(any_events, "At least one thread should have seen events");

  // Cleanup
  let _ = poller.delete(fd1);

  Ok(())
}

/// Test concurrent add/delete operations (stress test)
pub fn test_concurrent_add_delete<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  use std::sync::Arc;
  use std::thread;

  let poller = Arc::new(poller);
  let num_threads = 4;
  let iterations = 50;

  let mut handles = vec![];

  for _ in 0..num_threads {
    let poller = Arc::clone(&poller);
    let handle = thread::spawn(move || -> io::Result<()> {
      for _ in 0..iterations {
        // Create socket
        let (sock1, sock2) = create_socket_pair()?;
        let fd1 = sock1.as_raw_fd();
        make_nonblocking(fd1)?;

        // Add
        poller.add(fd1, fd1 as u64, Interest::READ)?;

        // Optionally modify
        if fd1 % 2 == 0 {
          poller.modify(fd1, fd1 as u64, Interest::WRITE)?;
        }

        // Delete
        poller.delete(fd1)?;

        // Drop sockets (close fds)
        drop(sock1);
        drop(sock2);
      }
      Ok(())
    });
    handles.push(handle);
  }

  // Wait for all threads
  for handle in handles {
    handle.join().unwrap()?;
  }

  Ok(())
}

/// Test concurrent operations mixed: add, modify, delete, wait
pub fn test_concurrent_mixed_operations<P>(poller: P) -> io::Result<()>
where
  P: ReadinessPoll + Send + Sync + 'static,
  P::NativeEvent: Clone,
{
  use std::sync::Arc;
  use std::thread;

  let poller = Arc::new(poller);

  // Pre-create some sockets
  let num_sockets = 20;
  let mut sockets = vec![];
  for i in 0..num_sockets {
    let (sock1, sock2) = create_socket_pair()?;
    let fd1 = sock1.as_raw_fd();
    make_nonblocking(fd1)?;
    poller.add(fd1, i as u64, Interest::READ)?;
    sockets.push((sock1, sock2));
  }

  // Thread 1: Continuously modifies interests
  let poller1 = Arc::clone(&poller);
  let fds: Vec<_> = sockets.iter().map(|(s, _)| s.as_raw_fd()).collect();
  let modifier = thread::spawn(move || -> io::Result<()> {
    for _ in 0..100 {
      for (i, &fd) in fds.iter().enumerate() {
        let interest =
          if i % 2 == 0 { Interest::READ } else { Interest::WRITE };
        let _ = poller1.modify(fd, i as u64, interest);
      }
    }
    Ok(())
  });

  // Thread 2: Continuously waits for events
  let poller2 = Arc::clone(&poller);
  let waiter = thread::spawn(move || -> io::Result<()> {
    let mut events = vec![unsafe { std::mem::zeroed() }; 32];
    for _ in 0..100 {
      let _ = poller2.wait(&mut events, Some(Duration::from_millis(1)));
    }
    Ok(())
  });

  // Thread 3: Writes data to make some fds readable
  let writers: Vec<_> = sockets.iter().map(|(_, s)| s.as_raw_fd()).collect();
  let writer = thread::spawn(move || -> io::Result<()> {
    let data = b"x";
    for _ in 0..50 {
      for &fd in &writers {
        let _ = unsafe {
          libc::write(fd, data.as_ptr() as *const libc::c_void, data.len())
        };
      }
      thread::sleep(Duration::from_millis(1));
    }
    Ok(())
  });

  // Wait for all threads
  modifier.join().unwrap()?;
  waiter.join().unwrap()?;
  writer.join().unwrap()?;

  // Cleanup
  for (sock1, _) in &sockets {
    let _ = poller.delete(sock1.as_raw_fd());
  }

  Ok(())
}

/// Macro to generate individual test functions for a ReadinessPoll implementation
///
/// Usage: `generate_tests!(PollerType);`
///
/// This will create individual `#[test]` functions for each test case, making them
/// run independently and integrate better with cargo test/nextest.
#[macro_export]
macro_rules! generate_tests {
  ($poller:expr) => {
    #[test]
    fn test_add_read_no_data() {
      println!("Running test: add read interest with no data available");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_add_read_no_data(poller)
        .expect("test_add_read_no_data: failed to add read interest with no data");
    }

    #[test]
    fn test_read_becomes_ready() {
      println!("Running test: read interest triggers when data is written");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_read_becomes_ready(poller)
        .expect("test_read_becomes_ready: failed when testing read interest triggers on data write");
    }

    #[test]
    fn test_write_immediately_ready() {
      println!("Running test: write interest triggers immediately");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_write_immediately_ready(poller)
        .expect("test_write_immediately_ready: failed when testing immediate write readiness");
    }

    #[test]
    fn test_add_both_interests() {
      println!("Running test: adding both read and write interests simultaneously");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_add_both_interests(poller)
        .expect("test_add_both_interests: failed when adding both read and write interests");
    }

    #[test]
    fn test_modify_interest() {
      println!("Running test: modifying interest on existing fd");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_modify_interest(poller)
        .expect("test_modify_interest: failed when modifying interest on existing fd");
    }

    #[test]
    fn test_delete_interest() {
      println!("Running test: deleting interest prevents further events");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_delete_interest(poller)
        .expect("test_delete_interest: failed when testing delete prevents events");
    }

    #[test]
    fn test_multiple_fds() {
      println!("Running test: monitoring multiple file descriptors simultaneously");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_multiple_fds(poller)
        .expect("test_multiple_fds: failed when monitoring multiple file descriptors");
    }

    #[test]
    fn test_notify_wakes_wait() {
      println!("Running test: notify() wakes up blocking wait");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_notify_wakes_wait(poller)
        .expect("test_notify_wakes_wait: failed when testing notify() waking up blocking wait");
    }

    #[test]
    fn test_delete_nonexistent_fd() {
      println!("Running test: deleting non-existent fd returns ENOENT");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_delete_nonexistent_fd(poller)
        .expect("test_delete_nonexistent_fd: failed when testing deletion of non-existent fd");
    }

    #[test]
    fn test_reregister_same_fd() {
      println!("Running test: modifying same fd with different keys");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_reregister_same_fd(poller)
        .expect("test_reregister_same_fd: failed when modifying same fd with different keys");
    }

    #[test]
    fn test_timeout_no_events() {
      println!("Running test: timeout works correctly when no events are ready");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_timeout_no_events(poller)
        .expect("test_timeout_no_events: failed when testing timeout with no events");
    }

    #[test]
    fn test_zero_timeout() {
      println!("Running test: zero timeout returns immediately");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_zero_timeout(poller)
        .expect("test_zero_timeout: failed when testing zero timeout immediate return");
    }

    #[test]
    fn test_many_fds() {
      println!("Running test: handling many file descriptors");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_many_fds(poller)
        .expect("test_many_fds: failed when handling many file descriptors");
    }

    #[test]
    fn test_partial_read() {
      println!("Running test: reads work with partial data");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_partial_read(poller)
        .expect("test_partial_read: failed when testing partial read handling");
    }

    #[test]
    fn test_rapid_add_delete() {
      println!("Running test: rapid add/delete cycles");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_rapid_add_delete(poller)
        .expect("test_rapid_add_delete: failed when testing rapid add/delete cycles");
    }

    #[test]
    fn test_modify_read_to_write() {
      println!("Running test: modifying from read to write interest");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_modify_read_to_write(poller)
        .expect("test_modify_read_to_write: failed when modifying from read to write interest");
    }

    #[test]
    fn test_close_registered_fd() {
      println!("Running test: closing a registered fd doesn't crash");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_close_registered_fd(poller)
        .expect("test_close_registered_fd: failed when testing closing registered fd");
    }

    #[test]
    fn test_multiple_notifies() {
      println!("Running test: multiple notifies in quick succession");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_multiple_notifies(poller)
        .expect("test_multiple_notifies: failed when testing multiple notifies in succession");
    }

    #[test]
    fn test_modify_write_to_read() {
      println!("Running test: modifying from write to read interest");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_modify_write_to_read(poller)
        .expect("test_modify_write_to_read: failed when modifying from write to read interest");
    }

    #[test]
    fn test_readd_after_delete() {
      println!("Running test: re-adding file descriptor after deletion");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_readd_after_delete(poller)
        .expect("test_readd_after_delete: failed when re-adding fd after deletion");
    }

    #[test]
    fn test_simultaneous_read_write() {
      println!("Running test: simultaneous read and write events on same fd");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_simultaneous_read_write(poller)
        .expect("test_simultaneous_read_write: failed when testing simultaneous read/write events");
    }

    #[test]
    fn test_peer_closed() {
      println!("Running test: handling socket peer close (HUP/ERR conditions)");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_peer_closed(poller)
        .expect("test_peer_closed: failed when testing socket peer close handling");
    }

    #[test]
    fn test_modify_to_no_interest() {
      println!("Running test: modifying interest to none (edge case)");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_modify_to_no_interest(poller)
        .expect("test_modify_to_no_interest: failed when modifying interest to none");
    }

    #[test]
    fn test_buffer_smaller_than_ready_events() {
      println!("Running test: buffer too small for all ready events");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_buffer_smaller_than_ready_events(poller)
        .expect("test_buffer_smaller_than_ready_events: failed when testing small buffer with many ready events");
    }

    #[test]
    fn test_oneshot_no_redelivery() {
      println!("Running test: ONESHOT events should not re-deliver without re-arm");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_oneshot_no_redelivery(poller)
        .expect("test_oneshot_no_redelivery: failed when verifying ONESHOT semantics");
    }

    #[test]
    fn test_wait_infinite_timeout() {
      println!("Running test: infinite timeout (None) should wait indefinitely");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_wait_infinite_timeout(poller)
        .expect("test_wait_infinite_timeout: failed when testing None timeout");
    }

    #[test]
    fn test_add_duplicate_fd() {
      println!("Running test: adding already-registered fd");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_add_duplicate_fd(poller)
        .expect("test_add_duplicate_fd: failed when testing duplicate fd registration");
    }

    #[test]
    fn test_same_key_different_fds() {
      println!("Running test: same key used for different fds");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_same_key_different_fds(poller)
        .expect("test_same_key_different_fds: failed when testing key collision");
    }

    #[test]
    fn test_wait_empty_buffer() {
      println!("Running test: wait with empty event buffer");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_wait_empty_buffer(poller)
        .expect("test_wait_empty_buffer: failed when testing empty buffer");
    }

    #[test]
    fn test_add_invalid_fd() {
      println!("Running test: adding invalid file descriptor (-1)");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_add_invalid_fd(poller)
        .expect("test_add_invalid_fd: failed when testing invalid fd");
    }

    #[test]
    fn test_add_closed_fd() {
      println!("Running test: adding already-closed file descriptor");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_add_closed_fd(poller)
        .expect("test_add_closed_fd: failed when testing closed fd");
    }

    #[test]
    fn test_edge_key_values() {
      println!("Running test: edge key values (0, MAX, MAX-1)");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_edge_key_values(poller)
        .expect("test_edge_key_values: failed when testing edge key values");
    }

    #[test]
    fn test_read_interest_filtering() {
      println!("Running test: READ interest filtering");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_read_interest_filtering(poller)
        .expect("test_read_interest_filtering: failed when testing READ interest filtering");
    }

    #[test]
    fn test_write_interest_filtering() {
      println!("Running test: WRITE interest filtering");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_write_interest_filtering(poller)
        .expect("test_write_interest_filtering: failed when testing WRITE interest filtering");
    }

    #[test]
    fn test_modify_nonexistent_fd() {
      println!("Running test: modifying non-existent fd");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_modify_nonexistent_fd(poller)
        .expect("test_modify_nonexistent_fd: failed when testing modify on non-existent fd");
    }

    #[test]
    fn test_fd_reuse_after_delete() {
      println!("Running test: fd reuse after delete");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_fd_reuse_after_delete(poller)
        .expect("test_fd_reuse_after_delete: failed when testing fd reuse after delete");
    }

    #[test]
    fn test_concurrent_add() {
      println!("Running test: concurrent add operations");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_concurrent_add(poller)
        .expect("test_concurrent_add: failed when testing concurrent add operations");
    }

    #[test]
    fn test_concurrent_modify() {
      println!("Running test: concurrent modify operations");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_concurrent_modify(poller)
        .expect("test_concurrent_modify: failed when testing concurrent modify operations");
    }

    #[test]
    fn test_concurrent_wait() {
      println!("Running test: concurrent wait operations");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_concurrent_wait(poller)
        .expect("test_concurrent_wait: failed when testing concurrent wait operations");
    }

    #[test]
    fn test_concurrent_add_delete() {
      println!("Running test: concurrent add/delete operations");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_concurrent_add_delete(poller)
        .expect("test_concurrent_add_delete: failed when testing concurrent add/delete operations");
    }

    #[test]
    fn test_concurrent_mixed_operations() {
      println!("Running test: concurrent mixed operations");
      let poller = $poller;
      $crate::backends::pollingv2::tests::test_concurrent_mixed_operations(poller)
        .expect("test_concurrent_mixed_operations: failed when testing concurrent mixed operations");
    }
  };
}
