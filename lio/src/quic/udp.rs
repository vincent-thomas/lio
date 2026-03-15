//! Async UDP socket for QUIC transport using lio.

use std::io;
use std::net::SocketAddr;

use crate::net::Socket;

/// An async UDP socket for QUIC transport.
pub struct UdpSocket {
    inner: Socket,
    local_addr: SocketAddr,
}

impl UdpSocket {
    /// Binds a UDP socket to the specified address.
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let domain = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
        let socket = Socket::new(domain, libc::SOCK_DGRAM, 0).await?;
        socket.bind(addr).await?;
        let local_addr = socket.local_addr()?;
        Ok(Self { inner: socket, local_addr })
    }

    /// Returns the local address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local_addr)
    }

    /// Sends data to the specified address.
    pub async fn send_to(&self, buf: Vec<u8>, addr: SocketAddr) -> io::Result<usize> {
        let (result, _buf) = crate::api::sendto(&self.inner, buf, addr, None).await;
        Ok(result? as usize)
    }

    /// Receives data from the socket.
    ///
    /// Returns the data, number of bytes received, and source address.
    pub async fn recv_from(&self, buf: Vec<u8>) -> io::Result<(Vec<u8>, usize, SocketAddr)> {
        let (result, buf, addr) = crate::api::recvfrom(&self.inner, buf, None).await;
        let n = result? as usize;
        let addr = addr.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no source address"))?;
        Ok((buf, n, addr))
    }
}
