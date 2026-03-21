//! A simple TCP echo server using lio.
//!
//! Usage: cargo run --example echo_server [port]
//!
//! Default port is 8080. Connect with: nc localhost 8080

use lio::api::resource::Resource;
use lio::{Lio, api};
use std::env;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;

fn main() -> std::io::Result<()> {
  let port: u16 =
    env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8080);

  let lio = Lio::new(64)?;

  // Create socket
  let rx =
    api::socket(libc::AF_INET, libc::SOCK_STREAM, 0).with_lio(&lio).send();
  let server = run(&lio, rx)?;

  // Set SO_REUSEADDR
  let optval: libc::c_int = 1;
  unsafe {
    libc::setsockopt(
      server.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_REUSEADDR,
      &optval as *const _ as *const libc::c_void,
      std::mem::size_of::<libc::c_int>() as libc::socklen_t,
    );
  }

  // Bind
  let addr: SocketAddr = format!("0.0.0.0:{}", port).parse().unwrap();
  let rx = api::bind(&server, addr).with_lio(&lio).send();
  run(&lio, rx)?;

  // Listen
  let rx = api::listen(&server, 128).with_lio(&lio).send();
  run(&lio, rx)?;

  println!("Echo server listening on port {}", port);
  println!("Connect with: nc localhost {}", port);

  // Accept and handle clients one at a time (simple demo)
  loop {
    // Accept
    let rx = api::accept(&server).with_lio(&lio).send();
    let (client, client_addr) = run(&lio, rx)?;
    println!("Client connected from {}", client_addr);

    // Echo loop for this client
    if let Err(e) = handle_client(&lio, &client) {
      eprintln!("Client error: {}", e);
    }
    println!("Client {} disconnected", client_addr);
  }
}

fn handle_client(lio: &Lio, client: &Resource) -> std::io::Result<()> {
  let mut buf = vec![0u8; 1024];

  loop {
    // Receive
    let rx = api::recv(client, buf, None).with_lio(lio).send();
    let (result, returned_buf) = run(lio, rx);
    buf = returned_buf;

    let n = result? as usize;
    if n == 0 {
      break; // Client closed
    }

    // Echo back
    let to_send = buf[..n].to_vec();
    let rx = api::send(client, to_send, None).with_lio(lio).send();
    let (result, _) = run(lio, rx);
    result?;
  }

  Ok(())
}

fn run<T>(lio: &Lio, mut rx: api::io::Receiver<T>) -> T {
  loop {
    lio.try_run().expect("lio.try_run()");
    if let Some(result) = rx.try_recv() {
      return result;
    }
    lio.run().expect("lio.run()");
  }
}
