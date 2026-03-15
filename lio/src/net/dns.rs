//! Asynchronous DNS resolution for lio.
//!
//! This module provides async DNS resolution capabilities using lio's async I/O
//! primitives. It implements a DNS client that sends queries over UDP
//! to configured DNS servers.
//!
//! # Overview
//!
//! Unlike many async DNS implementations that simply spawn blocking threads,
//! this module performs true async DNS resolution using lio's UDP socket
//! operations. It uses the hickory-proto library for DNS protocol handling.
//!
//! # Feature Flag
//!
//! This module requires the `dns` feature to be enabled:
//!
//! ```toml
//! [dependencies]
//! lio = { version = "0.4", features = ["dns"] }
//! ```
//!
//! # Examples
//!
//! ## Using the default system resolver
//!
//! ```rust,ignore
//! use lio::net::dns::Resolver;
//!
//! async fn example() -> std::io::Result<()> {
//!     let resolver = Resolver::from_system()?;
//!     let addrs = resolver.resolve("example.com", 80).await?;
//!     for addr in &addrs {
//!         println!("Resolved: {}", addr);
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Using a custom DNS server
//!
//! ```rust,ignore
//! use lio::net::dns::Resolver;
//! use std::net::IpAddr;
//!
//! async fn example() -> std::io::Result<()> {
//!     // Use Cloudflare DNS
//!     let dns_server: IpAddr = "1.1.1.1".parse().unwrap();
//!     let resolver = Resolver::new(dns_server);
//!
//!     let addrs = resolver.resolve("example.com", 443).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Resolve and connect
//!
//! ```rust,ignore
//! use lio::net::{dns::Resolver, TcpSocket};
//!
//! async fn connect_to_host(host: &str, port: u16) -> std::io::Result<TcpSocket> {
//!     let resolver = Resolver::from_system()?;
//!     let addrs = resolver.resolve(host, port).await?;
//!
//!     let mut last_err = None;
//!     for addr in addrs {
//!         match TcpSocket::connect_async(addr).await {
//!             Ok(socket) => return Ok(socket),
//!             Err(e) => last_err = Some(e),
//!         }
//!     }
//!
//!     Err(last_err.unwrap_or_else(|| {
//!         std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses resolved")
//!     }))
//! }
//! ```

use std::{
  io,
  net::{IpAddr, SocketAddr},
};

use hickory_proto::{
  op::{Message, MessageType, OpCode, Query},
  rr::{DNSClass, Name, RData, RecordType},
  serialize::binary::{BinDecodable, BinEncodable},
};

use super::Socket;
use crate::api;

/// An async DNS resolver that uses lio's I/O primitives.
///
/// `Resolver` performs DNS queries over UDP using the configured DNS server.
/// It supports both IPv4 (A) and IPv6 (AAAA) record lookups.
///
/// # Examples
///
/// ```rust,ignore
/// use lio::net::dns::Resolver;
///
/// async fn example() -> std::io::Result<()> {
///     // Use system-configured DNS server
///     let resolver = Resolver::from_system()?;
///
///     // Or use a specific DNS server
///     let resolver = Resolver::new("8.8.8.8".parse().unwrap());
///
///     let addrs = resolver.resolve("example.com", 80).await?;
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Resolver {
  dns_server: IpAddr,
}

impl Resolver {
  /// Creates a new resolver with the specified DNS server.
  ///
  /// # Arguments
  ///
  /// * `dns_server` - The IP address of the DNS server to use
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// use lio::net::dns::Resolver;
  /// use std::net::IpAddr;
  ///
  /// // Use Google Public DNS
  /// let resolver = Resolver::new("8.8.8.8".parse().unwrap());
  ///
  /// // Use Cloudflare DNS
  /// let resolver = Resolver::new("1.1.1.1".parse().unwrap());
  ///
  /// // Use a local DNS server
  /// let resolver = Resolver::new("192.168.1.1".parse().unwrap());
  /// ```
  pub fn new(dns_server: IpAddr) -> Self {
    Self { dns_server }
  }

  /// Creates a resolver using the system's configured DNS server.
  ///
  /// On Unix systems, this reads from `/etc/resolv.conf`.
  /// On Windows, this attempts to use the system's DNS configuration.
  ///
  /// # Errors
  ///
  /// Returns an error if no DNS server could be determined from system configuration.
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// use lio::net::dns::Resolver;
  ///
  /// let resolver = Resolver::from_system()?;
  /// ```
  pub fn from_system() -> io::Result<Self> {
    let dns_server = get_system_dns_server()?;
    Ok(Self { dns_server })
  }

  /// Returns the DNS server address used by this resolver.
  pub fn dns_server(&self) -> IpAddr {
    self.dns_server
  }

  /// Resolves a hostname to a list of socket addresses.
  ///
  /// This method performs true async DNS resolution using lio's UDP socket
  /// operations. It queries both A (IPv4) and AAAA (IPv6) records.
  ///
  /// # Arguments
  ///
  /// * `hostname` - The hostname to resolve (e.g., "example.com")
  /// * `port` - The port number to include in the resulting `SocketAddr`s
  ///
  /// # Returns
  ///
  /// A `Vec<SocketAddr>` containing all resolved addresses (IPv4 first, then IPv6).
  ///
  /// # Errors
  ///
  /// Returns an error if:
  /// - The hostname cannot be resolved
  /// - A network error occurs
  /// - The DNS response is malformed
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// use lio::net::dns::Resolver;
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let resolver = Resolver::from_system()?;
  ///     let addrs = resolver.resolve("google.com", 443).await?;
  ///     println!("Resolved {} addresses", addrs.len());
  ///     Ok(())
  /// }
  /// ```
  pub async fn resolve(
    &self,
    hostname: &str,
    port: u16,
  ) -> io::Result<Vec<SocketAddr>> {
    // First, check if it's already an IP address
    if let Ok(ip) = hostname.parse::<IpAddr>() {
      return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let dns_addr = SocketAddr::new(self.dns_server, 53);

    // Query both A and AAAA records
    let mut addresses = Vec::new();

    // Query A records (IPv4)
    if let Ok(ipv4_addrs) =
      self.query_dns(&dns_addr, hostname, RecordType::A).await
    {
      for ip in ipv4_addrs {
        addresses.push(SocketAddr::new(ip, port));
      }
    }

    // Query AAAA records (IPv6)
    if let Ok(ipv6_addrs) =
      self.query_dns(&dns_addr, hostname, RecordType::AAAA).await
    {
      for ip in ipv6_addrs {
        addresses.push(SocketAddr::new(ip, port));
      }
    }

    if addresses.is_empty() {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("could not resolve hostname: {}", hostname),
      ));
    }

    Ok(addresses)
  }

  /// Resolves a hostname to the first available socket address.
  ///
  /// This is a convenience method that returns only the first resolved address.
  ///
  /// # Examples
  ///
  /// ```rust,ignore
  /// use lio::net::{dns::Resolver, TcpSocket};
  ///
  /// async fn example() -> std::io::Result<()> {
  ///     let resolver = Resolver::from_system()?;
  ///     let addr = resolver.resolve_one("example.com", 80).await?;
  ///     let socket = TcpSocket::connect_async(addr).await?;
  ///     Ok(())
  /// }
  /// ```
  pub async fn resolve_one(
    &self,
    hostname: &str,
    port: u16,
  ) -> io::Result<SocketAddr> {
    let addrs = self.resolve(hostname, port).await?;
    addrs.into_iter().next().ok_or_else(|| {
      io::Error::new(io::ErrorKind::NotFound, "no addresses resolved")
    })
  }

  /// Performs a DNS query for a specific record type.
  async fn query_dns(
    &self,
    dns_server: &SocketAddr,
    hostname: &str,
    record_type: RecordType,
  ) -> io::Result<Vec<IpAddr>> {
    // Create UDP socket
    let domain =
      if dns_server.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    let socket = Socket::new(domain, libc::SOCK_DGRAM, 0).await?;

    // Build DNS query using hickory-proto
    let name = Name::from_ascii(hostname).map_err(|e| {
      io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid hostname: {}", e),
      )
    })?;

    let mut message = Message::new();
    message.set_id(rand_id());
    message.set_message_type(MessageType::Query);
    message.set_op_code(OpCode::Query);
    message.set_recursion_desired(true);
    message.add_query(
      Query::query(name, record_type).set_query_class(DNSClass::IN).clone(),
    );

    let query_bytes = message.to_bytes().map_err(|e| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        format!("failed to encode DNS query: {}", e),
      )
    })?;

    // Send query
    let (result, _) =
      api::sendto(&socket, query_bytes.to_vec(), *dns_server, None).await;
    result?;

    // Receive response
    let recv_buf = vec![0u8; 512]; // Standard DNS UDP packet size
    let (result, response, _peer) =
      api::recvfrom(&socket, recv_buf, None).await;
    let bytes_received = result? as usize;

    // Parse response using hickory-proto
    let response_msg = Message::from_bytes(&response[..bytes_received])
      .map_err(|e| {
        io::Error::new(
          io::ErrorKind::InvalidData,
          format!("failed to parse DNS response: {}", e),
        )
      })?;

    // Check response code
    if response_msg.response_code() != hickory_proto::op::ResponseCode::NoError
    {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("DNS query failed: {:?}", response_msg.response_code()),
      ));
    }

    // Extract IP addresses from answers
    let mut addresses = Vec::new();
    for answer in response_msg.answers() {
      match answer.data() {
        RData::A(a) => addresses.push(IpAddr::V4(a.0)),
        RData::AAAA(aaaa) => addresses.push(IpAddr::V6(aaaa.0)),
        _ => {}
      }
    }

    Ok(addresses)
  }
}

/// Generate a random DNS transaction ID.
fn rand_id() -> u16 {
  (std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .subsec_nanos()
    & 0xFFFF) as u16
}

/// Gets the primary DNS server from system configuration.
#[cfg(unix)]
fn get_system_dns_server() -> io::Result<IpAddr> {
  // Try to read /etc/resolv.conf
  let contents = std::fs::read_to_string("/etc/resolv.conf").map_err(|e| {
    io::Error::new(
      io::ErrorKind::NotFound,
      format!("could not read /etc/resolv.conf: {}", e),
    )
  })?;

  for line in contents.lines() {
    let line = line.trim();
    if line.starts_with("nameserver") {
      if let Some(addr_str) = line.split_whitespace().nth(1) {
        if let Ok(addr) = addr_str.parse::<IpAddr>() {
          return Ok(addr);
        }
      }
    }
  }

  Err(io::Error::new(
    io::ErrorKind::NotFound,
    "no DNS server found in /etc/resolv.conf",
  ))
}

#[cfg(windows)]
fn get_system_dns_server() -> io::Result<IpAddr> {
  // On Windows, we'd need to use the Windows API to get DNS servers
  // For now, return an error asking the user to specify one
  Err(io::Error::new(
    io::ErrorKind::Unsupported,
    "automatic DNS server detection not supported on Windows; use Resolver::new() with an explicit DNS server",
  ))
}

/// Convenience function to resolve using the system's DNS server.
///
/// This is equivalent to `Resolver::from_system()?.resolve(hostname, port).await`.
///
/// # Examples
///
/// ```rust,ignore
/// use lio::net::dns;
///
/// async fn example() -> std::io::Result<()> {
///     let addrs = dns::resolve("example.com", 80).await?;
///     Ok(())
/// }
/// ```
pub async fn resolve(hostname: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
  Resolver::from_system()?.resolve(hostname, port).await
}

/// Convenience function to resolve to a single address using the system's DNS server.
///
/// This is equivalent to `Resolver::from_system()?.resolve_one(hostname, port).await`.
///
/// # Examples
///
/// ```rust,ignore
/// use lio::net::dns;
///
/// async fn example() -> std::io::Result<()> {
///     let addr = dns::resolve_one("example.com", 80).await?;
///     Ok(())
/// }
/// ```
pub async fn resolve_one(hostname: &str, port: u16) -> io::Result<SocketAddr> {
  Resolver::from_system()?.resolve_one(hostname, port).await
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_resolver_new() {
    let ip: IpAddr = "8.8.8.8".parse().unwrap();
    let resolver = Resolver::new(ip);
    assert_eq!(resolver.dns_server(), ip);
  }

  #[test]
  fn test_parse_ip_address() {
    // If input is already an IP, it should parse directly
    let ip: IpAddr = "192.168.1.1".parse().unwrap();
    assert!(matches!(ip, IpAddr::V4(_)));

    let ip: IpAddr = "::1".parse().unwrap();
    assert!(matches!(ip, IpAddr::V6(_)));
  }

  #[test]
  fn test_rand_id() {
    let id1 = rand_id();
    // Just verify it doesn't panic and returns something
    assert!(id1 <= u16::MAX);
  }

  #[cfg(unix)]
  #[test]
  fn test_get_system_dns_server() {
    // This may or may not succeed depending on the system
    let result = get_system_dns_server();
    if let Ok(server) = result {
      assert!(server.is_ipv4() || server.is_ipv6());
    }
  }
}
