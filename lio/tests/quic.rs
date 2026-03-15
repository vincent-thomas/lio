//! Integration tests for lio QUIC module.

#![cfg(feature = "quic")]

use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use lio::quic::{ClientConfig, Endpoint, ServerConfig};
use lio::Lio;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// A simple waker that does nothing (we poll manually)
struct NoopWaker;

impl Wake for NoopWaker {
    fn wake(self: Arc<Self>) {}
}

/// Drive a future to completion by polling lio in a loop.
fn block_on<F: std::future::Future>(lio: &mut Lio, fut: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWaker));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);

    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);

    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => {
                lio.run_timeout(Duration::from_millis(10)).unwrap();

                if start.elapsed() > timeout {
                    panic!("block_on timed out after {:?}", timeout);
                }
            }
        }
    }
}

/// Drive two futures concurrently until both complete.
fn block_on_both<F1, F2>(lio: &mut Lio, fut1: F1, fut2: F2) -> (F1::Output, F2::Output)
where
    F1: std::future::Future,
    F2: std::future::Future,
{
    let waker = Waker::from(Arc::new(NoopWaker));
    let mut cx = Context::from_waker(&waker);
    let mut fut1 = pin!(fut1);
    let mut fut2 = pin!(fut2);

    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(30);

    let mut result1: Option<F1::Output> = None;
    let mut result2: Option<F2::Output> = None;

    loop {
        if result1.is_none() {
            if let Poll::Ready(r) = fut1.as_mut().poll(&mut cx) {
                result1 = Some(r);
            }
        }
        if result2.is_none() {
            if let Poll::Ready(r) = fut2.as_mut().poll(&mut cx) {
                result2 = Some(r);
            }
        }

        if result1.is_some() && result2.is_some() {
            return (result1.unwrap(), result2.unwrap());
        }

        lio.run_timeout(Duration::from_millis(5)).unwrap();

        if start.elapsed() > timeout {
            panic!("block_on_both timed out after {:?}", timeout);
        }
    }
}

fn generate_test_certs() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let cert = vec![CertificateDer::from(cert.cert.der().to_vec())];
    (cert, key)
}

#[test]
fn quic_echo_server() {
    let mut lio = Lio::new(64).unwrap();
    lio::install_global(lio.clone());

    let (certs, key) = generate_test_certs();

    let server_config = ServerConfig::with_single_cert(certs, key).unwrap();
    let server_endpoint =
        block_on(&mut lio, Endpoint::server("127.0.0.1:0".parse().unwrap(), server_config))
            .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let client_endpoint =
        block_on(&mut lio, Endpoint::client("127.0.0.1:0".parse().unwrap())).unwrap();
    let client_config = ClientConfig::insecure_skip_verify();

    let (_, client_results) = block_on_both(
        &mut lio,
        async {
            let conn = server_endpoint.accept().await.unwrap().unwrap();

            // Echo server: accept streams and echo back
            for _ in 0..5 {
                let mut stream = conn.accept_bi().await.unwrap();
                let mut buf = vec![0u8; 1024];
                let n = stream.recv.read(&mut buf).await.unwrap().unwrap_or(0);
                stream.send.write_all(&buf[..n]).await.unwrap();
                stream.send.finish().await.unwrap();
            }
        },
        async {
            let conn = client_endpoint
                .connect(server_addr, "localhost", client_config)
                .await
                .unwrap();

            let mut results = Vec::new();

            for i in 0..5 {
                let mut stream = conn.open_bi().await.unwrap();
                let msg = format!("echo-{}", i);

                stream.send.write_all(msg.as_bytes()).await.unwrap();
                stream.send.finish().await.unwrap();

                let mut response = vec![0u8; 1024];
                let n = stream.recv.read(&mut response).await.unwrap().unwrap_or(0);
                results.push(String::from_utf8_lossy(&response[..n]).to_string());
            }

            results
        },
    );

    assert_eq!(
        client_results,
        vec!["echo-0", "echo-1", "echo-2", "echo-3", "echo-4"]
    );

    lio::uninstall_global();
}

#[test]
fn quic_multiple_messages() {
    let mut lio = Lio::new(64).unwrap();
    lio::install_global(lio.clone());

    let (certs, key) = generate_test_certs();

    let server_config = ServerConfig::with_single_cert(certs, key).unwrap();
    let server_endpoint =
        block_on(&mut lio, Endpoint::server("127.0.0.1:0".parse().unwrap(), server_config))
            .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let client_endpoint =
        block_on(&mut lio, Endpoint::client("127.0.0.1:0".parse().unwrap())).unwrap();
    let client_config = ClientConfig::insecure_skip_verify();

    let (_, client_results) = block_on_both(
        &mut lio,
        async {
            let conn = server_endpoint.accept().await.unwrap().unwrap();
            let mut stream = conn.accept_bi().await.unwrap();

            // Echo multiple messages on same stream
            for _ in 0..10 {
                let mut buf = vec![0u8; 256];
                let n = stream.recv.read(&mut buf).await.unwrap().unwrap_or(0);
                stream.send.write_all(&buf[..n]).await.unwrap();
            }
            stream.send.finish().await.unwrap();
        },
        async {
            let conn = client_endpoint
                .connect(server_addr, "localhost", client_config)
                .await
                .unwrap();
            let mut stream = conn.open_bi().await.unwrap();

            let mut results = Vec::new();
            for i in 0..10 {
                let msg = format!("msg-{}", i);
                stream.send.write_all(msg.as_bytes()).await.unwrap();

                let mut buf = vec![0u8; 256];
                let n = stream.recv.read(&mut buf).await.unwrap().unwrap_or(0);
                results.push(String::from_utf8_lossy(&buf[..n]).to_string());
            }
            stream.send.finish().await.unwrap();
            results
        },
    );

    let expected: Vec<String> = (0..10).map(|i| format!("msg-{}", i)).collect();
    assert_eq!(client_results, expected);

    lio::uninstall_global();
}

#[test]
fn quic_bidirectional_communication() {
    let mut lio = Lio::new(64).unwrap();
    lio::install_global(lio.clone());

    let (certs, key) = generate_test_certs();

    let server_config = ServerConfig::with_single_cert(certs, key).unwrap();
    let server_endpoint =
        block_on(&mut lio, Endpoint::server("127.0.0.1:0".parse().unwrap(), server_config))
            .unwrap();
    let server_addr = server_endpoint.local_addr().unwrap();

    let client_endpoint =
        block_on(&mut lio, Endpoint::client("127.0.0.1:0".parse().unwrap())).unwrap();
    let client_config = ClientConfig::insecure_skip_verify();

    let (_, client_response) = block_on_both(
        &mut lio,
        async {
            let conn = server_endpoint.accept().await.unwrap().unwrap();
            let mut stream = conn.accept_bi().await.unwrap();

            // Read request
            let mut buf = vec![0u8; 1024];
            let n = stream.recv.read(&mut buf).await.unwrap().unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();

            // Send response based on request
            let response = format!("Response to: {}", request);
            stream.send.write_all(response.as_bytes()).await.unwrap();
            stream.send.finish().await.unwrap();
        },
        async {
            let conn = client_endpoint
                .connect(server_addr, "localhost", client_config)
                .await
                .unwrap();
            let mut stream = conn.open_bi().await.unwrap();

            // Send request
            stream.send.write_all(b"Hello QUIC").await.unwrap();
            stream.send.finish().await.unwrap();

            // Read response
            let mut buf = vec![0u8; 1024];
            let n = stream.recv.read(&mut buf).await.unwrap().unwrap_or(0);
            String::from_utf8_lossy(&buf[..n]).to_string()
        },
    );

    assert_eq!(client_response, "Response to: Hello QUIC");

    lio::uninstall_global();
}
