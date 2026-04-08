//! Minimal TCP echo server using io_uring.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use lio_uring::{
  LioUring,
  operation::{Accept, Bind, Listen, Recv, Send, Socket},
};

const PORT: u16 = 8080;
const BACKLOG: i32 = 128;

fn check(result: i32) -> io::Result<i32> {
  if result < 0 {
    Err(io::Error::from_raw_os_error(-result))
  } else {
    Ok(result)
  }
}

fn main() -> io::Result<()> {
  println!("io_uring TCP Echo Server Example");
  println!("=================================\n");

  let mut ring = LioUring::new(256)?;

  let socket = Socket::new(libc::AF_INET, libc::SOCK_STREAM, 0).build();
  unsafe { ring.push(socket, 1)? };
  ring.submit()?;
  let sock_fd = check(ring.wait()?.result())?;
  let socket = unsafe { OwnedFd::from_raw_fd(sock_fd) };

  let optval: i32 = 1;
  let rc = unsafe {
    libc::setsockopt(
      socket.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      (&optval as *const i32).cast(),
      mem::size_of::<i32>() as libc::socklen_t,
    )
  };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }

  let mut addr: libc::sockaddr_in = unsafe { mem::zeroed() };
  addr.sin_family = libc::AF_INET as libc::sa_family_t;
  addr.sin_port = PORT.to_be();
  addr.sin_addr.s_addr = libc::INADDR_ANY.to_be();

  let bind = Bind::new(
    socket.as_raw_fd(),
    (&addr as *const libc::sockaddr_in).cast(),
    mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
  )
  .build();
  unsafe { ring.push(bind, 2)? };
  ring.submit()?;
  check(ring.wait()?.result())?;

  let listen = Listen::new(socket.as_raw_fd(), BACKLOG).build();
  unsafe { ring.push(listen, 3)? };
  ring.submit()?;
  check(ring.wait()?.result())?;

  println!("Listening on 0.0.0.0:{PORT}");
  println!("Try: echo 'Hello' | nc localhost {PORT}\n");

  let mut client_addr: libc::sockaddr_in = unsafe { mem::zeroed() };
  let mut client_addrlen =
    mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
  let accept = Accept::new(
    socket.as_raw_fd(),
    (&mut client_addr as *mut libc::sockaddr_in).cast(),
    &mut client_addrlen,
  )
  .build();
  unsafe { ring.push(accept, 4)? };
  ring.submit()?;
  let client_fd = check(ring.wait()?.result())?;
  let client_socket = unsafe { OwnedFd::from_raw_fd(client_fd) };

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

  let mut buffer = vec![0_u8; 4096];
  loop {
    let recv = Recv::new(
      client_socket.as_raw_fd(),
      buffer.as_mut_ptr(),
      buffer.len() as u32,
    )
    .build();
    unsafe { ring.push(recv, 10)? };
    ring.submit()?;

    let bytes_read = check(ring.wait()?.result())?;
    if bytes_read == 0 {
      println!("Client disconnected");
      break;
    }

    println!(
      "Received: {:?}",
      String::from_utf8_lossy(&buffer[..bytes_read as usize])
    );

    let send =
      Send::new(client_socket.as_raw_fd(), buffer.as_ptr(), bytes_read as u32)
        .build();
    unsafe { ring.push(send, 11)? };
    ring.submit()?;
    let bytes_sent = check(ring.wait()?.result())?;
    println!("Echoed {bytes_sent} bytes back");
  }

  Ok(())
}
