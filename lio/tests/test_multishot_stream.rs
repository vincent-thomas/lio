//! Tests for multishot stream functionality.
//!
//! These tests verify that streaming operations correctly produce multiple
//! completions from a single registration.

#![cfg(unix)]

use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::pin::pin;
use std::time::Duration;

use lio::{api, Lio};

/// Helper to create a listening socket on an ephemeral port
fn setup_listener(lio: &Lio) -> (api::resource::Resource, SocketAddr) {
    // Create socket
    let sock = api::socket(libc::AF_INET, libc::SOCK_STREAM, 0)
        .with_lio(lio)
        .send();
    lio.try_run().unwrap();
    let sock = sock.recv().unwrap();

    // Set SO_REUSEADDR
    let optval: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }

    // Bind to any port
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let bind_rx = api::bind(&sock, addr).with_lio(lio).send();
    lio.try_run().unwrap();
    bind_rx.recv().unwrap();

    // Get actual port
    let sockfd = sock.as_raw_fd();
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    unsafe {
        libc::getsockname(
            sockfd,
            &mut storage as *mut _ as *mut libc::sockaddr,
            &mut len,
        );
    }
    let port = unsafe {
        let addr_in = &*(&storage as *const _ as *const libc::sockaddr_in);
        u16::from_be(addr_in.sin_port)
    };
    let server_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

    // Listen
    let listen_rx = api::listen(&sock, 128).with_lio(lio).send();
    lio.try_run().unwrap();
    listen_rx.recv().unwrap();

    (sock, server_addr)
}

fn noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable, Waker};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |p| RawWaker::new(p, &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

/// Test that accept_stream yields multiple connections from one submission.
#[test]
fn test_accept_stream_multiple_connections() {
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::thread;

    let lio = Lio::new(64).unwrap();
    let (listener, server_addr) = setup_listener(&lio);

    // Create the accept stream
    let mut stream = api::accept_stream(&listener).with_lio(&lio);

    let accepted = Arc::new(AtomicUsize::new(0));
    let expected_connections = 3;

    // Spawn clients
    let handles: Vec<_> = (0..expected_connections)
        .map(|i| {
            let addr = server_addr;
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50 + i as u64 * 20));
                let mut conn = TcpStream::connect(addr).expect("Failed to connect");
                conn.write_all(&[i as u8]).expect("Failed to write");
                conn
            })
        })
        .collect();

    // Accept connections using the stream
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(5);

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    while accepted.load(Ordering::SeqCst) < expected_connections {
        if start.elapsed() > timeout {
            panic!(
                "Timed out waiting for connections. Got {} of {}",
                accepted.load(Ordering::SeqCst),
                expected_connections
            );
        }

        // Run the event loop
        lio.run_timeout(Duration::from_millis(100)).unwrap();

        // Poll for next item
        let mut next_fut = pin!(stream.next());
        match next_fut.as_mut().poll(&mut cx) {
            Poll::Ready(Some(result)) => {
                let (_client, addr) = result.expect("Accept should succeed");
                assert!(addr.port() > 0, "Client should have a valid port");
                accepted.fetch_add(1, Ordering::SeqCst);
            }
            Poll::Ready(None) => {
                panic!("Stream ended unexpectedly");
            }
            Poll::Pending => {
                // Still waiting for more
            }
        }
    }

    // Cleanup
    for h in handles {
        let _ = h.join();
    }

    assert_eq!(
        accepted.load(Ordering::SeqCst),
        expected_connections,
        "Should have accepted {} connections",
        expected_connections
    );
}

/// Test that stream registration is reused (not recreated for each accept).
#[test]
fn test_accept_stream_single_registration() {
    use std::net::TcpStream;
    use std::task::{Context, Poll};
    use std::thread;

    let lio = Lio::new(64).unwrap();
    let (listener, server_addr) = setup_listener(&lio);

    // Create the accept stream
    let mut stream = api::accept_stream(&listener).with_lio(&lio);

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // First poll should submit the stream operation
    {
        let mut next_fut = pin!(stream.next());
        let poll1 = next_fut.as_mut().poll(&mut cx);
        assert!(matches!(poll1, Poll::Pending), "First poll should be pending");
    }

    // Connect first client
    let _c1 = thread::spawn({
        let addr = server_addr;
        move || TcpStream::connect(addr).expect("Failed to connect")
    });

    thread::sleep(Duration::from_millis(50));
    lio.run_timeout(Duration::from_millis(100)).unwrap();

    // Should get first connection
    {
        let mut next_fut = pin!(stream.next());
        let poll2 = next_fut.as_mut().poll(&mut cx);
        assert!(matches!(poll2, Poll::Ready(Some(Ok(_)))), "Should have first connection");
    }

    // Connect second client
    let _c2 = thread::spawn({
        let addr = server_addr;
        move || TcpStream::connect(addr).expect("Failed to connect")
    });

    thread::sleep(Duration::from_millis(50));
    lio.run_timeout(Duration::from_millis(100)).unwrap();

    // Should get second connection without needing to resubmit
    {
        let mut next_fut = pin!(stream.next());
        let poll3 = next_fut.as_mut().poll(&mut cx);
        assert!(
            matches!(poll3, Poll::Ready(Some(Ok(_)))),
            "Should have second connection from same stream"
        );
    }
}

/// Test that dropping a stream cancels the inflight operation.
#[test]
fn test_stream_cancellation_on_drop() {
    use std::net::TcpStream;
    use std::task::{Context, Poll};
    use std::thread;

    let lio = Lio::new(64).unwrap();
    let (listener, server_addr) = setup_listener(&lio);

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // Create and start a stream
    {
        let mut stream = api::accept_stream(&listener).with_lio(&lio);

        // Submit the stream operation
        {
            let mut next_fut = pin!(stream.next());
            let _ = next_fut.as_mut().poll(&mut cx);
        }

        // Connect a client
        let _client = thread::spawn({
            let addr = server_addr;
            move || TcpStream::connect(addr).expect("Failed to connect")
        });

        thread::sleep(Duration::from_millis(50));
        lio.run_timeout(Duration::from_millis(100)).unwrap();

        // Get one connection
        {
            let mut next_fut = pin!(stream.next());
            let poll = next_fut.as_mut().poll(&mut cx);
            assert!(matches!(poll, Poll::Ready(Some(Ok(_)))));
        }

        // Stream is dropped here - should cancel the multishot operation
    }

    // After drop, no panics should occur and we should be able to create new operations
    let nop_rx = api::nop().with_lio(&lio).send();
    lio.try_run().unwrap();
    nop_rx.recv().expect("Nop should succeed after stream drop");
}

/// Test concurrent stream and single-shot operations.
#[test]
fn test_stream_with_concurrent_ops() {
    use std::net::TcpStream;
    use std::task::{Context, Poll};
    use std::thread;

    let lio = Lio::new(64).unwrap();
    let (listener, server_addr) = setup_listener(&lio);

    // Create the accept stream
    let mut stream = api::accept_stream(&listener).with_lio(&lio);

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    // Submit the stream operation
    {
        let mut next_fut = pin!(stream.next());
        let _ = next_fut.as_mut().poll(&mut cx);
    }

    // Also submit a single-shot nop operation
    let nop_rx = api::nop().with_lio(&lio).send();

    // Run the event loop - nop should complete
    lio.try_run().unwrap();
    nop_rx.recv().expect("Nop should succeed");

    // Connect a client
    let _client = thread::spawn({
        let addr = server_addr;
        move || TcpStream::connect(addr).expect("Failed to connect")
    });

    thread::sleep(Duration::from_millis(50));
    lio.run_timeout(Duration::from_millis(100)).unwrap();

    // Stream should still work
    {
        let mut next_fut = pin!(stream.next());
        let poll = next_fut.as_mut().poll(&mut cx);
        assert!(
            matches!(poll, Poll::Ready(Some(Ok(_)))),
            "Stream should receive connection after concurrent nop"
        );
    }
}
