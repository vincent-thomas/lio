use std::{
  ffi::CString, mem::MaybeUninit, net::SocketAddr, sync::mpsc, time::Duration,
};

use lio::{Lio, api, api::io::Receiver, api::resource::Resource};
use std::os::fd::{AsFd, AsRawFd};

/// Utility function to create a unique temporary file path for proptest tests.
/// Returns a CString path that includes the process ID and a unique value to avoid conflicts.
#[allow(dead_code)]
pub fn make_temp_path(test_name: &str, unique_value: u64) -> CString {
  CString::new(format!(
    "/tmp/lio_test_{}_{}_{}.txt",
    test_name,
    std::process::id(),
    unique_value
  ))
  .expect("Failed to create CString path")
}

/// A connected TCP client-server pair.
#[allow(dead_code)]
pub struct TcpPair {
  pub server_sock: Resource,
  pub client_sock: Resource,
  pub accepted_fd: Resource,
}

/// Setup a connected TCP client-server pair.
/// Returns a TcpPair with server_sock, client_sock, and accepted_fd.
#[allow(dead_code)]
pub fn setup_tcp_pair(lio: &mut Lio) -> TcpPair {
  let (sender_sock, receiver_sock) = mpsc::channel();
  let (sender_unit, receiver_unit) = mpsc::channel();

  // Create server socket
  lio::test_utils::tcp_socket().with_lio(lio).send_with(sender_sock.clone());

  let server_sock = poll_until_recv(lio, &receiver_sock)
    .expect("Failed to create server socket");

  // Bind to any available port
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  api::bind(&server_sock, addr).with_lio(lio).send_with(sender_unit.clone());
  poll_until_recv(lio, &receiver_unit).expect("Failed to bind");

  // Get actual bound address
  let bound_addr = get_bound_addr(&server_sock);

  // Listen
  api::listen(&server_sock, 128).with_lio(lio).send_with(sender_unit.clone());
  poll_until_recv(lio, &receiver_unit).expect("Failed to listen");

  // Create client socket
  lio::test_utils::tcp_socket().with_lio(lio).send_with(sender_sock.clone());
  let client_sock = poll_until_recv(lio, &receiver_sock)
    .expect("Failed to create client socket");

  // Connect and accept
  let (sender_connect, receiver_connect) = mpsc::channel();
  let (sender_accept, receiver_accept) = mpsc::channel();

  api::connect(&client_sock, bound_addr)
    .with_lio(lio)
    .send_with(sender_connect);
  api::accept(&server_sock).with_lio(lio).send_with(sender_accept);

  poll_until_recv(lio, &receiver_connect).expect("Failed to connect");
  let (accepted_fd, _) =
    poll_until_recv(lio, &receiver_accept).expect("Failed to accept");

  TcpPair { server_sock, client_sock, accepted_fd }
}

/// Get the bound address of a socket.
#[allow(dead_code)]
pub fn get_bound_addr(sock: &Resource) -> SocketAddr {
  unsafe {
    let mut addr_storage = MaybeUninit::<libc::sockaddr_in>::zeroed();
    let mut addr_len =
      std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    libc::getsockname(
      sock.as_fd().as_raw_fd(),
      addr_storage.as_mut_ptr() as *mut libc::sockaddr,
      &mut addr_len,
    );
    let sockaddr_in = addr_storage.assume_init();
    let port = u16::from_be(sockaddr_in.sin_port);
    format!("127.0.0.1:{}", port).parse::<SocketAddr>().unwrap()
  }
}

/// RAII temporary file that cleans up on drop.
#[allow(dead_code)]
pub struct TempFile {
  pub path: CString,
}

impl TempFile {
  #[allow(dead_code)]
  pub fn new(name: &str) -> Self {
    let path = CString::new(format!(
      "/tmp/lio_test_{}_{}.txt",
      name,
      std::process::id()
    ))
    .expect("Failed to create CString path");
    Self { path }
  }
}

impl Drop for TempFile {
  fn drop(&mut self) {
    unsafe {
      libc::unlink(self.path.as_ptr());
    }
  }
}

/// Poll the lio event loop until a result is received on the channel.
/// Blocks in kqueue/epoll for up to 5ms per iteration — no busy-spin, no attempt cap.
pub fn poll_until_recv<T>(lio: &mut Lio, receiver: &mpsc::Receiver<T>) -> T {
  loop {
    lio.run_timeout(Duration::from_millis(5)).unwrap();
    match receiver.try_recv() {
      Ok(result) => return result,
      Err(mpsc::TryRecvError::Empty) => {}
      Err(mpsc::TryRecvError::Disconnected) => panic!("Channel disconnected"),
    }
  }
}

/// Poll the lio event loop until a lio [`Receiver`] has a result.
///
/// This helper runs the event loop in short iterations (5ms timeout each) until
/// the async operation completes. Unlike blocking `.recv()`, this ensures the
/// event loop continues processing, preventing deadlocks in tests.
///
/// # Example
///
/// ```ignore
/// let mut recv = api::connect(&sock, addr).with_lio(&mut lio).send();
/// let result = poll_recv(&mut lio, &mut recv);
/// ```
///
/// # Panics
///
/// - Panics if the operation doesn't complete within 5 seconds.
/// - Panics if the sender is dropped without sending (internal lio error).
#[allow(dead_code)]
pub fn poll_recv<T>(lio: &mut Lio, receiver: &mut Receiver<T>) -> T {
  let start = std::time::Instant::now();
  let timeout = Duration::from_secs(5);

  loop {
    if let Some(result) = receiver.try_recv() {
      return result;
    }
    if start.elapsed() > timeout {
      panic!(
        "poll_recv timed out after {:?} waiting for operation to complete",
        timeout
      );
    }
    lio.run_timeout(Duration::from_millis(5)).unwrap();
  }
}
