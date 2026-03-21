//! Demo: Various I/O operations with lio
//!
//! Run with: cargo run --example demo_io
//!
//! This demonstrates:
//! - Network I/O (socket, bind, listen, accept, connect, send, recv)
//! - Timers (sleep)
//! - Concurrent operations

use lio::{Lio, api};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

/// Helper to run until operation completes
fn run<T>(lio: &Lio, mut rx: lio::api::io::Receiver<T>) -> T {
  loop {
    // Process any ready completions
    lio.try_run().expect("lio.try_run()");
    // Check if our operation completed
    if let Some(result) = rx.try_recv() {
      return result;
    }
    // Block for more events
    lio.run().expect("lio.run()");
  }
}

fn main() -> std::io::Result<()> {
  let lio = Lio::new(64)?;

  println!("=== lio I/O Demo ===\n");

  // 1. Timer
  println!("--- Timer ---");
  timer_demo(&lio)?;

  // 2. Network I/O
  println!("\n--- Network I/O ---");
  network_demo(&lio)?;

  // 3. Concurrent operations
  println!("\n--- Concurrent Operations ---");
  concurrent_demo(&lio)?;

  println!("\n=== Demo Complete ===");
  Ok(())
}

fn timer_demo(lio: &Lio) -> std::io::Result<()> {
  let start = Instant::now();

  // Sleep for 100ms using lio's timer
  let rx = api::sleep(Duration::from_millis(100)).with_lio(lio).send();
  let _ = run(lio, rx);

  let elapsed = start.elapsed();
  println!("  sleep(100ms) -> elapsed {:?}", elapsed);

  Ok(())
}

fn network_demo(lio: &Lio) -> std::io::Result<()> {
  // Create server socket
  let rx =
    api::socket(libc::AF_INET, libc::SOCK_STREAM, 0).with_lio(lio).send();
  let server = run(lio, rx)?;
  println!("  socket() -> server fd {}", server.as_raw_fd());

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

  // Bind to random port
  let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
  let rx = api::bind(&server, addr).with_lio(lio).send();
  run(lio, rx)?;

  // Get the actual port
  let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
  let mut len =
    std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
  unsafe {
    libc::getsockname(
      server.as_raw_fd(),
      &mut storage as *mut _ as *mut libc::sockaddr,
      &mut len,
    );
  }
  let port = unsafe {
    let addr_in = &*(&storage as *const _ as *const libc::sockaddr_in);
    u16::from_be(addr_in.sin_port)
  };
  let server_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
  println!("  bind() -> bound to {}", server_addr);

  // Listen
  let rx = api::listen(&server, 128).with_lio(lio).send();
  run(lio, rx)?;
  println!("  listen() -> listening");

  // Create client socket and connect (in background thread for demo)
  let client_handle = std::thread::spawn(move || -> std::io::Result<()> {
    std::thread::sleep(Duration::from_millis(10)); // Let server call accept first
    let stream = std::net::TcpStream::connect(server_addr)?;
    stream.set_nodelay(true)?;
    use std::io::Write;
    let mut stream = stream;
    stream.write_all(b"Hello from client!")?;
    Ok(())
  });

  // Accept connection
  let rx = api::accept(&server).with_lio(lio).send();
  let (client_conn, client_addr) = run(lio, rx)?;
  println!(
    "  accept() -> client fd {} from {}",
    client_conn.as_raw_fd(),
    client_addr
  );

  // Receive data
  let buf = vec![0u8; 64];
  let rx = api::recv(&client_conn, buf, None).with_lio(lio).send();
  let (result, buf) = run(lio, rx);
  let n = result? as usize;
  println!("  recv() -> {} bytes: {:?}", n, String::from_utf8_lossy(&buf[..n]));

  // Send response
  let response = b"Hello from server!".to_vec();
  let rx = api::send(&client_conn, response, None).with_lio(lio).send();
  let (result, _) = run(lio, rx);
  let sent = result?;
  println!("  send() -> {} bytes sent", sent);

  // Shutdown (may fail if client closed first - that's ok)
  let rx = api::shutdown(&client_conn, libc::SHUT_RDWR).with_lio(lio).send();
  let _ = run(lio, rx);
  println!("  shutdown() -> connection closed");

  client_handle.join().expect("client thread")?;

  Ok(())
}

fn concurrent_demo(lio: &Lio) -> std::io::Result<()> {
  let start = Instant::now();

  // Submit multiple sleeps concurrently
  let mut rx1 = api::sleep(Duration::from_millis(50)).with_lio(lio).send();
  let mut rx2 = api::sleep(Duration::from_millis(100)).with_lio(lio).send();
  let mut rx3 = api::sleep(Duration::from_millis(75)).with_lio(lio).send();
  let mut rx4 = api::nop().with_lio(lio).send();

  println!("  Submitted 3 sleeps (50ms, 100ms, 75ms) + 1 nop");

  // Wait for all - they run concurrently so total time should be ~100ms, not 225ms
  let mut done = [false; 4];
  while !done.iter().all(|&d| d) {
    lio.try_run().expect("try_run");
    if !done[0] && rx1.try_recv().is_some() {
      done[0] = true;
    }
    if !done[1] && rx2.try_recv().is_some() {
      done[1] = true;
    }
    if !done[2] && rx3.try_recv().is_some() {
      done[2] = true;
    }
    if !done[3] && rx4.try_recv().is_some() {
      done[3] = true;
    }
    if !done.iter().all(|&d| d) {
      lio.run().expect("run");
    }
  }

  let elapsed = start.elapsed();
  println!("  All completed in {:?} (concurrent, not sequential)", elapsed);

  // Verify concurrency worked
  assert!(elapsed < Duration::from_millis(150), "Should be ~100ms, not 225ms");

  Ok(())
}
