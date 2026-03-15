//! Integration tests for DNS resolution.
//!
//! These tests require network access and a working DNS server.

#![cfg(feature = "dns")]

mod common;

use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;
use std::sync::Arc;

use lio::net::dns::Resolver;
use lio::Lio;

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
    // Try to poll the future
    match fut.as_mut().poll(&mut cx) {
      Poll::Ready(result) => return result,
      Poll::Pending => {
        // Drive the lio event loop
        lio.run_timeout(Duration::from_millis(10)).unwrap();

        if start.elapsed() > timeout {
          panic!("block_on timed out after {:?}", timeout);
        }
      }
    }
  }
}

#[test]
fn test_resolver_new() {
  let ip: IpAddr = "8.8.8.8".parse().unwrap();
  let resolver = Resolver::new(ip);
  assert_eq!(resolver.dns_server(), ip);
}

#[test]
fn test_resolver_new_ipv6() {
  let ip: IpAddr = "2001:4860:4860::8888".parse().unwrap();
  let resolver = Resolver::new(ip);
  assert_eq!(resolver.dns_server(), ip);
}

#[test]
#[cfg(unix)]
fn test_resolver_from_system() {
  // This should work on most Unix systems with /etc/resolv.conf
  let result = Resolver::from_system();
  // Don't assert success - some CI environments may not have resolv.conf
  if let Ok(resolver) = result {
    let server = resolver.dns_server();
    assert!(server.is_ipv4() || server.is_ipv6());
  }
}

#[test]
fn test_resolve_ip_address_passthrough() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  // Resolving an IP address should return it directly without DNS lookup
  let result = block_on(&mut lio, resolver.resolve("192.168.1.1", 80));

  lio::uninstall_global();

  let addrs = result.expect("resolve failed");
  assert_eq!(addrs.len(), 1);
  assert_eq!(addrs[0], "192.168.1.1:80".parse::<SocketAddr>().unwrap());
}

#[test]
fn test_resolve_ipv6_address_passthrough() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  // Resolving an IPv6 address should return it directly
  let result = block_on(&mut lio, resolver.resolve("::1", 443));

  lio::uninstall_global();

  let addrs = result.expect("resolve failed");
  assert_eq!(addrs.len(), 1);
  assert_eq!(addrs[0], "[::1]:443".parse::<SocketAddr>().unwrap());
}

#[test]
fn test_resolve_google_dns() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  // google.com should always resolve
  let result = block_on(&mut lio, resolver.resolve("google.com", 443));

  lio::uninstall_global();

  match result {
    Ok(addrs) => {
      assert!(
        !addrs.is_empty(),
        "google.com should resolve to at least one address"
      );
      for addr in &addrs {
        assert_eq!(addr.port(), 443);
        assert!(addr.ip().is_ipv4() || addr.ip().is_ipv6());
      }
    }
    Err(e) => {
      // Network might not be available in CI
      eprintln!("DNS resolution failed (network may be unavailable): {}", e);
    }
  }
}

#[test]
fn test_resolve_one_google() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  let result = block_on(&mut lio, resolver.resolve_one("google.com", 80));

  lio::uninstall_global();

  match result {
    Ok(addr) => {
      assert_eq!(addr.port(), 80);
      assert!(addr.ip().is_ipv4() || addr.ip().is_ipv6());
    }
    Err(e) => {
      eprintln!("DNS resolution failed (network may be unavailable): {}", e);
    }
  }
}

#[test]
fn test_resolve_nonexistent_domain() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  // This domain should not exist
  let result = block_on(
    &mut lio,
    resolver.resolve("this-domain-definitely-does-not-exist-12345.invalid", 80),
  );

  lio::uninstall_global();

  match result {
    Ok(addrs) => {
      // Some DNS servers might return addresses even for invalid domains (DNS hijacking)
      eprintln!("Warning: got addresses for invalid domain: {:?}", addrs);
    }
    Err(e) => {
      // Expected - domain should not resolve
      assert!(
        e.kind() == std::io::ErrorKind::NotFound
          || format!("{}", e).contains("NXDOMAIN")
          || format!("{}", e).contains("could not resolve"),
        "unexpected error: {}",
        e
      );
    }
  }
}

#[test]
fn test_resolve_with_cloudflare_dns() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  // Test with Cloudflare DNS
  let resolver = Resolver::new("1.1.1.1".parse().unwrap());

  let result = block_on(&mut lio, resolver.resolve("cloudflare.com", 443));

  lio::uninstall_global();

  match result {
    Ok(addrs) => {
      assert!(!addrs.is_empty());
      for addr in &addrs {
        assert_eq!(addr.port(), 443);
      }
    }
    Err(e) => {
      eprintln!("DNS resolution failed (network may be unavailable): {}", e);
    }
  }
}

#[test]
fn test_convenience_resolve_function() {
  // Skip if no system DNS is configured
  if Resolver::from_system().is_err() {
    eprintln!("Skipping: no system DNS configured");
    return;
  }

  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let result = block_on(&mut lio, lio::net::dns::resolve("example.com", 80));

  lio::uninstall_global();

  match result {
    Ok(addrs) => {
      assert!(!addrs.is_empty());
    }
    Err(e) => {
      eprintln!("DNS resolution failed: {}", e);
    }
  }
}

#[test]
fn test_convenience_resolve_one_function() {
  // Skip if no system DNS is configured
  if Resolver::from_system().is_err() {
    eprintln!("Skipping: no system DNS configured");
    return;
  }

  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let result = block_on(&mut lio, lio::net::dns::resolve_one("example.com", 443));

  lio::uninstall_global();

  match result {
    Ok(addr) => {
      assert_eq!(addr.port(), 443);
    }
    Err(e) => {
      eprintln!("DNS resolution failed: {}", e);
    }
  }
}

#[test]
fn test_multiple_resolutions_same_resolver() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  // Resolve multiple domains with the same resolver
  let domains = ["google.com", "github.com", "rust-lang.org"];

  for domain in domains {
    let result = block_on(&mut lio, resolver.resolve(domain, 443));

    match result {
      Ok(addrs) => {
        assert!(!addrs.is_empty(), "{} should resolve", domain);
      }
      Err(e) => {
        eprintln!("Failed to resolve {}: {}", domain, e);
      }
    }
  }

  lio::uninstall_global();
}

#[test]
fn test_invalid_hostname_empty() {
  let mut lio = Lio::new(64).unwrap();
  lio::install_global(lio.clone());

  let resolver = Resolver::new("8.8.8.8".parse().unwrap());

  // Empty hostname should fail
  let result = block_on(&mut lio, resolver.resolve("", 80));

  lio::uninstall_global();

  assert!(result.is_err(), "empty hostname should fail");
}
