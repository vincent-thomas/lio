//! TCP echo server using io_uring
//!
//! This example demonstrates:
//! - Socket operations with io_uring
//! - Accept connections
//! - Async read/write on sockets
//! - Handling multiple concurrent connections

use lio_uring::operation::*;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const PORT: u16 = 8080;
const BACKLOG: i32 = 128;

fn main() -> io::Result<()> {
  println!("io_uring TCP Echo Server Example");
  println!("=================================\n");

  let (mut cq, mut sq) = lio_uring::with_capacity(256)?;

  // Create a socket
  println!("Creating socket...");
  let socket_op = Socket {
    domain: libc::AF_INET,
    sock_type: libc::SOCK_STREAM,
    protocol: 0,
    flags: 0,
  };

  unsafe { sq.push(&socket_op, 1) }?;
  sq.submit()?;

  let sock_fd = cq.next()?.result()?;
  let socket = unsafe { OwnedFd::from_raw_fd(sock_fd) };
  println!("Socket created: fd={}", sock_fd);

  // Set socket options
  let optval: i32 = 1;
  unsafe {
    libc::setsockopt(
      socket.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &optval as *const _ as *const _,
      mem::size_of::<i32>() as u32,
    );
  }

  // Bind to address
  println!("Binding to 0.0.0.0:{}...", PORT);
  let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
  addr.sin_family = libc::AF_INET as u16;
  addr.sin_port = PORT.to_be();
  addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();

  let bind_op = Bind {
    sockfd: socket.as_raw_fd(),
    addr: &addr as *const _ as *const _,
    addrlen: mem::size_of::<libc::sockaddr_in>() as u32,
  };

  unsafe { sq.push(&bind_op, 2) }?;
  sq.submit()?;
  cq.next()?.result()?;
  println!("Bound successfully");

  // Listen for connections
  println!("Listening for connections...");
  let listen_op = Listen { sockfd: socket.as_raw_fd(), backlog: BACKLOG };

  unsafe { sq.push(&listen_op, 3) }?;
  sq.submit()?;
  cq.next()?.result()?;
  println!("Listening on port {}", PORT);
  println!("\nWaiting for connections (press Ctrl+C to exit)...");
  println!("Try: echo 'Hello' | nc localhost {}\n", PORT);

  // Accept one connection as demonstration
  let mut client_addr: libc::sockaddr_in = unsafe { mem::zeroed() };
  let mut client_addrlen = mem::size_of::<libc::sockaddr_in>() as u32;

  let accept_op = Accept {
    fd: socket.as_raw_fd(),
    addr: &mut client_addr as *mut _ as *mut _,
    addrlen: &mut client_addrlen as *mut _,
    flags: 0,
  };

  unsafe { sq.push(&accept_op, 4) }?;
  sq.submit()?;

  let client_fd = cq.next()?.result()?;
  let client_socket = unsafe { OwnedFd::from_raw_fd(client_fd) };

  // Convert address for display
  let ip = u32::from_be(client_addr.sin_addr.s_addr);
  let port = u16::from_be(client_addr.sin_port);
  println!(
    "Accepted connection from {}.{}.{}.{}:{}",
    (ip >> 24) & 0xFF,
    (ip >> 16) & 0xFF,
    (ip >> 8) & 0xFF,
    ip & 0xFF,
    port
  );

  // Echo loop: read data and write it back
  let mut buffer = vec![0u8; 4096];
  loop {
    // Read from client
    let recv_op = Recv {
      sockfd: client_socket.as_raw_fd(),
      buf: buffer.as_mut_ptr().cast(),
      len: buffer.len(),
      flags: 0,
    };

    unsafe { sq.push(&recv_op, 10) }?;
    sq.submit()?;

    let recv_completion = cq.next()?;
    let bytes_read = recv_completion.result()?;

    if bytes_read == 0 {
      println!("Client disconnected");
      break;
    }

    println!("Received {} bytes", bytes_read);
    println!(
      "Data: {:?}",
      String::from_utf8_lossy(&buffer[..bytes_read as usize])
    );

    // Echo back to client
    let send_op = Send {
      sockfd: client_socket.as_raw_fd(),
      buf: buffer.as_ptr().cast(),
      len: bytes_read as usize,
      flags: 0,
    };

    unsafe { sq.push(&send_op, 11) }?;
    sq.submit()?;

    let send_completion = cq.next()?;
    let bytes_sent = send_completion.result()?;
    println!("Echoed {} bytes back", bytes_sent);
  }

  println!("\nServer shutting down...");
  Ok(())
}
